//! `dotfinder apps`: your own things in ~/Applications (web apps, scripts
//! saved as apps, folders of aliases), which no package manager can
//! reinstall, backed up to a folder that iCloud Drive keeps.
//!
//! Things other tools put in ~/Applications and recreate on their own
//! (Home Manager, Parallels, JetBrains Toolbox) are left out, and so is
//! anything an ignore rule covers.

use crate::config::Config;
use crate::diff::Ignores;
use crate::json::Value;
use crate::state::{self, read_json};
use crate::term::{bold, dim, green, red, yellow};
use crate::util::{self, expand, fnv64, now_unix, run, state_dir, tilde, write_atomic};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A top-level entry in ~/Applications.
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// The tool that made it, for entries that tool recreates.
    pub made_by: Option<&'static str>,
}

fn home_apps() -> PathBuf {
    expand("~/Applications")
}

/// The id ignore rules match, the same one `audit` uses for apps.
fn id(name: &str) -> String {
    format!("apps:~/Applications/{name}")
}

fn bundle_id(app: &Path) -> String {
    crate::snapshot::read_plist_file(&app.join("Contents/Info.plist"))
        .and_then(|i| {
            i.get("CFBundleIdentifier")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// The tool that made an entry, if it's one that recreates its own.
pub fn made_by(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name == "Home Manager Apps" {
        return Some("Home Manager");
    }
    if name.ends_with(".app") {
        return bundle_id(path)
            .starts_with("com.jetbrains.")
            .then_some("JetBrains Toolbox");
    }
    // Parallels keeps a folder per virtual machine of shortcuts to its apps.
    let parallels = std::fs::read_dir(path).ok()?.flatten().any(|e| {
        e.file_name().to_string_lossy().ends_with(".app")
            && bundle_id(&e.path()).starts_with("com.parallels.")
    });
    parallels.then_some("Parallels")
}

pub fn entries() -> Vec<Entry> {
    let Ok(dir) = std::fs::read_dir(home_apps()) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "Icon\r" {
                return None;
            }
            let path = e.path();
            let made_by = made_by(&path);
            Some(Entry { name, path, made_by })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Entries that are yours: not made by a tool, not ignored.
pub fn yours(ignores: &Ignores) -> Vec<Entry> {
    entries()
        .into_iter()
        .filter(|e| e.made_by.is_none() && !ignores.matches_id(&id(&e.name)))
        .collect()
}

/// A fingerprint of a file or folder: every file's relative path, size and
/// modification time.
fn fingerprint(path: &Path) -> String {
    fn walk(base: &Path, p: &Path, acc: &mut Vec<u8>) {
        let Ok(meta) = std::fs::symlink_metadata(p) else {
            return;
        };
        let rel = p.strip_prefix(base).unwrap_or(p).to_string_lossy();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        acc.extend_from_slice(format!("{rel}\0{}\0{mtime}\n", meta.len()).as_bytes());
        if meta.is_dir()
            && let Ok(rd) = std::fs::read_dir(p)
        {
            let mut children: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            children.sort();
            for c in children {
                walk(base, &c, acc);
            }
        }
    }
    let mut acc = Vec::new();
    walk(path, path, &mut acc);
    format!("{:016x}", fnv64(&acc))
}

/// `home-apps.json` in the state directory: what was backed up and when.
struct BackupState {
    path: PathBuf,
    entries: BTreeMap<String, (String, u64)>,
}

impl BackupState {
    fn load() -> Result<BackupState, String> {
        let path = state_dir().join("home-apps.json");
        let v = read_json(&path)?;
        let entries = v
            .get("entries")
            .and_then(Value::as_obj)
            .into_iter()
            .flatten()
            .map(|(k, e)| {
                let fp = e
                    .get("fingerprint")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let at = e.get("backed_up_at").and_then(Value::as_i64).unwrap_or(0) as u64;
                (k.clone(), (fp, at))
            })
            .collect();
        Ok(BackupState { path, entries })
    }

    fn save(&self) -> std::io::Result<()> {
        let entries: BTreeMap<String, Value> = self
            .entries
            .iter()
            .map(|(k, (fp, at))| {
                let mut o = BTreeMap::new();
                o.insert("fingerprint".to_string(), Value::Str(fp.clone()));
                o.insert("backed_up_at".to_string(), Value::Int(*at as i64));
                (k.clone(), Value::Obj(o))
            })
            .collect();
        let mut root = BTreeMap::new();
        root.insert("entries".to_string(), Value::Obj(entries));
        write_atomic(&self.path, Value::Obj(root).to_pretty().as_bytes())
    }
}

/// Copies with `ditto`, which keeps app bundles intact (resource forks,
/// extended attributes, code signatures).
fn ditto(from: &Path, to: &Path) -> Result<(), String> {
    let (f, t) = (from.to_string_lossy(), to.to_string_lossy());
    let out = run(&["ditto", &f, &t], None, Duration::from_secs(600)).map_err(|e| format!("ditto: {e}"))?;
    if out.ok() {
        Ok(())
    } else {
        Err(out.stderr.trim().to_string())
    }
}

fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Entries that are yours and not backed up as they are now, for `audit`.
pub fn pending(cfg: &Config) -> Vec<String> {
    let (Ok(ignores), Ok(st)) = (state::ignores(cfg), BackupState::load()) else {
        return Vec::new();
    };
    yours(&ignores)
        .into_iter()
        .filter_map(|e| match st.entries.get(&e.name) {
            None => Some(format!("{} has never been backed up", e.name)),
            Some((fp, _)) if *fp != fingerprint(&e.path) => {
                Some(format!("{} changed since the last backup", e.name))
            }
            _ => None,
        })
        .collect()
}

pub fn status(cfg: &Config) -> Result<(), String> {
    let ignores = state::ignores(cfg)?;
    let st = BackupState::load()?;
    println!("{} {}", bold("Backup:"), tilde(&cfg.apps_backup));
    let now = now_unix();
    for e in entries() {
        let line = if let Some(tool) = e.made_by {
            dim(&format!("made by {tool}, which recreates it; not backed up"))
        } else if ignores.matches_id(&id(&e.name)) {
            dim("ignored")
        } else {
            match st.entries.get(&e.name) {
                None => yellow("never backed up"),
                Some((fp, _)) if *fp != fingerprint(&e.path) => yellow("changed since the last backup"),
                Some((_, at)) => green(&format!("backed up {}", util::age(now.saturating_sub(*at)))),
            }
        };
        println!("  {}  {line}", e.name);
    }
    Ok(())
}

pub fn backup(cfg: &Config) -> Result<(), String> {
    let ignores = state::ignores(cfg)?;
    let mut st = BackupState::load()?;
    let dest = &cfg.apps_backup;
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", tilde(dest)))?;
    let mut failures = 0;
    for e in yours(&ignores) {
        let fp = fingerprint(&e.path);
        let target = dest.join(&e.name);
        if st.entries.get(&e.name).is_some_and(|(saved, _)| *saved == fp) && target.exists() {
            println!("  {}  {}", e.name, dim("unchanged"));
            continue;
        }
        // Copy beside the old backup, then swap, so a failed copy leaves the
        // previous one in place.
        let tmp = dest.join(format!(".{}.dotfinder-tmp", e.name));
        let _ = remove(&tmp);
        let result = ditto(&e.path, &tmp).and_then(|()| {
            remove(&target).map_err(|err| err.to_string())?;
            std::fs::rename(&tmp, &target).map_err(|err| err.to_string())
        });
        match result {
            Ok(()) => {
                st.entries.insert(e.name.clone(), (fp, now_unix()));
                println!("  {}  {}", e.name, green("backed up"));
            }
            Err(err) => {
                let _ = remove(&tmp);
                println!("  {}  {}", e.name, red(&err));
                failures += 1;
            }
        }
    }
    st.save().map_err(|e| e.to_string())?;
    if failures > 0 {
        return Err(format!("{failures} couldn't be backed up"));
    }
    Ok(())
}

/// Copies backed-up entries that aren't in ~/Applications back into it.
/// Nothing already there is replaced.
pub fn restore(cfg: &Config) -> Result<(), String> {
    let src = &cfg.apps_backup;
    let Ok(dir) = std::fs::read_dir(src) else {
        return Err(format!("{} doesn't exist", tilde(src)));
    };
    let home = home_apps();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let mut st = BackupState::load()?;
    let mut names: Vec<String> = dir
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.') && n != "Icon\r")
        .collect();
    names.sort();
    let mut failures = 0;
    for name in names {
        let target = home.join(&name);
        if target.exists() {
            println!("  {name}  {}", dim("already in ~/Applications, left as is"));
            continue;
        }
        match ditto(&src.join(&name), &target) {
            Ok(()) => {
                st.entries
                    .insert(name.clone(), (fingerprint(&target), now_unix()));
                println!("  {name}  {}", green("restored"));
            }
            Err(err) => {
                println!("  {name}  {}", red(&err));
                failures += 1;
            }
        }
    }
    st.save().map_err(|e| e.to_string())?;
    if failures > 0 {
        return Err(format!("{failures} couldn't be restored"));
    }
    Ok(())
}

/// Whether an app path is in ~/Applications (not Home Manager's folder).
pub fn is_home_app(item: &str) -> bool {
    item.starts_with("~/Applications/") && !item.starts_with("~/Applications/Home Manager Apps/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Change, Kind};

    fn as_change(name: &str) -> Change {
        Change {
            section: "apps".into(),
            item: format!("~/Applications/{name}"),
            key: None,
            kind: Kind::Added,
            old: None,
            new: None,
        }
    }

    #[test]
    fn fingerprints_follow_content() {
        let dir = std::env::temp_dir().join(format!("dotfinder-homeapps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = dir.join("Calc.app/Contents");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("Info.plist"), "a").unwrap();
        let first = fingerprint(&dir.join("Calc.app"));
        assert_eq!(first, fingerprint(&dir.join("Calc.app")));
        std::fs::write(app.join("Info.plist"), "ab").unwrap();
        assert_ne!(first, fingerprint(&dir.join("Calc.app")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_match_app_ignore_rules() {
        let ignores = Ignores {
            patterns: vec!["apps:~/Applications/Claude Code URL Handler.app".into()],
        };
        assert!(ignores.matches_id(&id("Claude Code URL Handler.app")));
        assert!(ignores.matches(&as_change("Claude Code URL Handler.app")));
        assert!(is_home_app("~/Applications/Open Calculator.app"));
        assert!(!is_home_app("~/Applications/Home Manager Apps/foo.app"));
    }
}
