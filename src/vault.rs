//! `dotfinder vault`: keeps private files (SSH config, git identity, keys)
//! as attachments in a KeePassXC database, via `keepassxc-cli`.
//!
//! Each file becomes an entry in the configured group, titled with its path
//! relative to the home folder (`/` written as `::`, which keepassxc-cli
//! would otherwise read as a group separator), holding the file as an
//! attachment named `content`.
//!
//! Files whose paths shouldn't be public either are listed privately, in
//! `~/.local/share/dotfinder/vault-files.json`. That list belongs in the
//! config's list, so a restore brings it back before reading it.

use crate::config::{Config, Vault, VaultFile, parse_vault_files};
use crate::json;
use crate::state::VaultState;
use crate::term::{bold, dim, green, read_password, red, yellow};
use crate::util::{self, RunError, expand, fnv64, home, run, tilde};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ATTACHMENT: &str = "content";

fn vault_cfg(cfg: &Config) -> Result<&Vault, String> {
    cfg.vault
        .as_ref()
        .ok_or_else(|| "no vault is configured (`vault` in the dotfinder config)".to_string())
}

fn entry_path(v: &Vault, f: &VaultFile) -> String {
    let p = expand(&f.path);
    let rel = match p.strip_prefix(home()) {
        Ok(r) => r.display().to_string(),
        Err(_) => p.display().to_string(),
    };
    format!("{}/{}", v.group, rel.trim_start_matches('/').replace('/', "::"))
}

pub fn private_list_path() -> PathBuf {
    util::data_dir().join("vault-files.json")
}

/// The private list, `{"files": [...]}` with entries as in the config's
/// `vault.files`. Empty when it doesn't exist.
fn private_files() -> Result<Vec<VaultFile>, String> {
    private_files_at(&private_list_path())
}

fn private_files_at(p: &Path) -> Result<Vec<VaultFile>, String> {
    let text = match std::fs::read_to_string(p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    let v = json::parse(&text).map_err(|e| format!("{}: {e}", p.display()))?;
    parse_vault_files(v.get("files")).map_err(|e| format!("{}: {e}", p.display()))
}

/// Private files the config doesn't already list.
fn extra_private_files(v: &Vault) -> Result<Vec<VaultFile>, String> {
    Ok(private_files()?
        .into_iter()
        .filter(|f| !v.files.iter().any(|g| g.path == f.path))
        .collect())
}

/// The config's files, then the private ones.
fn all_files(v: &Vault) -> Result<Vec<VaultFile>, String> {
    let mut all = v.files.clone();
    all.extend(extra_private_files(v)?);
    Ok(all)
}

fn hash_file(p: &Path) -> Option<String> {
    std::fs::read(p).ok().map(|b| format!("{:016x}", fnv64(&b)))
}

pub fn status(cfg: &Config) -> Result<(), String> {
    let v = vault_cfg(cfg)?;
    let st = VaultState::load()?;
    println!("{} {}", bold("Vault:"), tilde(&v.database));
    if !v.database.exists() {
        println!("  {}", red("the database file doesn't exist"));
    }
    let now = util::now_unix();
    for f in &all_files(v)? {
        let p = expand(&f.path);
        let current = hash_file(&p);
        let line = match (current, st.files.get(&f.path)) {
            (None, Some(_)) => yellow("missing on this Mac (`dotfinder vault restore` brings it back)"),
            (None, None) => dim("doesn't exist; nothing to back up"),
            (Some(_), None) => yellow("never backed up"),
            (Some(h), Some((saved, at))) if &h == saved => {
                green(&format!("backed up {}", util::age(now.saturating_sub(*at))))
            }
            (Some(_), Some(_)) => yellow("changed since the last backup"),
        };
        println!("  {}  {line}", f.path);
    }
    Ok(())
}

/// Problems worth surfacing in `audit`: files never backed up or changed.
pub fn pending(cfg: &Config) -> Vec<String> {
    let (Some(v), Ok(st)) = (cfg.vault.as_ref(), VaultState::load()) else {
        return Vec::new();
    };
    all_files(v)
        .unwrap_or_else(|_| v.files.clone())
        .iter()
        .filter_map(|f| {
            let h = hash_file(&expand(&f.path))?;
            match st.files.get(&f.path) {
                None => Some(format!("{} has never been backed up", f.path)),
                Some((saved, _)) if *saved != h => Some(format!("{} changed since the last backup", f.path)),
                _ => None,
            }
        })
        .collect()
}

struct Cli<'a> {
    db: String,
    password: &'a str,
}

impl Cli<'_> {
    /// `keepassxc-cli <cmd> -q <flags…> <database> <args…>`, with the
    /// password on stdin.
    fn run(&self, cmd: &str, flags: &[&str], args: &[&str]) -> Result<util::Output, String> {
        let mut argv = vec!["keepassxc-cli", cmd, "-q"];
        argv.extend_from_slice(flags);
        argv.push(&self.db);
        argv.extend_from_slice(args);
        let stdin = format!("{}\n", self.password);
        match run(&argv, Some(&stdin), Duration::from_secs(120)) {
            Ok(o) => Ok(o),
            Err(RunError::NotFound) => {
                Err("keepassxc-cli isn't installed (it comes with the KeePassXC cask)".into())
            }
            Err(e) => Err(format!("keepassxc-cli: {e}")),
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("failed")
        .trim()
        .to_string()
}

fn unlock<'a>(v: &Vault, password: &'a str) -> Result<Cli<'a>, String> {
    let cli = Cli {
        db: v.database.display().to_string(),
        password,
    };
    if !v.database.exists() {
        return Err(format!("{} doesn't exist", tilde(&v.database)));
    }
    // A cheap command to check the password before touching anything.
    let out = cli.run("ls", &[], &[])?;
    if !out.ok() {
        return Err(format!("couldn't open the vault: {}", first_line(&out.stderr)));
    }
    Ok(cli)
}

pub fn backup(cfg: &Config) -> Result<(), String> {
    let v = vault_cfg(cfg)?;
    let password =
        read_password(&format!("Master password for {}: ", tilde(&v.database))).map_err(|e| e.to_string())?;
    let cli = unlock(v, &password)?;
    let mut st = VaultState::load()?;
    let mut group_ready = false;
    let mut failures = 0;

    for f in &all_files(v)? {
        let p = expand(&f.path);
        let Some(hash) = hash_file(&p) else {
            println!("  {}  {}", f.path, dim("doesn't exist, skipped"));
            continue;
        };
        if st.files.get(&f.path).is_some_and(|(h, _)| *h == hash) {
            println!("  {}  {}", f.path, dim("unchanged"));
            continue;
        }
        let entry = entry_path(v, f);
        let path = p.display().to_string();
        let mut out = cli.run("attachment-import", &["-f"], &[&entry, ATTACHMENT, &path])?;
        if !out.ok() {
            // Most likely the entry doesn't exist yet: create it, then retry.
            if !group_ready {
                let _ = cli.run("mkdir", &[], &[&v.group])?;
                group_ready = true;
            }
            let added = cli.run("add", &[], &[&entry])?;
            if !added.ok() {
                println!("  {}  {}", f.path, red(&first_line(&added.stderr)));
                failures += 1;
                continue;
            }
            out = cli.run("attachment-import", &["-f"], &[&entry, ATTACHMENT, &path])?;
        }
        if out.ok() {
            st.files.insert(f.path.clone(), (hash, util::now_unix()));
            println!("  {}  {}", f.path, green("backed up"));
        } else {
            println!("  {}  {}", f.path, red(&first_line(&out.stderr)));
            failures += 1;
        }
    }
    st.save().map_err(|e| e.to_string())?;
    if failures > 0 {
        return Err(format!("{failures} files couldn't be backed up"));
    }
    println!(
        "{}",
        dim("If KeePassXC is open, it reloads the vault to show the changes.")
    );
    Ok(())
}

pub fn restore(cfg: &Config) -> Result<(), String> {
    let v = vault_cfg(cfg)?;
    let password =
        read_password(&format!("Master password for {}: ", tilde(&v.database))).map_err(|e| e.to_string())?;
    let cli = unlock(v, &password)?;
    let mut st = VaultState::load()?;
    let mut failures = 0;
    for f in &v.files {
        if !restore_file(&cli, v, f, &mut st)? {
            failures += 1;
        }
    }
    // The private list is in place now, if it was in the vault.
    for f in &extra_private_files(v)? {
        if !restore_file(&cli, v, f, &mut st)? {
            failures += 1;
        }
    }
    st.save().map_err(|e| e.to_string())?;
    if failures > 0 {
        return Err(format!("{failures} files weren't restored"));
    }
    Ok(())
}

/// Writes one file back from the vault. False when it failed.
fn restore_file(cli: &Cli, v: &Vault, f: &VaultFile, st: &mut VaultState) -> Result<bool, String> {
    let target = expand(&f.path);
    let dir = target.parent().map(Path::to_path_buf).unwrap_or_else(home);
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // Folders holding private files are private too.
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let tmp: PathBuf = dir.join(format!(
        ".{}.dotfinder-restore",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let entry = entry_path(v, f);
    let out = cli.run(
        "attachment-export",
        &[],
        &[&entry, ATTACHMENT, &tmp.display().to_string()],
    )?;
    if !out.ok() {
        let _ = std::fs::remove_file(&tmp);
        let why = first_line(&out.stderr);
        if why.contains("Could not find") {
            // Never backed up: nothing to restore, not an error.
            println!("  {}  {}", f.path, dim("not in the vault"));
            return Ok(true);
        }
        println!("  {}  {}", f.path, red(&why));
        return Ok(false);
    }
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(f.mode));
    let new_hash = hash_file(&tmp).unwrap_or_default();
    match hash_file(&target) {
        Some(h) if h == new_hash => {
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(f.mode));
            println!("  {}  {}", f.path, dim("already up to date"));
        }
        existing => {
            if existing.is_some() {
                let keep = target.with_file_name(format!(
                    "{}.pre-restore",
                    target
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ));
                std::fs::rename(&target, &keep).map_err(|e| format!("{}: {e}", target.display()))?;
                println!(
                    "  {}  {}",
                    f.path,
                    dim(&format!("existing copy kept as {}", tilde(&keep)))
                );
            }
            std::fs::rename(&tmp, &target).map_err(|e| format!("{}: {e}", target.display()))?;
            println!("  {}  {}", f.path, green("restored"));
        }
    }
    st.files.insert(f.path.clone(), (new_hash, util::now_unix()));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_titles() {
        let v = Vault {
            database: "/x.kdbx".into(),
            group: "dotfinder".into(),
            files: vec![],
        };
        let f = |p: &str| VaultFile {
            path: p.into(),
            mode: 0o600,
        };
        assert_eq!(entry_path(&v, &f("~/.ssh/config")), "dotfinder/.ssh::config");
        assert_eq!(entry_path(&v, &f("/etc/hosts")), "dotfinder/etc::hosts");
    }

    #[test]
    fn private_list() {
        let d = std::env::temp_dir().join(format!("dotfinder-vault-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let list = d.join("vault-files.json");
        assert!(private_files_at(&list).unwrap().is_empty(), "missing list");
        std::fs::write(
            &list,
            r#"{"files": ["~/.ssh/key", {"path": "~/.local/bin/job", "mode": "755"}]}"#,
        )
        .unwrap();
        let files = private_files_at(&list).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].path.as_str(), files[0].mode), ("~/.ssh/key", 0o600));
        assert_eq!(
            (files[1].path.as_str(), files[1].mode),
            ("~/.local/bin/job", 0o755)
        );
        std::fs::write(&list, r#"{"files": [{"mode": "600"}]}"#).unwrap();
        assert!(private_files_at(&list).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
