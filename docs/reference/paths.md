# Filesystem Layout

> Every file dotagent reads or writes, where it lives, and who owns it.

dotagent picks **convergence over XDG**: everything lives under a
single root. The trade-off is documented in
[`crates/dotagent-state/src/paths.rs`](../../crates/dotagent-state/src/paths.rs):
finding / inspecting / wiping is easier when one `$DOTAGENT_HOME` holds
state, logs, config, and the audit log together. If you religiously
follow the XDG Base Directory Spec, set `DOTAGENT_HOME` to your
preferred path.

---

## The root

```text
$DOTAGENT_HOME/                   # default: ~/.config/dotagent
```

Resolution order:

1. `$DOTAGENT_HOME` env var (if set, absolute path)
2. `$HOME/.config/dotagent`
3. `./.dotagent` (last-resort sentinel if `$HOME` isn't set)

Everything below is relative to this root.

---

## Top-level

```text
$DOTAGENT_HOME/
├── agents/                       # YOUR manifests (or symlinks to them)
├── skills/                       # YOUR procedures (optional; also read from ~/.claude/skills)
├── commands/                     # YOUR commands (optional; one .md each)
├── plugins/                      # YOUR custom plugin binaries (optional)
├── config.toml                   # global config (optional)
├── secrets.env                   # daemon-loaded KEY=VALUE secrets (optional, 0600)
├── state/                        # daemon state (read-write, machine-managed)
├── logs/                         # operational logs (rotated)
├── audit.log                     # append-only hash-chained event log
└── audit.log.20260806T101500     # rotated segment (kept forever)
```

| Path             | Who writes         | Who reads           | Notes                                                                  |
|------------------|--------------------|---------------------|------------------------------------------------------------------------|
| `agents/`        | **you**            | daemon, CLI         | Manifests OR symlinks to manifests living elsewhere (e.g., dotfiles).  |
| `skills/`        | **you** (optional) | `dotagent mcp`      | One directory per procedure. `~/.claude/skills/` is searched too, so a skill written for Claude Code needs no copy. See [`concepts/skills.md`](../concepts/skills.md). |
| `commands/`      | **you** (optional) | daemon, `dotagent mcp` | One `.md` per command, subdirectories namespaced with `:`. Published as the Telegram menu. `~/.claude/commands/` is **not** searched unless `[commands] claude_commands = true`. See [`concepts/commands.md`](../concepts/commands.md). |
| `plugins/`       | **you**            | `PluginClient`      | Per-user plugin binaries. Skip if you install plugins via brew / cargo. |
| `config.toml`    | **you** (optional) | daemon              | Schema in [`config-reference.md`](../guides/config-reference.md).      |
| `secrets.env`    | **you** (optional) | daemon              | KEY=VALUE secrets for `${VAR}` interpolation in notifier configs. **Must be mode 0600.** See [`concepts/secrets.md`](../concepts/secrets.md). |
| `state/`         | daemon, runner     | daemon, CLI         | **Don't edit by hand.**                                                |
| `logs/`          | daemon             | you (via `dotagent logs` / `tail`) | Rotated daily, gzipped after 1d, deleted after retention horizon. |
| `audit.log`      | daemon             | you (`tail`, `jq`)  | Append-only, hash-chained. Rotates at 32MB into `audit.log.<stamp>` segments, which are **never deleted automatically**. |

---

## `agents/<name>/`

Each direct subdirectory containing an `agent.toml` is an agent.

```text
agents/<name>/
├── agent.toml          # REQUIRED — manifest
├── agent.fish          # (or agent.py / agent.go / a built binary)
├── prompt.md           # optional — LLM prompt
├── config.json         # optional — static data
├── CLAUDE.md           # optional — doc-for-LLMs
└── README.md           # optional — doc-for-humans
```

**Discovery roots** (a manifest is picked up from the first match):

1. Every directory in `$DOTAGENT_ROOT` (colon-separated; for one-off
   overrides / CI).
2. `$DOTAGENT_HOME/agents/` ← typical production
3. `$CWD/agents/`
4. `$CWD`

Each direct subdirectory of a search root that has an `agent.toml`
becomes one agent. dotagent indexes by `agent.name` from the manifest,
not by directory name — duplicates resolve to first-found.

---

## `skills/<name>/`

Each direct subdirectory containing a `SKILL.md` is a skill — a procedure an
assistant loads on demand, not something that runs on a schedule.

```text
skills/<name>/
├── SKILL.md            # REQUIRED — frontmatter (name, description) + procedure
├── scripts/            # optional — the only files `skill-run` will execute
└── references/         # optional — fetched one at a time with `skill-read`
```

**Discovery roots** (first match wins a name):

1. `[skills] paths` from `config.toml`
2. `$DOTAGENT_ROOT` and `$DOTAGENT_ROOT/skills`
3. `$DOTAGENT_HOME/skills/` ← typical
4. `~/.claude/skills/` and `$CWD/.claude/skills/` — unless
   `[skills] claude_skills = false`
5. `$CWD/skills/`

Not scaffolded: an empty catalog is a valid state, and step 4 means a skill you
already wrote for Claude Code is reachable without being copied. Symlinks work
here the same way they do under `agents/`. See
[`concepts/skills.md`](../concepts/skills.md).

---

## `commands/`

Each `.md` file is one command — a prompt **you** invoke by name from the
Telegram menu. Not a directory, unlike a skill: a command is text and nothing
else, so there is nothing to package alongside it.

```text
commands/
├── standup.md          # → /standup
└── git/
    └── status.md       # → /git_status  (namespaced `git:status`)
```

**Discovery roots** (first match wins a name):

1. `[commands] paths` from `config.toml`
2. `$DOTAGENT_ROOT/commands`
3. `$DOTAGENT_HOME/commands/` ← typical
4. `~/.claude/commands/` and `$CWD/.claude/commands/` — **only** when
   `[commands] claude_commands = true`
5. `$CWD/commands/`

Step 4 is opt-in, the reverse of skills: a command becomes a published menu
entry, and a Claude Code catalog usually assumes a shell it will not have.

Nesting goes three levels deep. Not scaffolded, same as `skills/`. See
[`concepts/commands.md`](../concepts/commands.md).

---

## `plugins/`

Custom plugin binaries you install yourself (vs. brew / cargo install,
which drop binaries into `$PATH` already).

```text
plugins/
├── dotagent-plugin-notify-discord
├── dotagent-plugin-sink-notion
└── ...
```

**Discovery order** (`dotagent-plugin-<name>` is resolved against, in
sequence):

1. Every directory in `$DOTAGENT_PLUGIN_PATH` (colon-separated)
2. `$DOTAGENT_HOME/plugins/`
3. `/usr/local/lib/dotagent/plugins/`
4. `$PATH`

First match wins. See [`plugin-protocol.md`](plugin-protocol.md#discovery).

---

## `config.toml`

Optional global config. dotagent works with **zero config** — defaults
are baked into the binary.

```text
config.toml
```

When present, fields you set override defaults; missing fields fall
back. Full schema: [`config-reference.md`](../guides/config-reference.md).

---

## `state/`

```text
state/
├── agents/<name>/<slug>.heartbeat.json
├── windows/<name>-<slug>-<YYYY-MM-DD-HHMM>.json
├── plugins/<plugin>/<key>.json
├── notify/<driver>/<slug>.json
├── notify/alerts.json              # re-notification ladder for ongoing alerts
├── known_manifests.json
├── supervisor.json                 # live subprocess registry (daemon → CLI)
└── daemon.pid
```

### `state/supervisor.json`

Snapshot of every subprocess the daemon is supervising right now (agents
+ plugin invocations). The daemon rewrites this file every 2s; out-of-
process consumers (`dotagent status`, `dotagent doctor`) read it to
surface live processes with age vs deadline. Cleared on daemon exit.

```jsonc
[
  {
    "id": 42,
    "pid": 3980,
    "kind": "sink",
    "owner": {
      "agent": "hn-digest",
      "schedule": "weekday-morning",
      "hook_event": "success",
      "plugin": "sink-file"
    },
    "label": "sink-file.invoke",
    "started_at": "2026-05-21T15:00:00.123456-03:00",
    "deadline_seconds": 300,
    "age_seconds": 47,
    "deadline_pct": 15,
    "pgid": 3980,
    "command": "dotagent-plugin-sink-file invoke"
  }
]
```

`pgid` and `command` are not for display — `dotagent status` shows
neither. They exist so a **later** daemon can prove that a pid it read
off disk is still the process that was recorded, before it signals
anything. Both are optional on read: a snapshot written by an older
build has neither, and such a record must deserialize into one that is
refused rather than fail the whole file.

Missing file ⇒ daemon not running. Stale file (>5s) ⇒ the previous
daemon did not run its shutdown path (`SIGKILL`, panic, `launchctl
kickstart -k`), because that path deletes this file. The next daemon
treats the leftover as a record of possible orphans: it reaps the
entries whose identity it can confirm, leaves every other entry alone,
and then removes the file. An unreadable or unparseable snapshot
signals nothing and is left on disk as evidence. See
[Boot orphan reap](../guides/daemon-lifecycle.md#boot-orphan-reap).

> **Scope**: `supervisor.json` reflects ONLY the daemon's supervisor.
> `dotagent run-now` / `dotagent run` invoked standalone instantiate their
> own short-lived supervisor and do NOT publish to this file — they
> aren't visible in `dotagent status`. Follow-up to issue #36 will
> either emit per-PID snapshot files (`supervisor-<pid>.json`) or move
> to a real IPC channel.

### `state/agents/<name>/<slug>.heartbeat.json`

One file per `(agent, slug)` pair. Written before AND after each
non-dry-run execution. The shape is intentionally compatible with the
legacy Fish framework's heartbeat (see
[`migrating-from-fish.md`](../guides/migrating-from-fish.md)).

```jsonc
{
  "name": "finops-weekly",
  "slug": "period_dia-anterior",
  "args": ["--period", "dia-anterior"],
  "started_at": 1700000000,
  "started_at_iso": "2023-11-14T22:13:20+0000",
  "finished_at": 1700000100,
  "finished_at_iso": "2023-11-14T22:15:00+0000",
  "exit_code": 0,
  "duration_seconds": 100,
  "last_success_at": 1700000100,         // preserved across runs; never zeroed on failure
  "last_success_at_iso": "2023-11-14T22:15:00+0000"
}
```

**Slug derivation** (from the schedule's `args`):

| `args`                              | slug                  |
|-------------------------------------|-----------------------|
| `[]`                                | `default`             |
| `["--period", "dia-anterior"]`      | `period_dia-anterior` |
| `["--mode", "unsubscribe"]`         | `mode_unsubscribe`    |

Rules: strip leading dashes, lowercase, non-alphanumeric → `_`,
collapse `_`, trim trailing `_`. Empty → `default`.

### `state/windows/<name>-<slug>-<YYYY-MM-DD-HHMM>.json`

One file per `(agent, schedule, expected_at)`, plus a `.lock` beside
it. Tracks whether the expected window has been satisfied — drives the
retry policy and health computation.

The only state directory with a retention horizon: a schedule never
revisits a window once it passes, so the directory would otherwise grow
forever. The daily 03:00 sweep deletes each pair older than
[`[state] window_retention_days`](../guides/config-reference.md#state)
(default 30).

```jsonc
{
  "agent": "hn-digest",
  "schedule_id": "weekday-morning",
  "expected_at": 1700000000,
  "attempts": 1,
  "last_attempt_at": 1700000000,
  "last_attempt_exit_code": 0,
  "last_attempt_stderr": null,
  "given_up": false,
  "given_up_at": null
}
```

`attempts` counts **dispatches**, not retries — the one that finally
succeeded is in there too. So a window that worked on the first try
reads `attempts: 1` with `last_attempt_exit_code: 0`, and that is `ok`,
not `degraded`.

### `state/plugins/<plugin>/<key>.json`

Per-plugin scratch. Format is plugin-defined — dotagent doesn't read
this directory itself.

Convention from the in-tree plugins (e.g., `sink-roam`):
`<key>` is a stable identifier the plugin picks (slug, hash, etc.).

### `state/notify/<driver>/<slug>.json`

Built-in notifier rate-limit state. Each driver decides what to write
here. The `imessage` driver, for example, persists the last-send
timestamp per `(agent, slug)` so `rate_limit_minutes` works across
daemon restarts.

### `<any state file>.lock`

Empty file beside every JSON state file, held with `flock` during a write.

It is **not** removed afterwards, on purpose. Unlinking it while still holding
the lock lets the next writer open a fresh inode and take its own "exclusive"
lock, so two processes end up writing the same file believing they have it to
themselves. Safe to delete when nothing is running; it reappears on the next
write.

### `outl/`

Long-term agent memory, as an [outl](https://github.com/avelino/outl)
workspace. Scaffolded when the daemon starts.

```text
outl/
├── journals/YYYY-MM-DD.md      # the facts, one block each
├── journals/YYYY-MM-DD.outl    # block identity sidecar
├── ops/                        # op log — source of truth
└── pages/ templates/ assets/
```

A normal outl workspace, on purpose: open it in the desktop app, read what an
agent remembered, fix a wrong memory, delete a page. Relocate with
`[memory] workspace` in `config.toml`. See [Memory](../concepts/memory.md).

### `state/<agent>/`

Whatever an agent needs to keep between runs, under a directory named after
it. dotagent writes nothing here; the agent owns it.

[`examples/telegram-assistant`](https://github.com/avelino/dotagent/tree/main/examples/telegram-assistant)
uses `state/telegram-assistant/` for which `claude` session belongs to which
chat, and how many times that session has been retired for length:

```text
state/telegram-assistant/
├── <chat-id>.started      # session id, written only after a run that created it
└── <chat-id>.gen          # generation counter, bumped on each retirement
```

An agent that keeps state here should declare the path in `[security]
filesystem_writable`, so `doctor` can audit what it writes.

### `state/notify/alerts.json`

When each ongoing alert last spoke, so a condition that stays true does not
notify on every tick.

```jsonc
{
  "episodes": {
    "disk-alert/every-15min/stale": {
      "first_notified_at": 1785925367,
      "last_notified_at": 1786011767,
      "count": 2
    }
  }
}
```

Keyed `agent/schedule/event`. An episode starts at the first notification and
is **deleted** when that schedule succeeds again, so the next failure alerts
immediately instead of inheriting the previous episode's spacing. Re-notify
ladder: on entry, then 1h, 6h, and daily thereafter.

Losing the file costs at most one duplicate alert — the opposite failure
(losing the alert) is the expensive one, so a missing or corrupt table is read
as "nothing has been notified yet". See
[notifications](../concepts/notifications.md#stale-and-why-alerts-repeat).

### `state/notify/telegram/sent.json`

What each outbound notification was about, so a reply to one resolves to the
run that caused it.

```jsonc
{
  "entries": {
    "4821": {
      "agent": "disk-alert",
      "schedule": "every-15min",
      "event": "given_up",
      "at": 1785925367
    }
  }
}
```

Capped at the most recent 500, oldest dropped first. It is a lookup table for
alerts you might still answer, not a history — and losing it costs correlation
on old messages, never delivery.

### `state/notify/telegram/offset.json`

Last acknowledged Telegram `update_id`, written tmp-then-rename.

```jsonc
{ "offset": 481923771 }
```

Telegram redelivers every update until a higher `offset` acknowledges it.
Without this file a daemon restart replays the backlog — and for a bot that
runs agents, replay means re-running whatever the last messages asked for.
Delivery is at-most-once on purpose. See [Telegram](../concepts/telegram.md).

### `state/known_manifests.json`

Cache of `sha256(agent.toml)` for every loaded manifest. Drives
[manifest drift detection](../security/threat-model.md):

```jsonc
{
  "entries": {
    "finops-weekly": {
      "sha256": "a3f9...",
      "path": "/Users/me/.config/dotagent/agents/finops-weekly/agent.toml",
      "first_seen_at_iso": "2026-05-19T14:00:00-0300"
    }
  }
}
```

On each daemon load:

- New name in `agents/` not in the cache → `PhantomAgentDetected` (critical, notify).
- Existing name but mismatched sha → `ManifestDriftDetected` (critical, notify).

### `state/daemon.pid`

The running daemon's PID. Used by `dotagent reload` (sends SIGHUP) and
removed on graceful exit via a `Drop` guard. Stale pidfile (no live
process at that PID) means the daemon crashed without cleanup — restart
it.

---

## `logs/`

```text
logs/
├── daemon/
│   ├── dotagent.log                       # structured JSON, daily rotation
│   ├── dotagent.log.2026-05-19            # yesterday's rolled file
│   ├── dotagent.log.2026-05-18.gz         # older, gzipped
│   ├── run.avelino.dotagent.log           # launchd / systemd stdout capture
│   └── run.avelino.dotagent-error.log     # launchd / systemd stderr — crashes only
├── agents/
│   └── <name>/
│       ├── <name>.log                     # raw stdout+stderr from the agent
│       ├── <name>.log.2026-05-19          # rolled
│       └── <name>.log.2026-05-18.gz       # gzipped
└── plugins/
    └── <plugin>/                          # (currently unused; reserved)
```

| File                                 | Format                 | Rotation                                  | Retention default |
|--------------------------------------|------------------------|-------------------------------------------|-------------------|
| `daemon/dotagent.log`                | NDJSON (`tracing`)     | daily                                     | 30 days           |
| `daemon/run.avelino.dotagent.log`    | Raw text               | (launchd / systemd appends)               | (managed by OS)   |
| `daemon/run.avelino.dotagent-error.log` | Raw text             | (launchd / systemd appends)               | (managed by OS)   |
| `agents/<name>/<name>.log`           | Raw stdout+stderr      | daily                                     | 14 days           |

Compression: rotated files older than `compress_after_days` (default 1)
get gzipped in-place. Deletion: files older than the retention horizon
are removed by the 03:00 sweep. The **active** (non-rotated) file is never
deleted, whatever the horizon says: launchd and systemd hold an open fd on
it, so unlinking it would strand every subsequent write in a file with no
name.

### The stderr file is a crash channel, not an activity log

`run.avelino.dotagent-error.log` receives panics and pre-logging startup
failures. It does **not** receive the daemon's `tracing` stream.

The daemon installs its stderr mirror layer only when stderr is a terminal.
Under launchd / systemd stderr is an appended plain file that no rotation
policy covers, so mirroring there would duplicate `dotagent.log` into a file
that grows without bound. On a healthy daemon this file stays empty — that is
the intended state, not a symptom.

| `DOTAGENT_LOG_STDERR` | Effect                                                |
|-----------------------|-------------------------------------------------------|
| unset (default)       | mirror on **only** if stderr is a TTY                  |
| `1` / `true` / `yes` / `on`  | force the mirror on — for units rewired to journald, which does rotate |
| `0` / `false` / `no` / `off` | force it off, even in a terminal               |
| anything else         | ignored; falls back to the TTY default                 |

ANSI colour follows the same rule and additionally honours `NO_COLOR`: set it
to any value and escapes are suppressed in the daemon and in every subcommand.

The full schema + jq examples are in
[`guides/observability.md`](../guides/observability.md).

---

## `audit.log`

```text
audit.log
```

**Hash-chained, append-only. Rotates by size; segments are never
deleted automatically.**

One JSON object per line. Each line carries `prev_hash = sha256(previous
line's full JSON)`. The very first line ever written has
`prev_hash = "GENESIS"`. On startup the daemon verifies the chain; if it
breaks, an `AuditChainBroken` entry is appended (which itself becomes a
chained entry — anchoring the new chain to the broken position).

### Rotation and the seam

Past 32MB the live file is renamed to `audit.log.<YYYYMMDDTHHMMSS>` and a
fresh `audit.log` opens with a **seam** as its first line:

```jsonc
{
  "ts": "2026-08-06T10:15:00-0300",
  "severity": "notice",
  "event": {
    "event_type": "audit_log_rotated",
    "rotated_to": "audit.log.20260806T101500",
    "entries": 38513,
    "first_ts": "2026-05-19T08:31:07-0300",
    "tail_hash": "c8d2..."
  },
  "prev_hash": "c8d2..."          // == tail_hash: the chain crosses the rename
}
```

The seam's `prev_hash` **is** the rotated segment's tail hash, so the chain
has no gap at the rename. And because the seam names what left, deleting an
old segment is legible: verification reports *"intact since `first_ts`;
38513 earlier entries lived in `audit.log.20260806T101500`, which is gone"*
rather than *"chain broken"*. Cutting lines off the head of the live file
takes the seam with them, and the orphaned `prev_hash` that remains is
accounted for by nothing — which is the case that fires
`audit_chain_broken`.

Segments are **never removed by dotagent**. The log rotation sweeper in
`[logs]` does not touch them; this is the only forensic artifact dotagent
keeps and pruning it is the operator's call. `rm` them when you mean to.

Verification has two scopes:

| Scope | What it walks | Used by |
|---|---|---|
| current segment | the live `audit.log` only | daemon at boot — the live file is the only one that changes |
| full | follows seams backwards through every segment still present | on demand, when you want the guarantee to reach `GENESIS` |

Example line (pretty-printed):

```jsonc
{
  "ts": "2026-05-19T14:30:01-0300",
  "severity": "info",
  "event": {
    "event_type": "agent_run",
    "agent": "finops-weekly",
    "schedule": "weekly",
    "slug": "default",
    "manifest_sha256": "a3f9...",
    "exit_code": 0,
    "duration_seconds": 47,
    "timed_out": false
  },
  "prev_hash": "c8d2..."
}
```

Audit events emitted by dotagent:

| `event_type`              | When                                                                        | Severity      |
|---------------------------|-----------------------------------------------------------------------------|---------------|
| `daemon_started`          | Daemon process boots                                                        | info          |
| `daemon_stopped`          | Daemon receives SIGTERM / SIGINT                                            | info          |
| `tick_started`            | **No longer emitted.** Parsed so existing logs stay readable — see below     | info          |
| `tick_completed`          | **No longer emitted.** Parsed so existing logs stay readable — see below     | info          |
| `agent_run`               | An agent completed (success or failure)                                     | info / critical |
| `agent_recovered`         | A previously-failing schedule passed                                        | notice        |
| `agent_given_up`          | All retries exhausted for a window                                          | critical      |
| `preflight_failed`        | A preflight plugin blocked the run                                          | critical      |
| `plugin_invoked`          | Any plugin / notifier invocation                                            | info / notice |
| `manifest_loaded`         | Manifest read on daemon start / SIGHUP                                      | info          |
| `manifest_drift_detected` | sha256(manifest) doesn't match cache                                        | critical      |
| `phantom_agent_detected`  | Discovered agent not in `known_manifests.json`                              | critical      |
| `audit_chain_broken`      | Hash chain verification failed at line N, or the head of the log was removed with nothing accounting for it | critical      |
| `audit_log_rotated`       | The log passed 32MB; the seam naming the segment it came from and that segment's tail hash | notice        |
| `config_reloaded`         | SIGHUP picked up changes to `config.toml`                                   | notice        |
| `secrets_loaded`          | Daemon read `secrets.env`; payload has `path`, `key_count`, `unresolved_references` (no values, no `op://` paths) | notice |
| `secrets_refused`         | Daemon rejected `secrets.env` (insecure mode, parse, or IO)                 | critical      |
| `manifest_invalid`        | An `agent.toml` exists but failed to parse or validate; that agent is skipped, the rest keep running | critical |
| `trigger_received`        | Inbound message accepted; records `actor` and `reply_to`, never the text    | notice        |
| `trigger_rejected`        | Inbound message refused (allowlist, rate limit) before anything ran         | critical      |
| `agent_triggered`         | A run started from a trigger rather than a schedule window                  | notice        |
| `command_dispatched`      | A `/name` invocation was resolved; records the name and whether it was known, never the arguments | notice |
| `skill_invoked`           | A `scripts/` executable inside a skill ran — code outside any manifest, which `agent_run` would not record | notice |
| `skill_invalid`           | A `SKILL.md` exists but failed to parse; that procedure is missing from the catalog, the rest keep working | notice |
| `remediation_invoked`     | A declared `[[preflight]] remediation` ran. The one event where a chat message changed the machine | critical |
| `orphan_reaped`           | A process a previous daemon left running was killed at boot, after its identity was confirmed against the snapshot | critical |

`Critical` severity drives out-of-band notifier dispatch. Defined in
[`crates/dotagent-core/src/audit.rs`](../../crates/dotagent-core/src/audit.rs).

### Retired: `tick_started` / `tick_completed`

The daemon no longer writes these. A tick is telemetry, not an auditable
event — "woke up and looked at 17 agents" tells a forensic reader nothing,
and it dominated the file: on one real 38,510-entry log the two variants were
64% of every line. Because each append re-reads the log to find the tail hash,
that noise also made every event worth recording more expensive to write. The
daemon emits `tracing` at `debug` level instead, which already rotates — see
[observability](../guides/observability.md).

The enum variants are **kept** so existing logs stay parseable. Removing them
would make `dotagent status --audit` and chain verification fail the moment
they reached a historic tick entry, and a chain you can no longer verify is
worse than one carrying dead weight.

---

## Platform-specific paths (outside `$DOTAGENT_HOME`)

These are written by `dotagent install`, not by the daemon at runtime:

### macOS

```text
~/Library/LaunchAgents/run.avelino.dotagent.plist
```

Template: [`crates/dotagent-unit-gen/templates/daemon.plist`](../../crates/dotagent-unit-gen/templates/daemon.plist).

Rendered properties:

- `Label` = `run.avelino.dotagent`
- `ProgramArguments` = `["<dotagent binary>", "daemon"]`
- `RunAtLoad` = true
- `KeepAlive` = true
- `ProcessType` = `Background`
- `ThrottleInterval` = 10
- `StandardOutPath` = `$DOTAGENT_HOME/logs/daemon/run.avelino.dotagent.log`
- `StandardErrorPath` = `$DOTAGENT_HOME/logs/daemon/run.avelino.dotagent-error.log`

### Linux

```text
~/.config/systemd/user/run.avelino.dotagent.service
```

Template: [`crates/dotagent-unit-gen/templates/daemon.service`](../../crates/dotagent-unit-gen/templates/daemon.service).

Rendered properties:

- `[Service] Type=simple`
- `ExecStart=<dotagent binary> daemon`
- `Restart=always`
- `RestartSec=10`
- `StandardOutput=append:<logs/daemon/run.avelino.dotagent.log>`
- `StandardError=append:<logs/daemon/run.avelino.dotagent-error.log>`
- `[Install] WantedBy=default.target`

---

## Permissions

dotagent runs as **your user** (no daemon root, no setuid). Files are
written with your umask (`0644` for regular files, `0755` for
directories on a typical user shell). Override by setting umask before
launching the daemon:

```bash
# In your launchd / systemd unit override, or shell rc:
umask 027    # group-readable only
```

macOS Full Disk Access: if your agent or `sink-file` writes into
`~/Documents` / `~/Downloads` / `~/Desktop`, the daemon binary needs
the **Full Disk Access** entitlement under System Settings → Privacy
& Security.

---

## What dotagent **does NOT** put under `$DOTAGENT_HOME`

- launchd plist / systemd unit (platform-specific, see above).
- The `dotagent` binary itself (lives in `~/.cargo/bin/`, `/opt/homebrew/bin/`,
  `/usr/local/bin/`, etc.).
- Plugin binaries installed via `cargo install` / `brew` (those go to
  `~/.cargo/bin/` / Homebrew prefix).
- Your agent scripts' working data, unless your script writes there
  deliberately. dotagent gives each run a fresh `$AGENT_TMPDIR` that
  auto-cleans on exit.

---

## Related

- [`env-vars.md`](env-vars.md) — `DOTAGENT_HOME`, `DOTAGENT_ROOT`,
  `DOTAGENT_PLUGIN_PATH`, etc.
- [`config-reference.md`](../guides/config-reference.md) — `config.toml`
  schema
- [`observability.md`](../guides/observability.md) — log format + jq
  recipes
- [`threat-model.md`](../security/threat-model.md) — audit log's
  forensic role
