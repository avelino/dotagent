# Changelog

All notable changes to dotagent are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/) and the project
adheres to [Semantic Versioning](https://semver.org/).

Pre-1.0: minor bumps may include breaking changes; both `agent.toml`
schema and the plugin protocol are flagged in each entry.

## [Unreleased]

### Added

- **Commands** — procedures *you* pick, published as a Telegram menu. A command
  is one markdown file in [Claude Code slash command][cc-slash] format under
  `~/.config/dotagent/commands/`, so a catalog you already keep is reusable
  rather than transcribed. Type `/` in the chat and the client renders
  everything installed; pick one and it runs. Skills cover the case where a
  model decides what applies — commands cover the case where you already know,
  and paying for a dispatch decision that was already made is latency plus a
  chance of picking something else. `$ARGUMENTS` and `$1`…`$N` substitute what
  was typed after the name; arguments given to a body with no placeholder are
  appended rather than silently dropped. Configurable under `[commands]`,
  including `claude_commands` — **off** by default, unlike its skills
  counterpart, because a command becomes a published menu entry and a Claude
  Code catalog is usually full of things that assume a shell. Docs:
  [`concepts/commands.md`](docs/concepts/commands.md).
- Commands carry **two derived names**, and they collide under different rules.
  Telegram allows `[a-z0-9_]{1,32}` — no hyphens — so `commit-message`
  registers as `/commit_message` while the MCP tool is
  `command-commit-message`. The catalog is deduped twice, a name Telegram would
  refuse fails validation instead of being truncated into a menu entry that
  resolves to nothing, and `dotagent doctor` names the *files* in a collision so
  you know which to rename.
- `dotagent mcp` gains `command-get` and `command-list` — **two tools for the
  whole catalog, not one per command**, deliberately unlike `skill-*`. There the
  catalog is a menu a model picks from; here a human already picked, and
  publishing N tools would re-open that decision and let a model call the wrong
  one. The daemon parses `/name args` lexically and publishes the catalog via
  `setMyCommands`; it never resolves a command to its body, which keeps
  "dotagent itself interprets nothing" true.
- The Telegram menu registers per allowlisted chat rather than globally. The
  allowlist already gates execution, so a global menu would not be an
  authorization hole — but it would publish every command name and description
  to anyone who finds the bot. New audit event `command_dispatched` records the
  name and whether it was known, never the arguments.
- Built-in `/help` lists what is installed, and an unknown `/typo` is answered
  from the catalog instead of falling through to a model that would improvise.
  Writing your own `help.md` replaces the built-in.

[cc-slash]: https://docs.claude.com/en/docs/claude-code/slash-commands

### Fixed

- `tools/call` parsed the agent argument schema before dispatching, so a tool
  whose `args` is not a list — `command-get`, whose `args` is the string the
  sender typed — was rejected with the *agent* schema's error before its handler
  ran. The typed parse now happens once the tool is known to be an agent.

- **Skills** — procedures an assistant loads when they apply. A skill is a
  directory with a `SKILL.md` (frontmatter + markdown), and `dotagent mcp`
  exposes one `skill-<name>` tool per installed skill: the description is the
  trigger a model matches on, the body arrives only when it calls. Agents are
  verbs and memory is facts; this is the third thing, *how to do something* —
  which previously had to bloat a system prompt or be forced into a script.
  Two more tools reach inside one: `skill-read` for the reference files a
  procedure points at (without it, "see references/x.md" is a dead end for a
  caller with no filesystem tool) and `skill-run` for executables under
  `scripts/`, supervised with a deadline and kill-tree, audited as
  `skill_invoked`. `~/.claude/skills/` is searched by default — the format is
  Anthropic's Agent Skills layout, so a skill written for Claude Code needs no
  copy, with the honest caveat that a skill whose steps assume that harness
  (Bash, Read, nested skills) does not port. Configurable under `[skills]`.
  Docs: [`concepts/skills.md`](docs/concepts/skills.md), threat vector V10.

- **Long-term memory** — new crate `dotagent-memory` embeds
  [outl](https://github.com/avelino/outl) (`outl-ws` + `outl-actions`) as an
  agent memory backend, and `dotagent mcp` exposes `memory-remember` /
  `memory-recall`. Facts land as one block per journal entry in a workspace
  at `~/.config/dotagent/outl`, scaffolded when the daemon starts. It is a
  normal outl workspace on purpose: open it, read what an agent kept, fix a
  wrong memory, delete a page. Configurable under `[memory]`, and a
  configured path is never scaffolded so a typo fails loudly. Recall is
  substring rather than semantic — a near-miss an assistant states as fact
  is worse than no match. Docs: [`concepts/memory.md`](docs/concepts/memory.md).
- **Replies carry what they answer.** When a Telegram message is a reply, the
  quoted text rides in `AGENT_TRIGGER_PAYLOAD.reply_to_text`. A bare "sim"
  means nothing on its own, and inferring it from conversation history breaks
  the moment someone answers an older message out of order.
- Memory is a **graph**, not a list. `memory-remember` takes topics, each
  becoming a `[[link]]` to a page that outl's backlinks fill in, so a fact
  stored once on the day it was learned is reachable both chronologically and
  by subject. `memory-recall` accepts a `topic` to ask the graph instead of
  guessing which words the fact used, and `memory-topics` lists what exists —
  reusing a topic is what makes the gathering work, and inventing a near
  duplicate splits a subject in two.
- Topic names are normalized (`Roam Research`, `roam research` → `roam-research`)
  so capitalization alone cannot fragment the graph. `/` survives, since
  hierarchy is a real page path in outl.
- **Introspection tools** on the MCP server: `dotagent-status`,
  `dotagent-logs`, `dotagent-inspect`, `dotagent-doctor` and
  `dotagent-next-runs`. An assistant asked "did it run?" can now quote the log
  or the health table instead of reasoning about what probably happened.
  Read-only by selection: `install`, `uninstall`, `reload` and `daily-summary`
  are deliberately absent, and `bootstrap` most of all — it marks every window
  as ok, silencing a failure instead of fixing it. Agent names resolve through
  discovery, so `../` is refused rather than followed.
- `dotagent doctor` reports where memory lives, or that it is off.

### Changed

- `reqwest` 0.13.1 → 0.13.4. The `webpki-roots` feature was dropped upstream
  and `rustls` now carries the trust anchors; keeping the old feature name
  pinned the lock and silently blocked every patch release. Verified with a
  real TLS handshake against `api.telegram.org` before and after.

## [0.2.0] - 2026-08-04

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
- Replies quote the message they answer (`reply_parameters`, Bot API 7.0+).
  Runs are asynchronous and a chat can have several questions in flight, so
  an unquoted answer leaves you guessing which one it belongs to. Sent with
  `allow_sending_without_reply`, so deleting the original mid-run still
  delivers the answer instead of failing with a 400.

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
- **One malformed Telegram update no longer drops the whole batch.** A
  required field missing from a single update failed the deserialization of
  the entire `getUpdates` response, which in a long-poll means the offset
  never advances and Telegram redelivers that batch forever. One bad message
  would have wedged the bot permanently.

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
