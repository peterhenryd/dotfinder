//! Configuration, read from `$DOTFINDER_CONFIG` or
//! `~/.config/dotfinder/config.json`. Every field is optional; a missing
//! file means all defaults.

use crate::json::{self, Value};
use crate::util::expand;
use std::path::PathBuf;

pub struct Config {
    /// The dotfiles flake. `adopt` writes `dotfinder/adopted.json` and
    /// `dotfinder/ignored.json` inside it.
    pub dotfiles: PathBuf,
    /// The `darwinConfigurations` attribute; defaults to the LocalHostName,
    /// which is what darwin-rebuild uses.
    pub host: Option<String>,
    pub builtin_ignores: bool,
    pub ignore: Vec<String>,
    /// Also read sandboxed apps' preferences in ~/Library/Containers. Off by
    /// default: macOS asks for permission to read other apps' data.
    pub containers: bool,
    pub watch: Vec<Watch>,
    pub collectors: Vec<Collector>,
    pub vault: Option<Vault>,
    /// Where `dotfinder apps backup` copies your own things from
    /// ~/Applications; a folder in iCloud Drive by default.
    pub apps_backup: PathBuf,
}

const APPS_BACKUP: &str = "~/Library/Mobile Documents/com~apple~CloudDocs/Backups/Applications";

pub struct Watch {
    pub path: String,
    /// Only list entries whose names start with a dot (for `~`).
    pub hidden_only: bool,
    /// Whether `audit` lists this folder's unmanaged entries. Off for folders
    /// that apps fill on their own (Application Support), where only
    /// changes (`drift`) are interesting.
    pub audit: bool,
}

/// An external command whose output is a list of installed things.
pub struct Collector {
    pub name: String,
    pub command: Vec<String>,
    /// Skip lines starting with any of these (indented detail lines, bullets).
    pub skip_prefixes: Vec<String>,
    /// Keep only lines starting with this, with the prefix removed.
    pub prefix: Option<String>,
    /// Keep only this whitespace-separated field (0-based).
    pub field: Option<usize>,
    /// Split the whole output on this separator instead of lines.
    pub split: Option<String>,
    /// Parse the output as JSON and list the keys of the object at this path.
    pub json_keys: Option<String>,
}

pub struct Vault {
    pub database: PathBuf,
    pub group: String,
    pub files: Vec<VaultFile>,
}

#[derive(Clone)]
pub struct VaultFile {
    pub path: String,
    pub mode: u32,
}

/// A vault file list: paths, or `{"path": …, "mode": "644"}` objects
/// (mode 600 when not given).
pub fn parse_vault_files(files: Option<&Value>) -> Result<Vec<VaultFile>, String> {
    files
        .and_then(Value::as_arr)
        .into_iter()
        .flatten()
        .map(|f| match f {
            Value::Str(s) => Ok(VaultFile {
                path: s.clone(),
                mode: 0o600,
            }),
            Value::Obj(_) => Ok(VaultFile {
                path: f
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("vault file without `path`")?
                    .to_string(),
                mode: match f.get("mode") {
                    Some(Value::Str(m)) => {
                        u32::from_str_radix(m, 8).map_err(|_| format!("bad mode `{m}`"))?
                    }
                    None => 0o600,
                    _ => return Err("vault file `mode` is an octal string like \"600\"".to_string()),
                },
            }),
            _ => Err("vault files are strings or objects".to_string()),
        })
        .collect()
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let path = match std::env::var_os("DOTFINDER_CONFIG") {
            Some(p) => PathBuf::from(p),
            None => expand("~/.config/dotfinder/config.json"),
        };
        let v = match std::fs::read_to_string(&path) {
            Ok(text) => json::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::obj(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Config::from_value(&v)
    }

    pub fn from_value(v: &Value) -> Result<Config, String> {
        let dotfiles = expand(
            v.get("dotfiles")
                .and_then(Value::as_str)
                .unwrap_or("~/.config/dotfiles"),
        );
        let host = v.get("host").and_then(Value::as_str).map(str::to_string);
        let builtin_ignores = v.get("builtin_ignores").and_then(Value::as_bool).unwrap_or(true);
        let ignore = strings(v.get("ignore"));
        let containers = v.get("containers").and_then(Value::as_bool).unwrap_or(false);

        let watch = match v.get("watch").and_then(Value::as_arr) {
            Some(items) => items
                .iter()
                .map(|w| match w {
                    Value::Str(s) => Ok(Watch {
                        path: s.clone(),
                        hidden_only: false,
                        audit: true,
                    }),
                    Value::Obj(_) => Ok(Watch {
                        path: w
                            .get("path")
                            .and_then(Value::as_str)
                            .ok_or("watch entry without `path`")?
                            .to_string(),
                        hidden_only: w.get("hidden_only").and_then(Value::as_bool).unwrap_or(false),
                        audit: w.get("audit").and_then(Value::as_bool).unwrap_or(true),
                    }),
                    _ => Err("watch entries are strings or objects".to_string()),
                })
                .collect::<Result<Vec<_>, String>>()?,
            None => default_watch(),
        };

        let collectors = match v.get("collectors").and_then(Value::as_arr) {
            Some(items) => items.iter().map(collector).collect::<Result<Vec<_>, String>>()?,
            None => default_collectors(),
        };

        let vault = match v.get("vault") {
            Some(vv @ Value::Obj(_)) => Some(Vault {
                database: expand(
                    vv.get("database")
                        .and_then(Value::as_str)
                        .ok_or("vault without `database`")?,
                ),
                group: vv
                    .get("group")
                    .and_then(Value::as_str)
                    .unwrap_or("dotfinder")
                    .to_string(),
                files: parse_vault_files(vv.get("files"))?,
            }),
            _ => None,
        };

        let apps_backup = expand(
            v.get("apps_backup")
                .and_then(Value::as_str)
                .unwrap_or(APPS_BACKUP),
        );

        Ok(Config {
            dotfiles,
            host,
            builtin_ignores,
            ignore,
            containers,
            watch,
            collectors,
            vault,
            apps_backup,
        })
    }

    pub fn adopted_path(&self) -> PathBuf {
        self.dotfiles.join("dotfinder/adopted.json")
    }

    pub fn ignored_path(&self) -> PathBuf {
        self.dotfiles.join("dotfinder/ignored.json")
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_arr)
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn collector(v: &Value) -> Result<Collector, String> {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or("collector without `name`")?
        .to_string();
    let command = strings(v.get("command"));
    if command.is_empty() {
        return Err(format!("collector `{name}` has no `command`"));
    }
    Ok(Collector {
        name,
        command,
        skip_prefixes: strings(v.get("skip_prefixes")),
        prefix: v.get("prefix").and_then(Value::as_str).map(str::to_string),
        field: v.get("field").and_then(Value::as_i64).map(|i| i as usize),
        split: v.get("split").and_then(Value::as_str).map(str::to_string),
        json_keys: v.get("json_keys").and_then(Value::as_str).map(str::to_string),
    })
}

fn default_watch() -> Vec<Watch> {
    let mut w = vec![Watch {
        path: "~".into(),
        hidden_only: true,
        audit: true,
    }];
    // (path, listed by audit)
    for (p, audit) in [
        ("~/.config", true),
        ("~/.local/bin", true),
        ("~/Library/LaunchAgents", true),
        ("~/Library/Fonts", true),
        ("~/Library/Services", true),
        ("~/Library/Keyboard Layouts", true),
        ("~/Library/Input Methods", true),
        ("/Library/LaunchAgents", false),
        ("/Library/LaunchDaemons", false),
        ("~/Library/Application Support", false),
    ] {
        w.push(Watch {
            path: p.into(),
            hidden_only: false,
            audit,
        });
    }
    w
}

fn default_collectors() -> Vec<Collector> {
    fn c(name: &str, cmd: &[&str]) -> Collector {
        Collector {
            name: name.into(),
            command: cmd.iter().map(|s| s.to_string()).collect(),
            skip_prefixes: vec![],
            prefix: None,
            field: None,
            split: None,
            json_keys: None,
        }
    }
    vec![
        Collector {
            prefix: Some("Name:".into()),
            ..c("nix-profile", &["nix", "profile", "list"])
        },
        Collector {
            skip_prefixes: vec![" ".into(), "\t".into()],
            field: Some(0),
            ..c("cargo-install", &["cargo", "install", "--list"])
        },
        Collector {
            json_keys: Some("dependencies".into()),
            ..c("npm-global", &["npm", "ls", "-g", "--depth=0", "--json"])
        },
        Collector {
            skip_prefixes: vec!["-".into(), " ".into()],
            field: Some(0),
            ..c("uv-tool", &["uv", "tool", "list"])
        },
        Collector {
            field: Some(0),
            ..c("pipx", &["pipx", "list", "--short"])
        },
        Collector {
            field: Some(0),
            ..c("rustup-toolchain", &["rustup", "toolchain", "list"])
        },
        Collector {
            split: Some(",".into()),
            ..c(
                "login-items",
                &[
                    "osascript",
                    "-e",
                    "tell application \"System Events\" to get the name of every login item",
                ],
            )
        },
    ]
}

/// Item names from a collector's output.
/// `text` without ANSI escape sequences (colors, bold).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, then one final byte in @..~.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn collector_items(c: &Collector, output: &str) -> Result<Vec<String>, String> {
    // Some commands color their output even when it isn't a terminal
    // (`nix profile list` bolds names); the codes would end up in item names.
    let output = &strip_ansi(output);
    if let Some(path) = &c.json_keys {
        let v = json::parse(output).map_err(|e| e.to_string())?;
        return Ok(v
            .pointer(path)
            .and_then(Value::as_obj)
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default());
    }
    let pieces: Vec<&str> = match &c.split {
        Some(sep) => output.split(sep.as_str()).collect(),
        None => output.lines().collect(),
    };
    let mut items = Vec::new();
    for raw in pieces {
        if c.skip_prefixes.iter().any(|p| raw.starts_with(p.as_str())) {
            continue;
        }
        let mut s = raw.trim();
        if let Some(p) = &c.prefix {
            match s.strip_prefix(p.as_str()) {
                Some(rest) => s = rest.trim(),
                None => continue,
            }
        }
        if let Some(i) = c.field {
            match s.split_whitespace().nth(i) {
                Some(f) => s = f,
                None => continue,
            }
        }
        let s = s.trim_end_matches(':');
        if !s.is_empty() {
            items.push(s.to_string());
        }
    }
    items.sort();
    items.dedup();
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(name: &str) -> Collector {
        default_collectors().into_iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn cargo_install_list() {
        let out = "ripgrep v14.1.0:\n    rg\ncargo-edit v0.12.3:\n    cargo-add\n    cargo-rm\n";
        assert_eq!(
            collector_items(&find("cargo-install"), out).unwrap(),
            ["cargo-edit", "ripgrep"]
        );
    }

    #[test]
    fn nix_profile_list() {
        let out = "Name:               hello\nFlake attribute:    legacyPackages.aarch64-darwin.hello\n\nName:               jq\n";
        assert_eq!(
            collector_items(&find("nix-profile"), out).unwrap(),
            ["hello", "jq"]
        );
        let colored = "Name:               \u{1b}[1mhello\u{1b}[0m\n";
        assert_eq!(collector_items(&find("nix-profile"), colored).unwrap(), ["hello"]);
    }

    #[test]
    fn npm_and_uv_and_login_items() {
        let npm = r#"{"name":"lib","dependencies":{"typescript":{"version":"5"},"npm":{"version":"10"}}}"#;
        assert_eq!(
            collector_items(&find("npm-global"), npm).unwrap(),
            ["npm", "typescript"]
        );
        let uv = "ruff v0.6.0\n- ruff\nhttpie v3.2.4\n- http\n- https\n";
        assert_eq!(collector_items(&find("uv-tool"), uv).unwrap(), ["httpie", "ruff"]);
        let li = "AeroSpace, Karabiner-Elements, SwipeAeroSpace\n";
        assert_eq!(
            collector_items(&find("login-items"), li).unwrap(),
            ["AeroSpace", "Karabiner-Elements", "SwipeAeroSpace"]
        );
    }

    #[test]
    fn config_parsing() {
        let v = json::parse(
            r#"{"dotfiles":"/tmp/d","ignore":["x"],"vault":{"database":"/tmp/v.kdbx","files":["~/.ssh/config",{"path":"~/.config/git/identity","mode":"644"}]}}"#,
        )
        .unwrap();
        let c = Config::from_value(&v).unwrap();
        assert_eq!(c.dotfiles, PathBuf::from("/tmp/d"));
        let vault = c.vault.unwrap();
        assert_eq!(vault.group, "dotfinder");
        assert_eq!(vault.files[0].mode, 0o600);
        assert_eq!(vault.files[1].mode, 0o644);
        assert!(!c.watch.is_empty() && !c.collectors.is_empty());
    }
}
