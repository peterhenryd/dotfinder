# dotfinder

Finds what on a Mac isn't captured by its declarative config
([nix-darwin](https://github.com/nix-darwin/nix-darwin) and
[home-manager](https://github.com/nix-community/home-manager)), helps fold it
in, and keeps private files that can't go in a public repo in a
[KeePassXC](https://keepassxc.org) vault.

It exists because configuring a Mac by hand loses that work on the next
reset. dotfinder notices those changes so they can be kept.

## How it works

A **snapshot** records the machine's state:

- every preferences domain (`defaults export`), including per-Mac
  (`-currentHost`) ones and, optionally, sandboxed apps' containers
- Homebrew formulae and casks
- applications and where each came from (Homebrew, App Store, Nix, Apple,
  or installed by hand)
- the entries of watched folders (`~/.config`, `~/Library/LaunchAgents`,
  fonts and so on), and whether each is managed by the config
- the output of collector commands, such as `cargo install --list`, global
  npm packages, `uv tool list`, `nix profile list` and login items

A **baseline** is a snapshot taken right after a rebuild, when the machine
matches the config. Anything that differs later is drift.

## Commands

| Command | What it does |
| --- | --- |
| `dotfinder rebuild` | Warns about Homebrew packages the switch would uninstall, runs `darwin-rebuild switch`, then records a new baseline |
| `dotfinder upgrade` | Runs `brew upgrade` with renamed apps under their Homebrew names for the duration, then `uv tool upgrade --all`, then records a new baseline |
| `dotfinder drift` | What changed since the baseline |
| `dotfinder audit` | Everything the config doesn't capture, changed recently or not |
| `dotfinder adopt` | Walks through drift one change at a time: add it to the config, or ignore it from then on, publicly or privately |
| `dotfinder adopt --audit` | The same, for audit findings |
| `dotfinder push` | Shows what changed in the dotfiles repo, asks for a commit message, commits everything and pushes |
| `dotfinder baseline` | Records the current state as the baseline |
| `dotfinder apps status / backup / restore` | Keeps your own things in ~/Applications backed up in iCloud Drive |
| `dotfinder vault status / backup / restore` | Keeps the configured private files in the vault |
| `dotfinder snapshot` | Prints the current state as JSON |

`drift` and `audit` take `--all` to show what ignore rules hide.

## The dotfiles side

`adopt` never edits hand-written Nix. It writes two JSON files into the
dotfiles repo:

- `dotfinder/adopted.json`: casks, formulae, Python tools installed with uv
  (`uvTools`), renamed cask apps (`renames`), preferences, and apps the
  config doesn't install: App Store apps (`masApps`, by store ID) and apps
  to install by hand (`manualApps`). A small Nix module reads it into
  `homebrew.casks`, `homebrew.brews` and
  `system.defaults.CustomUserPreferences`, and applies the renames.
- `dotfinder/ignored.json`: ignore rules.

Both must be tracked by git, or the flake can't see them.

A minimal module:

```nix
{ lib, ... }:
let
  file = ./dotfinder/adopted.json;
  adopted = if builtins.pathExists file then builtins.fromJSON (builtins.readFile file) else { };
in
{
  homebrew.casks = adopted.casks or [ ];
  homebrew.brews = adopted.brews or [ ];
  system.defaults.CustomUserPreferences = adopted.defaults or { };
  system.activationScripts.postActivation.text = lib.concatStrings (
    lib.mapAttrsToList (cask: r: ''
      if [ -e "/Applications/${r.from}" ] && [ ! -e "/Applications/${r.to}" ]; then
        mv "/Applications/${r.from}" "/Applications/${r.to}"
      fi
    '') (adopted.renames or { })
  );
}
```

For `uvTools`, the module also needs a step that installs missing tools;
see [Python tools](#python-tools).

`adopt` refuses preferences that hold dates or binary data (nix-darwin can't
write them) and anything that looks like a secret.

For a folder, `l` in `adopt` lists what's in it. Most folders in ~ hold a
tool's own data and are best ignored; for a few tools' folders (Cargo,
Gemini CLI, GnuPG), `adopt` names the settings files in them worth moving
into the repo and the credentials to keep out of it.

Ignoring something privately (`p` in `adopt`) writes the rule to
`~/.local/share/dotfinder/ignored.json` instead, outside the repo, so the
name of what it hides isn't published. Back that file up with the vault.

## Python tools

Some Python command-line tools move faster than nixpkgs, or aren't in it
(mlx-lm, for one). For a tool installed with `uv tool install`, `adopt`
offers to add it to `uvTools`. Entries take what `uv tool install` takes,
extras and versions included (`"mlx-lm[train]>=0.20"`), and `audit` stops
naming listed tools and the commands uv links into `~/.local/bin`. A
home-manager activation step installs the missing ones on each switch:

```nix
home-manager.users.${config.system.primaryUser} =
  { lib, pkgs, ... }:
  {
    home.activation.uvTools = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
      uvInstalled=" $(${pkgs.uv}/bin/uv tool list 2>/dev/null | while read -r name _; do
        case "$name" in -* | "") ;; *) printf '%s ' "$name" ;; esac
      done)"
      for spec in ${lib.escapeShellArgs (adopted.uvTools or [ ])}; do
        name="''${spec%%[][=<>~!@; ]*}"
        case "$uvInstalled" in
          *" $name "*) ;;
          *) run ${pkgs.uv}/bin/uv tool install "$spec" || echo "uv couldn't install $spec" >&2 ;;
        esac
      done
    '';
  };
```

A tool taken off the list stays installed until `uv tool uninstall`.

## Apps

For an app installed from the App Store or by hand, `adopt` offers:

- the Homebrew cask that installs it, when one exists. Quit the app and
  move it to the Trash before the next rebuild, so Homebrew can install its
  own copy; `dotfinder rebuild` stops to say so if one is still in the way.
- listing it as an App Store app, by its store ID. On a Mac where it's
  missing, `audit` names it with a link to its App Store page.
- listing it as an app to install by hand. `audit` then names it on any Mac
  where it's missing.

App Store apps stay out of Homebrew on purpose: once a Brewfile lists any
App Store app (`homebrew.masApps`), Homebrew's cleanup uninstalls every
App Store app it doesn't list. If your config uses `homebrew.masApps`
anyway, `dotfinder rebuild` lists the App Store apps the switch would
remove and asks first.

Apps are matched to casks by bundle name first and by bundle ID second, so
a renamed app is still recognized: casks name the app's bundle ID in their
uninstall and zap steps.

### ~/Applications

`/Applications` is treated as installed software, which Homebrew or the App
Store can reinstall. `~/Applications` is treated as yours: web apps, scripts
saved as apps, folders of aliases. Nothing can reinstall those, so instead
of adopting them, `dotfinder apps backup` copies them (with `ditto`, which
keeps app bundles intact) to `~/Library/Mobile Documents/com~apple~CloudDocs/Backups/Applications`
(`apps_backup` in the config changes it), and `dotfinder apps restore` copies
back any that are missing. `audit` lists the ones not backed up as they are
now.

Things other tools put there and recreate on their own are skipped: Home
Manager's apps folder, Parallels' folders of Windows app shortcuts, and
JetBrains Toolbox's IDEs. An ignore rule skips anything else, such as
`apps:~/Applications/Claude Code URL Handler.app`.

### Renaming apps

Homebrew keeps track of an app by the name its cask installs it under. A
cask app you rename shows up in `audit` until `adopt` records the rename
(`r`). With the rename recorded:

- the switch renames the app after Homebrew installs it, so a new Mac gets
  your name too (the module above does this; without it, `dotfinder rebuild`
  does);
- `dotfinder upgrade` gives it Homebrew's name while `brew upgrade` runs and
  renames it back afterwards;
- `dotfinder rebuild` gives it Homebrew's name back before the switch
  uninstalls its cask, so nothing is left behind.

`brew upgrade` skips apps that update themselves, so for those a rename
only comes into play on install and uninstall.

## Ignore rules

Every change has an id like `section:item:key`:

- `defaults:com.apple.dock:autohide`
- `homebrew-casks:spotify`
- `dir:~/.config:opencode`
- `cmd:cargo-install:ripgrep`

Rules are case-insensitive globs over those ids (`*` and `?`). A rule for
`section:item` also covers its keys, so `defaults:com.example.app` ignores
that whole domain. Built-in rules hide window positions, recent files,
timestamps and similar state that changes constantly; `"builtin_ignores":
false` turns them off.

## Configuration

`~/.config/dotfinder/config.json` (or `$DOTFINDER_CONFIG`). Every field is
optional.

```json
{
  "dotfiles": "~/.config/dotfiles",
  "host": "Peters-MacBook-Pro",
  "ignore": ["defaults:com.example.noisy"],
  "containers": false,
  "watch": [{ "path": "~", "hidden_only": true }, "~/.config", { "path": "~/Library/Application Support", "audit": false }],
  "collectors": [
    { "name": "cargo-install", "command": ["cargo", "install", "--list"], "skip_prefixes": [" "], "field": 0 },
    { "name": "npm-global", "command": ["npm", "ls", "-g", "--depth=0", "--json"], "json_keys": "dependencies" }
  ],
  "vault": {
    "database": "~/Documents/Recovery/Vault.kdbx",
    "group": "dotfinder",
    "files": [{ "path": "~/.ssh/config", "mode": "600" }, "~/.config/git/identity"]
  }
}
```

- `host` defaults to the LocalHostName, which is what `darwin-rebuild` uses.
- `containers` also reads sandboxed apps' preferences. macOS asks for
  permission to read other apps' data, so it's off by default.
- `watch` and `collectors` replace the defaults when given.
- Collector options shape how command output becomes a list of items:
  - `skip_prefixes`: drop lines starting with any of these
  - `prefix`: keep only lines starting with it, with the prefix removed
  - `field`: keep only this whitespace-separated field
  - `split`: split on this separator instead of on lines
  - `json_keys`: list the keys of the JSON object at this path

## The vault

Each file is stored as an attachment named `content` on an entry in the
configured group. The entry is titled with the file's path relative to the
home folder, with `/` written as `::`, for example `.ssh::config`.
`keepassxc-cli` must be installed; the KeePassXC Homebrew cask includes it.

`restore` keeps any existing copy that differs as `<name>.pre-restore`, and
applies the configured permissions.

Files whose paths shouldn't be public either go in a private list,
`~/.local/share/dotfinder/vault-files.json`, in the same format as
`vault.files`:

```json
{ "files": [{ "path": "~/.ssh/deploy", "mode": "600" }, { "path": "~/.local/bin/job", "mode": "755" }] }
```

Add that list itself to the config's `vault.files`. `restore` restores the
config's files first, so on a new Mac the list is back before it's read.

## Building

dotfinder has no dependencies beyond the Rust standard library: it reads
private machine state, so it carries no third-party code.

```
nix build
```

or `cargo build --release` in `nix develop`.

## License

MIT
