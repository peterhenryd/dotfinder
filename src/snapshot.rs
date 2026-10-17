//! Captures the machine's state as a `Snapshot`: a set of named sections,
//! each a map from item name to value.
//!
//! Sections:
//! - `defaults`, `defaults-currenthost`: every preferences domain
//! - `container-defaults`: sandboxed apps' preferences (opt-in, see config)
//! - `homebrew-formulae`, `homebrew-casks`: what Homebrew has installed
//! - `apps`: application bundles, with where each came from
//! - `dir:<path>`: entries of each watched folder
//! - `cmd:<name>`: output of each collector command

use crate::config::{Config, collector_items};
use crate::json::{self, Value};
use crate::plist;
use crate::util::{self, RunError, expand, run, run_simple};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub const VERSION: i64 = 1;

/// Sections whose items are objects compared key by key (e.g. a defaults
/// domain is compared per preference key).
pub fn is_nested(section: &str) -> bool {
    matches!(
        section,
        "defaults" | "defaults-currenthost" | "container-defaults" | "apps"
    )
}

pub struct Section {
    pub available: bool,
    /// Why the section is unavailable, or what was skipped.
    pub note: Option<String>,
    pub items: BTreeMap<String, Value>,
}

impl Section {
    fn ok(items: BTreeMap<String, Value>) -> Section {
        Section {
            available: true,
            note: None,
            items,
        }
    }
    fn unavailable(note: impl Into<String>) -> Section {
        Section {
            available: false,
            note: Some(note.into()),
            items: BTreeMap::new(),
        }
    }
}

pub struct Snapshot {
    pub taken_at: u64,
    pub kind: String,
    /// The nix-darwin system the machine was running (`/run/current-system`).
    pub generation: Option<String>,
    pub sections: BTreeMap<String, Section>,
}

impl Snapshot {
    pub fn take(cfg: &Config, kind: &str, progress: bool) -> Snapshot {
        let mut sections = BTreeMap::new();
        let step = |name: &str| {
            if progress {
                eprintln!("{}", crate::term::dim(&format!("  reading {name}…")));
            }
        };

        step("preferences");
        sections.insert("defaults".into(), defaults_section(false));
        sections.insert("defaults-currenthost".into(), defaults_section(true));
        if cfg.containers {
            step("sandboxed app preferences");
            sections.insert("container-defaults".into(), container_section());
        }

        step("Homebrew");
        let (formulae, casks, installed) = homebrew_sections();
        sections.insert("homebrew-formulae".into(), formulae);
        sections.insert("homebrew-casks".into(), casks);

        step("applications");
        sections.insert("apps".into(), apps_section(&installed));

        step("folders");
        for w in &cfg.watch {
            sections.insert(
                format!("dir:{}", w.path),
                dir_section(&w.path, w.hidden_only, &cfg.dotfiles),
            );
        }

        step("other package managers");
        let results = par_map(cfg.collectors.iter().collect(), 4, |c| {
            let argv: Vec<&str> = c.command.iter().map(String::as_str).collect();
            let section = match run(&argv, None, Duration::from_secs(60)) {
                Err(RunError::NotFound) => Section::unavailable("not installed"),
                Err(e) => Section::unavailable(e.to_string()),
                Ok(out) if !out.ok() => {
                    let first = out
                        .stderr
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("failed");
                    Section::unavailable(crate::term::clip(first, 100))
                }
                Ok(out) => match collector_items(c, &out.stdout) {
                    Ok(items) => Section::ok(items.into_iter().map(|i| (i, Value::Null)).collect()),
                    Err(e) => Section::unavailable(e),
                },
            };
            (format!("cmd:{}", c.name), section)
        });
        sections.extend(results);

        Snapshot {
            taken_at: util::now_unix(),
            kind: kind.into(),
            generation: current_generation(),
            sections,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut root = BTreeMap::new();
        root.insert("version".into(), Value::Int(VERSION));
        root.insert("taken_at".into(), Value::Int(self.taken_at as i64));
        root.insert("taken_at_utc".into(), Value::Str(util::iso8601(self.taken_at)));
        root.insert("kind".into(), Value::Str(self.kind.clone()));
        root.insert(
            "generation".into(),
            self.generation.clone().map(Value::Str).unwrap_or(Value::Null),
        );
        let mut secs = BTreeMap::new();
        for (name, s) in &self.sections {
            let mut o = BTreeMap::new();
            o.insert("available".into(), Value::Bool(s.available));
            if let Some(n) = &s.note {
                o.insert("note".into(), Value::Str(n.clone()));
            }
            o.insert("items".into(), Value::Obj(s.items.clone()));
            secs.insert(name.clone(), Value::Obj(o));
        }
        root.insert("sections".into(), Value::Obj(secs));
        Value::Obj(root)
    }

    pub fn from_value(v: &Value) -> Result<Snapshot, String> {
        let version = v.get("version").and_then(Value::as_i64).unwrap_or(0);
        if version != VERSION {
            return Err(format!(
                "snapshot format {version} isn't supported (expected {VERSION})"
            ));
        }
        let mut sections = BTreeMap::new();
        for (name, s) in v
            .get("sections")
            .and_then(Value::as_obj)
            .ok_or("snapshot has no sections")?
        {
            sections.insert(
                name.clone(),
                Section {
                    available: s.get("available").and_then(Value::as_bool).unwrap_or(false),
                    note: s.get("note").and_then(Value::as_str).map(str::to_string),
                    items: s
                        .get("items")
                        .and_then(Value::as_obj)
                        .cloned()
                        .unwrap_or_default(),
                },
            );
        }
        Ok(Snapshot {
            taken_at: v.get("taken_at").and_then(Value::as_i64).unwrap_or(0) as u64,
            kind: v
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            generation: v.get("generation").and_then(Value::as_str).map(str::to_string),
            sections,
        })
    }
}

pub fn current_generation() -> Option<String> {
    std::fs::read_link(util::sys("/run/current-system"))
        .ok()
        .map(|p| p.display().to_string())
}

/// Runs `f` over `items` on `workers` threads, preserving input order.
pub fn par_map<T: Send, R: Send>(items: Vec<T>, workers: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let n = items.len();
    let queue = Mutex::new(items.into_iter().enumerate());
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..workers.max(1).min(n.max(1)) {
            s.spawn(|| {
                loop {
                    let next = queue.lock().unwrap().next();
                    let Some((i, item)) = next else { break };
                    let r = f(item);
                    results.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.unwrap())
        .collect()
}

// --------------------------------------------------------------- defaults

fn defaults_section(current_host: bool) -> Section {
    let mut list_cmd = vec!["defaults"];
    if current_host {
        list_cmd.push("-currentHost");
    }
    list_cmd.push("domains");
    let out = match run_simple(&list_cmd) {
        Ok(o) if o.ok() => o,
        Ok(o) => return Section::unavailable(crate::term::clip(o.stderr.trim(), 100)),
        Err(e) => return Section::unavailable(format!("`defaults`: {e}")),
    };
    let mut domains: Vec<String> = out
        .stdout
        .split(',')
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    domains.push("NSGlobalDomain".into());
    domains.sort();
    domains.dedup();

    let results = par_map(domains, 8, |domain| {
        let mut argv = vec!["defaults"];
        if current_host {
            argv.push("-currentHost");
        }
        argv.extend(["export", domain.as_str(), "-"]);
        let parsed = run(&argv, None, Duration::from_secs(30))
            .ok()
            .filter(|o| o.ok())
            .and_then(|o| plist::parse(&o.stdout).ok());
        (domain, parsed)
    });

    let mut items = BTreeMap::new();
    let mut failed = 0;
    for (domain, parsed) in results {
        match parsed {
            Some(v) => {
                items.insert(domain, v);
            }
            None => failed += 1,
        }
    }
    let mut s = Section::ok(items);
    if failed > 0 {
        s.note = Some(format!("{failed} domains couldn't be read"));
    }
    s
}

fn container_section() -> Section {
    let root = expand("~/Library/Containers");
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) => return Section::unavailable(format!("~/Library/Containers: {e} (needs Full Disk Access)")),
    };
    let mut files = Vec::new();
    for e in entries.flatten() {
        let prefs = e.path().join("Data/Library/Preferences");
        if let Ok(ps) = std::fs::read_dir(&prefs) {
            for p in ps.flatten() {
                let path = p.path();
                if path.extension().is_some_and(|x| x == "plist") {
                    files.push((e.file_name().to_string_lossy().into_owned(), path));
                }
            }
        }
    }
    let results = par_map(files, 8, |(container, path)| {
        let name = format!(
            "{container}/{}",
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        (name, read_plist_file(&path))
    });
    Section::ok(
        results
            .into_iter()
            .filter_map(|(n, v)| v.map(|v| (n, v)))
            .collect(),
    )
}

pub fn read_plist_file(path: &Path) -> Option<Value> {
    let p = path.to_str()?;
    let out = run(
        &["plutil", "-convert", "xml1", "-o", "-", p],
        None,
        Duration::from_secs(10),
    )
    .ok()?;
    if !out.ok() {
        return None;
    }
    plist::parse(&out.stdout).ok()
}

// --------------------------------------------------------------- Homebrew

/// An installed cask: the app bundle names it installs (e.g. "Ghostty.app")
/// and its definition as JSON text, which names the app's bundle ID in its
/// uninstall and zap steps.
pub struct InstalledCask {
    pub token: String,
    pub apps: Vec<String>,
    text: String,
}

/// Returns the formulae and casks sections, plus the installed casks.
fn homebrew_sections() -> (Section, Section, Vec<InstalledCask>) {
    let formulae = match run_simple(&["brew", "leaves", "--installed-on-request"]) {
        Ok(o) if o.ok() => Section::ok(
            o.stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| (l.to_string(), Value::Null))
                .collect(),
        ),
        Ok(o) => Section::unavailable(crate::term::clip(o.stderr.trim(), 100)),
        Err(e) => Section::unavailable(format!("`brew`: {e}")),
    };
    let mut installed = Vec::new();
    let casks = match run(
        &["brew", "info", "--cask", "--json=v2", "--installed"],
        None,
        Duration::from_secs(120),
    ) {
        Ok(o) if o.ok() => match json::parse(&o.stdout) {
            Ok(v) => {
                let mut items = BTreeMap::new();
                for c in v.get("casks").and_then(Value::as_arr).into_iter().flatten() {
                    let Some(token) = c.get("token").and_then(Value::as_str) else {
                        continue;
                    };
                    let version = c
                        .get("installed")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    items.insert(token.to_string(), Value::Str(version));
                    installed.push(InstalledCask {
                        token: token.to_string(),
                        apps: cask_app_names(c),
                        text: c.to_compact(),
                    });
                }
                Section::ok(items)
            }
            Err(e) => Section::unavailable(e.to_string()),
        },
        Ok(o) => Section::unavailable(crate::term::clip(o.stderr.trim(), 100)),
        Err(e) => Section::unavailable(format!("`brew`: {e}")),
    };
    (formulae, casks, installed)
}

/// The installer packages that put this file in place, from the receipts
/// macOS keeps (`pkgutil --file-info`). Apps copied by hand or from the App
/// Store have none.
fn pkg_receipts(path: &Path) -> Vec<String> {
    let p = path.to_string_lossy();
    run(&["pkgutil", "--file-info", &p], None, Duration::from_secs(10))
        .ok()
        .filter(|o| o.ok())
        .map(|o| parse_pkg_receipts(&o.stdout))
        .unwrap_or_default()
}

fn parse_pkg_receipts(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("pkgid:"))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect()
}

/// Whether `text` names the bundle ID `id` as a whole identifier, possibly
/// followed by a file suffix (`com.foo.app.plist`), but not as part of a
/// longer word.
pub fn mentions_id(text: &str, id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    let word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    text.match_indices(id).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + id.len()..].chars().next();
        !before.is_some_and(|c| word(c) || c == '.') && !after.is_some_and(word)
    })
}

/// App bundle names a cask installs, from its `artifacts` list.
fn cask_app_names(cask: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for art in cask
        .get("artifacts")
        .and_then(Value::as_arr)
        .into_iter()
        .flatten()
    {
        for entry in art.get("app").and_then(Value::as_arr).into_iter().flatten() {
            match entry {
                Value::Str(s) => names.push(file_name(s)),
                Value::Obj(_) => {
                    if let Some(t) = entry.get("target").and_then(Value::as_str) {
                        names.push(file_name(t));
                    }
                }
                _ => {}
            }
        }
    }
    names
}

fn file_name(s: &str) -> String {
    Path::new(s)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| s.to_string())
}

// ------------------------------------------------------------------- apps

fn apps_section(installed: &[InstalledCask]) -> Section {
    let mut bundles: Vec<PathBuf> = Vec::new();
    for dir in [
        "/Applications",
        "/Applications/Utilities",
        "/Applications/Nix Apps",
    ]
    .into_iter()
    .map(util::sys)
    .chain([
        expand("~/Applications"),
        expand("~/Applications/Home Manager Apps"),
    ]) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".app") && !name.starts_with('.') {
                    bundles.push(e.path());
                }
            }
        }
    }
    if bundles.is_empty() && !util::sys("/Applications").exists() {
        return Section::unavailable("no /Applications folder");
    }
    let by_name: BTreeMap<String, String> = installed
        .iter()
        .flat_map(|c| c.apps.iter().map(|a| (a.clone(), c.token.clone())))
        .collect();
    // Casks whose apps aren't where Homebrew put them: renamed or moved.
    let app_dirs = [util::sys("/Applications"), expand("~/Applications")];
    let displaced: Vec<&InstalledCask> = installed
        .iter()
        .filter(|c| !c.apps.is_empty() && !c.apps.iter().any(|a| app_dirs.iter().any(|d| d.join(a).exists())))
        .collect();
    let results = par_map(bundles, 8, |path| {
        let info = classify_app(&path, &by_name, &displaced, installed);
        (util::tilde(&path), info)
    });
    Section::ok(results.into_iter().collect())
}

/// Where an app came from. Homebrew apps are matched to their cask by bundle
/// name; apps a cask installs with a .pkg installer, by the installer
/// receipt that lists the app (casks name their receipts in their uninstall
/// step); and a Homebrew app that was renamed, by bundle ID among casks whose
/// apps are missing from where Homebrew put them, recording the name
/// Homebrew knows it by.
fn classify_app(
    path: &Path,
    by_name: &BTreeMap<String, String>,
    displaced: &[&InstalledCask],
    installed: &[InstalledCask],
) -> Value {
    let name = file_name(&path.to_string_lossy());
    let is_link = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    let info = read_plist_file(&path.join("Contents/Info.plist"));
    let id = info
        .as_ref()
        .and_then(|i| i.get("CFBundleIdentifier"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let version = info
        .as_ref()
        .and_then(|i| {
            i.get("CFBundleShortVersionString")
                .or_else(|| i.get("CFBundleVersion"))
        })
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let p = path.to_string_lossy();
    let renamed = || displaced.iter().find(|c| mentions_id(&c.text, &id));
    let from_pkg = || {
        let receipts = pkg_receipts(path);
        installed
            .iter()
            .find(|c| receipts.iter().any(|r| mentions_id(&c.text, r)))
    };
    let mut renamed_from = None;
    let source = if p.contains("/Nix Apps/") || p.contains("/Home Manager Apps/") {
        "nix".to_string()
    } else if let Some(token) = by_name.get(&name) {
        format!("homebrew:{token}")
    } else if path.join("Contents/_MASReceipt/receipt").exists() {
        "app-store".to_string()
    } else if let Some(c) = from_pkg() {
        format!("homebrew:{}", c.token)
    } else if let Some(c) = renamed() {
        renamed_from = c.apps.first().cloned();
        format!("homebrew:{}", c.token)
    } else if is_link || id.starts_with("com.apple.") {
        "apple".to_string()
    } else {
        "manual".to_string()
    };
    let mut o = BTreeMap::new();
    o.insert("id".into(), Value::Str(id));
    o.insert("version".into(), Value::Str(version));
    o.insert("source".into(), Value::Str(source));
    if let Some(from) = renamed_from {
        o.insert("renamed_from".into(), Value::Str(from));
    }
    Value::Obj(o)
}

// ---------------------------------------------------------------- folders

fn dir_section(path: &str, hidden_only: bool, dotfiles: &Path) -> Section {
    let dir = expand(path);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Section::unavailable("doesn't exist"),
        Err(e) => return Section::unavailable(e.to_string()),
    };
    let dotfiles = std::fs::canonicalize(dotfiles).unwrap_or_else(|_| dotfiles.to_path_buf());
    let mut items = BTreeMap::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if matches!(
            name.as_str(),
            ".DS_Store" | ".localized" | ".Trash" | ".CFUserTextEncoding"
        ) {
            continue;
        }
        if hidden_only && !name.starts_with('.') {
            continue;
        }
        let p = e.path();
        let meta = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let kind = if meta.file_type().is_symlink() {
            "symlink"
        } else if meta.is_dir() {
            "folder"
        } else {
            "file"
        };
        let mut o = BTreeMap::new();
        o.insert("kind".into(), Value::Str(kind.into()));
        if meta.is_dir() {
            // home-manager creates real folders holding managed links (e.g.
            // ~/.config/git/config); a folder counts as managed when
            // everything in it is.
            let mut unmanaged = Vec::new();
            let managed_count = scan_folder(&p, Path::new(""), &dotfiles, 0, &mut unmanaged);
            o.insert(
                "managed".into(),
                Value::Bool(managed_count > 0 && unmanaged.is_empty()),
            );
            if managed_count > 0 && !unmanaged.is_empty() {
                unmanaged.truncate(8);
                o.insert(
                    "unmanaged".into(),
                    Value::Arr(unmanaged.into_iter().map(Value::Str).collect()),
                );
            }
        } else {
            o.insert("managed".into(), Value::Bool(is_managed(&p, &dotfiles)));
        }
        items.insert(name, Value::Obj(o));
    }
    Section::ok(items)
}

/// Where nix-darwin keeps the files it copies into the home folder (user
/// launch agents) for the current system generation.
const DARWIN_USER_FILES: &str = "/run/current-system/user";

fn is_managed(p: &Path, dotfiles: &Path) -> bool {
    is_managed_link(p, dotfiles) || is_darwin_copy(p, &util::home(), Path::new(DARWIN_USER_FILES))
}

/// A symlink into the Nix store or into the dotfiles repo (home-manager's
/// out-of-store links go through the store first).
fn is_managed_link(p: &Path, dotfiles: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
        && (std::fs::read_link(p).is_ok_and(|t| t.starts_with("/nix/store"))
            || std::fs::canonicalize(p).is_ok_and(|t| t.starts_with(dotfiles)))
}

/// A regular file identical to nix-darwin's copy of it. nix-darwin copies
/// user launch agents into the home folder instead of linking them; a copy
/// edited by hand no longer counts.
fn is_darwin_copy(p: &Path, home: &Path, darwin_user: &Path) -> bool {
    let Ok(rel) = p.strip_prefix(home) else {
        return false;
    };
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
        && match (std::fs::read(p), std::fs::read(darwin_user.join(rel))) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// Counts managed links under `dir` (a few levels deep) and collects the
/// relative paths of everything else.
fn scan_folder(dir: &Path, rel: &Path, dotfiles: &Path, depth: usize, unmanaged: &mut Vec<String>) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut managed = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        if name == ".DS_Store" {
            continue;
        }
        let p = e.path();
        let r = rel.join(&name);
        if is_managed(&p, dotfiles) {
            managed += 1;
        } else if depth < 3 && std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
            managed += scan_folder(&p, &r, dotfiles, depth + 1, unmanaged);
        } else {
            unmanaged.push(r.display().to_string());
        }
    }
    managed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cask_artifacts() {
        let v = json::parse(
            r#"{"token":"karabiner-elements","artifacts":[{"uninstall":[]},{"app":["Karabiner-Elements.app","Karabiner-EventViewer.app"]},{"app":["Foo.app",{"target":"Bar.app"}]},{"binary":["x"]}]}"#,
        )
        .unwrap();
        assert_eq!(
            cask_app_names(&v),
            [
                "Karabiner-Elements.app",
                "Karabiner-EventViewer.app",
                "Foo.app",
                "Bar.app"
            ]
        );
    }

    #[test]
    fn par_map_keeps_order() {
        let out = par_map((0..100).collect(), 7, |i: i32| i * 2);
        assert_eq!(out, (0..100).map(|i| i * 2).collect::<Vec<_>>());
        let empty: Vec<i32> = par_map(Vec::new(), 4, |i: i32| i);
        assert!(empty.is_empty());
    }

    #[test]
    fn snapshot_round_trip() {
        let mut sections = BTreeMap::new();
        let mut items = BTreeMap::new();
        items.insert("ghostty".to_string(), Value::Str("1.2".into()));
        sections.insert("homebrew-casks".to_string(), Section::ok(items));
        sections.insert("cmd:pipx".to_string(), Section::unavailable("not installed"));
        let s = Snapshot {
            taken_at: 5,
            kind: "baseline".into(),
            generation: None,
            sections,
        };
        let back = Snapshot::from_value(&json::parse(&s.to_value().to_compact()).unwrap()).unwrap();
        assert_eq!(back.kind, "baseline");
        assert!(back.sections["homebrew-casks"].available);
        assert_eq!(back.sections["cmd:pipx"].note.as_deref(), Some("not installed"));
    }

    #[test]
    fn folder_listing() {
        let tmp = std::env::temp_dir().join(format!("dotfinder-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("watched/sub")).unwrap();
        std::fs::create_dir_all(tmp.join("dotfiles/home/nvim")).unwrap();
        std::fs::write(tmp.join("watched/.hidden"), "").unwrap();
        std::os::unix::fs::symlink(tmp.join("dotfiles/home/nvim"), tmp.join("watched/nvim")).unwrap();
        // A real folder of managed links, and one with a private file mixed in.
        std::fs::create_dir_all(tmp.join("watched/full/lib")).unwrap();
        std::os::unix::fs::symlink(tmp.join("dotfiles/home/nvim"), tmp.join("watched/full/lib/x")).unwrap();
        std::fs::create_dir_all(tmp.join("watched/git")).unwrap();
        std::os::unix::fs::symlink(tmp.join("dotfiles/home/nvim"), tmp.join("watched/git/config")).unwrap();
        std::fs::write(tmp.join("watched/git/identity"), "").unwrap();
        let s = dir_section(
            tmp.join("watched").to_str().unwrap(),
            false,
            &tmp.join("dotfiles"),
        );
        assert_eq!(s.items["sub"].get("kind").unwrap().as_str(), Some("folder"));
        assert_eq!(s.items["nvim"].get("managed"), Some(&Value::Bool(true)));
        assert_eq!(s.items["full"].get("managed"), Some(&Value::Bool(true)));
        assert_eq!(s.items["git"].get("managed"), Some(&Value::Bool(false)));
        assert_eq!(
            s.items["git"].get("unmanaged"),
            Some(&Value::Arr(vec![Value::Str("identity".into())]))
        );
        assert_eq!(s.items["sub"].get("managed"), Some(&Value::Bool(false)));
        assert_eq!(s.items[".hidden"].get("managed"), Some(&Value::Bool(false)));
        let hidden = dir_section(tmp.join("watched").to_str().unwrap(), true, &tmp.join("dotfiles"));
        assert_eq!(hidden.items.keys().collect::<Vec<_>>(), [".hidden"]);
        assert!(!dir_section(tmp.join("nope").to_str().unwrap(), false, &tmp).available);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pkgutil_receipts() {
        let out = "volume: /\npath: /Applications/Karabiner-EventViewer.app\n\npkgid: org.pqrs.Karabiner-Elements\npkg-version: 16.3.0\ninstall-time: 1792148253\nuid: 0\n";
        assert_eq!(parse_pkg_receipts(out), ["org.pqrs.Karabiner-Elements"]);
        assert!(parse_pkg_receipts("").is_empty());
        let cask = r#"{"artifacts":[{"uninstall":[{"pkgutil":["org.pqrs.Karabiner-DriverKit-VirtualHIDDevice","org.pqrs.Karabiner-Elements"]}]},{"pkg":["Karabiner-Elements.pkg"]}]}"#;
        assert!(mentions_id(cask, "org.pqrs.Karabiner-Elements"));
    }

    #[test]
    fn bundle_id_mentions() {
        let cask = r#"{"artifacts":[{"uninstall":[{"signal":["TERM","com.crystalidea.macsfancontrol"]}]},{"zap":[{"trash":"~/Library/Preferences/com.crystalidea.macsfancontrol.plist"}]}]}"#;
        assert!(mentions_id(cask, "com.crystalidea.macsfancontrol"));
        assert!(
            !mentions_id(cask, "com.crystalidea.macsfan"),
            "part of a longer word"
        );
        assert!(
            !mentions_id(cask, "crystalidea.macsfancontrol"),
            "suffix of a longer ID"
        );
        assert!(!mentions_id(cask, ""));
    }

    #[test]
    fn darwin_copies() {
        let tmp = std::env::temp_dir().join(format!("dotfinder-darwin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (home, sys) = (tmp.join("home"), tmp.join("system-user"));
        let agents = Path::new("Library/LaunchAgents");
        std::fs::create_dir_all(home.join(agents)).unwrap();
        std::fs::create_dir_all(sys.join(agents)).unwrap();
        // The system generation links to the store; the home copy is a real file.
        std::fs::write(tmp.join("store.plist"), "<plist/>").unwrap();
        std::os::unix::fs::symlink(tmp.join("store.plist"), sys.join(agents).join("a.plist")).unwrap();
        std::os::unix::fs::symlink(tmp.join("store.plist"), sys.join(agents).join("b.plist")).unwrap();
        std::fs::write(home.join(agents).join("a.plist"), "<plist/>").unwrap();
        std::fs::write(home.join(agents).join("b.plist"), "<plist>edited</plist>").unwrap();
        std::fs::write(home.join(agents).join("c.plist"), "<plist/>").unwrap();
        let copy = |name: &str| is_darwin_copy(&home.join(agents).join(name), &home, &sys);
        assert!(copy("a.plist"));
        assert!(!copy("b.plist"), "edited by hand");
        assert!(!copy("c.plist"), "not from nix-darwin");
        assert!(
            !is_darwin_copy(&tmp.join("store.plist"), &home, &sys),
            "outside home"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
