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
├── plugins/                      # YOUR custom plugin binaries (optional)
├── config.toml                   # global config (optional)
├── secrets.env                   # daemon-loaded KEY=VALUE secrets (optional, 0600)
├── state/                        # daemon state (read-write, machine-managed)
├── logs/                         # operational logs (rotated)
└── audit.log                     # append-only hash-chained event log
```

| Path             | Who writes         | Who reads           | Notes                                                                  |
|------------------|--------------------|---------------------|------------------------------------------------------------------------|
| `agents/`        | **you**            | daemon, CLI         | Manifests OR symlinks to manifests living elsewhere (e.g., dotfiles).  |
| `plugins/`       | **you**            | `PluginClient`      | Per-user plugin binaries. Skip if you install plugins via brew / cargo. |
| `config.toml`    | **you** (optional) | daemon              | Schema in [`config-reference.md`](../guides/config-reference.md).      |
| `secrets.env`    | **you** (optional) | daemon              | KEY=VALUE secrets for `${VAR}` interpolation in notifier configs. **Must be mode 0600.** See [`concepts/secrets.md`](../concepts/secrets.md). |
| `state/`         | daemon, runner     | daemon, CLI         | **Don't edit by hand.**                                                |
| `logs/`          | daemon             | you (via `dotagent logs` / `tail`) | Rotated daily, gzipped after 1d, deleted after retention horizon. |
| `audit.log`      | daemon             | you (`tail`, `jq`)  | Append-only, hash-chained. **NEVER rotated.**                          |

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
      "agent": "databricks-cost-daily",
      "schedule": "weekly",
      "hook_event": "success",
      "plugin": "sink-roam"
    },
    "label": "sink-roam.invoke",
    "started_at": "2026-05-21T15:00:00.123456-03:00",
    "deadline_seconds": 300,
    "age_seconds": 47,
    "deadline_pct": 15
  }
]
```

Missing file ⇒ daemon not running. Stale file (>5s) ⇒ daemon may have
crashed without graceful shutdown; treat with suspicion.

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

One file per `(agent, schedule, expected_at)`. Tracks whether the
expected window has been satisfied — drives the retry policy and
health computation.

```jsonc
{
  "attempts": 1,
  "first_attempt_at": 1700000000,
  "last_attempt_at": 1700000000,
  "succeeded_at": null
}
```

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

### `state/known_manifests.json`

Cache of `sha256(agent.toml)` for every loaded manifest. Drives
[manifest drift detection](../security/threat-model.md):

```jsonc
{
  "entries": {
    "finops-weekly": {
      "sha256": "a3f9...",
      "path": "/Users/avelino/.config/dotagent/agents/finops-weekly/agent.toml",
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
│   └── run.avelino.dotagent-error.log     # launchd / systemd stderr capture
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
are removed by the 03:00 sweep.

The full schema + jq examples are in
[`guides/observability.md`](../guides/observability.md).

---

## `audit.log`

```text
audit.log
```

**Hash-chained, append-only, never rotated.**

One JSON object per line. Each line carries `prev_hash = sha256(previous
line's full JSON)`. The first line has `prev_hash = "GENESIS"`. On
startup the daemon verifies the chain; if it breaks, an
`AuditChainBroken` entry is appended (which itself becomes a chained
entry — anchoring the new chain to the broken position).

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
| `tick_started`            | A scheduler tick begins                                                     | info          |
| `tick_completed`          | Tick finishes; `next_event_iso` recorded                                    | info          |
| `agent_run`               | An agent completed (success or failure)                                     | info / critical |
| `agent_recovered`         | A previously-failing schedule passed                                        | notice        |
| `agent_given_up`          | All retries exhausted for a window                                          | critical      |
| `preflight_failed`        | A preflight plugin blocked the run                                          | critical      |
| `plugin_invoked`          | Any plugin / notifier invocation                                            | info / notice |
| `manifest_loaded`         | Manifest read on daemon start / SIGHUP                                      | info          |
| `manifest_drift_detected` | sha256(manifest) doesn't match cache                                        | critical      |
| `phantom_agent_detected`  | Discovered agent not in `known_manifests.json`                              | critical      |
| `audit_chain_broken`      | Hash chain verification failed at line N                                    | critical      |
| `config_reloaded`         | SIGHUP picked up changes to `config.toml`                                   | notice        |
| `secrets_loaded`          | Daemon read `secrets.env`; payload has `path`, `key_count`, `unresolved_references` (no values, no `op://` paths) | notice |
| `secrets_refused`         | Daemon rejected `secrets.env` (insecure mode, parse, or IO)                 | critical      |

`Critical` severity drives out-of-band notifier dispatch. Defined in
[`crates/dotagent-core/src/audit.rs`](../../crates/dotagent-core/src/audit.rs).

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
