//! A small JSON value type with a parser and a printer.
//!
//! Objects use `BTreeMap`, so output is sorted and stable, which keeps
//! snapshots and the files written into the dotfiles repo diffable. Floats are
//! always printed with a decimal point so that Nix's `builtins.fromJSON` reads
//! them back as floats, not integers.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(BTreeMap<String, Value>),
}

impl Value {
    pub fn obj() -> Value {
        Value::Obj(BTreeMap::new())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_obj_mut(&mut self) -> Option<&mut BTreeMap<String, Value>> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }

    /// `value.get("a")` on an object; `None` for anything else.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_obj().and_then(|o| o.get(key))
    }

    /// Follows a `/`-separated path of object keys.
    pub fn pointer(&self, path: &str) -> Option<&Value> {
        let mut cur = self;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = cur.get(part)?;
        }
        Some(cur)
    }

    pub fn to_compact(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, None, 0);
        out
    }

    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, Some(2), 0);
        out.push('\n');
        out
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

// ---------------------------------------------------------------- printing

fn write_value(out: &mut String, v: &Value, indent: Option<usize>, depth: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) => write_float(out, *f),
        Value::Str(s) => write_string(out, s),
        Value::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Value::Obj(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_string(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(n) = indent {
        out.push('\n');
        for _ in 0..n * depth {
            out.push(' ');
        }
    }
}

fn write_float(out: &mut String, f: f64) {
    if !f.is_finite() {
        // JSON has no NaN or infinity.
        out.push_str("null");
    } else if f.fract() == 0.0 && f.abs() < 1e16 {
        let _ = write!(out, "{f:.1}");
    } else {
        let _ = write!(out, "{f}");
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ----------------------------------------------------------------- parsing

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(input: &str) -> Result<Value, ParseError> {
    let mut p = Parser {
        s: input.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> ParseError {
        ParseError {
            message: msg.to_string(),
            offset: self.i,
        }
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn expect(&mut self, lit: &str) -> Result<(), ParseError> {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(())
        } else {
            Err(self.err(&format!("expected `{lit}`")))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > 256 {
            return Err(self.err("nesting too deep"));
        }
        match self.peek() {
            Some(b'n') => self.expect("null").map(|_| Value::Null),
            Some(b't') => self.expect("true").map(|_| Value::Bool(true)),
            Some(b'f') => self.expect("false").map(|_| Value::Bool(false)),
            Some(b'"') => self.string().map(Value::Str),
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(Value::Arr(items));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::Arr(items));
                        }
                        _ => return Err(self.err("expected `,` or `]`")),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut map = BTreeMap::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(Value::Obj(map));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return Err(self.err("expected a string key"));
                    }
                    let k = self.string()?;
                    self.ws();
                    self.expect(":")?;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    map.insert(k, v);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Obj(map));
                        }
                        _ => return Err(self.err("expected `,` or `}`")),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        let mut float = false;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' | b'+' | b'-' => float = true,
                _ => break,
            }
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap();
        if !float && let Ok(i) = text.parse::<i64>() {
            return Ok(Value::Int(i));
        }
        text.parse::<f64>()
            .map(Value::Float)
            .map_err(|_| self.err("invalid number"))
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.err("short \\u escape"))?;
        let h = std::str::from_utf8(h).map_err(|_| self.err("bad \\u escape"))?;
        let n = u32::from_str_radix(h, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'"' && self.s[self.i] != b'\\' {
                if self.s[self.i] < 0x20 {
                    return Err(self.err("control character in string"));
                }
                self.i += 1;
            }
            out.push_str(std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("invalid UTF-8"))?);
            match self.peek() {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                self.expect("\\u")?;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.err("invalid surrogate pair"));
                                }
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            }
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                _ => return Err(self.err("unterminated string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let src = r#"{"a":[1,2.5,-3,1e3,true,false,null],"b":{"c":"x\"y\\z\né😀"}}"#;
        let v = parse(src).unwrap();
        let again = parse(&v.to_compact()).unwrap();
        assert_eq!(v, again);
        assert_eq!(v.pointer("b/c").unwrap().as_str().unwrap(), "x\"y\\z\né😀");
    }

    #[test]
    fn floats_keep_a_decimal_point() {
        assert_eq!(Value::Float(2.0).to_compact(), "2.0");
        assert_eq!(Value::Float(0.15).to_compact(), "0.15");
        assert_eq!(Value::Int(2).to_compact(), "2");
    }

    #[test]
    fn pretty_is_stable() {
        let v = parse(r#"{"b":1,"a":[]}"#).unwrap();
        assert_eq!(v.to_pretty(), "{\n  \"a\": [],\n  \"b\": 1\n}\n");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("{").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("1 2").is_err());
    }
}
