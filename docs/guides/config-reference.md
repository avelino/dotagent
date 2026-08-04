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
"host.name" = "avelino-igloo"

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
| `enabled` | `true` | Expose `memory-remember` / `memory-recall` from `dotagent mcp`. |
| `workspace` | `""` | Workspace path. Empty resolves to `$DOTAGENT_HOME/outl`. |

The default workspace is scaffolded when the daemon starts, so memory works without writing any config. A **configured** path is never scaffolded: a typo there must fail loudly rather than create an empty workspace nobody will look at. `dotagent doctor` reports which case you are in.

Pointing this at a workspace you already use puts what an agent remembers next to your own notes, synced to your peers. Convenient, and also means an agent writes where you write.

Full behavior in [Memory](../concepts/memory.md).

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
2. Files older than `compress_after_days` → gzipped in-place.
3. Files older than `retention_days` (daemon) or
   `per_agent_retention_days` (agents) → deleted.

The audit log (`audit.log`) is **never** swept regardless of these
settings — by design. See
[`observability.md`](observability.md#audit-log-vs-operational-log).

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
"host.name" = "avelino-igloo"
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
| Daily summary target / time            | (Hardcoded today; `config.toml` integration is on the roadmap.)                |
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
