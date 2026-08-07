# Architecture

> What the daemon does, what it doesn't, and which crate owns each decision.

dotagent is built like a coffee shop, not a kitchen brigade. **One person
behind the counter takes every order**. The "daemon" is that one person —
it watches the schedules, decides what to fire next, supervises each
agent, and goes back to waiting. There is no per-agent process running
in the background. There is no scheduler poll loop.

Read this if you want to:

- Picture what happens between `dotagent install` and `your agent runs`.
- Know which crate to touch when changing behavior.
- Understand why a panicking plugin doesn't take down everything.

For the user-facing concepts see [`agents.md`](agents.md) and
[`plugins.md`](plugins.md). For the schema details see the
[reference docs](../reference/agent-spec.md).

---

## The 30-second mental model

```mermaid
flowchart LR
    OS["OS scheduler<br/>(launchd / systemd)"] -->|spawn at boot| D[dotagent daemon]
    D -->|reads| M["agent.toml × N"]
    D -->|sleeps until<br/>next event| D
    D -->|spawn| A[your agent script]
    A -->|stdout / exit code| D
    D -->|fire| P["plugins<br/>(preflight, sink)"]
    D -->|fire| N["notifiers<br/>(in-process)"]
    D -->|append| AU[(audit log)]
    D -->|write| ST[(state: heartbeat, window)]
```

Three things to internalize:

1. **The OS owns when the daemon runs.** launchd / systemd keep
   `dotagent daemon` alive. dotagent does not install itself as a
   service-aware program — `dotagent install` writes a unit file, and
   the OS does the rest.
2. **The daemon owns when each agent runs.** Not the OS. There is **one**
   unit file (`run.avelino.dotagent`), not one per agent. The daemon's
   adaptive scheduler computes the next event across every schedule of
   every agent and sleeps until that exact moment.
3. **Your agent is a one-shot subprocess.** It reads env vars, does its
   thing, writes stdout, exits. No SDK, no IPC, no long-running state.
   The one exception is opt-in: an agent declaring `[lifecycle] mode =
   "persistent"` is kept alive between runs and handed requests over JSON
   lines. Same supervisor, same deadline, same reaping — see
   [`lifecycle.md`](lifecycle.md).

Everything else in this doc is detail.

---

## Crate layout

The workspace has eleven crates. Each has one responsibility and a single
direction of "depends on" — no cycles.

```mermaid
flowchart TD
    cli[dotagent<br/>CLI binary] --> runner[dotagent-runner<br/>spawn + hooks]
    cli --> scheduler[dotagent-scheduler<br/>pure functions]
    cli --> mcp[dotagent-mcp<br/>JSON-RPC + MCP types]
    cli --> unitgen[dotagent-unit-gen<br/>plist / service]
    runner --> core[dotagent-core<br/>types]
    runner --> state[dotagent-state<br/>filesystem state]
    runner --> plugin[dotagent-plugin<br/>subprocess JSON]
    runner --> telemetry[dotagent-telemetry<br/>tracing + OTel]
    runner --> supervisor[dotagent-supervisor<br/>process lifecycle]
    scheduler --> core
    state --> core
    telemetry --> core
    telemetry --> state
    plugin --> supervisor
    core --> notify[dotagent-notify<br/>notifiers + ingress]
    notify --> secrets[dotagent-secrets<br/>secrets.env loader]
```

Two edges surprise people:

**`core → notify`, not the other way.** The manifest re-exports
`NotifierEntry`, so the type crate depends on the driver crate. It also
means `cargo publish` has to publish `notify` *before* `core`.

**`notify → secrets`, and nothing depends on `secrets`.** It exists as its
own crate precisely to avoid a `core → notify → core` cycle: the Telegram
driver needs `${VAR}` resolution, and putting that loader in `core` would
close the loop.

| Crate                 | Owns                                                                                            |
|-----------------------|-------------------------------------------------------------------------------------------------|
| `dotagent-core`       | Shared types: `AgentManifest`, `Schedule`, `Heartbeat`, `WindowState`, `Config`, `AuditEvent`, `TriggerRequest`. |
| `dotagent-scheduler`  | **Pure** scheduling math. `compute_next_event`, `expected_at`, `should_retry`, `health_state`. No IO. |
| `dotagent-runner`     | Spawn the agent subprocess. Timeout, env injection, stdio capture, heartbeat lifecycle, hook firing. Also the pool of live processes for `[lifecycle] mode = "persistent"` agents. |
| `dotagent-state`      | Filesystem state: heartbeats, window state, audit log, plugin state, manifest cache.            |
| `dotagent-notify`     | Built-in notifier drivers (`desktop`, `slack`, `ntfy`, `pushover`, `telegram`, `imessage`) **and** inbound Telegram transport. |
| `dotagent-secrets`    | Loader for `secrets.env`. Separate crate to keep `core → notify` acyclic.                       |
| `dotagent-plugin`     | `PluginClient` — discover + spawn + JSON-stdio for preflight / sink / third-party notify.       |
| `dotagent-mcp`        | JSON-RPC 2.0 + Model Context Protocol wire types. No IO, no agent knowledge.                     |
| `dotagent-supervisor` | Subprocess lifecycle: deadlines, kill-tree via POSIX process groups, live registry.             |
| `dotagent-telemetry`  | `tracing` setup, JSON file logging, daily rotation, retention sweep, optional OTLP export.      |
| `dotagent-unit-gen`   | Render `daemon.plist` (macOS) / `daemon.service` (Linux) from templates.                        |
| `dotagent` (binary)   | CLI subcommands. Wires the crates together.                                                     |

**Rule of thumb**: if you can write it as `fn f(now: DateTime, ...) -> X`
with no `std::fs`, it goes in `dotagent-scheduler`. Anything that touches
disk or processes goes in `dotagent-state`, `dotagent-runner`, or
`dotagent-plugin`.

---

## Lifecycle of a single run

The full path from "the daemon wakes up" to "your agent has run and the
audit is written" — step by step.

```mermaid
sequenceDiagram
    autonumber
    participant OS as OS scheduler
    participant D as daemon (tick loop)
    participant SCH as scheduler
    participant ST as state store
    participant PF as preflight plugin
    participant A as your agent
    participant SK as sink plugin
    participant N as notifier (in-process)
    participant AU as audit log

    OS->>D: spawn at boot (KeepAlive=true)
    D->>D: tick()
    D->>SCH: compute_next_event()
    SCH-->>D: ts = 2026-05-19T08:30
    D->>D: sleep until min(ts, +30min)
    Note over D: SIGHUP → wake early & reload
    D->>SCH: expected_at(sched, now) - any window missed?
    SCH-->>D: yes — fire schedule X
    D->>PF: invoke (subprocess + JSON stdin)
    PF-->>D: {"ok": true}
    D->>ST: write heartbeat (started_at)
    D->>A: spawn with AGENT_* env vars + tmpdir
    A-->>D: stdout / stderr / exit code
    D->>ST: update heartbeat (finished_at, exit_code)
    D->>SK: invoke with stdout tail
    SK-->>D: {"ok": true}
    D->>N: fire (rate-limit check → POST to slack/etc.)
    D->>AU: append AgentRun, PluginInvoked × N
    D->>D: back to tick()
```

What's worth pointing out:

- **Step 4 (sleep).** No polling. The daemon computes `ts` once and
  `tokio::time::sleep` until then. `MAX_SLEEP_MINUTES = 30` is the
  safety cap so a new manifest dropped into `agents/` is picked up
  within that window even if no scheduled event fires. The
  [daily summary](../guides/config-reference.md#daily_summary) is a
  third input to that `min()`: it is the one scheduled thing the daemon
  does that no agent schedule accounts for, so it schedules its own
  wake-up rather than relying on the cap to land inside its window.
- **Step 7-9 (preflight).** If any preflight returns `ok=false`, the
  agent is **never spawned**. dotagent emits `PreflightFailed` to audit
  and fires the matching notifier.
- **Step 10 (heartbeat start).** Written before the spawn. Crash
  detection: if `started_at` exists but `finished_at` doesn't, the
  previous run died.
- **Step 11 (env injection).** dotagent sets nine `AGENT_*` env vars
  plus whatever `[env].extra` declares. See
  [`reference/env-vars.md`](../reference/env-vars.md).
- **Step 14-15 (sink).** Fires only when the agent exited zero.
  `stdout_tail` (last 500 lines) is the payload.
- **Step 16 (notifier).** Built-in drivers run in-process — no fork.
  Rate-limit state is read from `state/notify/<driver>/<slug>.json`.

---

## Where each decision lives

When debugging "why did X happen", you need to know which crate to grep.

| Question                                              | Where to look                                                    |
|-------------------------------------------------------|------------------------------------------------------------------|
| When does a schedule fire next?                       | `dotagent-scheduler` — `compute_next_event`                       |
| Is this window expected to have run already?          | `dotagent-scheduler` — `expected_at`                              |
| Should this failed window be retried?                 | `dotagent-scheduler` — `should_retry`                             |
| Is this agent `ok` / `degraded` / `failing` / `stale`?| `dotagent-scheduler` — `health_state`                             |
| How does the agent receive env vars?                  | `dotagent-runner` — `apply_env`                                   |
| When does the agent get killed for timeout?           | `dotagent-runner` — `tokio::time::timeout` then `kill` + `SIGKILL` after 5s grace |
| Where is the heartbeat written?                       | `dotagent-state` — `StateStore::write_heartbeat`                  |
| How is `marker_regex` resolved?                       | The sink plugin itself (e.g., `plugins/sink-roam/src/main.rs`)    |
| How are plugins discovered?                           | `dotagent-plugin` — `PluginClient::resolve`                       |
| Where does `dotagent install` write the unit?         | `dotagent-unit-gen` — `launchd::generate_daemon` / `systemd::generate_daemon` |
| What's logged where?                                  | `dotagent-telemetry` — `init_from_default_config`                 |

---

## Crash isolation

A single panicking plugin must not take down the daemon. Same for a
runaway agent or a misconfigured notifier. The boundaries:

```mermaid
flowchart LR
    subgraph daemon_proc["daemon process"]
        D[adaptive scheduler]
        N[notifiers]
    end
    subgraph agent_proc["agent process (one-shot subprocess)"]
        A[your script]
    end
    subgraph plugin_proc["plugin process (one-shot subprocess)"]
        P[plugin binary]
    end
    D --> A
    D --> P
```

| Failure                       | Containment                                                                                            |
|-------------------------------|--------------------------------------------------------------------------------------------------------|
| Agent script panics / segfaults | Subprocess dies. `exit_code` captured. Daemon writes the heartbeat and moves on. Retry policy kicks in. |
| Persistent instance dies          | The next request finds the dead pipe, discards the instance and spawns a replacement. A death in the window between setting the deadline and writing the request costs one retry, not the message. |
| Agent hangs                     | `agent.timeout_seconds` triggers SIGTERM, then SIGKILL after 5s. `exit_code = 124`.                    |
| Plugin panics                   | Subprocess dies. dotagent records `PluginInvoked { ok: false }` and continues. No retry of the plugin. |
| Notifier driver fails (e.g., Slack 503) | The notifier call returns `Err` but the **run already happened**. Audit records the failure. The run is still considered successful. |
| Daemon crashes                  | launchd `KeepAlive=true` / systemd `Restart=always` brings it back. Audit reconstructs state on startup. |
| Audit log tampered              | `verify_chain()` on startup catches it. Emits `AuditChainBroken` (which is itself a chained audit entry) and fires the configured `Critical`-severity notifier. A log that merely **rotated** (32MB → `audit.log.<stamp>` + a hash seam) verifies clean, and a segment the operator deleted reads as "intact since `<ts>`" rather than broken — see [`security/threat-model.md`](../security/threat-model.md#what-the-hash-chain-guarantees-and-what-it-does-not). |

The trade-off: **fork+exec per plugin (~5-10ms)**. Plugins fire on
discrete events (preflight, sink-on-success, third-party notify),
never in hot loops, so the cost is invisible. The built-in notifiers
were promoted out of the plugin protocol precisely because they fire
*often* (every failure attempt) — see
[`notifications.md`](notifications.md) for the trade-off.

---

## State on disk

dotagent is "stateless" in the sense that the daemon can be killed and
restarted without losing any decision context — every consequential
write is committed to disk first. The full layout is at
[`reference/paths.md`](../reference/paths.md); the high-points:

```text
~/.config/dotagent/                # $DOTAGENT_HOME
├── agents/                        # YOUR manifests (or symlinks)
├── plugins/                       # YOUR custom plugin binaries
├── config.toml                    # optional global config
├── state/
│   ├── agents/<name>/<slug>.heartbeat.json   # per-run lifecycle
│   ├── windows/<name>-<slug>-<ts>.json       # expected-vs-actual ledger
│   ├── plugins/<plugin>/<key>.json           # plugin-owned state
│   ├── notify/<driver>/<slug>.json           # built-in notifier rate-limit
│   ├── known_manifests.json                  # sha256 cache for drift detection
│   └── daemon.pid                            # for `dotagent reload`
├── logs/
│   ├── daemon/dotagent.log                   # structured JSON, daily rotation
│   ├── daemon/run.avelino.dotagent.log       # launchd/systemd stdout
│   └── agents/<name>/<name>.log              # raw agent stdout+stderr
├── audit.log                                 # append-only, hash-chained (live)
└── audit.log.20260806T101500                 # sealed segment, never deleted
```

Two important properties:

1. **`audit.log` is the source of truth for "did this happen".** It is
   append-only and hash-chained (`prev_hash` field). Past 32MB it rotates
   into a sealed `audit.log.<stamp>` segment, stitched to the new file by
   a hash **seam** so the chain never has a gap — and no segment is ever
   deleted automatically. Operational logs are dense (debug-grade) and
   disposable; the audit log is sparse and forever.
2. **`known_manifests.json` is how drift / phantom agents are detected.**
   sha256 of every loaded manifest is cached. On the next load, mismatch
   → `ManifestDriftDetected`; new agent not in the cache →
   `PhantomAgentDetected`. Both are `Critical` severity (out-of-band
   notify).

---

## What the daemon does NOT do

- **Run business logic.** Every domain concern (APIs to hit, prompts to
  draft, files to write) belongs in your agent script. dotagent's
  surface ends at "spawn the process, watch what came out."
- **Sleep to wait for time.** Scheduling math is `compute_next_event` +
  `tokio::time::sleep`. No `loop { sleep(1s); check() }`. This is why a
  hundred schedules don't burn CPU.
- **Embed AI / call LLMs.** dotagent has zero LLM dependencies. Your
  agent decides whether and when to invoke `claude -p`, `openai`, the
  `mcp` CLI, or nothing at all.
- **Replace the `mcp` CLI.** dotagent and `mcp` are independent
  projects. Agents that use Roam / Sentry / Grafana / etc. call `mcp`
  directly the same way they did before dotagent existed. dotagent
  *serves* MCP via `dotagent mcp` (agents as tools) but has no MCP
  client of its own — see [`../reference/mcp.md`](../reference/mcp.md).
- **Sandbox the agent.** The `[security]` block in `agent.toml` is v0
  schema-only — `doctor` reports inconsistency, but the runner doesn't
  yet enforce `allowed_commands` / `filesystem_writable` / network
  policy. Enforcement (sandbox-exec / bwrap / firejail) lands as a
  follow-up; see [`threat-model.md`](../security/threat-model.md).

---

## Related

- [`agents.md`](agents.md) — what an agent looks like from the author's side
- [`plugins.md`](plugins.md) — how the plugin protocol works
- [`notifications.md`](notifications.md) — why notifiers are NOT plugins
- [`lifecycle.md`](lifecycle.md) — agents kept alive between runs
- [`triggers.md`](triggers.md) — runs caused by an event, not the clock
- [`telegram.md`](telegram.md) — inbound chat
- [`../reference/mcp.md`](../reference/mcp.md) — agents as MCP tools
- [`../reference/cli.md`](../reference/cli.md) — every CLI subcommand
- [`../reference/agent-spec.md`](../reference/agent-spec.md) — full manifest schema
- [`../reference/paths.md`](../reference/paths.md) — filesystem layout
- [`../security/threat-model.md`](../security/threat-model.md) — security posture
