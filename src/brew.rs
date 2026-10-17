//! What the config declares for Homebrew, and lookups against the taps.

use crate::config::Config;
use crate::json::{self, Value};
use crate::util::{run, run_simple};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Default, Debug)]
pub struct Brewfile {
    pub taps: Vec<String>,
    pub brews: Vec<String>,
    pub casks: Vec<String>,
    /// App Store app IDs (`mas "Name", id: 123`).
    pub mas: Vec<i64>,
}

impl Brewfile {
    pub fn parse(text: &str) -> Brewfile {
        let mut b = Brewfile::default();
        for line in text.lines() {
            let line = line.trim();
            let Some((kind, rest)) = line.split_once(' ') else {
                continue;
            };
            let Some(name) = rest.trim().strip_prefix('"').and_then(|r| r.split('"').next()) else {
                continue;
            };
            match kind {
                "tap" => b.taps.push(name.to_string()),
                "brew" => b.brews.push(name.to_string()),
                "cask" => b.casks.push(name.to_string()),
                "mas" => {
                    if let Some(id) = rest
                        .split("id:")
                        .nth(1)
                        .and_then(|r| r.trim().trim_end_matches(',').split(',').next())
                        .and_then(|n| n.trim().parse::<i64>().ok())
                    {
                        b.mas.push(id);
                    }
                }
                _ => {}
            }
        }
        b
    }

    /// Whether `name` (as `brew` lists it, without a tap prefix) is declared.
    pub fn declares_brew(&self, name: &str) -> bool {
        self.brews.iter().any(|b| short(b) == name)
    }

    pub fn declares_cask(&self, token: &str) -> bool {
        self.casks.iter().any(|c| short(c) == token)
    }
}

/// `nikitabobko/tap/aerospace` → `aerospace`.
pub fn short(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// The Brewfile of the system that's currently active, found through the
/// activation script nix-darwin generated.
pub fn current_brewfile() -> Option<Brewfile> {
    let script = std::fs::read_to_string(crate::util::sys("/run/current-system/activate")).ok()?;
    let start = script.find("--file='")? + "--file='".len();
    let end = script[start..].find('\'')? + start;
    let text = std::fs::read_to_string(&script[start..end]).ok()?;
    Some(Brewfile::parse(&text))
}

pub fn host(cfg: &Config) -> Option<String> {
    if let Some(h) = &cfg.host {
        return Some(h.clone());
    }
    let out = run_simple(&["scutil", "--get", "LocalHostName"]).ok()?;
    out.ok().then(|| out.stdout.trim().to_string())
}

/// The Brewfile the dotfiles repo declares right now, including edits that
/// haven't been applied yet, by evaluating the flake.
pub fn declared_brewfile(cfg: &Config) -> Result<Brewfile, String> {
    let host = host(cfg).ok_or("couldn't determine the host name; set `host` in the config")?;
    let attr = format!(
        "{}#darwinConfigurations.\"{host}\".config.homebrew.brewfile",
        cfg.dotfiles.display()
    );
    let out = run(&["nix", "eval", "--raw", &attr], None, Duration::from_secs(300))
        .map_err(|e| format!("`nix`: {e}"))?;
    if !out.ok() {
        let msg: Vec<&str> = out.stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        return Err(format!("evaluating the config failed:\n{}", msg.join("\n")));
    }
    Ok(Brewfile::parse(&out.stdout))
}

/// The tap a formula or cask comes from, e.g. `homebrew/core`.
pub fn tap_of(cask: bool, name: &str) -> Option<String> {
    let kind = if cask { "--cask" } else { "--formula" };
    let out = run(
        &["brew", "info", kind, "--json=v2", name],
        None,
        Duration::from_secs(60),
    )
    .ok()?;
    if !out.ok() {
        return None;
    }
    let v = json::parse(&out.stdout).ok()?;
    let list = if cask { "casks" } else { "formulae" };
    v.get(list)?
        .as_arr()?
        .first()?
        .get("tap")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Whether a tap is part of the config. `homebrew/core` and `homebrew/cask`
/// count as declared under either spelling.
pub fn tap_declared(bf: &Brewfile, tap: &str) -> bool {
    let norm = |t: &str| t.replace("/homebrew-", "/").to_lowercase();
    bf.taps.iter().any(|t| norm(t) == norm(tap))
}

/// Every cask definition in the local taps, as (token, text), read once.
fn tap_casks() -> &'static [(String, String)] {
    static CASKS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    CASKS.get_or_init(|| {
        let mut all = Vec::new();
        let taps = crate::util::sys("/opt/homebrew/Library/Taps");
        walk_casks(&taps, &mut |file, text| {
            if let Some(stem) = file.file_stem() {
                all.push((stem.to_string_lossy().into_owned(), text.to_string()));
            }
        });
        all
    })
}

/// Casks that would install this app, best match first, and whether they
/// were found by bundle ID. Casks are looked up by the app's bundle name
/// (`Foo.app`); when none installs it under that name (it was renamed, or
/// the cask uses an installer package), by its bundle ID, which casks name
/// in their uninstall and zap steps. Other casks can name it too, such as a
/// plugin for it, so matches are ranked.
pub fn casks_for_app(app_name: &str, bundle_id: &str) -> (Vec<String>, bool) {
    let needle = format!("app \"{app_name}\"");
    let display = app_name.trim_end_matches(".app");
    let found = |pred: &dyn Fn(&str) -> bool| -> Vec<String> {
        let mut found: Vec<(i32, String)> = tap_casks()
            .iter()
            .filter(|(_, text)| pred(text) && disabled_reason(text).is_none())
            .map(|(token, text)| (-rank(token, text, display, bundle_id), token.clone()))
            .collect();
        found.sort();
        found.dedup();
        found.into_iter().map(|(_, t)| t).collect()
    };
    let by_name = found(&|text| text.contains(&needle));
    if !by_name.is_empty() || bundle_id.is_empty() {
        return (by_name, false);
    }
    let by_id = found(&|text| crate::snapshot::mentions_id(text, bundle_id));
    let any = !by_id.is_empty();
    (by_id, any)
}

/// How likely a cask is to be the one that installs this app: its name
/// stanza or token matches the app's name, and it names the bundle ID as the
/// app's own (its preferences or saved state) rather than inside another
/// app's folder.
fn rank(token: &str, text: &str, display: &str, bundle_id: &str) -> i32 {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let mut score = 0;
    let wanted = format!("name \"{}\"", display.to_lowercase());
    if text.lines().any(|l| l.trim().to_lowercase() == wanted) {
        score += 4;
    }
    if norm(token) == norm(display) {
        score += 4;
    }
    if !bundle_id.is_empty()
        && [".plist", ".savedState", "\"", "*"]
            .iter()
            .any(|end| text.contains(&format!("{bundle_id}{end}")))
    {
        score += 2;
    }
    score
}

/// What a cask's definition in the local taps says: the apps it installs,
/// as paths relative to /Applications (the `target:` path when it has one),
/// and why Homebrew refuses to install it, if it does.
#[derive(Default)]
pub struct CaskInfo {
    pub apps: Vec<String>,
    pub disabled: Option<String>,
}

pub fn cask_info(token: &str) -> CaskInfo {
    let mut info = CaskInfo::default();
    let taps = crate::util::sys("/opt/homebrew/Library/Taps");
    walk_casks_named(&taps, Some(short(token)), &mut |_, text| {
        info.apps.extend(app_stanzas(text));
        info.disabled = info.disabled.take().or_else(|| disabled_reason(text));
    });
    info.apps.sort();
    info.apps.dedup();
    info
}

/// Why a cask is disabled (`disable! ... because: ...`), in words; None when
/// it isn't. Homebrew refuses to install a disabled cask, which fails the
/// whole `brew bundle` and with it the switch.
fn disabled_reason(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| l.starts_with("disable!"))?;
    let because = line.split("because:").nth(1).map(str::trim).unwrap_or("");
    let reason = if let Some(sym) = because.strip_prefix(':') {
        match sym.trim_end_matches(',') {
            "fails_gatekeeper_check" => "it doesn't pass macOS's Gatekeeper check".to_string(),
            "discontinued" => "it's discontinued".to_string(),
            "no_longer_available" => "it's no longer available".to_string(),
            "unmaintained" => "it's unmaintained".to_string(),
            other => other.replace('_', " "),
        }
    } else {
        because.trim_matches(|c| c == '"' || c == ',').to_string()
    };
    Some(if reason.is_empty() {
        "Homebrew disabled it".into()
    } else {
        reason
    })
}

/// `app "X.app"` and `app "X.app", target: "Y.app"` lines in a cask file.
fn app_stanzas(text: &str) -> Vec<String> {
    let quoted = |s: &str| -> Option<String> {
        let start = s.find('"')? + 1;
        let len = s[start..].find('"')?;
        Some(s[start..start + len].to_string())
    };
    text.lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("app "))
        .filter_map(|rest| match rest.find("target:") {
            Some(i) => quoted(&rest[i..]),
            None => quoted(rest).map(|name| name.rsplit('/').next().unwrap_or(&name).to_string()),
        })
        .collect()
}

/// An App Store app's store ID, from its Spotlight metadata.
pub fn app_store_id(app: &Path) -> Option<i64> {
    let path = app.to_string_lossy();
    let out = run(
        &["mdls", "-raw", "-name", "kMDItemAppStoreAdamID", &path],
        None,
        Duration::from_secs(10),
    )
    .ok()?;
    out.stdout.trim().parse::<i64>().ok().filter(|id| *id > 0)
}

/// App Store apps as `mas list` reports them, or None without `mas`.
pub fn mas_list() -> Option<Vec<(i64, String)>> {
    let out = run_simple(&["mas", "list"]).ok()?;
    out.ok().then(|| parse_mas_list(&out.stdout))
}

fn parse_mas_list(text: &str) -> Vec<(i64, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (id, rest) = l.split_once(char::is_whitespace)?;
            let id = id.parse::<i64>().ok()?;
            let name = match rest.rfind('(') {
                Some(i) => &rest[..i],
                None => rest,
            };
            Some((id, name.trim().to_string()))
        })
        .collect()
}

fn walk_casks(dir: &Path, f: &mut dyn FnMut(&Path, &str)) {
    walk_casks_named(dir, None, f)
}

/// Like `walk_casks`, but with `token`, only reads the cask file of that name.
fn walk_casks_named(dir: &Path, token: Option<&str>, f: &mut dyn FnMut(&Path, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        // Follow symlinks: nix-homebrew's Taps folder is one.
        if std::fs::metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
            walk_casks_named(&p, token, f);
        } else if p.extension().is_some_and(|x| x == "rb")
            && token.is_none_or(|t| p.file_stem().is_some_and(|s| s == t))
            && p.components().any(|c| c.as_os_str() == "Casks")
            && let Ok(text) = std::fs::read_to_string(&p)
        {
            f(&p, &text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brewfile_app_store_entries() {
        let b = Brewfile::parse("brew \"mas\"\nmas \"AdGuard Mini\", id: 1440147259\n");
        assert_eq!(b.mas, [1440147259]);
    }

    /// `mas list` lines: id, name, version.
    #[test]
    fn mas_list_lines() {
        let out = "1440147259  AdGuard Mini            (2.1.0)\n   462058435  Microsoft Word  (16.90)\n";
        assert_eq!(
            parse_mas_list(out),
            [
                (1440147259, "AdGuard Mini".to_string()),
                (462058435, "Microsoft Word".to_string())
            ]
        );
    }

    #[test]
    fn ranks_the_apps_own_cask_first() {
        let word = "cask \"microsoft-word\" do\n  name \"Microsoft Word\"\n  pkg \"Microsoft_Word_Installer.pkg\"\n  zap trash: [\n    \"~/Library/Containers/com.microsoft.Word*\",\n    \"~/Library/Preferences/com.microsoft.Word.plist\",\n  ]\nend\n";
        let bookends = "cask \"bookends\" do\n  name \"Bookends\"\n  app \"Bookends.app\"\n  zap trash: \"~/Library/Application Scripts/com.microsoft.Word/Bookends.scpt\"\nend\n";
        let id = "com.microsoft.Word";
        assert!(
            rank("microsoft-word", word, "Microsoft Word", id)
                > rank("bookends", bookends, "Microsoft Word", id)
        );
        assert_eq!(rank("bookends", bookends, "Microsoft Word", id), 0);
    }

    #[test]
    fn disabled_casks() {
        let text = "cask \"openemu\" do\n  app \"OpenEmu.app\"\n  disable! date: \"2026-09-01\", because: :fails_gatekeeper_check\nend\n";
        assert_eq!(
            disabled_reason(text).as_deref(),
            Some("it doesn't pass macOS's Gatekeeper check")
        );
        assert_eq!(disabled_reason("cask \"ok\" do\nend\n"), None);
        assert_eq!(
            disabled_reason("  disable! date: \"2026-01-01\", because: \"the vendor pulled it\"\n")
                .as_deref(),
            Some("the vendor pulled it")
        );
    }

    #[test]
    fn cask_app_stanzas() {
        let text = "cask \"foo\" do\n  app \"Foo.app\"\n  app \"Foo Helper.app\", target: \"Foo Tools/Helper.app\"\n  binary \"#{appdir}/Foo.app/Contents/MacOS/foo\"\nend\n";
        assert_eq!(app_stanzas(text), ["Foo.app", "Foo Tools/Helper.app"]);
    }

    #[test]
    fn parses_nix_darwin_brewfile() {
        let text = "# Created by `nix-darwin`'s `homebrew` module\n\n# Taps\ntap \"homebrew/homebrew-cask\"\ntap \"nikitabobko/homebrew-tap\"\n\n# Brews\nbrew \"cmake\", trusted: true\n\n# Casks\ncask \"nikitabobko/tap/aerospace\", trusted: true\ncask \"ghostty\", trusted: true\n";
        let b = Brewfile::parse(text);
        assert_eq!(b.taps, ["homebrew/homebrew-cask", "nikitabobko/homebrew-tap"]);
        assert!(b.declares_brew("cmake"));
        assert!(b.declares_cask("aerospace"));
        assert!(b.declares_cask("ghostty"));
        assert!(!b.declares_cask("spotify"));
        assert!(tap_declared(&b, "homebrew/cask"));
        assert!(tap_declared(&b, "nikitabobko/tap"));
        assert!(!tap_declared(&b, "homebrew/core"));
    }
}
