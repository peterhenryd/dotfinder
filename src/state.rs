//! Files dotfinder keeps: baselines in the state directory, and the two
//! files it writes into the dotfiles repo.

use crate::config::Config;
use crate::diff::{Ignores, builtin_ignores};
use crate::json::{self, Value};
use crate::snapshot::Snapshot;
use crate::util::{state_dir, write_atomic};
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

fn read_json(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(Value::obj()),
        Ok(t) => json::parse(&t).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::obj()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Built-in rules, the config's rules, and the ones `adopt` saved.
pub fn ignores(cfg: &Config) -> Result<Ignores, String> {
    let mut patterns = if cfg.builtin_ignores {
        builtin_ignores()
    } else {
        Vec::new()
    };
    patterns.extend(cfg.ignore.iter().cloned());
    patterns.extend(IgnoredFile::load(cfg)?.patterns);
    Ok(Ignores { patterns })
}

/// `dotfinder/ignored.json` in the dotfiles repo: `{"ignore": [patterns]}`.
pub struct IgnoredFile {
    path: PathBuf,
    pub patterns: Vec<String>,
}

impl IgnoredFile {
    pub fn load(cfg: &Config) -> Result<IgnoredFile, String> {
        let path = cfg.ignored_path();
        let v = read_json(&path)?;
        let patterns = v
            .get("ignore")
            .and_then(Value::as_arr)
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Ok(IgnoredFile { path, patterns })
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
        write_atomic(&self.path, Value::Obj(o).to_pretty().as_bytes())
    }
}

/// `dotfinder/adopted.json` in the dotfiles repo, read by the dotfiles Nix
/// module: `{"casks": [], "brews": [], "defaults": {domain: {key: value}}}`.
pub struct AdoptedFile {
    path: PathBuf,
    pub casks: Vec<String>,
    pub brews: Vec<String>,
    pub defaults: BTreeMap<String, Value>,
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

    pub fn save(&mut self) -> std::io::Result<()> {
        self.casks.sort();
        self.casks.dedup();
        self.brews.sort();
        self.brews.dedup();
        let mut o = BTreeMap::new();
        o.insert(
            "casks".into(),
            Value::Arr(self.casks.iter().cloned().map(Value::Str).collect()),
        );
        o.insert(
            "brews".into(),
            Value::Arr(self.brews.iter().cloned().map(Value::Str).collect()),
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
