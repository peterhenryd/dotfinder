//! Cask apps kept under a name of your own (`renames` in adopted.json).
//!
//! Homebrew tracks an app by the name its cask installs it under, so a
//! renamed app is given that name back while Homebrew upgrades or
//! uninstalls it, and the rename is applied again afterwards.

use crate::state::Rename;
use crate::term::{dim, yellow};
use crate::util;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn apps_dir() -> PathBuf {
    util::sys("/Applications")
}

/// Moves `from` to `to` inside /Applications when `from` exists and `to`
/// doesn't. Returns whether it moved anything.
fn move_app(dir: &Path, from: &str, to: &str) -> Result<bool, String> {
    let (src, dst) = (dir.join(from), dir.join(to));
    if !src.exists() || dst.exists() {
        return Ok(false);
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::rename(&src, &dst).map_err(|e| format!("{} → {to}: {e}", src.display()))?;
    Ok(true)
}

fn run_all(renames: &BTreeMap<String, Rename>, only: Option<&[String]>, forward: bool) -> Vec<String> {
    let dir = apps_dir();
    let mut moved = Vec::new();
    for (token, r) in renames {
        if only.is_some_and(|o| !o.iter().any(|t| t == token)) {
            continue;
        }
        let (from, to) = if forward {
            (&r.from, &r.to)
        } else {
            (&r.to, &r.from)
        };
        match move_app(&dir, from, to) {
            Ok(true) => {
                println!("  {}", dim(&format!("{from} → {to}")));
                moved.push(token.clone());
            }
            Ok(false) => {}
            Err(e) => println!("  {} {e}", yellow("!")),
        }
    }
    moved
}

/// Gives renamed apps back the names their casks installed them under, so
/// Homebrew can find them. `only` limits it to some casks.
pub fn restore(renames: &BTreeMap<String, Rename>, only: Option<&[String]>) -> Vec<String> {
    run_all(renames, only, false)
}

/// Renames apps to the names you gave them.
pub fn apply(renames: &BTreeMap<String, Rename>) -> Vec<String> {
    run_all(renames, None, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_only_when_clear() {
        let dir = std::env::temp_dir().join(format!("dotfinder-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Macs Fan Control.app/Contents")).unwrap();
        assert!(move_app(&dir, "Macs Fan Control.app", "Fan Control.app").unwrap());
        assert!(dir.join("Fan Control.app/Contents").exists());
        // Nothing left to move, and an existing target is never replaced.
        assert!(!move_app(&dir, "Macs Fan Control.app", "Fan Control.app").unwrap());
        std::fs::create_dir_all(dir.join("Macs Fan Control.app")).unwrap();
        assert!(!move_app(&dir, "Macs Fan Control.app", "Fan Control.app").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
