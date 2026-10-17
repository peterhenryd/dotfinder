//! The top-level commands other than `adopt` and `vault`.

use crate::brew::{self, Brewfile};
use crate::config::Config;
use crate::diff::{Change, diff};
use crate::report::{audit_findings, print_audit, print_changes};
use crate::snapshot::{Snapshot, current_generation};
use crate::state::{self, AdoptedFile, latest_baseline};
use crate::term::{ask_line, bold, choose, dim, green, heading, red, warn, yellow};
use crate::util::{self, tilde};

pub fn baseline(cfg: &Config) -> Result<(), String> {
    eprintln!("Recording the current state as the baseline…");
    let s = Snapshot::take(cfg, "baseline", true);
    let path = state::save_baseline(&s).map_err(|e| e.to_string())?;
    report_unavailable(&s);
    println!("{} {}", green("Baseline recorded:"), tilde(&path));
    Ok(())
}

pub fn snapshot(cfg: &Config) -> Result<(), String> {
    let s = Snapshot::take(cfg, "manual", true);
    print!("{}", s.to_value().to_pretty());
    Ok(())
}

fn report_unavailable(s: &Snapshot) {
    let missing: Vec<String> = s
        .sections
        .iter()
        .filter(|(n, sec)| !sec.available && !n.starts_with("cmd:") && !n.starts_with("dir:"))
        .map(|(n, sec)| format!("{n} ({})", sec.note.as_deref().unwrap_or("unavailable")))
        .collect();
    if !missing.is_empty() {
        warn(&format!("couldn't read: {}", missing.join(", ")));
    }
}

/// Changes since the baseline, split into shown and ignored.
pub struct DriftResult {
    pub shown: Vec<Change>,
    pub hidden: usize,
}

pub fn compute_drift(
    cfg: &Config,
    show_all: bool,
) -> Result<Option<(DriftResult, Snapshot, Snapshot)>, String> {
    let Some(base) = latest_baseline()? else {
        println!(
            "There's no baseline yet. Record one with {} (or {}, which records one after every switch).",
            bold("dotfinder baseline"),
            bold("dotfinder rebuild")
        );
        return Ok(None);
    };
    let ignores = state::ignores(cfg)?;
    eprintln!(
        "Comparing with the baseline from {}…",
        util::age(util::now_unix().saturating_sub(base.taken_at))
    );
    let now = Snapshot::take(cfg, "manual", true);
    let d = diff(&base, &now);
    let mut shown = Vec::new();
    let mut hidden = 0;
    for c in d.changes {
        if !show_all && ignores.matches(&c) {
            hidden += 1;
        } else {
            shown.push(c);
        }
    }
    for (section, why) in &d.skipped {
        warn(&format!("{section} wasn't compared: {why}"));
    }
    Ok(Some((DriftResult { shown, hidden }, base, now)))
}

fn generation_warning(base: &Snapshot) {
    let current = current_generation();
    if base.generation.is_some() && current.is_some() && base.generation != current {
        warn(
            "the system was rebuilt after this baseline was recorded, so some changes below come from the rebuild. \
             `dotfinder rebuild` records a new baseline automatically; `dotfinder baseline` records one now.",
        );
    }
}

pub fn drift(cfg: &Config, show_all: bool) -> Result<(), String> {
    let Some((r, base, _now)) = compute_drift(cfg, show_all)? else {
        return Ok(());
    };
    generation_warning(&base);
    let declared = brew::current_brewfile();
    if r.shown.is_empty() {
        println!("\n{}", green("Nothing has changed since the baseline."));
    } else {
        print_changes(&r.shown, declared.as_ref());
    }
    if r.hidden > 0 {
        println!(
            "\n{}",
            dim(&format!(
                "{} changes hidden by ignore rules (`dotfinder drift --all` shows them).",
                r.hidden
            ))
        );
    }
    if !r.shown.is_empty() {
        println!(
            "\nNext: {} to add these to the config or ignore them.",
            bold("dotfinder adopt")
        );
    }
    Ok(())
}

pub fn audit_list(cfg: &Config, show_all: bool) -> Result<(Vec<Change>, usize, Option<Brewfile>), String> {
    let ignores = state::ignores(cfg)?;
    eprintln!("Reading the current state…");
    let now = Snapshot::take(cfg, "manual", true);
    let declared = brew::current_brewfile();
    if declared.is_none() {
        warn("couldn't find the active nix-darwin Brewfile, so Homebrew packages aren't checked");
    }
    let skip: Vec<String> = cfg
        .watch
        .iter()
        .filter(|w| !w.audit)
        .map(|w| format!("dir:{}", w.path))
        .collect();
    let (known_apps, renamed) = AdoptedFile::load(cfg)
        .map(|a| (a.known_app_names(), a.renames.keys().cloned().collect::<Vec<_>>()))
        .unwrap_or_default();
    let all = audit_findings(&now, declared.as_ref(), &skip, &known_apps, &renamed);
    let total = all.len();
    let shown: Vec<Change> = all
        .into_iter()
        .filter(|c| show_all || !ignores.matches(c))
        .collect();
    let hidden = total - shown.len();
    Ok((shown, hidden, declared))
}

pub fn audit(cfg: &Config, show_all: bool) -> Result<(), String> {
    let (shown, hidden, _) = audit_list(cfg, show_all)?;
    if shown.is_empty() {
        println!(
            "\n{}",
            green("Everything found is captured by the config or ignored.")
        );
    } else {
        print_audit(&shown, &|name, id| brew::casks_for_app(name, id));
    }

    let missing = missing_listed_apps(cfg);
    if !missing.is_empty() {
        heading("To install by hand");
        for (name, id) in &missing {
            match id {
                Some(id) => println!(
                    "  {} {name} {}",
                    yellow("!"),
                    dim(&format!("macappstore://apps.apple.com/app/id{id}"))
                ),
                None => println!("  {} {name}", yellow("!")),
            }
        }
        println!(
            "  {}",
            dim("Listed in dotfinder/adopted.json as apps nothing installs automatically.")
        );
    }

    let pending = crate::vault::pending(cfg);
    if !pending.is_empty() {
        heading("Vault");
        for p in &pending {
            println!("  {} {p}", yellow("!"));
        }
        println!("  {}", dim("`dotfinder vault backup` saves them."));
    }

    heading("Baseline");
    match latest_baseline()? {
        None => println!("  {} none recorded yet (`dotfinder baseline`)", yellow("!")),
        Some(b) => {
            let age = util::age(util::now_unix().saturating_sub(b.taken_at));
            if b.generation.is_some() && b.generation != current_generation() {
                println!("  {} recorded {age}, before the last rebuild", yellow("!"));
            } else {
                println!("  {} recorded {age}", green("✓"));
            }
        }
    }

    if hidden > 0 {
        println!(
            "\n{}",
            dim(&format!(
                "{hidden} findings hidden by ignore rules (`dotfinder audit --all` shows them)."
            ))
        );
    }
    if !shown.is_empty() {
        println!(
            "\nNext: {} to go through these one by one.",
            bold("dotfinder adopt --audit")
        );
    }
    Ok(())
}

/// Apps adopted.json lists that aren't on this Mac: (name, App Store ID if
/// it's an App Store app).
fn missing_listed_apps(cfg: &Config) -> Vec<(String, Option<i64>)> {
    let Ok(adopted) = AdoptedFile::load(cfg) else {
        return Vec::new();
    };
    let dirs = [
        util::sys("/Applications"),
        util::sys("/Applications/Utilities"),
        util::expand("~/Applications"),
    ];
    let present = |name: &str| dirs.iter().any(|d| d.join(format!("{name}.app")).exists());
    let manual = adopted.manual_apps.iter().map(|n| (n.clone(), None));
    let store = adopted.mas_apps.iter().map(|(n, id)| (n.clone(), Some(*id)));
    manual.chain(store).filter(|(n, _)| !present(n)).collect()
}

/// Before a switch: Homebrew packages installed but not declared, which the
/// switch's cleanup step would uninstall. Returns false to abort.
fn preflight(cfg: &Config) -> Result<bool, String> {
    eprintln!("Checking what the rebuild would uninstall…");
    let declared = match brew::declared_brewfile(cfg) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "{}\n{e}",
                red("Couldn't evaluate the config, so the rebuild would fail too.")
            );
            return Ok(false);
        }
    };
    let leaves = util::run_simple(&["brew", "leaves", "--installed-on-request"]);
    let casks = util::run_simple(&["brew", "list", "--cask"]);
    let (Ok(leaves), Ok(casks)) = (leaves, casks) else {
        warn("couldn't ask Homebrew what's installed; skipping the check");
        return Ok(true);
    };
    let undeclared_brews: Vec<String> = leaves
        .stdout
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty() && !declared.declares_brew(n))
        .map(str::to_string)
        .collect();
    let undeclared_casks: Vec<String> = casks
        .stdout
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty() && !declared.declares_cask(n))
        .map(str::to_string)
        .collect();
    if !in_the_way(&declared, &casks.stdout)? {
        return Ok(false);
    }
    // Once the config lists any App Store app, Homebrew's cleanup uninstalls
    // every App Store app it doesn't list.
    let undeclared_store: Vec<(i64, String)> = if declared.mas.is_empty() {
        Vec::new()
    } else {
        brew::mas_list()
            .unwrap_or_default()
            .into_iter()
            .filter(|(id, _)| !declared.mas.contains(id))
            .collect()
    };
    if !undeclared_store.is_empty() {
        println!(
            "\n{}",
            bold("The config lists App Store apps (homebrew.masApps), so the rebuild will uninstall these:")
        );
        for (id, name) in &undeclared_store {
            println!("  {} {name} {}", red("-"), dim(&format!("(App Store {id})")));
        }
        println!(
            "  {}",
            dim("Add them to homebrew.masApps, or remove homebrew.masApps, to keep them.")
        );
        let answer = choose(
            "\nWhat now?",
            &[('c', "continue and uninstall them"), ('q', "stop")],
        );
        if answer != Some('c') {
            return Ok(false);
        }
    }
    if undeclared_brews.is_empty() && undeclared_casks.is_empty() {
        return Ok(true);
    }
    println!(
        "\n{}",
        bold("Installed with Homebrew but not in the config. The rebuild will uninstall:")
    );
    for c in &undeclared_casks {
        println!("  {} {c} {}", red("-"), dim("(cask)"));
    }
    for b in &undeclared_brews {
        println!("  {} {b} {}", red("-"), dim("(formula)"));
    }
    let answer = choose(
        "\nWhat now?",
        &[
            ('a', "add them all to the config"),
            ('c', "continue and uninstall them"),
            ('q', "stop"),
        ],
    );
    match answer {
        Some('a') => {
            let mut adopted = AdoptedFile::load(cfg)?;
            for c in undeclared_casks {
                let tap = brew::tap_of(true, &c);
                adopted.casks.push(qualify(tap.as_deref(), &c));
            }
            for b in undeclared_brews {
                let tap = brew::tap_of(false, &b);
                adopted.brews.push(qualify(tap.as_deref(), &b));
            }
            adopted.save().map_err(|e| e.to_string())?;
            println!("{} {}", green("Added to"), tilde(&cfg.adopted_path()));
            Ok(true)
        }
        Some('c') => {
            // Homebrew uninstalls an app by the name its cask gave it.
            let renames = AdoptedFile::load(cfg)?.renames;
            crate::rename::restore(&renames, Some(&undeclared_casks));
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Declared casks that aren't installed yet but whose app already is, from
/// the App Store or by hand. Homebrew won't install over those, so the
/// switch would stop at them. Returns false to abort.
fn in_the_way(declared: &Brewfile, installed_casks: &str) -> Result<bool, String> {
    let installed: Vec<&str> = installed_casks.lines().map(str::trim).collect();
    let blocked: Vec<(String, String)> = declared
        .casks
        .iter()
        .filter(|c| !installed.contains(&brew::short(c)))
        .flat_map(|c| {
            brew::apps_of_cask(c)
                .into_iter()
                .filter(|app| util::sys("/Applications").join(app).exists())
                .map(move |app| (brew::short(c).to_string(), app))
        })
        .collect();
    if blocked.is_empty() {
        return Ok(true);
    }
    println!(
        "\n{}",
        bold("These apps are already installed outside Homebrew, so their casks can't install:")
    );
    for (cask, app) in &blocked {
        println!(
            "  {} /Applications/{app} {}",
            yellow("!"),
            dim(&format!("(cask {cask})"))
        );
    }
    println!(
        "  {}",
        dim("Quit each one and move it to the Trash, then rebuild. Their data in ~/Library stays.")
    );
    let answer = choose(
        "\nWhat now?",
        &[
            ('c', "continue anyway (Homebrew reports those casks as failed)"),
            ('q', "stop"),
        ],
    );
    Ok(answer == Some('c'))
}

fn qualify(tap: Option<&str>, name: &str) -> String {
    match tap {
        Some("homebrew/core" | "homebrew/cask") | None => name.to_string(),
        Some(t) => format!("{t}/{name}"),
    }
}

pub fn rebuild(cfg: &Config, extra: &[String]) -> Result<(), String> {
    if !preflight(cfg)? {
        return Err("stopped before rebuilding".into());
    }
    let flake = cfg.dotfiles.display().to_string();
    println!("\n{} {}", bold("Switching to"), tilde(&cfg.dotfiles));
    let status = std::process::Command::new("sudo")
        .args(["darwin-rebuild", "switch", "--flake", &flake])
        .args(extra)
        .status()
        .map_err(|e| format!("couldn't run darwin-rebuild: {e}"))?;
    if !status.success() {
        return Err("darwin-rebuild failed; the baseline wasn't updated".into());
    }
    // The dotfiles' Nix module renames apps during the switch too; this
    // covers configs without it.
    crate::rename::apply(&AdoptedFile::load(cfg)?.renames);
    println!();
    baseline(cfg)?;
    remind_to_push(cfg);
    Ok(())
}

/// After a rebuild: a nudge when the dotfiles repo has uncommitted changes.
fn remind_to_push(cfg: &Config) {
    let dir = cfg.dotfiles.display().to_string();
    if let Ok(o) = util::run_simple(&["git", "-C", &dir, "status", "--porcelain"])
        && o.ok()
        && !o.stdout.trim().is_empty()
    {
        println!(
            "\n{} has uncommitted changes; {} saves them.",
            tilde(&cfg.dotfiles),
            bold("dotfinder push")
        );
    }
}

/// `brew upgrade`, with renamed apps under their Homebrew names while it runs.
pub fn upgrade(cfg: &Config, extra: &[String]) -> Result<(), String> {
    let renames = AdoptedFile::load(cfg)?.renames;
    if !renames.is_empty() {
        println!(
            "{}",
            bold("Giving renamed apps their Homebrew names for the upgrade:")
        );
    }
    crate::rename::restore(&renames, None);
    let status = std::process::Command::new("brew")
        .arg("upgrade")
        .args(extra)
        .status()
        .map_err(|e| format!("couldn't run brew: {e}"));
    if !renames.is_empty() {
        println!("{}", bold("Renaming them back:"));
    }
    crate::rename::apply(&renames);
    if !status?.success() {
        return Err("brew upgrade failed; the baseline wasn't updated".into());
    }
    println!();
    baseline(cfg)
}

/// `dotfinder push`: commits everything changed in the dotfiles repo, with a
/// message you type, and pushes it. Also pushes commits made earlier.
pub fn push(cfg: &Config) -> Result<(), String> {
    let dir = cfg.dotfiles.display().to_string();
    let git = |args: &[&str]| -> Result<util::Output, String> {
        let mut argv = vec!["git", "-C", &dir];
        argv.extend_from_slice(args);
        util::run_simple(&argv).map_err(|e| format!("git: {e}"))
    };
    // Runs git with the terminal attached, so hooks and credential prompts
    // show their output.
    let git_live = |args: &[&str]| -> Result<bool, String> {
        std::process::Command::new("git")
            .args(["-C", &dir])
            .args(args)
            .status()
            .map(|s| s.success())
            .map_err(|e| format!("git: {e}"))
    };

    let status = git(&["status", "--porcelain"])?;
    if !status.ok() {
        return Err(format!(
            "{} isn't a git repo: {}",
            tilde(&cfg.dotfiles),
            status.stderr.trim()
        ));
    }
    let changes: Vec<&str> = status.stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    let unpushed = |git: &dyn Fn(&[&str]) -> Result<util::Output, String>| -> usize {
        git(&["rev-list", "--count", "@{upstream}..HEAD"])
            .ok()
            .filter(|o| o.ok())
            .and_then(|o| o.stdout.trim().parse().ok())
            .unwrap_or(0)
    };

    if changes.is_empty() && unpushed(&git) == 0 {
        println!("{}", green("Nothing to commit or push."));
        return Ok(());
    }

    if !changes.is_empty() {
        heading(&format!("Changes in {}", tilde(&cfg.dotfiles)));
        for line in &changes {
            let (code, path) = line.split_at(line.len().min(3));
            let mark = match code.trim() {
                "??" | "A" => green("+"),
                "D" => red("-"),
                _ => yellow("~"),
            };
            println!("  {mark} {path}");
        }
        let Some(message) = ask_line("\nCommit message (empty to cancel): ") else {
            return Ok(());
        };
        if message.is_empty() {
            println!("Nothing committed.");
            return Ok(());
        }
        if !git_live(&["add", "-A"])? {
            return Err("git add failed".into());
        }
        if !git_live(&["commit", "-q", "-m", &message])? {
            return Err(
                "the commit failed; nothing was pushed (the pre-commit hook refuses key material)".into(),
            );
        }
    }

    let n = unpushed(&git);
    println!(
        "\n{} {n} commit{}",
        bold("Pushing"),
        if n == 1 { "" } else { "s" }
    );
    if !git_live(&["push", "-q"])? {
        return Err("git push failed; the commit is saved locally, so `dotfinder push` again retries".into());
    }
    println!("{}", green("Pushed."));
    Ok(())
}
