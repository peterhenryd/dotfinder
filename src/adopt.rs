//! `dotfinder adopt`: walks through changes one at a time and either adds
//! them to the config (via `dotfinder/adopted.json`) or ignores them from
//! then on (via `dotfinder/ignored.json`).

use crate::brew::{self, Brewfile};
use crate::config::Config;
use crate::diff::{Change, Kind};
use crate::json::Value;
use crate::report::{describe, marker, section_label};
use crate::state::{AdoptedFile, IgnoredFile, Rename};
use crate::term::{bold, choose, dim, yellow};
use crate::util::{expand, tilde};

/// Why a preference value can't go into the config, if it can't.
fn unadoptable(key: &str, v: &Value) -> Option<&'static str> {
    if contains_special(v) {
        return Some("it contains a date or binary data, which nix-darwin can't write");
    }
    if looks_secret(key, v) {
        return Some("it looks like a secret, and the config is a public repo");
    }
    None
}

fn contains_special(v: &Value) -> bool {
    match v {
        Value::Obj(o) => {
            o.contains_key("$date") || o.contains_key("$data") || o.values().any(contains_special)
        }
        Value::Arr(a) => a.iter().any(contains_special),
        _ => false,
    }
}

pub fn looks_secret(key: &str, v: &Value) -> bool {
    let k = key.to_lowercase();
    let words = [
        "token",
        "secret",
        "password",
        "passwd",
        "apikey",
        "api_key",
        "credential",
        "private",
        "cookie",
        "session",
        "auth",
    ];
    if words.iter().any(|w| k.contains(w)) {
        return true;
    }
    match v {
        Value::Str(s) => {
            let s = s.trim();
            s.len() >= 32
                && !s.contains(' ')
                && !s.contains('/') // paths and URLs
                && s.chars().all(|c| c.is_ascii_alphanumeric() || "+=_-.".contains(c))
                && s.chars().any(|c| c.is_ascii_digit())
        }
        Value::Obj(o) => o.iter().any(|(k, v)| looks_secret(k, v)),
        Value::Arr(a) => a.iter().any(|v| looks_secret("", v)),
        _ => false,
    }
}

const IGNORE: (char, &str) = ('i', "ignore from now on");
const PRIVATE: (char, &str) = ('p', "ignore privately");
const SKIP: (char, &str) = ('s', "skip for now");
const QUIT: (char, &str) = ('q', "save and quit");

/// The name to add to the config for a Homebrew package, with its tap
/// when it isn't Homebrew's own. Errors when the tap isn't in the config.
fn brew_entry(cask: bool, name: &str, declared: Option<&Brewfile>) -> Result<String, String> {
    match brew::tap_of(cask, name) {
        Some(tap) if declared.is_some_and(|bf| !brew::tap_declared(bf, &tap)) => Err(format!(
            "It comes from the tap `{tap}`, which the config doesn't include. Add the tap as a flake input in flake.nix and modules/darwin/homebrew.nix first."
        )),
        Some(tap) if tap != "homebrew/core" && tap != "homebrew/cask" => Ok(format!("{tap}/{name}")),
        _ => Ok(name.to_string()),
    }
}

pub fn adopt(cfg: &Config, mut changes: Vec<Change>, declared: Option<&Brewfile>) -> Result<(), String> {
    if changes.is_empty() {
        println!("Nothing to go through.");
        return Ok(());
    }
    let mut adopted = AdoptedFile::load(cfg)?;
    let mut ignored = IgnoredFile::load(cfg)?;
    let mut private = IgnoredFile::load_private()?;
    let (mut n_adopted, mut n_ignored, mut n_private) = (0, 0, 0);
    let mut ignored_parents: Vec<String> = Vec::new();

    changes.sort_by(|a, b| a.section.cmp(&b.section).then(a.item.cmp(&b.item)));
    let total = changes.len();
    'outer: for (i, c) in changes.iter().enumerate() {
        if ignored_parents.contains(&c.parent_id()) {
            continue;
        }
        println!(
            "\n{} {}\n  {} {}",
            dim(&format!("[{}/{total}]", i + 1)),
            bold(&section_label(&c.section)),
            marker(c.kind),
            describe(c)
        );

        let mut options: Vec<(char, &str)> = Vec::new();
        let mut note: Option<String> = None;
        let section = c.section.as_str();
        let value = c.new.as_ref();
        // For apps: the bundle's name without ".app", a cask that installs
        // it, and its App Store ID.
        let app_name = c
            .item
            .rsplit('/')
            .next()
            .unwrap_or(&c.item)
            .trim_end_matches(".app")
            .to_string();
        let mut app_cask: Option<String> = None;
        let mut app_store_id: Option<i64> = None;
        let mut app_rename: Option<(String, Rename)> = None;

        match (section, c.kind, &c.key) {
            ("homebrew-casks" | "homebrew-formulae", Kind::Added, None) => {
                options.push(('a', "add to the config"));
            }
            ("homebrew-casks" | "homebrew-formulae", Kind::Removed, None) => {
                note = Some(
                    "Removed since the baseline. If the config declares it, the next rebuild reinstalls it."
                        .into(),
                );
            }
            ("defaults", Kind::Added | Kind::Changed, Some(k)) => {
                match unadoptable(k, value.unwrap()) {
                    None => options.push(('a', "add to the config")),
                    Some(why) => note = Some(format!("Can't be added to the config: {why}.")),
                }
                options.push(('d', "ignore this whole domain"));
            }
            ("defaults", Kind::Added, None) => {
                options.push(('a', "add all its keys to the config"));
                options.push(('d', "ignore this whole domain"));
            }
            ("defaults", Kind::Removed, _) => {
                note = Some("nix-darwin can only set keys, not delete them.".into());
                if c.key.is_some() {
                    options.push(('d', "ignore this whole domain"));
                }
            }
            ("defaults-currenthost" | "container-defaults", _, key) => {
                note = Some(if section == "container-defaults" {
                    "Sandboxed apps keep preferences in their container, which nix-darwin doesn't write to."
                        .into()
                } else {
                    "Per-Mac (-currentHost) preferences aren't supported by nix-darwin's CustomUserPreferences.".into()
                });
                if key.is_some() {
                    options.push(('d', "ignore this whole domain"));
                }
            }
            ("apps", Kind::Added, None) => {
                let source = value
                    .and_then(|v| v.get("source"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let (Some(token), Some(from)) = (
                    source.strip_prefix("homebrew:"),
                    value.and_then(|v| v.get("renamed_from")).and_then(Value::as_str),
                ) {
                    match c.item.strip_prefix("/Applications/") {
                        Some(to) => {
                            note = Some(format!(
                                "Renamed from {from}, the name the cask `{token}` installs it under."
                            ));
                            app_rename = Some((
                                token.to_string(),
                                Rename {
                                    from: from.to_string(),
                                    to: to.to_string(),
                                },
                            ));
                            options.push(('r', "keep the new name (switches rename it on any Mac)"));
                        }
                        None => {
                            note = Some(format!(
                                "Moved and renamed from /Applications/{from}; only renames inside /Applications can be kept."
                            ))
                        }
                    }
                } else if matches!(source, "manual" | "app-store") {
                    let id = value
                        .and_then(|v| v.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    app_cask = brew::casks_for_app(&format!("{app_name}.app"), id)
                        .into_iter()
                        .next();
                    if source == "app-store" {
                        app_store_id = brew::app_store_id(&expand(&c.item));
                    }
                    let mut lines: Vec<String> = Vec::new();
                    match &app_cask {
                        Some(cask) => lines.push(format!("Homebrew has it as the cask `{cask}`.")),
                        None if source == "manual" => {
                            lines.push("Installed by hand, and no cask installs it.".into())
                        }
                        None => {}
                    }
                    if source == "app-store" && app_store_id.is_none() {
                        lines.push(
                            "Its App Store ID couldn't be read, so it can't be added from the App Store."
                                .into(),
                        );
                    }
                    if !lines.is_empty() {
                        note = Some(lines.join(" "));
                    }
                    if app_cask.is_some() {
                        options.push(('c', "use the cask"));
                    }
                    if app_store_id.is_some() {
                        options.push(('m', "list it as an App Store app"));
                    }
                    options.push(('n', "list it as an app to install by hand"));
                }
            }
            (s, Kind::Added, None) if s.starts_with("dir:") => {
                note = Some(format!(
                    "To manage it, move it into your dotfiles repo and link it from a home-manager module. Check it for secrets first: {} is public.",
                    tilde(&cfg.dotfiles)
                ));
            }
            ("cmd:login-items", Kind::Added, None) => {
                note = Some(
                    "Login items aren't part of the config. Ignore it if you expect it to start at login."
                        .into(),
                );
            }
            ("cmd:rustup-toolchain", Kind::Added, None) => {
                note = Some("Toolchains are managed by rustup; a rust-toolchain.toml in each project pins the version it needs.".into());
            }
            (s, Kind::Added, None) if s.starts_with("cmd:") => {
                note = Some("To make it reproducible, add it to home/packages.nix (if nixpkgs has it) and uninstall this copy.".into());
            }
            _ => {}
        }
        if let Some(n) = &note {
            println!("  {}", yellow(n));
        }
        options.push(IGNORE);
        options.push(PRIVATE);
        options.push(SKIP);
        options.push(QUIT);

        let Some(answer) = choose("  ", &options) else {
            break 'outer;
        };
        match answer {
            'a' => match (section, &c.key) {
                ("homebrew-casks" | "homebrew-formulae", None) => {
                    let cask = section == "homebrew-casks";
                    match brew_entry(cask, &c.item, declared) {
                        Err(why) => {
                            println!("  {}", yellow(&why));
                            continue;
                        }
                        Ok(name) if cask => adopted.casks.push(name),
                        Ok(name) => adopted.brews.push(name),
                    }
                    n_adopted += 1;
                }
                ("defaults", Some(k)) => {
                    adopted.set_default(&c.item, k, value.unwrap().clone());
                    n_adopted += 1;
                }
                ("defaults", None) => {
                    let mut skipped = Vec::new();
                    for (k, v) in value.and_then(Value::as_obj).into_iter().flatten() {
                        match unadoptable(k, v) {
                            None => {
                                adopted.set_default(&c.item, k, v.clone());
                                n_adopted += 1;
                            }
                            Some(_) => skipped.push(k.clone()),
                        }
                    }
                    if !skipped.is_empty() {
                        println!(
                            "  {}",
                            yellow(&format!(
                                "Left out (dates, data or secrets): {}",
                                skipped.join(", ")
                            ))
                        );
                    }
                }
                _ => {}
            },
            'c' => {
                let cask = app_cask.clone().unwrap_or_default();
                match brew_entry(true, &cask, declared) {
                    Err(why) => {
                        println!("  {}", yellow(&why));
                        continue;
                    }
                    Ok(name) => adopted.casks.push(name),
                }
                n_adopted += 1;
                println!(
                    "  {}",
                    dim(&format!(
                        "Before the next rebuild, quit {app_name} and move it to the Trash, so Homebrew can install its own copy. Its data in ~/Library stays."
                    ))
                );
            }
            'm' => {
                if let Some(id) = app_store_id {
                    adopted.mas_apps.insert(app_name.clone(), id);
                    n_adopted += 1;
                    println!(
                        "  {}",
                        dim("On a Mac without it, `dotfinder audit` links to its App Store page.")
                    );
                }
            }
            'n' => {
                adopted.manual_apps.push(app_name.clone());
                n_adopted += 1;
            }
            'r' => {
                if let Some((token, r)) = app_rename.clone() {
                    adopted.renames.insert(token, r);
                    n_adopted += 1;
                }
            }
            'i' => {
                ignored.add(c.id());
                n_ignored += 1;
            }
            'p' => {
                private.add(c.id());
                n_private += 1;
            }
            'd' => {
                ignored.add(c.parent_id());
                ignored_parents.push(c.parent_id());
                n_ignored += 1;
            }
            'q' => break 'outer,
            _ => {}
        }
    }

    adopted
        .save()
        .map_err(|e| format!("{}: {e}", cfg.adopted_path().display()))?;
    ignored
        .save()
        .map_err(|e| format!("{}: {e}", cfg.ignored_path().display()))?;
    if n_private > 0 {
        private
            .save()
            .map_err(|e| format!("{}: {e}", crate::state::private_ignored_path().display()))?;
    }
    println!("\n{n_adopted} added to the config, {n_ignored} ignored, {n_private} ignored privately.");
    if n_private > 0 {
        println!(
            "{}",
            dim(&format!(
                "Private ignore rules are in {}, outside the repo.",
                tilde(&crate::state::private_ignored_path())
            ))
        );
    }
    if n_adopted > 0 {
        println!(
            "Apply them with {}, then save them with {}.",
            bold("dotfinder rebuild"),
            bold("dotfinder push")
        );
    } else if n_ignored > 0 {
        println!("Save the ignore rules with {}.", bold("dotfinder push"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn secrets_are_refused() {
        assert!(looks_secret("APIToken", &Value::Str("x".into())));
        assert!(looks_secret(
            "x",
            &Value::Str("ghp_aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3z5A".into())
        ));
        assert!(!looks_secret("orientation", &Value::Str("left".into())));
        assert!(!looks_secret(
            "path",
            &Value::Str("/Users/peter/Library/Application Support/Something/Long/Path".into())
        ));
        assert!(looks_secret(
            "x",
            &parse(r#"{"inner":{"sessionKey":"a"}}"#).unwrap()
        ));
    }

    #[test]
    fn dates_and_data_are_refused() {
        assert!(unadoptable("when", &parse(r#"{"$date":"2026-01-01T00:00:00Z"}"#).unwrap()).is_some());
        assert!(unadoptable("list", &parse(r#"[{"$data":"4 bytes #00"}]"#).unwrap()).is_some());
        assert!(unadoptable("autohide", &Value::Bool(true)).is_none());
    }
}
