//! Comparing two snapshots, and the ignore rules that filter the result.
//!
//! Every change and every audit finding has an id of the form
//! `section:item` or `section:item:key`, for example
//! `defaults:com.apple.dock:autohide` or `dir:~/.config:opencode`. Ignore
//! rules are case-insensitive globs over those ids. A rule that matches
//! `section:item` also covers every key under that item.

use crate::json::Value;
use crate::snapshot::{Snapshot, is_nested};
use crate::util::glob;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub section: String,
    pub item: String,
    pub key: Option<String>,
    pub kind: Kind,
    pub old: Option<Value>,
    pub new: Option<Value>,
}

impl Change {
    pub fn id(&self) -> String {
        match &self.key {
            Some(k) => format!("{}:{}:{}", self.section, self.item, k),
            None => format!("{}:{}", self.section, self.item),
        }
    }

    /// The id one level up: the whole defaults domain or app for a key.
    pub fn parent_id(&self) -> String {
        format!("{}:{}", self.section, self.item)
    }
}

pub struct Diff {
    pub changes: Vec<Change>,
    /// Sections not compared because one side couldn't read them.
    pub skipped: Vec<(String, String)>,
}

pub fn diff(old: &Snapshot, new: &Snapshot) -> Diff {
    let mut changes = Vec::new();
    let mut skipped = Vec::new();
    let names: std::collections::BTreeSet<&String> = old.sections.keys().chain(new.sections.keys()).collect();
    for name in names {
        let (Some(a), Some(b)) = (old.sections.get(name), new.sections.get(name)) else {
            if new.sections.get(name).is_some_and(|s| s.available) {
                skipped.push((name.clone(), "not in the baseline".into()));
            }
            continue;
        };
        if !a.available || !b.available {
            if a.available != b.available {
                let why = if b.available {
                    a.note.clone()
                } else {
                    b.note.clone()
                };
                skipped.push((name.clone(), why.unwrap_or_else(|| "couldn't be read".into())));
            }
            continue;
        }
        let nested = is_nested(name);
        let items: std::collections::BTreeSet<&String> = a.items.keys().chain(b.items.keys()).collect();
        for item in items {
            let push = |changes: &mut Vec<Change>,
                        key: Option<&String>,
                        kind,
                        o: Option<&Value>,
                        n: Option<&Value>| {
                changes.push(Change {
                    section: name.clone(),
                    item: item.clone(),
                    key: key.cloned(),
                    kind,
                    old: o.cloned(),
                    new: n.cloned(),
                });
            };
            match (a.items.get(item), b.items.get(item)) {
                (None, Some(n)) => push(&mut changes, None, Kind::Added, None, Some(n)),
                (Some(o), None) => push(&mut changes, None, Kind::Removed, Some(o), None),
                (Some(o), Some(n)) if o != n => match (nested, o, n) {
                    (true, Value::Obj(om), Value::Obj(nm)) => {
                        let keys: std::collections::BTreeSet<&String> = om.keys().chain(nm.keys()).collect();
                        for k in keys {
                            match (om.get(k), nm.get(k)) {
                                (None, Some(v)) => push(&mut changes, Some(k), Kind::Added, None, Some(v)),
                                (Some(v), None) => push(&mut changes, Some(k), Kind::Removed, Some(v), None),
                                (Some(x), Some(y)) if x != y => {
                                    push(&mut changes, Some(k), Kind::Changed, Some(x), Some(y))
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => push(&mut changes, None, Kind::Changed, Some(o), Some(n)),
                },
                _ => {}
            }
        }
    }
    Diff { changes, skipped }
}

pub struct Ignores {
    pub patterns: Vec<String>,
}

impl Ignores {
    pub fn matches_id(&self, id: &str) -> bool {
        self.patterns.iter().any(|p| glob(p, id))
    }

    pub fn matches(&self, c: &Change) -> bool {
        self.matches_id(&c.id()) || (c.key.is_some() && self.matches_id(&c.parent_id()))
    }
}

/// Patterns for state that changes constantly without anyone touching a
/// setting: window positions, recent files, timestamps, counters, caches.
pub fn builtin_ignores() -> Vec<String> {
    let keys = [
        "NSWindow Frame *",
        "NSSplitView Subview Frames *",
        "NSToolbar Configuration *",
        "NSTableView *",
        "NSOutlineView *",
        "NSNavLastRootDirectory",
        "NSNavLastCurrentDirectory",
        "NSNavPanelFileListMode*",
        "NSOSPLastRootDirectory",
        "NSRecent*",
        "RecentDocuments*",
        "*RecentFolders*",
        "*RecentSearches*",
        "*LastUsed*",
        "*LastLaunch*",
        "*LastRun*",
        "*LastCheck*",
        "*LastUpdate*",
        "*LastSeen*",
        "*LastSync*",
        "*Last*Date*",
        "*LastAttempt*",
        "*Timestamp*",
        "*LaunchCount*",
        "SULastCheckTime",
        "SULastProfileSubmissionDate",
        "SUUpdateGroupIdentifier",
        "SUHasLaunchedBefore",
        "NSStatusItem Preferred Position *",
    ];
    let mut v: Vec<String> = keys.iter().map(|k| format!("defaults*:*:{k}")).collect();
    let domains = [
        "com.apple.suggestions*",
        "com.apple.knowledge-agent",
        "com.apple.routined",
        "com.apple.biome*",
        "ContextStoreAgent",
        "com.apple.CallHistory*",
        "com.apple.cloudd",
        "com.apple.cloudphotod",
        "com.apple.bird",
        "com.apple.CloudKit*",
        "com.apple.xpc.activity2",
        "com.apple.DataDeliveryServices",
        "com.apple.appstored",
        "com.apple.commerce*",
        "com.apple.AppStoreComponents",
        "com.apple.iTunes*",
        "com.apple.mediaanalysisd",
        "com.apple.photoanalysisd",
        "com.apple.ScreenTimeAgent",
        "com.apple.UsageTracking*",
        "com.apple.coreservices.useractivityd",
        "com.apple.tipsd",
        "com.apple.amsengagementd",
        "com.apple.ap.adprivacyd",
        "com.apple.iCloudNotificationAgent",
        "com.apple.icloud.fmfd",
        "com.apple.spotlight*",
        "com.apple.proactive*",
        "com.apple.duetexpertd",
        "com.apple.gamed",
        "com.apple.stocks*",
        "com.apple.weather*",
        "com.apple.news*",
        "com.apple.voicetrigger*",
        "com.apple.chronod",
        "com.apple.remindd",
        "com.apple.ncplugin*",
        "com.apple.security.KCN",
        "com.apple.TextInputMenuAgent",
        "com.apple.storeagent",
        "com.apple.ReportCrash",
        "com.apple.SetupAssistant",
        "com.apple.homed*",
        "com.apple.sidecar*",
        "com.apple.BKAgentService",
    ];
    v.extend(domains.iter().map(|d| format!("defaults*:{d}")));
    for k in [
        "mod-count",
        "lastShowIndicatorTime",
        "trash-full",
        "recent-apps",
        "last-analytics-stamp",
        "region",
        "loc",
        "version",
    ] {
        v.push(format!("defaults:com.apple.dock:{k}"));
    }
    for k in [
        "FXRecentFolders",
        "RecentMoveAndCopyDestinations",
        "GoToField*",
        "FXConnectToLastURL",
        "FXDesktopVolumePositions",
    ] {
        v.push(format!("defaults:com.apple.finder:{k}"));
    }
    for k in [
        "AppleSelectedInputSources",
        "AppleInputSourceHistory",
        "AppleCurrentKeyboardLayoutInputSourceID",
    ] {
        v.push(format!("defaults:com.apple.HIToolbox:{k}"));
    }
    for k in [
        "NSLinguisticDataAssets*",
        "AKLast*",
        "NSSpellCheckerDictionaryContainerTransitionComplete",
    ] {
        v.push(format!("defaults:NSGlobalDomain:{k}"));
    }
    // Entries in ~ that are containers for other watched things, caches or
    // shell history rather than configuration.
    for e in [
        ".config",
        ".local",
        ".cache",
        ".zsh_sessions",
        ".Trash",
        ".ssh",
        ".nix-*",
        "*history*",
        ".lesshst",
        ".viminfo",
    ] {
        v.push(format!("dir:~:{e}"));
    }
    // Apps update themselves; a new version isn't a configuration change.
    v.push("apps:*:version".into());
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;
    use crate::snapshot::Snapshot;

    fn snap(sections: &str) -> Snapshot {
        Snapshot::from_value(
            &parse(&format!(
                r#"{{"version":1,"taken_at":0,"kind":"t","sections":{sections}}}"#
            ))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn nested_and_flat() {
        let a = snap(
            r#"{"defaults":{"available":true,"items":{"com.apple.dock":{"autohide":true,"tilesize":48},"gone":{"x":1}}},
                "homebrew-casks":{"available":true,"items":{"ghostty":"1.0","old":"1"}},
                "cmd:pipx":{"available":false,"note":"not installed","items":{}}}"#,
        );
        let b = snap(
            r#"{"defaults":{"available":true,"items":{"com.apple.dock":{"autohide":false,"orientation":"left"},"new.app":{"y":2}}},
                "homebrew-casks":{"available":true,"items":{"ghostty":"1.1","spotify":"2"}},
                "cmd:pipx":{"available":true,"items":{"black":null}}}"#,
        );
        let d = diff(&a, &b);
        let ids: Vec<(String, Kind)> = d.changes.iter().map(|c| (c.id(), c.kind)).collect();
        assert!(ids.contains(&("defaults:com.apple.dock:autohide".into(), Kind::Changed)));
        assert!(ids.contains(&("defaults:com.apple.dock:tilesize".into(), Kind::Removed)));
        assert!(ids.contains(&("defaults:com.apple.dock:orientation".into(), Kind::Added)));
        assert!(ids.contains(&("defaults:gone".into(), Kind::Removed)));
        assert!(ids.contains(&("defaults:new.app".into(), Kind::Added)));
        assert!(ids.contains(&("homebrew-casks:ghostty".into(), Kind::Changed)));
        assert!(ids.contains(&("homebrew-casks:spotify".into(), Kind::Added)));
        assert!(ids.contains(&("homebrew-casks:old".into(), Kind::Removed)));
        // pipx became readable: skipped, not reported as everything added.
        assert!(!ids.iter().any(|(i, _)| i.starts_with("cmd:pipx")));
        assert_eq!(d.skipped.len(), 1);
    }

    #[test]
    fn ignoring() {
        let ig = Ignores {
            patterns: builtin_ignores(),
        };
        let c = |id: &str| {
            let mut parts = id.splitn(3, ':');
            Change {
                section: parts.next().unwrap().into(),
                item: parts.next().unwrap().into(),
                key: parts.next().map(str::to_string),
                kind: Kind::Changed,
                old: None,
                new: None,
            }
        };
        assert!(ig.matches(&c("defaults:com.apple.finder:NSWindow Frame Main")));
        assert!(ig.matches(&c("defaults-currenthost:com.apple.spotlight:x")));
        assert!(ig.matches(&c("defaults:com.apple.dock:mod-count")));
        assert!(ig.matches(&c("apps:/Applications/Ghostty.app:version")));
        assert!(!ig.matches(&c("defaults:com.apple.dock:autohide")));
        assert!(!ig.matches(&c("defaults:com.apple.dock:show-recents")));
        let whole = Ignores {
            patterns: vec!["defaults:com.example.app".into()],
        };
        assert!(whole.matches(&c("defaults:com.example.app:anything")));
    }
}
