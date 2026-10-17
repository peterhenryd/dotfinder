//! What the config declares for Homebrew, and lookups against the taps.

use crate::config::Config;
use crate::json::{self, Value};
use crate::util::{run, run_simple};
use std::path::Path;
use std::time::Duration;

#[derive(Default, Debug)]
pub struct Brewfile {
    pub taps: Vec<String>,
    pub brews: Vec<String>,
    pub casks: Vec<String>,
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

/// Casks that would install an app with this bundle name, found by
/// searching the local taps.
pub fn casks_for_app(app_name: &str) -> Vec<String> {
    let needle = format!("app \"{app_name}\"");
    let mut found = Vec::new();
    let taps = crate::util::sys("/opt/homebrew/Library/Taps");
    walk_casks(&taps, &mut |file, text| {
        if text.contains(&needle)
            && let Some(stem) = file.file_stem()
        {
            found.push(stem.to_string_lossy().into_owned());
        }
    });
    found.sort();
    found.dedup();
    found
}

/// The apps a cask installs, as paths relative to /Applications, read from
/// its definition in the local taps (the `target:` path when it has one).
pub fn apps_of_cask(token: &str) -> Vec<String> {
    let mut apps = Vec::new();
    let taps = crate::util::sys("/opt/homebrew/Library/Taps");
    walk_casks_named(&taps, Some(short(token)), &mut |_, text| {
        apps.extend(app_stanzas(text));
    });
    apps.sort();
    apps.dedup();
    apps
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

/// An App Store app's store ID, which `mas` installs it by, from its
/// Spotlight metadata.
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
