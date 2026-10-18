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
use std::path::PathBuf;

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

/// What an answer to a prompt does.
#[derive(Clone, Debug, PartialEq)]
enum Action {
    /// A Homebrew cask (`true`) or formula from the Homebrew sections.
    AddPackage(bool),
    /// One preference key.
    AddDefault,
    /// Every key of a new preferences domain.
    AddDomain,
    AddUvTool,
    /// An app becomes this cask.
    UseCask(String),
    ListAppStore(i64),
    ListManual,
    KeepRename(String, Rename),
    ShowContents(PathBuf),
    Ignore,
    IgnorePrivately,
    IgnoreDomain,
    Skip,
    Quit,
}

type Choice = (char, &'static str, Action);

const IGNORE: Choice = ('i', "ignore from now on", Action::Ignore);
const PRIVATE: Choice = ('p', "ignore privately", Action::IgnorePrivately);
const SKIP: Choice = ('s', "skip for now", Action::Skip);
const QUIT: Choice = ('q', "save and quit", Action::Quit);
const IGNORE_DOMAIN: Choice = ('d', "ignore this whole domain", Action::IgnoreDomain);

/// What `adopt` shows for one change: a note, and the choices beyond the
/// ignore, skip and quit that every change gets.
#[derive(Default)]
struct Prompt {
    note: Option<String>,
    choices: Vec<Choice>,
}

impl Prompt {
    fn note(note: impl Into<String>) -> Prompt {
        Prompt {
            note: Some(note.into()),
            choices: Vec::new(),
        }
    }

    fn with(mut self, choice: Choice) -> Prompt {
        self.choices.push(choice);
        self
    }
}

/// An app's name: its bundle's file name without ".app".
fn app_name(item: &str) -> String {
    item.rsplit('/')
        .next()
        .unwrap_or(item)
        .trim_end_matches(".app")
        .to_string()
}

fn str_field<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a str> {
    v.and_then(|v| v.get(key)).and_then(Value::as_str)
}

/// Builds the prompt for a change. Looks up casks and App Store IDs for
/// apps; everything else comes from the change itself.
fn prompt_for(c: &Change, cfg: &Config, declared: Option<&Brewfile>) -> Prompt {
    let section = c.section.as_str();
    let value = c.new.as_ref();
    let mut p = match (section, c.kind, &c.key) {
        ("homebrew-casks" | "homebrew-formulae", Kind::Added, None) => {
            Prompt::default().with(('a', "add to the config", Action::AddPackage(section == "homebrew-casks")))
        }
        ("homebrew-casks" | "homebrew-formulae", Kind::Removed, None) => {
            Prompt::note("Removed since the baseline. If the config declares it, the next rebuild reinstalls it.")
        }
        ("defaults", Kind::Added | Kind::Changed, Some(k)) => {
            let p = match value.and_then(|v| unadoptable(k, v)) {
                None => Prompt::default().with(('a', "add to the config", Action::AddDefault)),
                Some(why) => Prompt::note(format!("Can't be added to the config: {why}.")),
            };
            p.with(IGNORE_DOMAIN)
        }
        ("defaults", Kind::Added, None) => Prompt::default()
            .with(('a', "add all its keys to the config", Action::AddDomain))
            .with(IGNORE_DOMAIN),
        ("defaults", Kind::Removed, key) => {
            let p = Prompt::note("nix-darwin can only set keys, not delete them.");
            if key.is_some() { p.with(IGNORE_DOMAIN) } else { p }
        }
        ("defaults-currenthost" | "container-defaults", _, key) => {
            let p = Prompt::note(if section == "container-defaults" {
                "Sandboxed apps keep preferences in their container, which nix-darwin doesn't write to."
            } else {
                "Per-Mac (-currentHost) preferences aren't supported by nix-darwin's CustomUserPreferences."
            });
            if key.is_some() { p.with(IGNORE_DOMAIN) } else { p }
        }
        ("apps", Kind::Added, None) => {
            let source = str_field(value, "source").unwrap_or("");
            match (source.strip_prefix("homebrew:"), str_field(value, "renamed_from")) {
                (Some(token), Some(from)) => rename_prompt(&c.item, token, from),
                _ if matches!(source, "manual" | "app-store") => {
                    let id = str_field(value, "id").unwrap_or("");
                    let found = brew::casks_for_app(&format!("{}.app", app_name(&c.item)), id);
                    let store_id = if source == "app-store" {
                        brew::app_store_id(&expand(&c.item))
                    } else {
                        None
                    };
                    app_prompt(source, found, store_id, declared)
                }
                _ => Prompt::default(),
            }
        }
        (s, Kind::Added, None) if s.starts_with("dir:") => {
            let path = expand(&s[4..]).join(&c.item);
            let p = Prompt::note(crate::folders::note(&path, &s[4..] == "~", value, &cfg.dotfiles));
            if str_field(value, "kind") == Some("folder") {
                p.with(('l', "show what's inside", Action::ShowContents(path)))
            } else {
                p
            }
        }
        ("cmd:login-items", Kind::Added, None) => {
            Prompt::note("Login items aren't part of the config. Ignore it if you expect it to start at login.")
        }
        ("cmd:uv-tool", Kind::Added, None) => Prompt::note(
            "Switches install it with `uv tool install` when it's missing, and `dotfinder upgrade` keeps it current.",
        )
        .with(('a', "add it to the config", Action::AddUvTool)),
        ("cmd:rustup-toolchain", Kind::Added, None) => Prompt::note(
            "Toolchains are managed by rustup; a rust-toolchain.toml in each project pins the version it needs.",
        ),
        (s, Kind::Added, None) if s.starts_with("cmd:") => Prompt::note(
            "To make it reproducible, add it to home/packages.nix (if nixpkgs has it) and uninstall this copy.",
        ),
        _ => Prompt::default(),
    };
    p.choices.extend([IGNORE, PRIVATE, SKIP, QUIT]);
    p
}

/// A cask app under a name of its own. Only renames inside /Applications
/// can be kept, since that's where switches rename it.
fn rename_prompt(item: &str, token: &str, from: &str) -> Prompt {
    match item.strip_prefix("/Applications/") {
        Some(to) => Prompt::note(format!(
            "Renamed from {from}, the name the cask `{token}` installs it under."
        ))
        .with((
            'r',
            "keep the new name (switches rename it on any Mac)",
            Action::KeepRename(
                token.to_string(),
                Rename {
                    from: from.to_string(),
                    to: to.to_string(),
                },
            ),
        )),
        None => Prompt::note(format!(
            "Moved and renamed from /Applications/{from}; only renames inside /Applications can be kept."
        )),
    }
}

/// An app installed by hand or from the App Store. `found`: the casks that
/// may install it, best first, and whether the best was matched by bundle ID
/// rather than name. `store_id`: its App Store ID, when it came from there.
fn app_prompt(
    source: &str,
    (casks, by_id): (Vec<String>, bool),
    store_id: Option<i64>,
    declared: Option<&Brewfile>,
) -> Prompt {
    let mut cask = casks.into_iter().next();
    let mut lines: Vec<String> = Vec::new();
    // A cask the config already declares: this is most likely that cask's
    // copy, installed in a way dotfinder can't trace, so replacing it would
    // be wrong.
    if let Some(c) = &cask
        && declared.is_some_and(|bf| bf.declares_cask(c))
    {
        lines.push(format!(
            "The config already declares the cask `{c}`, so this is probably its copy. If so, ignore it."
        ));
        cask = None;
    }
    match &cask {
        Some(c) if by_id => lines.push(format!(
            "Homebrew probably has it as the cask `{c}`, matched by bundle ID rather than name; check that before using it."
        )),
        Some(c) => lines.push(format!("Homebrew has it as the cask `{c}`.")),
        None if source == "manual" && lines.is_empty() => lines.push("Installed by hand, and no cask installs it.".into()),
        None => {}
    }
    if source == "app-store" && store_id.is_none() {
        lines.push("Its App Store ID couldn't be read, so it can't be added from the App Store.".into());
    }
    let mut p = Prompt {
        note: (!lines.is_empty()).then(|| lines.join(" ")),
        choices: Vec::new(),
    };
    if let Some(c) = cask {
        p = p.with(('c', "use the cask", Action::UseCask(c)));
    }
    if let Some(id) = store_id {
        p = p.with(('m', "list it as an App Store app", Action::ListAppStore(id)));
    }
    p.with(('n', "list it as an app to install by hand", Action::ListManual))
}

/// What happens after an answer.
#[derive(Debug, PartialEq)]
enum Outcome {
    Next,
    /// Ask about the same change again: after a listing, or when an answer
    /// couldn't be carried out.
    Again,
    Quit,
}

/// The files `adopt` changes, and what it has done to them so far.
struct Session {
    adopted: AdoptedFile,
    ignored: IgnoredFile,
    private: IgnoredFile,
    n_adopted: usize,
    n_ignored: usize,
    n_private: usize,
    /// Domains ignored as a whole, whose remaining keys aren't asked about.
    ignored_parents: Vec<String>,
}

impl Session {
    fn new(adopted: AdoptedFile, ignored: IgnoredFile, private: IgnoredFile) -> Session {
        Session {
            adopted,
            ignored,
            private,
            n_adopted: 0,
            n_ignored: 0,
            n_private: 0,
            ignored_parents: Vec::new(),
        }
    }

    fn apply(&mut self, c: &Change, action: &Action, cfg: &Config, declared: Option<&Brewfile>) -> Outcome {
        match action {
            Action::AddPackage(cask) => match brew_entry(*cask, &c.item, declared) {
                Err(why) => {
                    println!("  {}", yellow(&why));
                    return Outcome::Again;
                }
                Ok(name) if *cask => self.adopted.casks.push(name),
                Ok(name) => self.adopted.brews.push(name),
            },
            Action::AddDefault => {
                if let (Some(k), Some(v)) = (&c.key, &c.new) {
                    self.adopted.set_default(&c.item, k, v.clone());
                }
            }
            Action::AddDomain => {
                let mut skipped = Vec::new();
                for (k, v) in c.new.as_ref().and_then(Value::as_obj).into_iter().flatten() {
                    match unadoptable(k, v) {
                        None => {
                            self.adopted.set_default(&c.item, k, v.clone());
                            self.n_adopted += 1;
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
                // Counted per key above.
                return Outcome::Next;
            }
            Action::AddUvTool => self.adopted.uv_tools.push(c.item.clone()),
            Action::UseCask(token) => match brew_entry(true, token, declared) {
                Err(why) => {
                    println!("  {}", yellow(&why));
                    return Outcome::Again;
                }
                Ok(name) => {
                    self.adopted.casks.push(name);
                    println!(
                        "  {}",
                        dim(&format!(
                            "Before the next rebuild, quit {} and move it to the Trash, so Homebrew can install its own copy. Its data in ~/Library stays.",
                            app_name(&c.item)
                        ))
                    );
                }
            },
            Action::ListAppStore(id) => {
                self.adopted.mas_apps.insert(app_name(&c.item), *id);
                println!(
                    "  {}",
                    dim("On a Mac without it, `dotfinder audit` links to its App Store page.")
                );
            }
            Action::ListManual => self.adopted.manual_apps.push(app_name(&c.item)),
            Action::KeepRename(token, r) => {
                self.adopted.renames.insert(token.clone(), r.clone());
            }
            Action::ShowContents(dir) => {
                for line in crate::folders::listing(dir, &cfg.dotfiles, 40) {
                    println!("    {}", dim(&line));
                }
                return Outcome::Again;
            }
            Action::Ignore => {
                self.ignored.add(c.id());
                self.n_ignored += 1;
                return Outcome::Next;
            }
            Action::IgnorePrivately => {
                self.private.add(c.id());
                self.n_private += 1;
                return Outcome::Next;
            }
            Action::IgnoreDomain => {
                self.ignored.add(c.parent_id());
                self.ignored_parents.push(c.parent_id());
                self.n_ignored += 1;
                return Outcome::Next;
            }
            Action::Skip => return Outcome::Next,
            Action::Quit => return Outcome::Quit,
        }
        self.n_adopted += 1;
        Outcome::Next
    }

    fn save_and_report(mut self, cfg: &Config) -> Result<(), String> {
        self.adopted
            .save()
            .map_err(|e| format!("{}: {e}", cfg.adopted_path().display()))?;
        self.ignored
            .save()
            .map_err(|e| format!("{}: {e}", cfg.ignored_path().display()))?;
        let private_path = crate::state::private_ignored_path();
        if self.n_private > 0 {
            self.private
                .save()
                .map_err(|e| format!("{}: {e}", private_path.display()))?;
        }
        println!(
            "\n{} added to the config, {} ignored, {} ignored privately.",
            self.n_adopted, self.n_ignored, self.n_private
        );
        if self.n_private > 0 {
            println!(
                "{}",
                dim(&format!(
                    "Private ignore rules are in {}, outside the repo.",
                    tilde(&private_path)
                ))
            );
        }
        if self.n_adopted > 0 {
            println!(
                "Apply them with {}, then save them with {}.",
                bold("dotfinder rebuild"),
                bold("dotfinder push")
            );
        } else if self.n_ignored > 0 {
            println!("Save the ignore rules with {}.", bold("dotfinder push"));
        }
        Ok(())
    }
}

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
    let mut session = Session::new(
        AdoptedFile::load(cfg)?,
        IgnoredFile::load(cfg)?,
        IgnoredFile::load_private()?,
    );
    changes.sort_by(|a, b| a.section.cmp(&b.section).then(a.item.cmp(&b.item)));
    let total = changes.len();
    'changes: for (i, c) in changes.iter().enumerate() {
        if session.ignored_parents.contains(&c.parent_id()) {
            continue;
        }
        println!(
            "\n{} {}\n  {} {}",
            dim(&format!("[{}/{total}]", i + 1)),
            bold(&section_label(&c.section)),
            marker(c.kind),
            describe(c)
        );
        let prompt = prompt_for(c, cfg, declared);
        if let Some(n) = &prompt.note {
            println!("  {}", yellow(n));
        }
        let menu: Vec<(char, &str)> = prompt.choices.iter().map(|(k, label, _)| (*k, *label)).collect();
        loop {
            let Some(key) = choose("  ", &menu) else {
                break 'changes;
            };
            let Some((_, _, action)) = prompt.choices.iter().find(|(k, ..)| *k == key) else {
                continue;
            };
            match session.apply(c, action, cfg, declared) {
                Outcome::Next => break,
                Outcome::Again => {}
                Outcome::Quit => break 'changes,
            }
        }
    }
    session.save_and_report(cfg)
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

    fn change(section: &str, item: &str, key: Option<&str>, kind: Kind, new: Option<Value>) -> Change {
        Change {
            section: section.into(),
            item: item.into(),
            key: key.map(str::to_string),
            kind,
            old: None,
            new,
        }
    }

    fn keys(p: &Prompt) -> String {
        p.choices.iter().map(|(k, ..)| *k).collect()
    }

    fn cfg() -> Config {
        Config::from_value(&parse(r#"{"dotfiles":"/tmp/dotfinder-test-dotfiles"}"#).unwrap()).unwrap()
    }

    #[test]
    fn prompts_offer_what_each_change_allows() {
        let cfg = cfg();
        let p = |c: Change| keys(&prompt_for(&c, &cfg, None));
        assert_eq!(
            p(change("homebrew-casks", "zed", None, Kind::Added, None)),
            "aipsq"
        );
        assert_eq!(
            p(change("homebrew-casks", "zed", None, Kind::Removed, None)),
            "ipsq"
        );
        let tok = Some(Value::Str("x".into()));
        assert_eq!(
            p(change(
                "defaults",
                "com.x",
                Some("autohide"),
                Kind::Changed,
                tok.clone()
            )),
            "adipsq"
        );
        // A key that looks like a secret can only be ignored.
        assert_eq!(
            p(change("defaults", "com.x", Some("apiToken"), Kind::Added, tok)),
            "dipsq"
        );
        assert_eq!(
            p(change("defaults", "com.x", None, Kind::Added, Some(Value::obj()))),
            "adipsq"
        );
        assert_eq!(
            p(change("defaults", "com.x", Some("k"), Kind::Removed, None)),
            "dipsq"
        );
        assert_eq!(p(change("defaults", "com.x", None, Kind::Removed, None)), "ipsq");
        assert_eq!(p(change("cmd:uv-tool", "ruff", None, Kind::Added, None)), "aipsq");
        assert_eq!(
            p(change("cmd:cargo-install", "x", None, Kind::Added, None)),
            "ipsq"
        );
        let folder = parse(r#"{"kind":"folder","managed":false}"#).unwrap();
        assert_eq!(
            p(change("dir:~/.config", "x", None, Kind::Added, Some(folder))),
            "lipsq"
        );
        let file = parse(r#"{"kind":"file","managed":false}"#).unwrap();
        assert_eq!(
            p(change("dir:~/.config", "x", None, Kind::Added, Some(file))),
            "ipsq"
        );
    }

    #[test]
    fn app_prompts() {
        let declared = Brewfile::parse("cask \"zed\"\n");
        let keys_of = |p: Prompt| keys(&p);
        assert_eq!(
            keys_of(app_prompt("manual", (vec!["anki".into()], false), None, None)),
            "cn"
        );
        assert_eq!(keys_of(app_prompt("manual", (vec![], false), None, None)), "n");
        assert_eq!(
            keys_of(app_prompt("app-store", (vec!["x".into()], false), Some(42), None)),
            "cmn"
        );
        // The config already has the cask: don't offer to replace its copy.
        let p = app_prompt("manual", (vec!["zed".into()], false), None, Some(&declared));
        assert!(p.note.unwrap().contains("already declares"));
        assert_eq!(
            keys(&app_prompt(
                "manual",
                (vec!["zed".into()], false),
                None,
                Some(&declared)
            )),
            "n"
        );
        assert!(
            app_prompt("manual", (vec!["word".into()], true), None, None)
                .note
                .unwrap()
                .contains("bundle ID")
        );
        assert_eq!(
            keys(&rename_prompt(
                "/Applications/Fan Control.app",
                "macs-fan-control",
                "Macs Fan Control.app"
            )),
            "r"
        );
        assert_eq!(
            keys(&rename_prompt("/Users/p/Applications/X.app", "x", "Y.app")),
            ""
        );
    }

    #[test]
    fn answers_change_the_files() {
        let cfg = cfg();
        let mut s = Session::new(
            AdoptedFile::default(),
            IgnoredFile::default(),
            IgnoredFile::default(),
        );
        let domain = parse(r#"{"autohide":true,"apiToken":"x"}"#).unwrap();
        let c = change("defaults", "com.apple.dock", None, Kind::Added, Some(domain));
        assert_eq!(s.apply(&c, &Action::AddDomain, &cfg, None), Outcome::Next);
        assert_eq!(s.n_adopted, 1, "the secret-looking key is left out");
        assert!(s.adopted.defaults["com.apple.dock"].get("apiToken").is_none());

        let app = change("apps", "/Applications/Phosphor.app", None, Kind::Added, None);
        assert_eq!(s.apply(&app, &Action::ListManual, &cfg, None), Outcome::Next);
        assert_eq!(s.adopted.manual_apps, ["Phosphor"]);
        assert_eq!(s.apply(&app, &Action::ListAppStore(7), &cfg, None), Outcome::Next);
        assert_eq!(s.adopted.mas_apps["Phosphor"], 7);

        let key = change(
            "defaults",
            "com.x",
            Some("k"),
            Kind::Added,
            Some(Value::Bool(true)),
        );
        assert_eq!(s.apply(&key, &Action::IgnoreDomain, &cfg, None), Outcome::Next);
        assert_eq!(s.ignored.patterns, ["defaults:com.x"]);
        assert_eq!(s.ignored_parents, ["defaults:com.x"]);
        assert_eq!(s.apply(&key, &Action::IgnorePrivately, &cfg, None), Outcome::Next);
        assert_eq!(s.private.patterns, ["defaults:com.x:k"]);
        assert_eq!(s.apply(&key, &Action::Skip, &cfg, None), Outcome::Next);
        assert_eq!(s.apply(&key, &Action::Quit, &cfg, None), Outcome::Quit);
        assert_eq!((s.n_adopted, s.n_ignored, s.n_private), (3, 1, 1));
    }

    #[test]
    fn dates_and_data_are_refused() {
        assert!(unadoptable("when", &parse(r#"{"$date":"2026-01-01T00:00:00Z"}"#).unwrap()).is_some());
        assert!(unadoptable("list", &parse(r#"[{"$data":"4 bytes #00"}]"#).unwrap()).is_some());
        assert!(unadoptable("autohide", &Value::Bool(true)).is_none());
    }
}
