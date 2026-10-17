//! What `adopt` says about files and folders in the watched folders: a note
//! on what to do with each, and a listing of a folder's contents.

use std::path::Path;

use crate::json::Value;
use crate::snapshot::is_managed;
use crate::util::tilde;

/// A folder in ~ that a tool keeps its own data in, with the few files in
/// it that hold settings worth managing.
struct ToolFolder {
    name: &'static str,
    tool: &'static str,
    holds: &'static str,
    settings: &'static [&'static str],
    /// Credentials and keys, which stay out of a public repo.
    private: &'static [&'static str],
}

const TOOL_FOLDERS: &[ToolFolder] = &[
    ToolFolder {
        name: ".cargo",
        tool: "Cargo",
        holds: "downloaded crates and the commands `cargo install` adds",
        settings: &["config.toml", "config"],
        private: &["credentials.toml", "credentials"],
    },
    ToolFolder {
        name: ".gemini",
        tool: "Gemini CLI",
        holds: "its sign-in and session data",
        settings: &["settings.json", "GEMINI.md"],
        private: &["oauth_creds.json", "google_accounts.json"],
    },
    ToolFolder {
        name: ".gnupg",
        tool: "GnuPG",
        holds: "your keyrings",
        settings: &["gpg.conf", "gpg-agent.conf", "dirmngr.conf"],
        private: &["private-keys-v1.d", "pubring.kbx", "trustdb.gpg"],
    },
];

/// The note `adopt` shows for an entry of a watched folder. `in_home`: the
/// entry is directly in ~, where the tool folders above live.
pub fn note(path: &Path, in_home: bool, v: Option<&Value>, dotfiles: &Path) -> String {
    let dotfiles = &canonical(dotfiles);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if in_home && let Some(t) = TOOL_FOLDERS.iter().find(|t| t.name == name) {
        return tool_note(t, path, dotfiles);
    }
    let kind = v.and_then(|v| v.get("kind")).and_then(Value::as_str);
    let partly = v.and_then(|v| v.get("unmanaged")).is_some();
    let public = format!(
        "Check {} for secrets first: {} is public.",
        if kind == Some("folder") { "them" } else { "it" },
        tilde(dotfiles)
    );
    let mut s = String::new();
    if partly {
        s.push_str("Part of it is already managed. ");
    }
    if kind == Some("folder") {
        s.push_str("If a tool made this folder for its own use, ignore it. If it holds files you wrote or settings you chose, move those into your dotfiles repo and link them from a home-manager module. ");
    } else {
        s.push_str("If a tool made it for its own use, ignore it. If you wrote it or it holds settings you chose, move it into your dotfiles repo and link it from a home-manager module. ");
    }
    s.push_str(&public);
    s
}

fn tool_note(t: &ToolFolder, dir: &Path, dotfiles: &Path) -> String {
    let present = |names: &[&'static str]| -> Vec<&'static str> {
        names
            .iter()
            .copied()
            .filter(|n| std::fs::symlink_metadata(dir.join(n)).is_ok())
            .collect()
    };
    let settings = present(t.settings);
    let (managed, unmanaged): (Vec<&str>, Vec<&str>) =
        settings.iter().partition(|n| is_managed(&dir.join(n), dotfiles));
    let mut s = format!("{} keeps {} here. ", t.tool, t.holds);
    if settings.is_empty() {
        s.push_str(&format!(
            "None of its settings files ({}) are in it, so ignore it.",
            t.settings.join(", ")
        ));
        return s;
    }
    if unmanaged.is_empty() {
        s.push_str(&format!(
            "Its settings ({}) are already managed, and the rest is {}'s own, so ignore the folder.",
            managed.join(", "),
            t.tool
        ));
        return s;
    }
    s.push_str(&match unmanaged.as_slice() {
        [one] => format!(
            "Its settings file, {one}, is the part worth managing: move it into your dotfiles repo and link it from a home-manager module."
        ),
        many => format!(
            "Its settings files, {}, are the part worth managing: move them into your dotfiles repo and link them from a home-manager module.",
            many.join(" and ")
        ),
    });
    s.push_str(" Either way, ignore the folder itself.");
    let private = present(t.private);
    if !private.is_empty() {
        s.push_str(&format!(" Keep {} out of the repo.", private.join(" and ")));
    }
    s
}

/// One line per entry in `dir`, sorted by name: folders with how many
/// items they hold, files with their size, links with their target.
/// Entries the config manages are marked.
pub fn listing(dir: &Path, dotfiles: &Path, limit: usize) -> Vec<String> {
    let dotfiles = &canonical(dotfiles);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec!["(can't be read)".into()];
    };
    let mut entries: Vec<_> = rd.flatten().filter(|e| e.file_name() != ".DS_Store").collect();
    entries.sort_by_key(|e| e.file_name());
    let total = entries.len();
    let mut lines: Vec<String> = entries
        .iter()
        .take(limit)
        .map(|e| {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let detail = match std::fs::symlink_metadata(&p) {
                Ok(m) if m.file_type().is_symlink() => match std::fs::read_link(&p) {
                    Ok(t) => format!("→ {}", tilde(&t)),
                    Err(_) => "link".into(),
                },
                Ok(m) if m.is_dir() => {
                    let n = std::fs::read_dir(&p).map(|r| r.count()).unwrap_or(0);
                    if n == 1 {
                        "1 item".into()
                    } else {
                        format!("{n} items")
                    }
                }
                Ok(m) => size(m.len()),
                Err(_) => String::new(),
            };
            let slash = if p.is_dir() && !p.is_symlink() { "/" } else { "" };
            let managed = if is_managed(&p, dotfiles) { ", managed" } else { "" };
            format!("{name}{slash}  {detail}{managed}")
        })
        .collect();
    if total == 0 {
        lines.push("(empty)".into());
    } else if total > limit {
        lines.push(format!("… and {} more", total - limit));
    }
    lines
}

/// Managed links are recognized by where they resolve to, so the repo's
/// own path has to be resolved too.
fn canonical(p: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dotfinder-folders-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn tool_folder_points_at_its_settings() {
        let d = scratch("cargo");
        let cargo = d.join(".cargo");
        fs::create_dir_all(cargo.join("registry")).unwrap();
        let none = note(&cargo, true, None, &d.join("dotfiles"));
        assert!(none.contains("None of its settings files"), "{none}");
        fs::write(cargo.join("config.toml"), "[build]\n").unwrap();
        fs::write(cargo.join("credentials.toml"), "x").unwrap();
        let some = note(&cargo, true, None, &d.join("dotfiles"));
        assert!(
            some.contains("Its settings file, config.toml, is the part worth managing"),
            "{some}"
        );
        assert!(some.contains("Keep credentials.toml out of the repo."), "{some}");
        // Only directly in ~.
        assert!(note(&cargo, false, None, &d.join("dotfiles")).starts_with("If a tool"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn listing_and_sizes() {
        let d = scratch("list");
        fs::create_dir_all(d.join("bin/x")).unwrap();
        fs::write(d.join("a.toml"), vec![0u8; 1500]).unwrap();
        fs::write(d.join(".DS_Store"), "").unwrap();
        std::os::unix::fs::symlink("/nowhere", d.join("z")).unwrap();
        assert_eq!(
            listing(&d, &d.join("dotfiles"), 2),
            ["a.toml  1.5 KB", "bin/  1 item", "… and 1 more"]
        );
        assert_eq!(listing(&d, &d.join("dotfiles"), 10)[2], "z  → /nowhere");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(12_300_000), "12 MB");
        fs::remove_dir_all(&d).unwrap();
    }
}
