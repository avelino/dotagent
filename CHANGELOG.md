# Changelog

All notable changes to dotagent are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/) and the project
adheres to [Semantic Versioning](https://semver.org/).

Pre-1.0: minor bumps may include breaking changes; both `agent.toml`
schema and the plugin protocol are flagged in each entry.

## [Unreleased]

### Breaking (library consumers only)

The `agent.toml` schema and the heartbeat file shape are **unchanged** — no
manifest needs editing and the Fish-framework compatibility holds. These
break only code that depends on the crates as libraries:

- `RunSpec` gained `slug_override` and `extra_env`. Struct literals must add
  both fields.
- `AuditEvent` gained `manifest_invalid`, `trigger_received`,
  `trigger_rejected` and `agent_triggered`. The enum is not
  `#[non_exhaustive]`, so exhaustive matches need new arms.
- `discovery::discover_all` no longer returns `Err` for an unparseable
  manifest; it skips it. Use `discovery::discover` to see what failed.

### Added

- **Inbound triggers** ([#40](https://github.com/avelino/dotagent/issues/40)) —
  a run can now start because something asked, not only because a window
  came due. Triggered runs keep the supervisor, heartbeat, audit trail and
  hooks of a scheduled run, but carry no window state (no attempts counter,
  no retry consumed) and use their own slug (`trigger-<source>`) so they
  can never overwrite `last_success_at` for a cron window that never fired.
  Docs: [`concepts/triggers.md`](docs/concepts/triggers.md).
- **`dotagent mcp`** — serves every discovered agent as an MCP tool over
  JSON-RPC 2.0 on stdio. A model picks from `tools/list` instead of
  composing a command, so "only run what the operator declared" becomes a
  property of the protocol. Reachable from Claude Code, Claude Desktop or
  any MCP proxy. New crate `dotagent-mcp` holds the wire types.
  Docs: [`reference/mcp.md`](docs/reference/mcp.md).
- **Inbound Telegram** — the daemon can long-poll `getUpdates` and hand
  accepted messages to a dispatcher agent, whose stdout is sent back to the
  same chat. Configured under a new daemon-level `[telegram]` section
  (`bot_token`, `allowed_user_ids`, `dispatcher_agent`,
  `poll_timeout_seconds`, `rate_limit_per_minute`). Off unless both a token
  and a non-empty numeric allowlist are set — an empty allowlist means
  nobody, never everybody. Docs:
  [`concepts/telegram.md`](docs/concepts/telegram.md).
- Four env vars on triggered runs: `AGENT_TRIGGER_SOURCE`,
  `AGENT_TRIGGER_ACTOR`, `AGENT_TRIGGER_REPLY_TO`, `AGENT_TRIGGER_PAYLOAD`.
  Applied *before* the `AGENT_*` block so a payload can never redefine
  `AGENT_NAME` or `AGENT_HEARTBEAT_FILE`. The message body travels here,
  never in argv.
- Audit events `trigger_received`, `trigger_rejected` (`Critical`) and
  `agent_triggered`. Message bodies are never recorded — the log carries
  attribution, not transcripts.
- Threat model gains **V8** (untrusted inbound message causes a local run)
  and **V9** (MCP client reaches the agent catalog). V8 is the first vector
  where someone with no access to the machine can cause local execution,
  and it is called out as an explicit exception to the document's premise.
- `examples/telegram-assistant/` — the whole loop: message → `claude -p`
  with the MCP server attached → agent → reply.

### Fixed

- **One broken `agent.toml` no longer stops all scheduling.** Discovery
  aborted the whole scan on the first parse failure, and the daemon turned
  that error into an empty agent list — so a single typo silently stopped
  every agent, with one `warn!` line as the only evidence. Broken manifests
  are now skipped, reported by `doctor`, and audited as `manifest_invalid`
  (`Critical`, so it fires out-of-band notification).
- **The state store deleted its own lock file while still holding the
  lock.** Another process opening the same path afterwards got a fresh
  inode and acquired its own "exclusive" lock, so two writers could believe
  they had the file to themselves. The lock file now persists, matching
  what `AuditLog::append` already did.
- **`.expect()` on the heartbeat could panic the runner.** If the file went
  missing mid-run (another process, an operator with `rm`), finishing a run
  killed the daemon. It now rebuilds the record and logs, without inventing
  a `last_success_at` that never happened.
- **`give_up` re-derived the state root** instead of using the store it was
  given, so a daemon running against a non-default `DOTAGENT_HOME` wrote
  the give-up marker where nothing else would look for it.

### Changed

- `[[schedules]]` is no longer required. An agent that only ever runs from
  a trigger may declare none and receives the synthetic schedule id
  `trigger`.
- `dotagent doctor` reports inbound Telegram status: whether the ingress is
  on, and the two ways it can be half-configured (empty allowlist, missing
  dispatcher agent).
- The threat model now states the `[security]` enforcement gap explicitly
  rather than leaving it as a "post-v0" footnote. It reads differently once
  an untrusted message can pick which declared agent runs.
- The Telegram driver now shares one `reqwest::Client` with a timeout
  instead of building one per send. Previously every notification rebuilt
  the TLS stack and no timeout was configured at all.
- `CLAUDE.md` trade-off #4 amended: dotagent now **serves** MCP while still
  not **consuming** it. The `mcp` proxy CLI remains separate and `sink-*`
  plugins still shell out to it.

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
