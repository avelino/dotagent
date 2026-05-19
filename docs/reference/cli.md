# CLI Reference

> Every `dotagent` subcommand, what it does, and a minimal example.

```text
dotagent <COMMAND> [ARGS] [FLAGS]
```

Run `dotagent --help` for the cheat-sheet, `dotagent <command> --help`
for per-command flags. Every subcommand documented below mirrors the
clap-generated help.

| Command          | Purpose                                                                       |
|------------------|-------------------------------------------------------------------------------|
| [`run`](#run)              | Run a single schedule of an agent in the foreground.                  |
| [`tick`](#tick)            | One-shot dispatch pass (what the daemon does on each cycle).          |
| [`daemon`](#daemon)        | Long-lived adaptive scheduler. Invoked by launchd / systemd.          |
| [`status`](#status)        | Textual health dashboard.                                             |
| [`daily-summary`](#daily-summary) | Send the end-of-day health summary.                            |
| [`bootstrap`](#bootstrap)  | Mark every schedule's window as ok (one-shot, post-install). *Not implemented yet.* |
| [`install`](#install)      | Generate + write the daemon unit file (launchd / systemd).             |
| [`uninstall`](#uninstall)  | Remove the daemon unit file.                                          |
| [`doctor`](#doctor)        | Validate manifests, resolve plugin references, warn on drift.         |
| [`plugin list`](#plugin-list) | List discovered plugins.                                           |
| [`plugin invoke`](#plugin-invoke) | Invoke a plugin manually for debugging. *Not implemented yet.* |
| [`logs`](#logs)            | Tail the daemon-captured stdout/stderr for one or all agents.         |
| [`inspect`](#inspect)      | Dump heartbeat + manifest hash + schedule state for one agent.        |
| [`reload`](#reload)        | Send SIGHUP to the running daemon.                                    |
| [`run-now`](#run-now)      | Force-run an agent immediately, ignoring schedule windows.            |

---

## `run`

Run a single schedule of an agent in the foreground. Useful for
development — no daemon involvement, no `on_*` plugin hooks, no
notifiers fire.

```bash
dotagent run <NAME> --schedule <ID> [--dry-run]
```

| Arg/flag        | Meaning                                                                    |
|-----------------|----------------------------------------------------------------------------|
| `NAME`          | The agent's `agent.name` (not the directory name).                          |
| `--schedule ID` | Which schedule to use — picks the `args` and the slug from this schedule.   |
| `--dry-run`     | Inject `AGENT_DRY_RUN=true`. The script can decide what to do (typically: skip side effects). dotagent skips writing the heartbeat. |

**What it does**: discovers the manifest, injects the
[env vars](env-vars.md), spawns the command, captures stdout/stderr,
prints stdout to your terminal, and exits with the agent's exit code.
**Notifiers and sinks do NOT fire** — use `run-now` for that.

**Example**:

```bash
dotagent run hello --schedule every-2min
dotagent run finops-weekly --schedule weekly --dry-run
```

---

## `tick`

One-shot dispatch pass. Same logic the daemon loop runs once, but
without sleeping or installing signal handlers. Used by the daemon
internally; exposed so you can debug "what would dotagent do RIGHT NOW".

```bash
dotagent tick [--dry-run] [--verbose]
```

| Flag        | Meaning                                                                                  |
|-------------|------------------------------------------------------------------------------------------|
| `--dry-run` | Don't actually spawn anything. Prints `scanned N; would dispatch M; next event: ts`.     |
| `--verbose` | (Reserved — currently a no-op.)                                                          |

**Example**:

```bash
dotagent tick --dry-run
# → (dry-run) scanned 4 agent(s); would dispatch 1; next event: 2026-05-19T08:30:00-0300
```

When debugging "is my agent gonna fire?", run this. It tells you which
schedules dotagent considers due.

---

## `daemon`

The long-lived adaptive scheduler. **You don't invoke this directly** —
launchd / systemd does, via the unit file `dotagent install` writes.

```bash
dotagent daemon
```

If you run it manually (e.g., for development), it stays in the
foreground until you `Ctrl+C` (SIGINT) or kill it (SIGTERM). It:

- Discovers manifests + plugins
- Writes `state/daemon.pid`
- Initializes structured JSON logging into `logs/daemon/dotagent.log`
- Initializes OTel export if `[telemetry]` is configured
- Loops forever: `tick → sleep until next event → wake on SIGHUP`
- Cleans up the PID file on exit (Drop guard)

The daemon process responds to:

| Signal   | Effect                                                                  |
|----------|-------------------------------------------------------------------------|
| `SIGHUP` | Wake immediately, re-read manifests + plugins on the next tick.         |
| `SIGTERM`| Graceful shutdown. Audit log gets a `DaemonStopped` entry.              |
| `SIGINT` | Same as SIGTERM.                                                        |

See [`guides/daemon-lifecycle.md`](../guides/daemon-lifecycle.md) for
how to install / start / stop / reload the daemon end-to-end.

---

## `status`

Textual health dashboard. Read-only — never writes to audit, never
dispatches.

```bash
dotagent status
```

**Output**:

```text
═══ Agent Health · 2026-05-19 14:32 ═══

  ✅ ok       3/5
  ⚠️  degraded 1
  ❌ failing  1
  🕑 stale    0

AGENT/SCHEDULE                         STATE        LAST RUN                  REASON
────────────────────────────────────────────────────────────────────────────────────
team-standup/daily          ❌ failing   2026-05-19T08:30:01      WARP disconnected
finops-weekly/weekly                    ⚠️  degraded 2026-05-18T17:00:01      recovered after 3 attempts
hello/every-2min                       ✅ ok        2026-05-19T14:30:01      last_success_at fresh
...

Logs:    /Users/avelino/.config/dotagent/logs/
State:   /Users/avelino/.config/dotagent/state/agents/
Audit:   /Users/avelino/.config/dotagent/audit.log
```

Agents with `monitor = false` in their manifest are excluded (those are
typically one-shot/manual examples).

---

## `daily-summary`

Send the end-of-day health summary. The daemon fires this internally at
**22:45 local time** (hardcoded). The CLI command is for testing.

```bash
dotagent daily-summary [--dry-run]
```

| Flag        | Meaning                                                                  |
|-------------|--------------------------------------------------------------------------|
| `--dry-run` | Print the message to stdout instead of delivering it via the notifier.   |

> **Known gap**: the live delivery currently uses a hardcoded notifier
> target (`notify-imessage` plugin → a specific phone number). This
> will be moved into `config.toml` before 1.0. For now, `--dry-run`
> is the reliably-useful mode.

---

## `bootstrap`

> **Not yet implemented.** Calling it returns
> `bootstrap — not yet implemented`.

Intent: mark every schedule's "current window" as ok in one shot, so a
fresh install doesn't trigger a flood of `failing` notifications for
schedules that haven't had a chance to run yet.

Workaround for now: run the daemon for one full cycle of each schedule
and it'll fill in.

---

## `install`

Generate and write the daemon unit file. **One unit per system** — not
one per agent.

```bash
dotagent install [--all] [NAME]
```

| Arg/flag  | Meaning                                                            |
|-----------|--------------------------------------------------------------------|
| `--all`   | (Accepted for backwards compat — **no-op**, prints a notice.)      |
| `NAME`    | (Accepted for backwards compat — **no-op**, prints a notice.)      |

`--all` and `NAME` are accepted but ignored — dotagent now uses one
daemon unit (`run.avelino.dotagent`) that manages every discovered
manifest internally.

**What gets written**:

- **macOS**: `~/Library/LaunchAgents/run.avelino.dotagent.plist`
  - `RunAtLoad=true`, `KeepAlive=true`, `ThrottleInterval=10`
- **Linux**: `~/.config/systemd/user/run.avelino.dotagent.service`
  - `Restart=always`, `RestartSec=10`

The unit points at the currently-running `dotagent` binary (resolved
via `std::env::current_exe`). If you move the binary, re-run `install`.

After writing the file, the command prints the platform-specific
"Next step" line to actually load the unit:

```bash
# macOS
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/run.avelino.dotagent.plist

# Linux
systemctl --user daemon-reload && systemctl --user enable --now run.avelino.dotagent
```

See [`guides/daemon-lifecycle.md`](../guides/daemon-lifecycle.md).

---

## `uninstall`

Remove the daemon unit file. Idempotent (no error if nothing to remove).

```bash
dotagent uninstall [--all] [NAME]
```

Same flag caveats as `install` — both are no-ops.

**Does NOT stop the daemon** if it's running — `launchctl bootout` /
`systemctl --user disable --now` first if you want a clean stop.

---

## `doctor`

Validate every discovered manifest, resolve plugin references, and warn
on inconsistencies. **Always safe to run** — read-only.

```bash
dotagent doctor
```

**What it checks**:

- Manifest TOML is parseable.
- `agent.name` and `[run].command` are non-empty.
- Schedule ids are unique within each manifest.
- Every plugin referenced by `[[preflight]]` / `[[on_success]]` /
  `[[on_failure]]` / `[[notifiers]] driver = "plugin"` resolves to a
  binary on `$PATH` / the discovery dirs.
- Each manifest has a `[security]` section. (If absent, emits a
  warning — schema-only in v0; see
  [`security/threat-model.md`](../security/threat-model.md).)
- Compares each manifest's sha256 against `state/known_manifests.json`.
  Mismatch → "manifest drift since last daemon run" warning.

**Exit code**: 0 if 0 errors, non-zero otherwise. Warnings do not
trigger a non-zero exit.

**Example**:

```bash
dotagent doctor
# ✓ hello: manifest ok
#     notifier driver=desktop (built-in)
#     ⚠ hello: no [security] section — blast radius is unbounded.
# ✓ finops-weekly: manifest ok
#     plugin sink-roam → /opt/homebrew/bin/dotagent-plugin-sink-roam
#     plugin preflight-warp → /opt/homebrew/bin/dotagent-plugin-preflight-warp
#     notifier driver=imessage (built-in)
#
# summary: 2 agent(s), 0 error(s), 1 warning(s)
```

---

## `plugin list`

List every plugin referenced by any discovered manifest, with its
resolved path + advertised version + kinds.

```bash
dotagent plugin list
```

**Output** (tab-separated):

```text
preflight-warp     0.0.1   "preflight"   /opt/homebrew/bin/dotagent-plugin-preflight-warp
sink-roam          0.0.1   "sink"        /opt/homebrew/bin/dotagent-plugin-sink-roam
```

If a plugin is referenced but not on `$PATH`, you'll see
`(not found: ...)`. Run `dotagent doctor` for a friendlier report.

---

## `plugin invoke`

> **Not yet implemented.** Currently returns `plugin invoke <name> —
> not yet implemented`.

Intent: run a plugin manually with a JSON payload. Until this lands,
invoke directly:

```bash
echo '{
  "kind": "preflight",
  "agent": "test",
  "schedule": "test",
  "event": "preflight",
  "config": {}
}' | dotagent-plugin-preflight-warp invoke
```

See [`reference/plugin-protocol.md`](plugin-protocol.md) for the
payload shape.

---

## `logs`

Tail the daemon-captured stdout/stderr.

```bash
dotagent logs [NAME] [-n LINES] [--follow] [--schedule ID]
```

| Arg/flag         | Meaning                                                                                    |
|------------------|--------------------------------------------------------------------------------------------|
| `NAME` (optional)| Tail one agent's logs. **Omit** to tail every agent at once (each chunk is prefixed by `tail` with `==> path <==`). |
| `-n LINES`       | Print the last `N` lines (default 50).                                                     |
| `--follow` / `-f`| `tail -F`-style follow. Survives rotation.                                                  |
| `--schedule ID`  | (Reserved — currently unused.)                                                              |

Reads from `~/.config/dotagent/logs/agents/<name>/<name>.log` plus any
rolled `<name>.log.YYYY-MM-DD` files (skips `.gz` — `tail` can't follow
compressed files).

**Examples**:

```bash
# One agent, follow
dotagent logs hello --follow

# Last 200 lines of every agent at once
dotagent logs -n 200

# Pipe to jq is doable but the file is raw text, not JSON — see the
# daemon log if you want structured data.
```

For **structured** logs (the daemon's own tracing output) read
`~/.config/dotagent/logs/daemon/dotagent.log` directly:

```bash
tail -F ~/.config/dotagent/logs/daemon/dotagent.log | jq .
```

See [`guides/observability.md`](../guides/observability.md) for the
log schema.

---

## `inspect`

Dump heartbeat + manifest hash + schedule state for one agent.

```bash
dotagent inspect <NAME>
```

**Output**:

```text
agent:        hello
manifest_dir: /Users/avelino/.config/dotagent/agents/hello
manifest_sha: a3f9... (first seen 2026-05-19T14:00:00-0300)
monitor:      true
timeout:      30s

─── schedule 'every-2min' (slug=default) ───
  {
    "name": "hello",
    "slug": "default",
    "args": [],
    "started_at": 1747680001,
    "started_at_iso": "2026-05-19T14:30:01-0300",
    "finished_at": 1747680002,
    "exit_code": 0,
    ...
  }
```

Use this when "is the heartbeat fresh?" / "did the last run succeed?"
is your question.

---

## `reload`

Send SIGHUP to the running daemon. The daemon picks up new manifests
and plugin changes on its next tick.

```bash
dotagent reload
```

Reads `~/.config/dotagent/state/daemon.pid` and sends `SIGHUP`.
Fails if:

- The PID file is missing (daemon not running).
- The PID exists but the process is gone (stale pidfile).

If you swapped the **`dotagent` binary itself**, SIGHUP isn't enough —
restart the daemon via launchctl/systemctl. See
[`guides/daemon-lifecycle.md`](../guides/daemon-lifecycle.md).

---

## `run-now`

Force-run an agent immediately, ignoring schedule windows. Unlike
`run`, this DOES fire preflight, sinks, and notifiers — it's a
single-shot version of what the daemon would do.

```bash
dotagent run-now <NAME> [--schedule ID]
```

| Arg/flag          | Meaning                                                                                |
|-------------------|----------------------------------------------------------------------------------------|
| `NAME`            | The agent's `agent.name`.                                                              |
| `--schedule ID`   | Which schedule's `args` to use. If omitted, uses the first schedule declared.          |

**Example**:

```bash
dotagent run-now finops-weekly --schedule weekly
# → run-now finops-weekly/weekly done in 47s — Ran(RunOutcome { exit_code: 0, ... })
```

Use this to:

- Trigger an agent after fixing a problem (don't wait for the next
  window).
- Manually exercise the full plugin chain (preflight → spawn → sink →
  notify).

---

## Exit codes

| Code            | Meaning                                                          |
|-----------------|------------------------------------------------------------------|
| `0`             | Success.                                                          |
| `1`             | Generic failure (manifest invalid, plugin not found, etc.).      |
| `124`           | `dotagent run` only — the agent timed out (SIGTERM + SIGKILL).   |
| Anything else   | `dotagent run` only — the agent's exit code is passed through.   |

---

## Environment variables

dotagent reads a small set of env vars for configuration overrides
(`DOTAGENT_HOME`, `DOTAGENT_ROOT`, `DOTAGENT_PLUGIN_PATH`, `RUST_LOG`,
`OTEL_EXPORTER_OTLP_HEADERS`). See [`env-vars.md`](env-vars.md) for the
complete list.

dotagent INJECTS env vars into the agent subprocess
(`AGENT_NAME`, `AGENT_TMPDIR`, etc.). Same doc.

---

## Related

- [Daemon lifecycle](../guides/daemon-lifecycle.md) — install / start /
  stop / reload
- [Troubleshooting](../guides/troubleshooting.md) — sintoma → diagnostic
- [Agent spec](agent-spec.md) — `agent.toml` schema
- [Plugin protocol](plugin-protocol.md) — for `plugin list` / `plugin invoke`
- [Observability](../guides/observability.md) — log format, OTel
