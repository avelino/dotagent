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
                                        # persistent mode: deadline for ONE request

# Required: how to run it
[run]
command = "fish"                        # the executable
args = ["./agent.fish"]                 # static args (the schedule's args are appended)
working_dir = "."                       # optional, relative to the manifest dir
protocol = "assistant-v1"                 # optional stdout protocol for triggered delivery

# Optional: environment variable injection
[env]
inherit = true                          # default true — inherit parent env
extra = { LOG_LEVEL = "info" }          # added on top

# Optional: how long one process lives. Absent = "oneshot", the shape every
# agent had before this section existed. See docs/concepts/lifecycle.md.
[lifecycle]
mode = "persistent"                     # "oneshot" (default) | "persistent"
key = "chat_id"                         # payload field deciding which instance
                                        # answers; absent = one instance total
idle_timeout_seconds = 1800             # recycle after this long with no request
max_invocations = 120                   # recycle after this many; 0 = unlimited
max_instances = 8                       # ceiling; past it, LRU is evicted
startup_timeout_seconds = 30            # window to answer the `hello` handshake

# Optional: agent-wide defaults for retry/backoff/stale
[defaults]
max_retries = 3
retry_backoff_minutes = [5, 15, 30]
stale_after_minutes = 120

# Optional: schedules. Trigger-only agents may declare none.
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
on_battery = "defer"                    # "run" (default) | "defer"
                                        # overrides [power] in config.toml for
                                        # this schedule only

# Optional: preflight checks (run BEFORE the agent; abort if any fail)
[[preflight]]
plugin = "preflight-warp"
config = { connect_command = "warp-cli connect" }
# Optional: what clears this check. Declaring it publishes one MCP tool,
# `remediate-<agent>-<plugin>`, so an assistant can offer to fix it instead
# of only reporting it. Split on whitespace into argv — there is no shell.
#
# The plugin's own `suggest` string is never executable: a command a plugin
# wrote, run from a chat message, is arbitrary execution. This is the
# operator saying it, in a file under review. See threat model V12.
# remediation = "warp-cli connect"
# Optional per-hook deadline (seconds). Default: 30s for preflight,
# 300s for on_success/on_failure invocations. The supervisor kills the
# whole process group (TERM → 5s grace → KILL) when exceeded.
# timeout_seconds = 60

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
# timeout_seconds = 600                # override default 300s for slow sinks

# Optional: security intent (schema-only in v0 — see threat-model)
[security]
allowed_commands     = ["fish", "/usr/local/bin/fish"]
allowed_plugins      = ["preflight-warp", "sink-roam"]
network              = "allow"                      # or "deny" or ["api.github.com", "..."]
filesystem_writable  = ["/Users/me/reports"]        # $AGENT_TMPDIR / heartbeat always writable
env_passthrough      = ["PATH", "HOME", "LANG"]
```

### `[lifecycle]` — how long one process lives

Absent means `mode = "oneshot"`: spawn, run, exit, one process per event. That
is the default and it is not going to change.

`mode = "persistent"` keeps the process alive between runs and delivers
requests over [JSON lines](persistent-protocol.md). The agent's own shape
changes with it — it reads requests and writes answers on a loop instead of
running once — so this is opt-in per manifest and never inferred.

| Field | Default | Meaning |
|---|---|---|
| `mode` | `"oneshot"` | `"oneshot"` or `"persistent"`. Anything else is a parse error, not a silent fallback. |
| `key` | *(none)* | Trigger-payload field that decides which instance answers. Absent = one instance for the whole agent. |
| `idle_timeout_seconds` | `1800` | Recycle after this long with no request. Must be > 0. |
| `max_invocations` | `120` | Recycle after this many requests. `0` = unlimited. |
| `max_instances` | `8` | Ceiling on live instances. Past it, the least recently used is terminated. Must be ≥ 1. |
| `startup_timeout_seconds` | `30` | Window to answer the `hello` handshake. Must be below `[agent] timeout_seconds` — a handshake that outlasts the request deadline means the first message can never land, and `doctor` refuses the manifest. |

Two things change meaning in this mode:

- **`[agent] timeout_seconds` is the deadline for one request**, not the
  lifetime of the process. Exceeding it recycles the instance.
- **`AGENT_TRIGGER_*` is not injected.** Those variables describe a single
  message and the process outlives many; the context rides in each request
  frame instead. `AGENT_LIFECYCLE` and `AGENT_PERSIST_KEY` are added — see
  [env-vars](env-vars.md).

Full reasoning in [Lifecycle](../concepts/lifecycle.md).

### `[run].protocol` — assistant stdout

The optional `protocol` field declares the stdout shape used by an assistant
dispatcher. The only supported value is `"assistant-v1"`:

```toml
[run]
command = "bash"
args = ["./agent.sh"]
protocol = "assistant-v1"
```

An `assistant-v1` agent emits one JSON object per stdout line with `type` set
to `delta`, `reply`, or `session`. The gateway forwards every raw stdout line
to a local client as `reply.delta`, and uses the last `reply` frame to shape the
final `reply` event. `session` is agent bookkeeping only; dotagent does not own
or persist the assistant transcript.

The value is validated when the manifest loads. Unknown protocol values fail
manifest validation instead of producing a run that the client cannot decode.
`assistant-v1` is the one-shot assistant stdout protocol, not the persistent
agent request/response protocol and not a version of the local Unix-socket API.
See [Local Client API](local-api.md).

### `[[notifiers]]` — built-in drivers

The `notifiers` array runs **in-process inside the daemon**, no subprocess.
Supported `driver` values: `desktop`, `slack`, `ntfy`, `pushover`,
`telegram`, `imessage` (macOS only — wraps `osascript`), and `plugin`
(escape hatch to the legacy plugin protocol). See
[`docs/concepts/notifications.md`](../concepts/notifications.md) for
per-driver config schemas.

Credential-bearing notifier fields accept `${VAR}`, resolved at send time
against the daemon-loaded secrets file (see
[`docs/concepts/secrets.md`](../concepts/secrets.md)), with `std::env` as
fallback: `slack.webhook_url`, `ntfy.token` / `base_url` / `topic`,
`pushover.token` / `user`, and `telegram.bot_token`. An unresolved
reference **fails the send** — there is no fall-through to the literal
`${…}` string.

`telegram.chat_id` and `imessage.to` are not expanded. They are addresses,
not credentials; write them literally.

### `[[on_failure]]` / `[[on_success]]` — legacy plugin hooks

These two arrays still drive the plugin protocol (`dotagent-plugin-<name>`
binaries). They are reserved for **sink-style** hooks (persist output,
publish to Roam, etc.). For notifications, prefer `[[notifiers]]` — it's
faster, has fewer moving parts, and ships with the daemon.

| Field             | Type                | Default | Notes                                                                                  |
|-------------------|---------------------|---------|----------------------------------------------------------------------------------------|
| `plugin`          | `string`            | —       | Short name; resolved to `dotagent-plugin-<plugin>` via the plugin client.              |
| `config`          | `object`            | `{}`    | Opaque JSON forwarded to the plugin's `invoke` verb.                                   |
| `events`          | `string[]`          | `[]`    | Filter (empty = all). For `on_failure`: `attempt_failed`, `given_up`, `stale`, `recovered`. |
| `timeout_seconds` | `integer` (>0)      | `300`   | Per-hook deadline. Supervisor kills the process group on overrun. Same field on `[[preflight]]` (default `30`). |

### Schedule types

| Type         | When a window is due                                                          |
|--------------|-------------------------------------------------------------------------------|
| `cron`       | weekday matches AND `(hour:minute)` matches                                   |
| `interval`   | every `interval_minutes`, anchored on the last success                        |
| `expression` | free-form cron string (**not yet implemented** — parses, never fires)         |

A **single** daemon (`run.avelino.dotagent`) owns every schedule. There is no
per-agent plist and no per-agent systemd timer: launchd / systemd start the
daemon, and the daemon computes the next event across all schedules and sleeps
until then. See [`daemon-lifecycle.md`](../guides/daemon-lifecycle.md).

#### `on_battery`

| Value     | Effect                                                            |
|-----------|-------------------------------------------------------------------|
| `run`     | Dispatch regardless of power source. The default.                 |
| `defer`   | Hold the run until the machine is back on mains power.            |

Absent, the schedule inherits `[power] on_battery` from `config.toml`
(itself `run` by default, so an untouched install is unaffected). Declared
here, it wins — the cost of a run is a property of the schedule, so an agent
can keep a cheap hourly check running unplugged while its expensive
15-minute sync waits for a charger.

Deferring does not queue: the window that fires when the charger goes back in
is the *current* one, not a backlog. The check runs after the staleness check,
so a window that ages past `stale_after_minutes` while unplugged is dropped
rather than run late. Full semantics — including the global
`min_battery_percent` floor, which is not overridable per schedule — in
[`config-reference.md`](../guides/config-reference.md).

#### How `interval` windows advance

An interval schedule is an arithmetic sequence of ticks anchored on the last
**success** — `last_success`, `+ interval`, `+ 2 · interval`, … — and the
window currently due is the greatest tick at or before now.

Anchoring on success is deliberate: a run that succeeds re-phases the sequence,
so a 90-minute agent that ran at 10:07 is next due at 11:37 rather than on some
fixed wall-clock grid.

The sequence keeps advancing **while runs fail**, exactly like the calendar
keeps producing new cron windows. This matters more than it sounds. A frozen
`last_success + interval` window is what deadlocked a failing interval agent:
its single window aged past `stale_after_minutes`, so every tick was skipped,
so `last_success` never advanced, so the window could never move — one observed
agent sat dead for 55 days. A rolling window means the agent keeps being
dispatched no matter how long it has been broken.

Health is measured against a **different** window: the *first* one missed after
the last success, not the rolling one. Judging staleness against a window that
rolls forward would report a chronically failing agent as a fresh failure
forever. So dispatch stays alive and `dotagent status` still says `stale`.

`interval_minutes` must be greater than zero. A value of `0` is rejected during
manifest validation instead of reaching the scheduler.

### Heartbeat slug

dotagent derives a slug from the schedule's `args` to namespace state files:

| `args`                              | slug                  |
|-------------------------------------|-----------------------|
| `[]`                                | `default`             |
| `["--period", "dia-anterior"]`      | `period_dia-anterior` |
| `["--mode", "unsubscribe"]`         | `mode_unsubscribe`    |

Rules: strip leading dashes, lowercase, replace non-alphanumeric with `_`,
collapse repeated `_`, trim trailing `_`. Empty input → `default`.

Triggered runs use a source namespace instead of schedule args. Without a
session, the slug keeps the existing `trigger-<source>` form:

| Source | No session | With `session_id = "chat-9_a"` |
|---|---|---|
| `telegram` | `trigger-telegram` | `trigger-telegram-chat-9_a` |
| `local` | `trigger-local` | `trigger-local-chat-9_a` |
| `mcp` | `trigger-mcp` | `trigger-mcp-chat-9_a` |
| `cli` | `trigger-cli` | `trigger-cli-chat-9_a` |

The session part is sanitized before it reaches a filename. The local API uses
the effective session `default` when `message.send` omits `session_id`, so a
local API request without an explicit session is associated with
`trigger-local-default`.

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

### `LANG`

Set to a UTF-8 locale (`en_US.UTF-8` on macOS, `C.UTF-8` elsewhere) **only
when neither `LANG` nor `LC_ALL` reaches the agent** — an inherited locale is
never overridden, and `[env.extra]` wins over both.

This is a default, not a policy. launchd and systemd start a daemon with no
locale at all, and a process in the resulting `C` locale has `MB_CUR_MAX == 1`:
it reads every **byte** of an environment variable as one character (Latin-1)
and writes it back out as UTF-8. A `AGENT_TRIGGER_PAYLOAD` carrying `é` reaches
such an agent as `Ã©`, still valid UTF-8, so nothing errors and the agent simply
acts on mangled text. Verified with fish 3.7.1, which round-trips `c3 a9` as
`c3 83 c2 a9` under `C` and unchanged under `en_US.UTF-8`.

Name your own if you need a different one:

```toml
[env.extra]
LANG = "pt_BR.UTF-8"
```

Runs started by a [trigger](../concepts/triggers.md) rather than a schedule get
additional context, applied *before* the block above so a payload can never redefine
`AGENT_NAME` or `AGENT_HEARTBEAT_FILE`:

| Variable                 | Value                                                    |
|--------------------------|----------------------------------------------------------|
| `AGENT_TRIGGER_SOURCE`   | `telegram`, `local`, `mcp`, `cli`                          |
| `AGENT_TRIGGER_ACTOR`    | who asked, as the source can attest it                    |
| `AGENT_TRIGGER_REPLY_TO` | opaque handle for the conversation to answer              |
| `AGENT_TRIGGER_PAYLOAD`  | source-specific JSON body                                 |
| `AGENT_SESSION_ID`       | opaque conversation id, when the trigger has one           |

`AGENT_SESSION_ID` is not a transcript or a Claude session id. It is an opaque
key supplied by the trigger source. The local client API validates it against
`^[A-Za-z0-9_-]{1,64}$`; the agent remains responsible for deciding whether to
persist conversation state under that key. See [Local Client API](local-api.md).

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
| `degraded` | the window succeeded, but only after at least one **failed** attempt             |
| `failing`  | window passed without success; retrying or already given up                      |
| `stale`    | never ran, OR window older than `stale_after_minutes`                            |

`attempts` in the window file counts **dispatches**, not retries — the daemon
bumps it on every dispatch including the one that succeeded. So a window that
worked on the first try lands on disk as `attempts: 1` with
`last_attempt_exit_code: 0`, and that is `ok`, not `degraded`. Only attempts
that actually failed make a recovered window `degraded`.

For `stale`, an interval schedule is judged against the first window it missed
after its last success — not the rolling window dispatch uses. See
[How `interval` windows advance](#how-interval-windows-advance).

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
- `given_up` — retries exhausted; repeated on a rising ladder while it holds
- `stale` — the schedule stopped running at all (window aged past
  `stale_after_minutes`, so nothing is even attempted). Delivered on the
  `given_up` channel when no entry lists `stale` explicitly
- `recovered` — a previously-failing window succeeded
- `success` — a normal successful run
- `timed_out` — agent killed for exceeding `agent.timeout_seconds`
- `preflight` — a preflight plugin returned `ok = false` and aborted the run
- `daily_summary` — the daemon's daily health roll-up, delivered at `[daily_summary].time` (default 22:45 local; also surfaced via `dotagent daily-summary`)

A plugin can filter via its manifest entry's `events` array — with one
exception. `daily_summary` belongs to the daemon rather than to any
agent, so it only ever reaches `[[daily_summary.notifiers]]` in
[`config.toml`](../guides/config-reference.md#daily_summary); listing it
in a manifest matches nothing.

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

- **Run the agent's business logic.** dotagent is a scheduler, supervisor, and
  trigger harness; the agent
  is an independent process.
- **Own conversation state.** Trigger `session_id` values are opaque and are
  passed through as `AGENT_SESSION_ID`. Transcripts, LLM state, and persistence
  belong to the agent.
- **Provide a network API.** The current local client API is a user-local Unix
  socket, not HTTP or TCP. It is enabled only when the configured dispatcher is
  discovered; see [Local Client API](local-api.md).
- **Provide an SDK.** No client library is required. Read env vars, write
  stdout/stderr, exit with a code. That's it.
- **Wait for the schedule.** The OS scheduler (launchd / systemd) fires the
  trigger; dotagent only computes "did the expected window succeed?" during
  `tick`.
