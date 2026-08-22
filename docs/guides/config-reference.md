# Configuration Reference

> Schema for `~/.config/dotagent/config.toml`. Every field is optional —
> dotagent ships with sensible defaults out of the box.

```text
$DOTAGENT_HOME/config.toml         # default: ~/.config/dotagent/config.toml
```

If this file is **missing**, dotagent uses the baked-in defaults
(spelled out below). You only need a `config.toml` to:

| Want to | Section |
|---|---|
| Bump verbosity, change log retention | [`[logging]`](#logging) |
| Export traces to Honeycomb / Tempo / Datadog | [`[telemetry]`](#telemetry) |
| Keep secrets somewhere other than the default path | [`[secrets]`](#secrets) |
| **Talk to your agents from Telegram** | [`[telegram]`](#telegram) |
| **Move or disable long-term memory** | [`[memory]`](#memory) |
| Retime the daily health summary, or send it somewhere other than the desktop | [`[daily_summary]`](#daily_summary) |
| **Stop agents from running on battery** | [`[power]`](#power) |

Two of those differ in kind. `[logging]`, `[telemetry]` and `[memory]` tune
something that already works; `[telegram]` **turns on** a path that does not
exist otherwise, and it is the one section that changes the threat model —
inbound messages mean untrusted input from the internet can cause a local
process to run.

There is no required field. Anything you don't write falls back to the
default.

After editing, run `dotagent reload` — the daemon picks up changes on
the next tick.

---

## Full example (every field)

```toml
# ~/.config/dotagent/config.toml

[logging]
level = "info"                     # off | error | warn | info | debug | trace
format = "json"                    # json | pretty | compact
retention_days = 30                # daemon logs older than this are deleted
per_agent_retention_days = 14      # agent logs (noisier; shorter horizon)
compress_after_days = 1            # rotated files older than N days → gzip

[state]
window_retention_days = 30         # state/windows/ files older than this are deleted

[telemetry]
otlp_endpoint = ""                 # empty = OTel disabled (default)
protocol = "grpc"                  # grpc | http
service_name = "dotagent"

[telemetry.headers]
# Vendor-specific auth headers. Sent on every OTLP request.
# OTEL_EXPORTER_OTLP_HEADERS env var wins over this table.
"x-honeycomb-team" = "your-api-key"

[telemetry.resource]
# Resource attributes attached to every span/log.
"deployment.environment" = "production"
"host.name" = "workstation-01"

[secrets]
file = ""                          # empty = default path or DOTAGENT_SECRETS_FILE

[telegram]
bot_token = "${TELEGRAM_BOT_TOKEN}"  # empty = inbound Telegram off (default)
allowed_user_ids = [123456789]       # numeric ids; empty = nobody
dispatcher_agent = "telegram-assistant"
poll_timeout_seconds = 30            # long-poll hold, capped at 50 by Telegram
rate_limit_per_minute = 10           # accepted messages per sender

[memory]
enabled = true                       # default; false removes the memory tools
workspace = ""                       # empty = $DOTAGENT_HOME/outl

[skills]
enabled = true                       # default; false removes the skill tools
claude_skills = true                 # default; also read ~/.claude/skills
paths = []                           # extra roots, searched before the defaults

[commands]
enabled = true                       # default; false removes the menu and tools
claude_commands = false              # default; opt in to ~/.claude/commands
paths = []                           # extra roots, searched before the defaults

[power]
on_battery = "run"                   # default; run | defer
min_battery_percent = 0              # default; 0 = charge is never consulted

[daily_summary]
enabled = true                       # default; governs the daemon's fire only
time = "22:45"                       # default; local HH:MM or HH:MM:SS
grace_minutes = 30                   # default; how late a wake-up still delivers

[[daily_summary.notifiers]]
# Same shape as a manifest's [[notifiers]]. None declared = desktop.
driver = "telegram"
bot_token = "${TELEGRAM_BOT_TOKEN}"
chat_id = "123456789"
```

---

## `[telegram]`

Inbound Telegram. **Off unless configured** — dotagent opens no inbound path
you did not ask for. Outbound notifications are unrelated and live in the
manifest's `[[notifiers]]`; see [Notifications](../concepts/notifications.md).

| Field | Default | Meaning |
|---|---|---|
| `bot_token` | `""` | Bot API token. Accepts `${VAR}`, resolved against the secrets store at poll time. |
| `allowed_user_ids` | `[]` | Numeric Telegram user ids allowed to trigger runs. |
| `dispatcher_agent` | `"telegram-assistant"` | Agent every accepted message is handed to. |
| `poll_timeout_seconds` | `30` | Seconds to hold `getUpdates` open. Telegram caps this at 50. |
| `rate_limit_per_minute` | `10` | Accepted messages per sender per minute. |

The ingress starts only when `bot_token` **and** at least one entry in
`allowed_user_ids` are present. A token with an empty allowlist stays off and
says so in the log: reading empty as "no restriction" would turn one forgotten
line into an open remote-execution endpoint.

This section is daemon-level rather than per-manifest because Telegram allows
exactly one `getUpdates` consumer per bot token. N manifests each polling would
compete for the same offset and silently drop each other's messages.

Enabling this changes the threat model — a message from the public internet can
cause a local process to run. Read
[V8 in the threat model](../security/threat-model.md) before turning it on, and
[Telegram](../concepts/telegram.md) for the full setup.

---

## `[memory]`

Long-term memory for agents, stored in an embedded [outl](https://github.com/avelino/outl) workspace. On by default.

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Expose the `memory-*` tools from `dotagent mcp`. |
| `workspace` | `""` | Workspace path. Empty resolves to `$DOTAGENT_HOME/outl`. |

The default workspace is scaffolded when the daemon starts, so memory works without writing any config. A **configured** path is never scaffolded: a typo there must fail loudly rather than create an empty workspace nobody will look at. `dotagent doctor` reports which case you are in.

Pointing this at a workspace you already use puts what an agent remembers next to your own notes, synced to your peers. Convenient, and also means an agent writes where you write.

> **Not the same `[memory]` as `agent.toml`.** This section says *where* the workspace lives and whether memory exists at all. The per-agent section of that name says whether *that agent* writes to it — see [`[memory]` in the agent spec](../reference/agent-spec.md#memory--memory-capture-for-a-plain-agent-opt-in).

Full behavior in [Memory](../concepts/memory.md).

---

## `[skills]`

Procedures exposed to MCP clients as `skill-*` tools. On by default, with an empty catalog that costs nothing.

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Expose `skill-*`, `skill-read` and `skill-run` from `dotagent mcp`. |
| `claude_skills` | `true` | Also search `~/.claude/skills/` and `$CWD/.claude/skills/`. |
| `paths` | `[]` | Extra roots, each holding one subdirectory per skill. Searched **first**, so a name declared here overrides one found later. |

`claude_skills` defaults to on because the skills worth exposing are usually already written for Claude Code, and requiring a copy would mean two versions drifting apart. Turn it off when that catalog is large and mostly irrelevant to an assistant that has no shell.

---

## `[commands]`

Procedures **you** invoke by name, published as a Telegram menu and resolved through `command-get`. On by default, with an empty catalog that costs nothing. See [Commands](../concepts/commands.md).

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Register the `/` menu and expose `command-get` / `command-list`. |
| `claude_commands` | `false` | Also search `~/.claude/commands/` and `$CWD/.claude/commands/`. |
| `paths` | `[]` | Extra roots, each holding one `.md` file per command. Searched **first**, so a name declared here overrides one found later. |

`claude_commands` defaults to **off**, unlike `claude_skills`. A skill costs a line in a list until a model judges it relevant; a command is published as a menu, and a Claude Code catalog is typically full of things that assume a shell and a working directory. Menu entries that cannot work are worse than absent ones. Turn it on when the catalog was written for an assistant rather than a terminal.

Use `paths` for a bundle that keeps its skills one level down — discovery does not walk recursively:

```toml
[skills]
paths = ["/Users/me/.claude/skills/radar/skills"]
```

`dotagent doctor` reports how many skills were found, which failed to parse, and any two names that collapse to the same tool name.

Full behavior in [Skills](../concepts/skills.md).

---

## `[daily_summary]`

The end-of-day health roll-up: one line per unhealthy
`(agent, schedule)`, healthy ones collapsed into a count. **On by
default** — nothing here is required to receive it.

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Deliver on the daemon's schedule. `false` stops the nightly fire and the wake-up it schedules. |
| `time` | `"22:45"` | Local time of day, `HH:MM` or `HH:MM:SS`. |
| `grace_minutes` | `30` | How long after `time` a delivery still counts. Clamped to `[1, 1440]`. |
| `notifiers` | `[]` | Array of tables, same shape as a manifest's `[[notifiers]]`. Empty = the `desktop` driver. |

```toml
[daily_summary]
time = "07:30"          # a morning report reads better than a bedtime one
grace_minutes = 60      # laptop opens late; still deliver

[[daily_summary.notifiers]]
driver = "telegram"
bot_token = "${TELEGRAM_BOT_TOKEN}"
chat_id = "123456789"
```

Declaring more than one entry delivers to all of them, `driver = "plugin"`
included. See [Notifications](../concepts/notifications.md) for every
driver and its fields.

### Why the defaults are what they are

**No notifier means `desktop`, not silence.** Every other driver needs
a chat id, a phone number or a webhook, and there is no universal
default for those. `desktop` is the only one with nothing to fill in
and nothing to leak — no credential, no network, nothing leaving the
machine. Delivering only when configured is how this feature spent its
early life: it fired nightly at a constant nobody owned and left no
trace when the message went nowhere.

**`[telegram]` is not used as a destination.** That section is
*ingress* — the bot that accepts your messages. Wiring it to *egress*
would send a nightly report to anyone who set up a bot to talk to their
agents and never asked for one.

**A `time` that does not parse falls back to `22:45`** rather than
disabling delivery. A typo should cost you the wrong hour, not a silent
month. `grace_minutes = 0` becomes `1` for the same reason: a
zero-width window is an empty one.

**An `events` filter inside `[[daily_summary.notifiers]]` is ignored.**
The list is already scoped to a single event, so a filter there could
only subtract. Entries get copied out of manifests, and
`events = ["given_up"]` riding along would match nothing and drop the
summary without a word.

### When it actually fires

The daemon schedules a **wake-up for `time`**, so delivery does not
depend on some other schedule happening to be due nearby.
`grace_minutes` covers the case where the wake-up could not happen at
all — machine asleep, machine off, a tick that overran its own sleep
budget. It fires once per window; re-entering it (a `dotagent reload`,
say) does not double-send.

`enabled = false` does **not** block `dotagent daily-summary` typed by
hand — that flag governs the daemon, and someone who ran the command
asked for that one. Each delivery is audited as `plugin_invoked` with
`plugin: "notifier:<driver>"`, failures included.

Full command reference in
[`cli.md`](../reference/cli.md#daily-summary).

---

## `[secrets]`

Override the path to the daemon-loaded secrets file. The default
(empty `file`) resolves to `$DOTAGENT_HOME/secrets.env`, with the
`DOTAGENT_SECRETS_FILE` env var as second-tier override.

| Field   | Type   | Default | Notes                                                                 |
|---------|--------|---------|-----------------------------------------------------------------------|
| `file`  | string | `""`    | Absolute path to the `KEY=VALUE` secrets file. Must be mode `0600`.   |

See [`concepts/secrets.md`](../concepts/secrets.md) for the file
format, posture, and which notifier configs honor `${VAR}` today.

```toml
[secrets]
file = "/run/secrets/dotagent.env"   # populated by a secret manager
```

---

## `[logging]`

Controls dotagent's own operational logs — the daemon's tracing output
under `logs/daemon/dotagent.log` and the per-agent rotated files under
`logs/agents/<name>/<name>.log`.

| Field                       | Type   | Default    | Valid values                                                                 |
|-----------------------------|--------|------------|------------------------------------------------------------------------------|
| `level`                     | string | `"info"`   | `off`, `error`, `warn`, `info`, `debug`, `trace`                              |
| `format`                    | string | `"json"`   | `json`, `pretty`, `compact`. **File output is always JSON regardless** — this controls the stderr stream the daemon writes for launchd/systemd to capture. |
| `retention_days`            | uint   | `30`       | Days to keep daemon logs (`logs/daemon/`).                                    |
| `per_agent_retention_days`  | uint   | `14`       | Days to keep per-agent logs (`logs/agents/<name>/`).                          |
| `compress_after_days`       | uint   | `1`        | Rotated files older than N days are gzipped in-place.                         |

### `level` semantics

Same as the `RUST_LOG` env-var grammar — but here you set a **single
filter** that applies to all targets. Per-target tuning is only
available via env var.

```toml
[logging]
level = "debug"          # → everything at debug level
```

Override transiently:

```bash
RUST_LOG=info,dotagent_runner=trace,dotagent_state=debug dotagent daemon
```

`RUST_LOG` wins when both are set.

### Retention behavior

A daily sweep at **03:00 local time** (single-shot per day):

1. Walks `logs/daemon/` and every `logs/agents/<name>/`.
2. Rotated files older than `compress_after_days` → gzipped in-place.
3. Rotated files older than `retention_days` (daemon) or
   `per_agent_retention_days` (agents) → deleted.

Both horizons apply to **rotated** files only. The active log is never
compressed and never deleted, whatever its age: launchd and systemd hold an
open fd on it, so unlinking it would strand every subsequent write.

The same 03:00 pass also sweeps `state/windows/` — see
[`[state]`](#state).

The audit log (`audit.log`) is **never** swept regardless of these
settings — by design. See
[`observability.md`](observability.md#audit-log-vs-operational-log).

---

## `[state]`

Retention for what dotagent writes under `state/`. **Nothing to
configure** — the default already bounds the only directory that grows
without limit.

| Field                   | Type | Default | Valid values                                                        |
|-------------------------|------|---------|---------------------------------------------------------------------|
| `window_retention_days` | uint | `30`    | Days to keep `state/windows/`. `0` disables the sweep entirely.      |

A schedule writes one window file per fired window and never revisits
it, so an agent on a 15-minute interval leaves ~96 files a day behind —
a `.json` plus the `.lock` next to it. Left alone, that directory grows
forever.

The nightly sweep deletes each aged-out window together with its
`.lock`, and skips any window a writer currently holds the lock on.
Windows are deleted, never gzipped: the daemon reads them as JSON.

### Why 30 days

The horizon has to clear the oldest window the daemon might still
consult. A window stops being actionable once it is older than the
schedule's [`stale_after_minutes`](../reference/agent-spec.md) (default
120), which 30 days exceeds by ~360×. Even a wildly permissive
`stale_after_minutes` of a full week still leaves four days of headroom.

Erring high costs a few MB. Erring low deletes retry state under a
running daemon, which resets `attempts` and re-fires an alert someone
already gave up on — so widen it freely, narrow it carefully:

```toml
[state]
window_retention_days = 90   # keep a quarter of retry history
```

Heartbeats are deliberately not covered: there is exactly one per
`(agent, schedule)` and it is rewritten in place, so
`state/agents/` is bounded by how many schedules exist.

---

## `[power]`

Whether a due run happens while the machine is on battery. **Off by
default** — dotagent dispatches exactly as it always did until you opt
in, and never probes the power source when the settings can't defer
anything.

| Field                  | Type   | Default | Valid values                                                        |
|------------------------|--------|---------|---------------------------------------------------------------------|
| `on_battery`           | string | `run`   | `run` dispatches regardless. `defer` holds runs until mains power.  |
| `min_battery_percent`  | uint   | `0`     | Defer below this charge whatever `on_battery` says. `0` disables.   |

The problem it solves is specific to laptops. A daemon that sleeps
between events costs nothing, but the *agents* it wakes up to run are
not free: a 15-minute interval agent fires 96 times a day whether the
machine is plugged in at a desk or in a bag at 12%.

```toml
[power]
on_battery = "defer"        # nothing scheduled runs on battery
min_battery_percent = 20    # ...and never below 20%, even if you set "run"
```

The two rules are independent. `min_battery_percent` is the common
case on its own: agents are welcome to run on battery in general, just
not when there is nearly none left.

```toml
[power]
min_battery_percent = 20    # on_battery stays "run"
```

### Deferring does not queue

A deferred run is not stored and replayed. Both schedule kinds resolve
to the *current* window rather than a backlog, so an agent deferred
across four hours of battery runs **once** when the charger goes in —
not sixteen times. This is the behavior that makes `defer` safe to set
on an aggressive interval.

The check sits after the staleness check, so a window that ages past
[`stale_after_minutes`](../reference/agent-spec.md) while on battery is
dropped rather than run hours late. That is the same call staleness
always makes; if you want a deferred agent to survive a long unplugged
stretch, widen its `stale_after_minutes`.

### Per-schedule override

`[power]` is the default. Any schedule can override it, because the
cost is per-schedule — an agent can keep a cheap hourly check running
on battery while its expensive every-15-minutes sync waits for a
charger:

```toml
[[schedules]]
id = "every-15min"
type = "interval"
interval_minutes = 15
on_battery = "defer"        # overrides [power] for this schedule only
```

See [`agent-spec.md`](../reference/agent-spec.md) for the field in
context. `min_battery_percent` is deliberately **not** overridable: "the
battery is nearly empty" is a fact about the machine, not about one
schedule's appetite.

### Detection

| Platform | Probe                                                     |
|----------|-----------------------------------------------------------|
| macOS    | `pmset -g batt`                                           |
| Linux    | `/sys/class/power_supply/*` (`type`, `online`, `capacity`) |
| Other    | undetectable                                              |

An undetectable power source is treated as mains power: a machine whose
battery cannot be read must keep running its agents. Failing to run is
the worse failure.

`dotagent tick` honors these settings too — a tick that dispatched what
the daemon would have held back would misreport the thing it exists to
reproduce.

---

## `[telemetry]`

Opt-in OpenTelemetry OTLP export. **Disabled by default** — nothing
leaves your machine until you set `otlp_endpoint`.

| Field           | Type   | Default        | Notes                                                                |
|-----------------|--------|----------------|----------------------------------------------------------------------|
| `otlp_endpoint` | string | `""`           | Empty = disabled. e.g., `"https://api.honeycomb.io:443"`.            |
| `protocol`      | string | `"grpc"`       | `grpc` or `http` (HTTP/protobuf).                                    |
| `service_name`  | string | `"dotagent"`   | `service.name` resource attribute on every span/log.                  |

### `[telemetry.headers]`

Inline TOML table. Keys/values sent verbatim as HTTP/gRPC headers on
every OTLP request.

```toml
[telemetry.headers]
"x-honeycomb-team" = "your-api-key"
"x-custom-tenant" = "acme-tech"
```

The `OTEL_EXPORTER_OTLP_HEADERS` env var (comma-separated `k=v`) wins
when both are set — useful for keeping secrets out of the config file.

### `[telemetry.resource]`

Inline TOML table of OpenTelemetry resource attributes attached to
every span and log record. Vendor-agnostic.

```toml
[telemetry.resource]
"deployment.environment" = "production"
"host.name" = "workstation-01"
"service.version" = "0.0.1"
```

Standard OTel semantic conventions apply — `deployment.environment`,
`service.namespace`, `host.name`, `service.version`, etc.

### What gets exported

Today the OTel pipeline exports **spans**:

- `daemon` — root span for the daemon process lifetime
- `tick` — one per scheduler tick
- `agent_run` — one per agent invocation
- `plugin_invoke` — one per plugin call (preflight / sink / notify-via-plugin)

Logs are NOT yet exported via OTLP — that bridge is on the roadmap.
For now, ship logs via a sidecar (`fluent-bit`, `vector`, `promtail`)
reading the JSON file directly.

See [`observability.md`](observability.md#opentelemetry-export) for
per-vendor recipes (Honeycomb, Tempo, Jaeger, Datadog).

---

## What's NOT in `config.toml`

| Concern                                | Where instead                                                                  |
|----------------------------------------|--------------------------------------------------------------------------------|
| Per-agent retry policy                 | `[defaults]` in the agent's own `agent.toml`                                   |
| Per-agent notifications                | `[[notifiers]]` in the agent's own `agent.toml`                                |
| Per-agent security policy              | `[security]` in the agent's own `agent.toml`                                   |
| Notifier defaults across agents        | (Not yet supported — declare per-agent for now.)                               |
| Daemon binary path / unit file content | Generated by `dotagent install` from the running binary. No override knob.     |

---

## Migrating partial configs

`config.toml` is **partial-overlay**: missing fields keep their
defaults. The minimal "I want debug logs" config:

```toml
[logging]
level = "debug"
```

Everything else (`format`, `retention_days`, `[telemetry]`, …) stays
default.

You don't need to write empty tables for sections you don't customize.

---

## Reloading

`config.toml` is re-read on:

- Daemon startup
- The next tick after a SIGHUP (`dotagent reload`)

Changes that need a full **restart** (not just reload):

- Switching `[logging].format` between `json` / `pretty` / `compact`
  for the stderr stream — the subscriber is initialized once at boot.
- Changing OTel `protocol` (gRPC ↔ HTTP) — the exporter is built once.

For those, use:

```bash
launchctl kickstart -k "gui/$(id -u)/run.avelino.dotagent"     # macOS
systemctl --user restart run.avelino.dotagent                   # Linux
```

---

## Verifying your config

```bash
# Parse-check the file (any syntax error fails here).
toml-cli get config.toml .

# Make sure the daemon actually loaded it.
tail -F ~/.config/dotagent/logs/daemon/dotagent.log \
  | jq -c 'select(.fields.message | contains("config"))'
```

To confirm OTel went live:

```bash
tail -F ~/.config/dotagent/logs/daemon/dotagent.log \
  | jq -c 'select(.fields.message | contains("otel") or contains("OTLP"))'
```

You should see a "telemetry initialized" or similar message after the
next reload/restart.

---

## Related

- [`observability.md`](observability.md) — logging architecture + OTel
  vendor recipes
- [`env-vars.md`](../reference/env-vars.md) — `RUST_LOG` and
  `OTEL_EXPORTER_OTLP_HEADERS` overrides
- [`paths.md`](../reference/paths.md) — where logs land on disk
- [`agent-spec.md`](../reference/agent-spec.md) — per-agent config
  (manifest)
