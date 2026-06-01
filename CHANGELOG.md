# Changelog

All notable changes to dotagent are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/) and the project
adheres to [Semantic Versioning](https://semver.org/).

Pre-1.0: minor bumps may include breaking changes; both `agent.toml`
schema and the plugin protocol are flagged in each entry.

## [0.1.4] - 2026-06-01

### Added

- **`sink-outl` plugin** — twin of `sink-roam` targeting
  [Outl](https://github.com/avelino/outl). Same config shape
  (`page`, `marker_regex`, `mcp_binary`) so an existing
  `[[on_success]]` block migrates by changing the plugin name. The
  whole publish is a single `outl_batch` call (N `block_delete` ops
  matched by `marker_regex` + one `block_append_tree`), removing the
  round-trips `sink-roam` makes against the Roam API.
- Daily slug resolution accepts the Roam-native ordinal form
  (`April 22nd, 2026`) and normalizes it to ISO (`2026-04-22`) before
  talking to Outl — same `agent.toml` works against both backends.
- Docs: [`docs/plugins/sink-outl.md`](docs/plugins/sink-outl.md), entry
  added to the plugin index, SUMMARY, `concepts/plugins.md`,
  `getting-started/installation.md`, `getting-started/next-steps.md`
  and `llms.txt`.

### Changed

- Homebrew formula comment lists the new `dotagent-plugin-sink-outl`
  binary; `bin.install Dir["bin/*"]` already shipped it automatically.

## [0.1.3] - 2026-05-22

### Added

- **`dotagent-supervisor` crate** — single subprocess lifecycle manager
  covering deadlines, POSIX process-group kill-tree, and a live
  registry. Every orchestrated subprocess (agent, plugin, hook) now
  passes through the supervisor; ad-hoc helpers inside notifier
  drivers (e.g. `osascript`) remain unchanged.

### Fixed

- `shutdown_signals_every_live_entry` flaky test on macOS CI.

## [0.1.2] - 2026-05-21

### Added

- **`dotagent-secrets` crate** — `secrets.env` loader with `op://`
  reference support, fed into the agent environment alongside the
  manifest's `[env]` block.
- Telegram notifier driver.
- Shell completion with dynamic agent-name autocomplete.

### Changed

- CLI run-now output pretty-prints the outcome instead of dumping
  `Debug` and tightens the renderer test suite.

[0.1.4]: https://github.com/avelino/dotagent/releases/tag/v0.1.4
[0.1.3]: https://github.com/avelino/dotagent/releases/tag/v0.1.3
[0.1.2]: https://github.com/avelino/dotagent/releases/tag/v0.1.2
