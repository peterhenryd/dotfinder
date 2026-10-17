//! Parser for XML property lists, the format `defaults export` and
//! `plutil -convert xml1` produce.
//!
//! Values map onto `json::Value`. The two plist types JSON lacks are kept
//! distinguishable so they are never mistaken for strings:
//! - `<date>` becomes `{"$date": "2026-10-16T12:00:00Z"}`
//! - `<data>` becomes `{"$data": "<n> bytes #<fnv64>"}`: a size and a hash
//!   instead of the bytes, which keeps snapshots small and keeps binary
//!   blobs (often tokens or bookmarks) out of them.

use crate::json::Value;
use crate::util::fnv64;
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct PlistError(pub String);

impl std::fmt::Display for PlistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid plist: {}", self.0)
    }
}

impl std::error::Error for PlistError {}

pub fn parse(xml: &str) -> Result<Value, PlistError> {
    let mut lx = Lexer { s: xml, i: 0 };
    // Skip the prolog up to and including <plist ...>.
    loop {
        match lx.next_tag()? {
            Some(Tag::Open(name)) if name == "plist" => break,
            Some(Tag::Empty(name)) if name == "plist" => return Ok(Value::obj()),
            Some(_) => continue,
            None => return Err(PlistError("no <plist> element".into())),
        }
    }
    let first = lx.next_tag()?.ok_or_else(|| PlistError("empty plist".into()))?;
    if let Tag::Close(name) = &first
        && name == "plist"
    {
        return Ok(Value::obj());
    }
    lx.value(first, 0)
}

enum Tag {
    Open(String),
    Close(String),
    Empty(String),
}

struct Lexer<'a> {
    s: &'a str,
    i: usize,
}

impl Lexer<'_> {
    /// Next element tag, skipping text, comments, `<?...?>` and `<!...>`.
    fn next_tag(&mut self) -> Result<Option<Tag>, PlistError> {
        loop {
            let Some(rel) = self.s[self.i..].find('<') else {
                return Ok(None);
            };
            self.i += rel;
            let rest = &self.s[self.i..];
            if rest.starts_with("<!--") {
                let end = rest
                    .find("-->")
                    .ok_or_else(|| PlistError("unterminated comment".into()))?;
                self.i += end + 3;
                continue;
            }
            if rest.starts_with("<?") || rest.starts_with("<!") {
                let end = rest
                    .find('>')
                    .ok_or_else(|| PlistError("unterminated declaration".into()))?;
                self.i += end + 1;
                continue;
            }
            let end = rest
                .find('>')
                .ok_or_else(|| PlistError("unterminated tag".into()))?;
            let inner = &rest[1..end];
            self.i += end + 1;
            if let Some(name) = inner.strip_prefix('/') {
                return Ok(Some(Tag::Close(name.trim().to_string())));
            }
            let (body, empty) = match inner.strip_suffix('/') {
                Some(b) => (b, true),
                None => (inner, false),
            };
            let name = body.split_whitespace().next().unwrap_or("").to_string();
            return Ok(Some(if empty { Tag::Empty(name) } else { Tag::Open(name) }));
        }
    }

    /// Raw text up to the closing tag `</name>`, with entities decoded.
    fn text_until_close(&mut self, name: &str) -> Result<String, PlistError> {
        let close = format!("</{name}>");
        let rel = self.s[self.i..]
            .find(&close)
            .ok_or_else(|| PlistError(format!("missing {close}")))?;
        let raw = &self.s[self.i..self.i + rel];
        self.i += rel + close.len();
        Ok(decode_entities(raw))
    }

    fn value(&mut self, tag: Tag, depth: usize) -> Result<Value, PlistError> {
        if depth > 128 {
            return Err(PlistError("nesting too deep".into()));
        }
        match tag {
            Tag::Empty(name) => Ok(match name.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "dict" => Value::obj(),
                "array" => Value::Arr(Vec::new()),
                "string" => Value::Str(String::new()),
                "data" => data_value(""),
                other => return Err(PlistError(format!("unexpected <{other}/>"))),
            }),
            Tag::Close(name) => Err(PlistError(format!("unexpected </{name}>"))),
            Tag::Open(name) => match name.as_str() {
                "string" => Ok(Value::Str(self.text_until_close("string")?)),
                "integer" => {
                    let t = self.text_until_close("integer")?;
                    let t = t.trim();
                    if let Ok(i) = t.parse::<i64>() {
                        Ok(Value::Int(i))
                    } else if let Ok(u) = t.parse::<u64>() {
                        // Beyond i64: keep it exact as a string rather than
                        // rounding it through a float.
                        Ok(Value::Str(u.to_string()))
                    } else {
                        Err(PlistError(format!("bad integer `{t}`")))
                    }
                }
                "real" => {
                    let t = self.text_until_close("real")?;
                    t.trim()
                        .parse::<f64>()
                        .map(Value::Float)
                        .map_err(|_| PlistError(format!("bad real `{t}`")))
                }
                "date" => {
                    let t = self.text_until_close("date")?;
                    let mut m = BTreeMap::new();
                    m.insert("$date".to_string(), Value::Str(t.trim().to_string()));
                    Ok(Value::Obj(m))
                }
                "data" => {
                    let t = self.text_until_close("data")?;
                    Ok(data_value(&t))
                }
                "array" => {
                    let mut items = Vec::new();
                    loop {
                        let t = self
                            .next_tag()?
                            .ok_or_else(|| PlistError("unterminated <array>".into()))?;
                        if let Tag::Close(n) = &t
                            && n == "array"
                        {
                            return Ok(Value::Arr(items));
                        }
                        items.push(self.value(t, depth + 1)?);
                    }
                }
                "dict" => {
                    let mut map = BTreeMap::new();
                    loop {
                        let t = self
                            .next_tag()?
                            .ok_or_else(|| PlistError("unterminated <dict>".into()))?;
                        let key = match t {
                            Tag::Close(n) if n == "dict" => return Ok(Value::Obj(map)),
                            Tag::Open(n) if n == "key" => self.text_until_close("key")?,
                            Tag::Empty(n) if n == "key" => String::new(),
                            _ => return Err(PlistError("expected <key> in <dict>".into())),
                        };
                        let vt = self
                            .next_tag()?
                            .ok_or_else(|| PlistError("missing value in <dict>".into()))?;
                        let v = self.value(vt, depth + 1)?;
                        map.insert(key, v);
                    }
                }
                other => Err(PlistError(format!("unknown element <{other}>"))),
            },
        }
    }
}

fn data_value(b64: &str) -> Value {
    let bytes = base64_decode(b64);
    let mut m = BTreeMap::new();
    m.insert(
        "$data".to_string(),
        Value::Str(format!("{} bytes #{:016x}", bytes.len(), fnv64(&bytes))),
    );
    Value::Obj(m)
}

fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(semi) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let ent = &rest[1..semi];
        let decoded = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = ent.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn base64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if let Some(v) = val(c) {
            buf = (buf << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
                buf &= (1 << bits) - 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>autohide</key>
	<true/>
	<key>autohide-delay</key>
	<real>0.0</real>
	<key>tilesize</key>
	<integer>48</integer>
	<key>orientation</key>
	<string>bottom &amp; &lt;left&gt; &#233;</string>
	<key>empty</key>
	<string/>
	<key>persistent-apps</key>
	<array>
		<dict>
			<key>tile-data</key>
			<dict>
				<key>file-label</key>
				<string>Ghostty</string>
			</dict>
		</dict>
	</array>
	<key>blob</key>
	<data>
	aGVsbG8=
	</data>
	<key>when</key>
	<date>2026-10-16T12:00:00Z</date>
	<key>big</key>
	<integer>18446744073709551615</integer>
</dict>
</plist>
"#;

    #[test]
    fn parses_every_type() {
        let v = parse(SAMPLE).unwrap();
        assert_eq!(v.get("autohide"), Some(&Value::Bool(true)));
        assert_eq!(v.get("autohide-delay"), Some(&Value::Float(0.0)));
        assert_eq!(v.get("tilesize"), Some(&Value::Int(48)));
        assert_eq!(v.get("orientation").unwrap().as_str(), Some("bottom & <left> é"));
        assert_eq!(v.get("empty").unwrap().as_str(), Some(""));
        assert_eq!(
            v.pointer("persistent-apps").unwrap().as_arr().unwrap()[0]
                .pointer("tile-data/file-label")
                .unwrap()
                .as_str(),
            Some("Ghostty")
        );
        assert!(
            v.pointer("blob/$data")
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("5 bytes #")
        );
        assert_eq!(
            v.pointer("when/$date").unwrap().as_str(),
            Some("2026-10-16T12:00:00Z")
        );
        assert_eq!(v.get("big").unwrap().as_str(), Some("18446744073709551615"));
    }

    #[test]
    fn empty_domains() {
        assert_eq!(
            parse("<plist version=\"1.0\"><dict/></plist>").unwrap(),
            Value::obj()
        );
        assert_eq!(parse("<plist version=\"1.0\"/>").unwrap(), Value::obj());
    }

    #[test]
    fn base64() {
        assert_eq!(base64_decode("aGVsbG8="), b"hello");
        assert_eq!(base64_decode("aGVs\n bG8="), b"hello");
    }
}
