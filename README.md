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
| `dotfinder drift` | What changed since the baseline |
| `dotfinder audit` | Everything the config doesn't capture, changed recently or not |
| `dotfinder adopt` | Walks through drift one change at a time: add it to the config, or ignore it from then on, publicly or privately |
| `dotfinder adopt --audit` | The same, for audit findings |
| `dotfinder baseline` | Records the current state as the baseline |
| `dotfinder vault status / backup / restore` | Keeps the configured private files in the vault |
| `dotfinder snapshot` | Prints the current state as JSON |

`drift` and `audit` take `--all` to show what ignore rules hide.

## The dotfiles side

`adopt` never edits hand-written Nix. It writes two JSON files into the
dotfiles repo:

- `dotfinder/adopted.json`: casks, formulae, App Store apps (`masApps`, by
  store ID), apps to install by hand (`manualApps`, names only) and
  preferences. A small Nix module reads it into `homebrew.casks`,
  `homebrew.brews`, `homebrew.masApps` and
  `system.defaults.CustomUserPreferences`.
- `dotfinder/ignored.json`: ignore rules.

Both must be tracked by git, or the flake can't see them.

Ignoring something privately (`p` in `adopt`) writes the rule to
`~/.local/share/dotfinder/ignored.json` instead, outside the repo, so the
name of what it hides isn't published. Back that file up with the vault.

## Apps

For an app installed from the App Store or by hand, `adopt` offers:

- the Homebrew cask that installs it, when one exists. Quit the app and
  move it to the Trash before the next rebuild, so Homebrew can install its
  own copy; `dotfinder rebuild` stops to say so if one is still in the way.
- adding it from the App Store, by its store ID. Rebuilds install it with
  `mas` while you're signed in to the App Store.
- listing it as an app to install by hand. `audit` then names it on any Mac
  where it's missing.

A minimal module:

```nix
{ lib, ... }:
let
  file = ./dotfinder/adopted.json;
  adopted = if builtins.pathExists file then builtins.fromJSON (builtins.readFile file) else { };
in
{
  homebrew.casks = adopted.casks or [ ];
  homebrew.brews = adopted.brews or [ ] ++ lib.optional ((adopted.masApps or { }) != { }) "mas";
  homebrew.masApps = adopted.masApps or { };
  system.defaults.CustomUserPreferences = adopted.defaults or { };
}
```

`adopt` refuses preferences that hold dates or binary data (nix-darwin can't
write them) and anything that looks like a secret.

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

## Building

dotfinder has no dependencies beyond the Rust standard library: it reads
private machine state, so it carries no third-party code.

```
nix build
```

or `cargo build --release` in `nix develop`.

## License

MIT
