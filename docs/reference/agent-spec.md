# Agent Spec

This document defines the contract between an agent and `dotagent`. If your
binary or script honors this contract, dotagent can schedule, run, monitor,
retry, and notify on it — regardless of the language it's written in.

## Layout

Every agent lives in a single directory. The minimum is:

```
my-agent/
  agent.toml      # manifest (this spec)
  agent.fish      # entry point (or `agent.py`, `agent.go`, a binary, ...)
```

The manifest tells dotagent **what** the agent is, **how** to run it, **when**
to run it, **what** to do on success/failure, and **which preflight checks**
to run before invoking it.

## Manifest — `agent.toml`

```toml
# Required: identity
[agent]
name = "my-agent"                       # unique within the dotagent root
description = "What this does."         # optional
monitor = true                          # default: true. false = excluded from `tick`
timeout_seconds = 1800                  # hard kill SIGTERM→SIGKILL; default 1800

# Required: how to run it
[run]
command = "fish"                        # the executable
args = ["./agent.fish"]                 # static args (the schedule's args are appended)
working_dir = "."                       # optional, relative to the manifest dir

# Optional: environment variable injection
[env]
inherit = true                          # default true — inherit parent env
extra = { LOG_LEVEL = "info" }          # added on top

# Optional: agent-wide defaults for retry/backoff/stale
[defaults]
max_retries = 3
retry_backoff_minutes = [5, 15, 30]
stale_after_minutes = 120

# Required: at least one schedule
[[schedules]]
id = "daily"                            # unique within this manifest
type = "cron"                           # "cron" | "interval" | "expression"
weekdays = [1, 2, 3, 4, 5]              # 0=Sun..6=Sat (matches launchd Weekday)
hours = [8]
minute = 30
args = ["--period", "dia-anterior"]     # appended to [run].args
# Per-schedule overrides (optional):
max_retries = 20
retry_backoff_minutes = [30]
stale_after_minutes = 240

[[schedules]]
id = "every-90"
type = "interval"
interval_minutes = 90
args = []

# Optional: preflight checks (run BEFORE the agent; abort if any fail)
[[preflight]]
plugin = "preflight-warp"
config = { connect_command = "warp-cli connect" }

# Optional: notifications (built into the daemon, no plugin subprocess)
[[notifiers]]
driver = "imessage"
to = "+5511999999999"
rate_limit_minutes = 60
events = ["attempt_failed", "given_up", "recovered"]  # empty = all events

[[notifiers]]
driver = "desktop"
title  = "finops-weekly"
sound  = true
events = ["given_up"]

# Optional: post-success sinks (still plugin-protocol)
[[on_success]]
plugin = "sink-file"
config = { path = "/tmp/last-success.txt", mode = "overwrite" }

# Optional: security intent (schema-only in v0 — see threat-model)
[security]
allowed_commands     = ["fish", "/usr/local/bin/fish"]
allowed_plugins      = ["preflight-warp", "sink-roam"]
network              = "allow"                      # or "deny" or ["api.github.com", "..."]
filesystem_writable  = ["/Users/me/reports"]        # $AGENT_TMPDIR / heartbeat always writable
env_passthrough      = ["PATH", "HOME", "LANG"]
```

### `[[notifiers]]` — built-in drivers

The `notifiers` array runs **in-process inside the daemon**, no subprocess.
Supported `driver` values: `desktop`, `slack`, `ntfy`, `pushover`, `imessage`
(macOS only — wraps `osascript`), and `plugin` (escape hatch to the legacy
plugin protocol). See [`docs/concepts/notifications.md`](../concepts/notifications.md)
for per-driver config schemas.

### `[[on_failure]]` / `[[on_success]]` — legacy plugin hooks

These two arrays still drive the plugin protocol (`dotagent-plugin-<name>`
binaries). They are reserved for **sink-style** hooks (persist output,
publish to Roam, etc.). For notifications, prefer `[[notifiers]]` — it's
faster, has fewer moving parts, and ships with the daemon.

### Schedule types

| Type         | When the OS scheduler fires                                                   |
|--------------|-------------------------------------------------------------------------------|
| `cron`       | weekday matches AND `(hour:minute)` matches (launchd `StartCalendarInterval`) |
| `interval`   | every `interval_minutes` (launchd `StartInterval`)                            |
| `expression` | free-form cron string (Linux only, via `systemd OnCalendar`)                  |

dotagent generates the launchd plist (macOS) or systemd `.timer` (Linux) from
the schedule. **dotagent never sleeps to wait for time** — the OS does it.

### Heartbeat slug

dotagent derives a slug from the schedule's `args` to namespace state files:

| `args`                              | slug                  |
|-------------------------------------|-----------------------|
| `[]`                                | `default`             |
| `["--period", "dia-anterior"]`      | `period_dia-anterior` |
| `["--mode", "unsubscribe"]`         | `mode_unsubscribe`    |

Rules: strip leading dashes, lowercase, replace non-alphanumeric with `_`,
collapse repeated `_`, trim trailing `_`. Empty input → `default`.

## Environment variables dotagent injects

When dotagent invokes an agent, it sets these variables (in addition to the
inherited parent environment, unless `env.inherit = false`):

| Variable               | Value                                                         |
|------------------------|---------------------------------------------------------------|
| `AGENT_NAME`           | manifest `agent.name`                                         |
| `AGENT_HOME`           | absolute path to the manifest directory                       |
| `AGENT_TMPDIR`         | freshly created tempdir, auto-cleaned after the run           |
| `AGENT_DRY_RUN`        | `"true"` or `"false"`                                         |
| `AGENT_SCHEDULE_ID`    | which schedule is firing (`daily`, `every-90`, ...)           |
| `AGENT_SLUG`           | derived heartbeat slug for this run                           |
| `AGENT_START_EPOCH`    | unix epoch seconds of `started_at`                            |
| `AGENT_ARGV`           | JSON array of the schedule's `args`                           |
| `AGENT_HEARTBEAT_FILE` | path to the heartbeat file (empty if `dry_run`)               |

The agent's positional arguments are `args` from `[run]` followed by `args`
from the schedule.

## Heartbeat & state

dotagent writes a heartbeat file before and after every (non-dry-run) execution:

```
~/.config/dotagent/state/agents/{name}/{slug}.heartbeat.json
```

Shape:

```jsonc
{
  "name": "my-agent",
  "slug": "default",
  "args": [],
  "started_at": 1700000000,
  "started_at_iso": "2023-11-14T22:13:20+0000",
  "finished_at": 1700000100,
  "finished_at_iso": "2023-11-14T22:15:00+0000",
  "exit_code": 0,
  "duration_seconds": 100,
  "last_success_at": 1700000100,         // preserved across runs; never overwritten on failure
  "last_success_at_iso": "2023-11-14T22:15:00+0000"
}
```

This shape is intentionally compatible with the legacy
`~/.local/state/agents/{name}/{slug}.heartbeat.json` written by the
Fish-based `agent_init` so existing tools that read it keep working during the
migration.

Window state (one file per `(agent, schedule, expected_at)`):

```
~/.config/dotagent/state/windows/{name}-{slug}-{YYYY-MM-DD-HHMM}.json
```

## Health states

For each `(agent, schedule)` dotagent computes one of:

| State      | Meaning                                                                          |
|------------|----------------------------------------------------------------------------------|
| `ok`       | `last_success_at >= expected_at` and no retries needed in the current window     |
| `degraded` | recovered after `attempts > 0`, OR interval-style overdue but within `2 * interval` |
| `failing`  | window passed without success; retrying or already given up                      |
| `stale`    | never ran, OR window older than `stale_after_minutes`                            |

## `[security]` — schema-only in v0

Declares the agent's intended blast radius. dotagent **parses** these
fields and `doctor` warns when an agent has no `[security]` block, but
the runner does **NOT** yet enforce them. Sandbox integration
(`sandbox-exec` / `bwrap` / `firejail`) lands as a follow-up — see
[`docs/security/threat-model.md`](../security/threat-model.md).

Declaring intent today still has value: it forces the agent author to
think through the surface area, surfaces it for review, and gives
`doctor` something to audit.

| Field                 | Type                                    | Default          | Meaning                                                                  |
|-----------------------|-----------------------------------------|------------------|--------------------------------------------------------------------------|
| `allowed_commands`    | `string[]`                              | `[]` (no whitelist) | Commands the agent is allowed to spawn. Empty = no enforcement.        |
| `allowed_plugins`     | `string[]`                              | `[]`             | Plugin names this agent may invoke. Empty = `[[preflight]]` / `[[on_*]]` plugins implicitly allowed. |
| `network`             | `"allow"` / `"deny"` / `string[]`       | `"allow"`        | Network policy. `"allow"` / `"deny"` are modes; an array of strings is a hostname allow-list. |
| `filesystem_writable` | `string[]`                              | `[]`             | Directories the agent may write to. Empty = unrestricted. `$AGENT_TMPDIR` and `$AGENT_HEARTBEAT_FILE` are **always** writable. |
| `env_passthrough`     | `string[]`                              | `[]`             | Env vars to pass through. Empty = full inheritance (matches `EnvConfig::inherit = true`). |

### Examples

**Minimal — document intent only**:

```toml
[security]
network = "allow"     # explicit
```

This silences the `⚠ no [security] section` warning from `doctor`
without changing behavior.

**Tighter — hostname allow-list**:

```toml
[security]
allowed_commands = ["python3"]
allowed_plugins  = ["sink-roam", "preflight-warp"]
network          = ["api.github.com", "acme.sentry.io"]
filesystem_writable = ["/Users/me/reports"]
```

When sandbox enforcement lands, this manifest forbids the agent from
spawning anything other than `python3`, talking to any host outside
the allow-list, writing outside `/Users/me/reports`, or invoking any
plugin other than `sink-roam` / `preflight-warp`.

**Network deny**:

```toml
[security]
network = "deny"
```

The agent will be denied any outbound network (when enforcement
lands). Useful for pure local processors.

## Plugin events

When dotagent fires `on_failure` / `on_success`, the `event` field is one of:

- `attempt_failed` — a retry attempt failed; will retry again
- `given_up` — retries exhausted
- `recovered` — a previously-failing window succeeded
- `success` — a normal successful run
- `timed_out` — agent killed for exceeding `agent.timeout_seconds`
- `preflight` — a preflight plugin returned `ok = false` and aborted the run
- `daily_summary` — daemon's internal 22:45 health roll-up (also surfaced via `dotagent daily-summary`)

A plugin can filter via its manifest entry's `events` array.

## Manifest hash & drift detection

dotagent caches `sha256(agent.toml)` for every loaded manifest at
`state/known_manifests.json`. On the next load:

- New `agent.name` not in the cache → `phantom_agent_detected` audit
  event + out-of-band notify (`Critical` severity). The agent still
  runs by default — see
  [V2 in the threat model](../security/threat-model.md#v2--phantom-agent).
- Existing name but mismatched sha → `manifest_drift_detected` audit
  event + out-of-band notify (`Critical` severity). The agent uses the
  current on-disk manifest.

Every `agent_run` audit entry records the manifest sha256 used, so
forensic reconstruction can correlate runs with a specific manifest
revision.

## What dotagent does NOT do

- **Run the agent's business logic.** dotagent is a scheduler+monitor; the agent
  is an independent process.
- **Provide an SDK.** No client library is required. Read env vars, write
  stdout/stderr, exit with a code. That's it.
- **Wait for the schedule.** The OS scheduler (launchd / systemd) fires the
  trigger; dotagent only computes "did the expected window succeed?" during
  `tick`.
