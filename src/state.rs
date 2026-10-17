//! Files dotfinder keeps: baselines in the state directory, the two files it
//! writes into the dotfiles repo, and private ignore rules kept outside it.

use crate::config::Config;
use crate::diff::{Ignores, builtin_ignores};
use crate::json::{self, Value};
use crate::snapshot::Snapshot;
use crate::util::{data_dir, state_dir, write_atomic};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const KEEP_BASELINES: usize = 20;

fn baselines_dir() -> PathBuf {
    state_dir().join("baselines")
}

pub fn save_baseline(s: &Snapshot) -> std::io::Result<PathBuf> {
    let path = baselines_dir().join(format!("{}.json", s.taken_at));
    write_atomic(&path, s.to_value().to_pretty().as_bytes())?;
    // Keep the most recent few.
    let mut all = list_baselines();
    if all.len() > KEEP_BASELINES {
        all.sort();
        for old in &all[..all.len() - KEEP_BASELINES] {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok(path)
}

fn list_baselines() -> Vec<PathBuf> {
    std::fs::read_dir(baselines_dir())
        .map(|es| {
            es.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect()
        })
        .unwrap_or_default()
}

pub fn latest_baseline() -> Result<Option<Snapshot>, String> {
    let mut all = list_baselines();
    // File names are Unix timestamps; sort numerically.
    all.sort_by_key(|p| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    });
    let Some(path) = all.pop() else { return Ok(None) };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let v = json::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Snapshot::from_value(&v)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn read_json(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(Value::obj()),
        Ok(t) => json::parse(&t).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::obj()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Built-in rules, the config's rules, and the ones `adopt` saved, both
/// public and private.
pub fn ignores(cfg: &Config) -> Result<Ignores, String> {
    let mut patterns = if cfg.builtin_ignores {
        builtin_ignores()
    } else {
        Vec::new()
    };
    patterns.extend(cfg.ignore.iter().cloned());
    patterns.extend(IgnoredFile::load(cfg)?.patterns);
    patterns.extend(IgnoredFile::load_private()?.patterns);
    Ok(Ignores { patterns })
}

/// Where private ignore rules are kept: outside the dotfiles repo, so the
/// names of what they hide aren't published.
pub fn private_ignored_path() -> PathBuf {
    data_dir().join("ignored.json")
}

/// Ignore rules, `{"ignore": [patterns]}`: `dotfinder/ignored.json` in the
/// dotfiles repo, or the private file outside it.
pub struct IgnoredFile {
    path: PathBuf,
    private: bool,
    pub patterns: Vec<String>,
}

impl IgnoredFile {
    pub fn load(cfg: &Config) -> Result<IgnoredFile, String> {
        Self::load_from(cfg.ignored_path(), false)
    }

    pub fn load_private() -> Result<IgnoredFile, String> {
        Self::load_from(private_ignored_path(), true)
    }

    fn load_from(path: PathBuf, private: bool) -> Result<IgnoredFile, String> {
        let v = read_json(&path)?;
        let patterns = v
            .get("ignore")
            .and_then(Value::as_arr)
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Ok(IgnoredFile {
            path,
            private,
            patterns,
        })
    }

    pub fn add(&mut self, pattern: String) {
        if !self.patterns.contains(&pattern) {
            self.patterns.push(pattern);
        }
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        self.patterns.sort();
        self.patterns.dedup();
        let mut o = BTreeMap::new();
        o.insert(
            "ignore".into(),
            Value::Arr(self.patterns.iter().cloned().map(Value::Str).collect()),
        );
        write_atomic(&self.path, Value::Obj(o).to_pretty().as_bytes())?;
        if self.private {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// `dotfinder/adopted.json` in the dotfiles repo, read by the dotfiles Nix
/// module: `{"casks": [], "brews": [], "uvTools": [], "masApps": {name: id},
/// "manualApps": [], "renames": {cask: {"from": app, "to": app}},
/// "defaults": {domain: {key: value}}}`.
/// `uvTools` are Python command-line tools that switches install with
/// `uv tool install` when they're missing.
/// `masApps` and `manualApps` are apps the config doesn't install, listed so
/// a new Mac's audit names them (App Store apps with a link to their page).
/// They stay out of Homebrew: once a Brewfile lists any App Store app,
/// Homebrew's cleanup uninstalls every App Store app it doesn't list. `renames` are cask apps kept under a name of your own; the
/// switch renames them after Homebrew installs them.
#[derive(Default)]
pub struct AdoptedFile {
    path: PathBuf,
    pub casks: Vec<String>,
    pub brews: Vec<String>,
    pub uv_tools: Vec<String>,
    pub mas_apps: BTreeMap<String, i64>,
    pub manual_apps: Vec<String>,
    pub renames: BTreeMap<String, Rename>,
    pub defaults: BTreeMap<String, Value>,
}

/// A cask's app under a name of your own: `from` is the bundle name the
/// cask installs, `to` the one you gave it, both relative to /Applications.
#[derive(Clone, Debug, PartialEq)]
pub struct Rename {
    pub from: String,
    pub to: String,
}

impl AdoptedFile {
    pub fn load(cfg: &Config) -> Result<AdoptedFile, String> {
        let path = cfg.adopted_path();
        let v = read_json(&path)?;
        let list = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_arr)
                .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        Ok(AdoptedFile {
            casks: list("casks"),
            brews: list("brews"),
            uv_tools: list("uvTools"),
            mas_apps: v
                .get("masApps")
                .and_then(Value::as_obj)
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_i64().map(|id| (k.clone(), id)))
                        .collect()
                })
                .unwrap_or_default(),
            manual_apps: list("manualApps"),
            renames: v
                .get("renames")
                .and_then(Value::as_obj)
                .map(|o| {
                    o.iter()
                        .filter_map(|(token, r)| {
                            let from = r.get("from")?.as_str()?.to_string();
                            let to = r.get("to")?.as_str()?.to_string();
                            Some((token.clone(), Rename { from, to }))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            defaults: v
                .get("defaults")
                .and_then(Value::as_obj)
                .cloned()
                .unwrap_or_default(),
            path,
        })
    }

    pub fn set_default(&mut self, domain: &str, key: &str, value: Value) {
        let d = self.defaults.entry(domain.to_string()).or_insert_with(Value::obj);
        if let Some(m) = d.as_obj_mut() {
            m.insert(key.to_string(), value);
        }
    }

    /// Names of the apps this file accounts for without a cask: App Store
    /// apps and apps to install by hand.
    pub fn known_app_names(&self) -> Vec<String> {
        self.mas_apps
            .keys()
            .chain(self.manual_apps.iter())
            .cloned()
            .collect()
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        self.casks.sort();
        self.casks.dedup();
        self.brews.sort();
        self.brews.dedup();
        self.uv_tools.sort();
        self.uv_tools.dedup();
        self.manual_apps.sort();
        self.manual_apps.dedup();
        let mut o = BTreeMap::new();
        o.insert(
            "casks".into(),
            Value::Arr(self.casks.iter().cloned().map(Value::Str).collect()),
        );
        o.insert(
            "brews".into(),
            Value::Arr(self.brews.iter().cloned().map(Value::Str).collect()),
        );
        o.insert(
            "uvTools".into(),
            Value::Arr(self.uv_tools.iter().cloned().map(Value::Str).collect()),
        );
        o.insert(
            "masApps".into(),
            Value::Obj(
                self.mas_apps
                    .iter()
                    .map(|(k, id)| (k.clone(), Value::Int(*id)))
                    .collect(),
            ),
        );
        o.insert(
            "manualApps".into(),
            Value::Arr(self.manual_apps.iter().cloned().map(Value::Str).collect()),
        );
        o.insert(
            "renames".into(),
            Value::Obj(
                self.renames
                    .iter()
                    .map(|(token, r)| {
                        let mut m = BTreeMap::new();
                        m.insert("from".to_string(), Value::Str(r.from.clone()));
                        m.insert("to".to_string(), Value::Str(r.to.clone()));
                        (token.clone(), Value::Obj(m))
                    })
                    .collect(),
            ),
        );
        o.insert("defaults".into(), Value::Obj(self.defaults.clone()));
        write_atomic(&self.path, Value::Obj(o).to_pretty().as_bytes())
    }
}

/// `vault.json` in the state directory: what was backed up and when.
pub struct VaultState {
    path: PathBuf,
    pub files: BTreeMap<String, (String, u64)>,
}

impl VaultState {
    pub fn load() -> Result<VaultState, String> {
        let path = state_dir().join("vault.json");
        let v = read_json(&path)?;
        let mut files = BTreeMap::new();
        for (k, f) in v.get("files").and_then(Value::as_obj).into_iter().flatten() {
            let hash = f.get("fnv").and_then(Value::as_str).unwrap_or("").to_string();
            let at = f.get("backed_up_at").and_then(Value::as_i64).unwrap_or(0) as u64;
            files.insert(k.clone(), (hash, at));
        }
        Ok(VaultState { path, files })
    }

    pub fn save(&self) -> std::io::Result<()> {
        let mut files = BTreeMap::new();
        for (k, (hash, at)) in &self.files {
            let mut o = BTreeMap::new();
            o.insert("fnv".into(), Value::Str(hash.clone()));
            o.insert("backed_up_at".into(), Value::Int(*at as i64));
            files.insert(k.clone(), Value::Obj(o));
        }
        let mut root = BTreeMap::new();
        root.insert("files".into(), Value::Obj(files));
        write_atomic(&self.path, Value::Obj(root).to_pretty().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dotfinder-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn adopted_round_trip() {
        let dir = temp("adopted");
        let cfg =
            Config::from_value(&json::parse(&format!(r#"{{"dotfiles":"{}"}}"#, dir.display())).unwrap())
                .unwrap();
        let mut a = AdoptedFile::load(&cfg).unwrap();
        a.casks.push("ghostty".into());
        a.mas_apps.insert("Things 3".into(), 904280696);
        a.manual_apps.extend(["Zed".to_string(), "Zed".to_string()]);
        a.renames.insert(
            "macs-fan-control".into(),
            Rename {
                from: "Macs Fan Control.app".into(),
                to: "Fan Control.app".into(),
            },
        );
        a.save().unwrap();
        let b = AdoptedFile::load(&cfg).unwrap();
        assert_eq!(b.casks, ["ghostty"]);
        assert_eq!(b.mas_apps.get("Things 3"), Some(&904280696));
        assert_eq!(b.manual_apps, ["Zed"]);
        assert_eq!(b.known_app_names(), ["Things 3", "Zed"]);
        assert_eq!(b.renames["macs-fan-control"].to, "Fan Control.app");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_ignores_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("private");
        let path = dir.join("ignored.json");
        let mut f = IgnoredFile::load_from(path.clone(), true).unwrap();
        f.add("apps:/Applications/Secret.app".into());
        f.save().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            IgnoredFile::load_from(path, true).unwrap().patterns,
            ["apps:/Applications/Secret.app"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
