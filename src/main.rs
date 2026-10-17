//! dotfinder: finds what on a Mac isn't captured by its declarative config,
//! helps fold it in, and keeps private files in a KeePassXC vault.

mod adopt;
mod brew;
mod commands;
mod config;
mod diff;
mod folders;
mod homeapps;
mod json;
mod plist;
mod rename;
mod report;
mod snapshot;
mod state;
mod term;
mod util;
mod vault;

use config::Config;

const HELP: &str = "\
dotfinder: finds what on this Mac isn't captured by its config.

Usage: dotfinder <command>

Commands:
  rebuild [args]   Check for Homebrew packages a switch would uninstall,
                   run `darwin-rebuild switch`, then record a new baseline.
                   Extra args go to darwin-rebuild.
  upgrade [args]   Run `brew upgrade`, giving renamed apps their Homebrew
                   names for the duration, then record a new baseline.
                   Extra args go to brew upgrade.
  drift [--all]    What changed since the last baseline.
  audit [--all]    Everything on the Mac the config doesn't capture, changed
                   recently or not.
  adopt [--audit]  Go through drift (or audit findings) one at a time: add
                   each to the config, keep a rename, or ignore it from then
                   on, publicly or privately.
  push             Commit what changed in the dotfiles repo, with a message
                   you type, and push it.
  baseline         Record the current state as the baseline.
  apps status      Which of your own things in ~/Applications are backed up.
  apps backup      Copy them to the backup folder in iCloud Drive.
  apps restore     Copy backed-up ones that are missing back.
  vault status     Which private files are backed up in the vault.
  vault backup     Copy the configured private files into the vault.
  vault restore    Write them back from the vault.
  snapshot         Print the current state as JSON.

--all also shows what ignore rules hide.
Config: ~/.config/dotfinder/config.json (or $DOTFINDER_CONFIG).
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    if matches!(cmd, "help" | "-h" | "--help") {
        print!("{HELP}");
        return;
    }
    if matches!(cmd, "-V" | "--version" | "version") {
        println!("dotfinder {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => fail(&format!("couldn't read the config: {e}")),
    };

    let result = match cmd {
        "rebuild" => commands::rebuild(&cfg, &args[1..]),
        "upgrade" => commands::upgrade(&cfg, &args[1..]),
        "drift" => commands::drift(&cfg, flag("--all")),
        "audit" => commands::audit(&cfg, flag("--all")),
        "baseline" => commands::baseline(&cfg),
        "push" => commands::push(&cfg),
        "snapshot" => commands::snapshot(&cfg),
        "adopt" => {
            if flag("--audit") {
                commands::audit_list(&cfg, false)
                    .and_then(|(findings, _, declared)| adopt::adopt(&cfg, findings, declared.as_ref()))
            } else {
                commands::compute_drift(&cfg, false).and_then(|r| match r {
                    Some((d, _, _)) => {
                        let declared = brew::current_brewfile();
                        adopt::adopt(&cfg, d.shown, declared.as_ref())
                    }
                    None => Ok(()),
                })
            }
        }
        "apps" => match args.get(1).map(String::as_str) {
            Some("status") | None => homeapps::status(&cfg),
            Some("backup") => homeapps::backup(&cfg),
            Some("restore") => homeapps::restore(&cfg),
            Some(other) => Err(format!(
                "unknown apps command `{other}` (status, backup, restore)"
            )),
        },
        "vault" => match args.get(1).map(String::as_str) {
            Some("status") | None => vault::status(&cfg),
            Some("backup") => vault::backup(&cfg),
            Some("restore") => vault::restore(&cfg),
            Some(other) => Err(format!(
                "unknown vault command `{other}` (status, backup, restore)"
            )),
        },
        other => Err(format!("unknown command `{other}`; `dotfinder help` lists them")),
    };
    if let Err(e) = result {
        fail(&e);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("{} {msg}", term::red("error:"));
    std::process::exit(1);
}
