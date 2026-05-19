# Environment Variables

Two categories:

1. **[Injected into the agent](#injected-into-the-agent-subprocess)** —
   what your script sees when dotagent invokes it.
2. **[Read by dotagent itself](#read-by-dotagent-itself)** — overrides
   for paths, verbosity, OTel headers.

---

## Injected into the agent subprocess

When dotagent spawns your agent, these `AGENT_*` variables are set on
top of the inherited environment (unless `env.inherit = false` in the
manifest). They are the **only API surface** you depend on — there's no
SDK to import.

| Variable               | Type     | Example                                            | When set                         |
|------------------------|----------|----------------------------------------------------|----------------------------------|
| `AGENT_NAME`           | string   | `finops-weekly`                                     | Always.                          |
| `AGENT_HOME`           | abs path | `/Users/avelino/.config/dotagent/agents/finops-weekly` | Always — the manifest directory. |
| `AGENT_TMPDIR`         | abs path | `/var/folders/.../tmpAbCdEf`                       | Always — fresh per run, auto-cleaned on exit. |
| `AGENT_DRY_RUN`        | `"true"` / `"false"` | `"false"`                              | Always.                          |
| `AGENT_SCHEDULE_ID`    | string   | `daily`                                            | Always — matches `[[schedules]].id`. |
| `AGENT_SLUG`           | string   | `period_dia-anterior`                              | Always — derived from the schedule's `args`. |
| `AGENT_START_EPOCH`    | int      | `1700000000`                                       | Always — unix epoch of `started_at`. |
| `AGENT_ARGV`           | JSON array | `["--period","dia-anterior"]`                    | Always — the schedule's `args` as JSON. |
| `AGENT_HEARTBEAT_FILE` | abs path | `~/.config/dotagent/state/agents/.../slug.heartbeat.json` | Set when NOT dry-run.        |

The positional `argv` of your process is `[run].command` + `[run].args`
+ schedule's `args`, so most scripts don't actually need `AGENT_ARGV`
unless they want JSON-shaped access.

### Slug derivation

`AGENT_SLUG` is computed from the schedule's `args`:

| `args`                          | slug                  |
|---------------------------------|-----------------------|
| `[]`                            | `default`             |
| `["--period", "dia-anterior"]`  | `period_dia-anterior` |
| `["--mode", "unsubscribe"]`     | `mode_unsubscribe`    |
| `["foo bar"]`                   | `foo_bar`             |

Rules: strip leading dashes, lowercase, replace non-alphanumeric with
`_`, collapse repeated `_`, trim trailing `_`. Empty input → `default`.

### Reading these vars

**Fish**:

```fish
echo "I am $AGENT_NAME running on schedule $AGENT_SCHEDULE_ID"
test "$AGENT_DRY_RUN" = "true"; and exit 0
cd $AGENT_TMPDIR
```

**Python**:

```python
import os, json
name = os.environ["AGENT_NAME"]
argv = json.loads(os.environ.get("AGENT_ARGV", "[]"))
if os.environ.get("AGENT_DRY_RUN") == "true":
    return
```

**Go**:

```go
name := os.Getenv("AGENT_NAME")
var argv []string
json.Unmarshal([]byte(os.Getenv("AGENT_ARGV")), &argv)
```

**Bash**:

```bash
: "${AGENT_NAME:?AGENT_NAME not set — running outside dotagent?}"
cd "$AGENT_TMPDIR" || exit
```

### Extra variables you declare

`[env].extra` in the manifest is merged on top. Standard idiom for
agent-tuning constants:

```toml
[env]
inherit = true                                  # default
[env.extra]
LOG_LEVEL          = "info"
PYTHONUNBUFFERED   = "1"
DISK_FREE_MIN_PCT  = "20"
```

Set `inherit = false` if you want a hermetic environment (no parent env
leaks in — careful, that removes `$PATH` too unless you re-add it under
`extra`).

---

## Read by dotagent itself

dotagent reads these to override defaults. None are required —
everything works out-of-the-box.

| Variable                       | Purpose                                                                | Default                            |
|--------------------------------|------------------------------------------------------------------------|------------------------------------|
| `DOTAGENT_HOME`                | Override the root directory.                                           | `~/.config/dotagent`               |
| `DOTAGENT_ROOT`                | Extra (colon-separated) directories to scan for manifests, prepended to the default search list. | (empty)            |
| `DOTAGENT_PLUGIN_PATH`         | Extra (colon-separated) directories to search for plugin binaries.     | (empty)                            |
| `RUST_LOG`                     | Tracing filter — overrides `[logging].level` in `config.toml`.         | `info`                             |
| `OTEL_EXPORTER_OTLP_HEADERS`   | OTLP auth headers (comma-separated `k=v`). Used when `[telemetry].otlp_endpoint` is set. | (empty)         |

### `DOTAGENT_HOME`

Moves everything dotagent owns (manifests, state, logs, audit, config)
under a different root.

```bash
DOTAGENT_HOME=/var/lib/dotagent dotagent doctor
DOTAGENT_HOME=/var/lib/dotagent dotagent daemon
```

Both the daemon and CLI must agree — if you start the daemon with
`DOTAGENT_HOME=A` and then run `dotagent reload` with `DOTAGENT_HOME=B`,
the reload reads the **wrong** PID file (the daemon under A's
`state/daemon.pid`).

When set in a launchd plist:

```xml
<key>EnvironmentVariables</key>
<dict>
    <key>DOTAGENT_HOME</key>
    <string>/var/lib/dotagent</string>
</dict>
```

When set for a systemd unit:

```ini
[Service]
Environment=DOTAGENT_HOME=/var/lib/dotagent
```

### `DOTAGENT_ROOT`

Adds **search roots** for manifest discovery, ahead of the defaults.
Useful for CI / testing without touching `~/.config/dotagent/`:

```bash
DOTAGENT_ROOT=$PWD/examples dotagent doctor
DOTAGENT_ROOT=$PWD/examples dotagent run hello-fish --schedule manual
```

The full discovery order with `DOTAGENT_ROOT` set:

1. Every directory in `$DOTAGENT_ROOT`
2. `$DOTAGENT_HOME/agents/`
3. `$CWD/agents/`
4. `$CWD`

Each direct subdirectory of these roots that contains an `agent.toml`
becomes one agent. Duplicates resolve to first-found by `agent.name`.

### `DOTAGENT_PLUGIN_PATH`

Adds search directories for plugin binary resolution, ahead of the
defaults:

```bash
DOTAGENT_PLUGIN_PATH=$PWD/target/release dotagent plugin list
```

Full plugin discovery order:

1. Every directory in `$DOTAGENT_PLUGIN_PATH`
2. `$DOTAGENT_HOME/plugins/`
3. `/usr/local/lib/dotagent/plugins/`
4. `$PATH`

First match wins. `dotagent plugin list` shows the resolved path.

> **Daemon gotcha**: when launchd starts the daemon, your interactive
> shell's `$DOTAGENT_PLUGIN_PATH` is NOT inherited. Set it in the plist
> (`EnvironmentVariables`), or move the plugin into `$DOTAGENT_HOME/plugins/`.

### `RUST_LOG`

Overrides `[logging].level` from `config.toml`. Same `EnvFilter` syntax
as the rest of the Rust ecosystem — per-target filters supported:

```bash
# Globally chatty
RUST_LOG=debug dotagent daemon

# Just the runner
RUST_LOG=info,dotagent_runner=trace dotagent daemon

# Quiet down a noisy crate
RUST_LOG=info,h2=warn,hyper_util=warn dotagent daemon
```

The `RUST_LOG` env var only affects the **CLI subcommand it's set
for**. To make it sticky for the daemon, put it in the launchd plist /
systemd unit `Environment=`.

### `OTEL_EXPORTER_OTLP_HEADERS`

Vendor-specific authentication for the OTLP exporter. Format is
comma-separated `k=v`. Used only when `[telemetry].otlp_endpoint` is
non-empty in `config.toml`.

```bash
# Honeycomb
export OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=YOUR_API_KEY"

# Grafana Cloud (base64-encoded basic auth)
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic $(printf %s "$STACK_ID:$API_TOKEN" | base64)"
```

Headers can also be declared in `[telemetry.headers]` of `config.toml`
— the env var wins when both are set.

See [`observability.md`](../guides/observability.md#vendor-recipes) for
full vendor recipes.

---

## What dotagent does NOT honor

Some env vars you might expect from other tools — dotagent ignores them
on purpose:

| Variable          | Why dotagent ignores it                                                            |
|-------------------|------------------------------------------------------------------------------------|
| `XDG_CONFIG_HOME` | dotagent uses `~/.config/dotagent` directly. Override via `DOTAGENT_HOME`.         |
| `XDG_DATA_HOME`   | dotagent doesn't separate config/data/cache — everything lives under `DOTAGENT_HOME`. |
| `EDITOR`          | dotagent has no interactive editing.                                                |
| `LAUNCH_AGENT`    | The daemon is the launchd-managed unit; agents themselves never touch launchd.      |

---

## Quick reference

```bash
# Agent script — runtime env it sees
AGENT_NAME            finops-weekly
AGENT_HOME            ~/.config/dotagent/agents/finops-weekly
AGENT_TMPDIR          /var/folders/.../tmpXXXXXX
AGENT_DRY_RUN         false
AGENT_SCHEDULE_ID     weekly
AGENT_SLUG            default
AGENT_START_EPOCH     1700000000
AGENT_ARGV            []
AGENT_HEARTBEAT_FILE  ~/.config/dotagent/state/agents/finops-weekly/default.heartbeat.json

# Caller — overrides for the dotagent CLI / daemon
DOTAGENT_HOME             /var/lib/dotagent          (default: ~/.config/dotagent)
DOTAGENT_ROOT             /tmp/test-agents           (prepended to manifest search)
DOTAGENT_PLUGIN_PATH      $PWD/target/release        (prepended to plugin search)
RUST_LOG                  info,dotagent_runner=debug
OTEL_EXPORTER_OTLP_HEADERS x-honeycomb-team=KEY
```

---

## Related

- [`paths.md`](paths.md) — where every file actually lives
- [`agent-spec.md`](agent-spec.md) — `[env]` block in the manifest
- [`config-reference.md`](../guides/config-reference.md) — `config.toml`
  options that env vars can override
- [`observability.md`](../guides/observability.md) — `RUST_LOG` and
  OTel headers in context
