//! Turning changes into readable lines, and the audit's findings.

use crate::brew::Brewfile;
use crate::diff::{Change, Kind};
use crate::json::Value;
use crate::snapshot::Snapshot;
use crate::state::AdoptedFile;
use crate::term::{bold, clip, cyan, dim, green, red, yellow};

pub fn section_label(section: &str) -> String {
    match section {
        "defaults" => "Preferences".into(),
        "defaults-currenthost" => "Preferences for this Mac only (-currentHost)".into(),
        "container-defaults" => "Sandboxed app preferences".into(),
        "homebrew-formulae" => "Homebrew formulae".into(),
        "homebrew-casks" => "Homebrew apps (casks)".into(),
        "apps" => "Applications".into(),
        s if s.starts_with("dir:") => format!("Folder {}", &s[4..]),
        "cmd:nix-profile" => "Installed with `nix profile install`".into(),
        "cmd:cargo-install" => "Installed with `cargo install`".into(),
        "cmd:npm-global" => "Installed with `npm install -g`".into(),
        "cmd:uv-tool" => "Installed with `uv tool install`".into(),
        "cmd:pipx" => "Installed with `pipx`".into(),
        "cmd:rustup-toolchain" => "Rust toolchains (rustup)".into(),
        "cmd:login-items" => "Login items".into(),
        s if s.starts_with("cmd:") => format!("From `{}`", &s[4..]),
        s => s.into(),
    }
}

/// Display order of sections.
fn section_rank(section: &str) -> (u8, String) {
    let r = match section {
        "homebrew-casks" => 0,
        "homebrew-formulae" => 1,
        "apps" => 2,
        "defaults" => 3,
        "defaults-currenthost" => 4,
        "container-defaults" => 5,
        s if s.starts_with("dir:") => 6,
        _ => 7,
    };
    (r, section.to_string())
}

pub fn show_value(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("\"{}\"", clip(s, 60)),
        Value::Obj(o) if o.len() == 1 && (o.contains_key("$date") || o.contains_key("$data")) => {
            let (k, inner) = o.iter().next().unwrap();
            format!("{}({})", &k[1..], inner.as_str().unwrap_or(""))
        }
        other => clip(&other.to_compact(), 60),
    }
}

fn item_detail(section: &str, v: &Value) -> String {
    if section == "apps" {
        let source = v.get("source").and_then(Value::as_str).unwrap_or("?");
        return match source {
            "manual" => "installed by hand".into(),
            "yours" => "yours, backed up with `dotfinder apps backup`".into(),
            "app-store" => "App Store".into(),
            "apple" => "Apple".into(),
            "nix" => "Nix".into(),
            s if s.starts_with("made-by:") => format!("made by {}", &s["made-by:".len()..]),
            s => match (
                s.strip_prefix("homebrew:"),
                v.get("renamed_from").and_then(Value::as_str),
            ) {
                (Some(t), Some(from)) => format!("Homebrew cask {t}, renamed from {from}"),
                (Some(t), None) => format!("Homebrew cask {t}"),
                _ => s.into(),
            },
        };
    }
    if section.starts_with("dir:") {
        let kind = v.get("kind").and_then(Value::as_str).unwrap_or("?");
        let managed = v.get("managed").and_then(Value::as_bool).unwrap_or(false);
        if let Some(rest) = v.get("unmanaged").and_then(Value::as_arr) {
            let names: Vec<&str> = rest.iter().filter_map(Value::as_str).collect();
            return format!("{kind}, partly managed; not: {}", names.join(", "));
        }
        return if managed {
            format!("{kind}, managed")
        } else {
            format!("{kind}, not managed")
        };
    }
    if section.starts_with("defaults") || section == "container-defaults" {
        let n = v.as_obj().map(|o| o.len()).unwrap_or(0);
        return format!("new domain, {n} keys");
    }
    match v {
        Value::Null => String::new(),
        Value::Str(s) if s.is_empty() => String::new(),
        Value::Str(s) => s.clone(),
        other => show_value(other),
    }
}

/// One line describing a change, without the leading marker.
pub fn describe(c: &Change) -> String {
    match (&c.key, c.kind) {
        (Some(k), Kind::Added) => format!(
            "{} › {} = {}",
            c.item,
            bold(k),
            show_value(c.new.as_ref().unwrap())
        ),
        (Some(k), Kind::Removed) => format!(
            "{} › {} (was {})",
            c.item,
            bold(k),
            show_value(c.old.as_ref().unwrap())
        ),
        (Some(k), Kind::Changed) => format!(
            "{} › {}: {} → {}",
            c.item,
            bold(k),
            show_value(c.old.as_ref().unwrap()),
            show_value(c.new.as_ref().unwrap())
        ),
        (None, Kind::Added) => {
            let d = item_detail(&c.section, c.new.as_ref().unwrap());
            if d.is_empty() {
                bold(&c.item)
            } else {
                format!("{} {}", bold(&c.item), dim(&format!("({d})")))
            }
        }
        (None, Kind::Removed) => bold(&c.item),
        (None, Kind::Changed) => format!(
            "{}: {} → {}",
            bold(&c.item),
            show_value(c.old.as_ref().unwrap()),
            show_value(c.new.as_ref().unwrap())
        ),
    }
}

pub fn marker(k: Kind) -> String {
    match k {
        Kind::Added => green("+"),
        Kind::Removed => red("-"),
        Kind::Changed => yellow("~"),
    }
}

/// A warning attached to a change, when one applies.
pub fn hint(c: &Change, declared: Option<&Brewfile>) -> Option<String> {
    match (c.section.as_str(), c.kind, &c.key) {
        ("homebrew-casks", Kind::Added, None) => match declared {
            Some(bf) if bf.declares_cask(&c.item) => None,
            _ => Some("not in the config: the next rebuild uninstalls it unless you adopt it".into()),
        },
        ("homebrew-formulae", Kind::Added, None) => match declared {
            Some(bf) if bf.declares_brew(&c.item) => None,
            _ => Some("not in the config: the next rebuild uninstalls it unless you adopt it".into()),
        },
        ("homebrew-casks" | "homebrew-formulae", Kind::Removed, None) => {
            Some("if the config declares it, the next rebuild reinstalls it".into())
        }
        _ => None,
    }
}

pub fn print_changes(changes: &[Change], declared: Option<&Brewfile>) {
    let mut sorted: Vec<&Change> = changes.iter().collect();
    sorted.sort_by_key(|a| section_rank(&a.section));
    let mut current = String::new();
    for c in sorted {
        if c.section != current {
            current = c.section.clone();
            println!("\n{}", bold(&section_label(&current)));
        }
        println!("  {} {}", marker(c.kind), describe(c));
        if let Some(h) = hint(c, declared) {
            println!("      {}", yellow(&h));
        }
    }
}

/// Everything on the machine that the config doesn't capture, as
/// "added" changes so `adopt --audit` can walk through them.
/// `skip_sections`: sections the audit leaves out (folders with `audit: false`).
/// The package name in a `uv tool install` argument: "mlx-lm" for
/// "mlx-lm[train]>=0.20".
fn uv_tool_name(spec: &str) -> &str {
    let end = spec.find(|c: char| "[=<>~!@; ".contains(c)).unwrap_or(spec.len());
    &spec[..end]
}

/// `adopted`: what dotfinder/adopted.json accounts for beyond Homebrew: App
/// Store apps and apps to install by hand, recorded renames, uv tools.
pub fn audit_findings(
    s: &Snapshot,
    declared: Option<&Brewfile>,
    skip_sections: &[String],
    adopted: &AdoptedFile,
) -> Vec<Change> {
    let known_apps = adopted.known_app_names();
    let mut out = Vec::new();
    let add = |out: &mut Vec<Change>, section: &str, item: &str, v: &Value| {
        out.push(Change {
            section: section.into(),
            item: item.into(),
            key: None,
            kind: Kind::Added,
            old: None,
            new: Some(v.clone()),
        });
    };
    for (name, sec) in &s.sections {
        if !sec.available || skip_sections.contains(name) {
            continue;
        }
        for (item, v) in &sec.items {
            let include = match name.as_str() {
                "homebrew-casks" => declared.is_some_and(|bf| !bf.declares_cask(item)),
                "homebrew-formulae" => declared.is_some_and(|bf| !bf.declares_brew(item)),
                "apps" => {
                    let name = item.rsplit('/').next().unwrap_or(item).trim_end_matches(".app");
                    let source = v.get("source").and_then(Value::as_str).unwrap_or("");
                    match source.strip_prefix("homebrew:") {
                        // A renamed cask app whose new name the config doesn't record.
                        Some(token) => {
                            v.get("renamed_from").is_some() && !adopted.renames.contains_key(token)
                        }
                        None => {
                            matches!(source, "manual" | "app-store") && !known_apps.iter().any(|k| k == name)
                        }
                    }
                }
                n if n.starts_with("dir:") => v.get("managed") == Some(&Value::Bool(false)),
                "cmd:uv-tool" => !adopted.uv_tools.iter().any(|t| uv_tool_name(t) == item),
                n if n.starts_with("cmd:") => n != "cmd:login-items" && n != "cmd:rustup-toolchain",
                _ => false,
            };
            if include {
                add(&mut out, name, item, v);
            }
        }
    }
    out
}

pub fn audit_intro(section: &str) -> Option<&'static str> {
    Some(match section {
        "homebrew-casks" | "homebrew-formulae" => {
            "Not declared in the config; the next rebuild uninstalls these:"
        }
        "apps" => "Not managed by Homebrew or Nix, or renamed without the config knowing:",
        s if s.starts_with("dir:") => "Not managed by the config (and not backed up by it):",
        s if s.starts_with("cmd:") => "Installed outside the config:",
        _ => return None,
    })
}

pub fn print_audit(findings: &[Change], cask_hint: &dyn Fn(&str, &str) -> Vec<String>) {
    let mut sorted: Vec<&Change> = findings.iter().collect();
    sorted.sort_by_key(|a| section_rank(&a.section));
    let mut current = String::new();
    for c in sorted {
        if c.section != current {
            current = c.section.clone();
            println!("\n{}", bold(&section_label(&current)));
            if let Some(intro) = audit_intro(&current) {
                println!("  {}", dim(intro));
            }
        }
        println!("  {} {}", cyan("•"), describe(c));
        if c.section == "apps"
            && matches!(
                c.new
                    .as_ref()
                    .and_then(|v| v.get("source"))
                    .and_then(Value::as_str),
                Some("manual" | "app-store")
            )
        {
            let name = c.item.rsplit('/').next().unwrap_or(&c.item);
            let id = c
                .new
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let casks = cask_hint(name, id);
            if !casks.is_empty() {
                println!(
                    "      {}",
                    dim(&format!("available as cask: {}", casks.join(", ")))
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Section;
    use std::collections::BTreeMap;

    #[test]
    fn listed_apps_are_not_findings() {
        let app = |source: &str| {
            let mut o = BTreeMap::new();
            o.insert("source".to_string(), Value::Str(source.into()));
            Value::Obj(o)
        };
        let mut items = BTreeMap::new();
        items.insert("/Applications/Things 3.app".to_string(), app("app-store"));
        items.insert("/Applications/Zed.app".to_string(), app("manual"));
        items.insert("/Applications/Other.app".to_string(), app("manual"));
        items.insert("/Applications/Ghostty.app".to_string(), app("homebrew:ghostty"));
        let mut sections = BTreeMap::new();
        sections.insert(
            "apps".to_string(),
            Section {
                available: true,
                note: None,
                items,
            },
        );
        let s = Snapshot {
            taken_at: 0,
            kind: "manual".into(),
            generation: None,
            sections,
        };
        let mut adopted = AdoptedFile::default();
        adopted.manual_apps.push("Zed".into());
        adopted.mas_apps.insert("Things 3".into(), 904280696);
        let found: Vec<String> = audit_findings(&s, None, &[], &adopted)
            .into_iter()
            .map(|c| c.item)
            .collect();
        assert_eq!(found, ["/Applications/Other.app"]);
    }

    #[test]
    fn listed_uv_tools_are_not_findings() {
        let mut items = BTreeMap::new();
        for t in ["mlx-lm", "mlx-vlm", "ruff"] {
            items.insert(t.to_string(), Value::Null);
        }
        let mut sections = BTreeMap::new();
        sections.insert(
            "cmd:uv-tool".to_string(),
            Section {
                available: true,
                note: None,
                items,
            },
        );
        let s = Snapshot {
            taken_at: 0,
            kind: "manual".into(),
            generation: None,
            sections,
        };
        let mut adopted = AdoptedFile::default();
        adopted.uv_tools = vec!["mlx-lm".into(), "mlx-vlm[torch]>=0.1".into()];
        let found: Vec<String> = audit_findings(&s, None, &[], &adopted)
            .into_iter()
            .map(|c| c.item)
            .collect();
        assert_eq!(found, ["ruff"]);
    }

    #[test]
    fn unrecorded_renames_are_findings() {
        let mut fan = BTreeMap::new();
        fan.insert(
            "source".to_string(),
            Value::Str("homebrew:macs-fan-control".into()),
        );
        fan.insert(
            "renamed_from".to_string(),
            Value::Str("Macs Fan Control.app".into()),
        );
        let mut items = BTreeMap::new();
        items.insert("/Applications/Fan Control.app".to_string(), Value::Obj(fan));
        let mut sections = BTreeMap::new();
        sections.insert(
            "apps".to_string(),
            Section {
                available: true,
                note: None,
                items,
            },
        );
        let s = Snapshot {
            taken_at: 0,
            kind: "manual".into(),
            generation: None,
            sections,
        };
        assert_eq!(audit_findings(&s, None, &[], &AdoptedFile::default()).len(), 1);
        let mut adopted = AdoptedFile::default();
        adopted.renames.insert(
            "macs-fan-control".into(),
            crate::state::Rename {
                from: "Macs Fan Control.app".into(),
                to: "Fan Control.app".into(),
            },
        );
        assert!(audit_findings(&s, None, &[], &adopted).is_empty());
    }
}
