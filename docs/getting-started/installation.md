# Installation

> Pick one path. All four end with `dotagent --version` working and
> `dotagent doctor` reporting "no agents discovered" (which is the
> healthy state before you write your first agent).

dotagent ships as a single Rust binary plus four first-party plugin
binaries. There's nothing to compile at runtime, nothing to install in
a language ecosystem (no `node_modules`, no `pip`, no `pyenv`). Pick:

| Path                  | When to use                                                                                   | Platforms             |
|-----------------------|-----------------------------------------------------------------------------------------------|-----------------------|
| [Homebrew](#1-homebrew-recommended) | macOS / Linux with Homebrew. The repo doubles as its own tap.                                 | macOS, Linux (homebrew on linux) |
| [GitHub Release binaries](#2-github-release-binaries) | You want a prebuilt binary today and don't have Rust toolchain.                                 | macOS arm64/x86_64, Linux arm64/x86_64 |
| [`cargo install`](#3-cargo-install)            | You have Rust stable and want the latest from `main`.                                          | macOS, Linux          |
| [Build from source](#4-build-from-source)             | You want to hack on dotagent itself.                                                            | macOS, Linux          |

> **Windows is not supported.** dotagent uses `kill(2)`, `osascript`,
> launchd, systemd — all Unix-shaped. Linux + WSL works; native Windows
> doesn't.

After whichever path you pick, jump to [Verify the install](#verify-the-install).

---

## 1. Homebrew (recommended)

The repo doubles as its own Homebrew tap — the formula lives at
[`Formula/dotagent.rb`](https://github.com/avelino/dotagent/blob/main/Formula/dotagent.rb)
in the default branch, and the release workflow rewrites it with
fresh `sha256` values on every tagged release. No separate
`homebrew-dotagent` repo to chase.

```bash
brew tap avelino/dotagent https://github.com/avelino/dotagent
brew install dotagent
brew services start dotagent     # runs `dotagent daemon` via launchd / systemd
```

> The URL is required because the tap repo isn't named
> `homebrew-dotagent`. Run `brew tap` once — afterwards
> `brew install dotagent` and `brew upgrade dotagent` Just Work.

`brew install dotagent` drops `dotagent` plus every first-party
plugin (`dotagent-plugin-preflight-warp`, `dotagent-plugin-sink-roam`,
etc.) into the same `bin/`, so plugin discovery via `$PATH` works
with zero config.

`brew services start` registers the daemon via `launchctl bootstrap`
(macOS) / `systemctl --user enable --now` (Linux). Skip this step if
you'd rather manage the daemon yourself — see
[`guides/daemon-lifecycle.md`](../guides/daemon-lifecycle.md).

### 1a. Beta channel (`dotagent@beta`)

Every push to `main` republishes a rolling `beta` GitHub release and
rewrites [`Formula/dotagent@beta.rb`](https://github.com/avelino/dotagent/blob/main/Formula/dotagent@beta.rb)
with fresh `sha256` values. The formula version is
`0.0.1-beta.<commit-count>`, monotonic so `brew upgrade dotagent@beta`
detects every new build.

```bash
brew tap avelino/dotagent https://github.com/avelino/dotagent
brew install dotagent@beta
brew link --overwrite dotagent@beta       # `dotagent@beta` is keg_only
brew services start dotagent@beta
```

`dotagent@beta` is `keg_only` (Homebrew convention for versioned
formulae), so `brew link --overwrite` is required to put `dotagent`
on `$PATH`. To switch back to stable:

```bash
brew unlink dotagent@beta
brew link dotagent
```

> **Beta caveats**: builds may break — the `beta` channel tracks the
> tip of `main`, including in-flight refactors. For production use,
> stick with stable.

The `beta` tag on GitHub Releases is **rolling**: it's deleted and
recreated on every push, so the URL `releases/download/beta/...`
always serves the latest build. Use stable (`v0.0.1`, `v0.0.2`, …)
when you need a pinned version.

---

## 2. GitHub Release binaries

Each tagged release publishes signed archives for **macOS arm64**,
**macOS x86_64**, **Linux arm64**, and **Linux x86_64**.

```bash
# Pick your platform (run `uname -ms` if unsure):
#   Darwin arm64    → aarch64-darwin
#   Darwin x86_64   → x86_64-darwin
#   Linux aarch64   → aarch64-linux
#   Linux x86_64    → x86_64-linux

VERSION="0.0.1"               # check https://github.com/avelino/dotagent/releases
ARCH="aarch64-darwin"         # adjust
NAME="dotagent-${VERSION}-${ARCH}"

curl -L -o "${NAME}.tar.gz"        "https://github.com/avelino/dotagent/releases/download/v${VERSION}/${NAME}.tar.gz"
curl -L -o "${NAME}.tar.gz.sha256" "https://github.com/avelino/dotagent/releases/download/v${VERSION}/${NAME}.tar.gz.sha256"

# Verify (the sha256 file ships next to the archive).
shasum -a 256 -c "${NAME}.tar.gz.sha256"

# Extract.
tar -xzf "${NAME}.tar.gz"
# → bin/dotagent  bin/dotagent-plugin-*  LICENSE  README.md
```

Install the binaries somewhere on `$PATH`:

```bash
# Option A: user-local (no sudo)
mkdir -p ~/.local/bin
cp bin/dotagent bin/dotagent-plugin-* ~/.local/bin/
export PATH="$HOME/.local/bin:$PATH"     # add to ~/.zshrc / ~/.bashrc

# Option B: system-wide (sudo)
sudo cp bin/dotagent bin/dotagent-plugin-* /usr/local/bin/
```

> **Plugins must live next to `dotagent`** or somewhere on `$PATH` —
> the CLI resolves them by name (`dotagent-plugin-sink-roam` etc.) via
> standard `$PATH` lookup. See [`reference/paths.md`](../reference/paths.md)
> for the full discovery order.

---

## 3. `cargo install`

If you already have a Rust toolchain (stable, 1.75+ recommended):

```bash
git clone https://github.com/avelino/dotagent
cd dotagent

# Install the CLI.
cargo install --path crates/dotagent

# Install each plugin you want.
cargo install --path plugins/preflight-warp
cargo install --path plugins/preflight-cmd
cargo install --path plugins/sink-roam
cargo install --path plugins/sink-file
```

The binaries land in `~/.cargo/bin/`, which most installers add to
`$PATH` automatically. Verify:

```bash
which dotagent                            # → ~/.cargo/bin/dotagent
which dotagent-plugin-preflight-warp      # → ~/.cargo/bin/...
```

Don't forget the plugins — `cargo install --path crates/dotagent`
alone does NOT pull them in. `dotagent doctor` will tell you which
ones are referenced by your manifests but missing on `$PATH`.

---

## 4. Build from source

For contributors and anyone wanting to vendor dotagent into a custom
distribution.

```bash
git clone https://github.com/avelino/dotagent
cd dotagent

# Build the whole workspace (CLI + every plugin).
cargo build --release --workspace

# Binaries end up in target/release/. Drop them on $PATH manually:
cp target/release/dotagent             ~/.local/bin/
cp target/release/dotagent-plugin-*    ~/.local/bin/
```

Self-verification (matches the CI matrix — never report "done"
without these three passing):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

A faster sanity check that exercises the discovery + run path:

```bash
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent doctor
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent run hello-fish --schedule manual --dry-run
```

If both succeed, the workspace is healthy.

---

## Verify the install

Whichever path you picked, run:

```bash
dotagent --version
# → dotagent 0.0.1 (or whichever)

dotagent --help
# → polyglot agent orchestrator. Usage: dotagent <COMMAND>

dotagent doctor
# → no agents discovered     ← expected, you haven't written one yet
```

If all three work, you're done. Skip ahead to
[`first-agent.md`](first-agent.md).

If `dotagent --help` works but `dotagent doctor` fails, see the
[CLI reference](../reference/cli.md#doctor) and the
[troubleshooting guide](../guides/troubleshooting.md).

---

## What got installed where

After install you should find:

| What                           | Path                                                                      |
|--------------------------------|---------------------------------------------------------------------------|
| `dotagent` binary              | depends on path — `~/.cargo/bin/`, `~/.local/bin/`, `/opt/homebrew/bin/`… |
| Plugin binaries                | same directory as `dotagent` (or anywhere on `$PATH`)                     |
| Default home                   | `~/.config/dotagent/` (created lazily on first run)                       |
| Default agents directory        | `~/.config/dotagent/agents/`                                              |
| launchd plist (after `install`)| `~/Library/LaunchAgents/run.avelino.dotagent.plist` (macOS)               |
| systemd unit (after `install`) | `~/.config/systemd/user/run.avelino.dotagent.service` (Linux)             |

The full disk layout is documented in [`reference/paths.md`](../reference/paths.md).

---

## Upgrading

| Path              | How to upgrade                                                                              |
|-------------------|---------------------------------------------------------------------------------------------|
| Homebrew (stable) | `brew update && brew upgrade dotagent`                                                       |
| Homebrew (`@beta`)| `brew update && brew upgrade dotagent@beta` (new build on every push to `main`)              |
| GitHub Release    | Download the new archive, overwrite binaries in `~/.local/bin/` (or `/usr/local/bin/`)       |
| `cargo install`   | `cargo install --path crates/dotagent --force` (and same for each plugin)                    |
| Source build      | `git pull && cargo build --release --workspace` then re-copy                                 |

After upgrading:

```bash
dotagent reload     # daemon picks up new manifests + plugin set on next tick
```

`reload` sends SIGHUP — it doesn't restart the binary. If you swapped
the `dotagent` binary itself, restart the daemon instead:

```bash
# macOS (launchd)
launchctl kickstart -k "gui/$(id -u)/run.avelino.dotagent"

# Linux (systemd)
systemctl --user restart run.avelino.dotagent
```

---

## Uninstall

```bash
# 1. Stop & remove the daemon unit.
dotagent uninstall
# → removes ~/Library/LaunchAgents/run.avelino.dotagent.plist (or systemd unit)

# 2. Remove the binaries (path varies).
brew uninstall dotagent                    # if homebrew
rm ~/.cargo/bin/dotagent ~/.cargo/bin/dotagent-plugin-*   # if cargo
rm ~/.local/bin/dotagent ~/.local/bin/dotagent-plugin-*   # if release / source
```

dotagent **does not delete** `~/.config/dotagent/` on uninstall —
your manifests, heartbeats, audit log, and config stay put.

```bash
# Nuke everything (irreversible).
rm -rf ~/.config/dotagent
```

---

## Next

- [Write your first agent →](first-agent.md)
- Already have an agent? [CLI reference](../reference/cli.md) and
  [daemon lifecycle](../guides/daemon-lifecycle.md).
