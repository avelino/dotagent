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
| [`api`](#api)              | Raw JSONL bridge to the daemon's local Unix socket.                  |
| [`status`](#status)        | Textual health dashboard.                                             |
| [`daily-summary`](#daily-summary) | Send the end-of-day health summary.                            |
| [`bootstrap`](#bootstrap)  | Mark every schedule's window as ok (one-shot, post-install). *Not implemented yet.* |
| [`install`](#install)      | Generate + write the daemon unit file (launchd / systemd).             |
| [`uninstall`](#uninstall)  | Remove the daemon unit file.                                          |
| [`doctor`](#doctor)        | Validate manifests, resolve plugin references, warn on drift.         |
| [`plugin list`](#plugin-list) | List discovered plugins.                                           |
| [`plugin invoke`](#plugin-invoke) | Invoke a plugin manually for debugging. *Not implemented yet.* |
| [`audit verify`](#audit-verify) | Verify the audit log's hash chain and report how far back the guarantee reaches. |
| [`logs`](#logs)            | Tail the daemon-captured stdout/stderr for one or all agents.         |
| [`inspect`](#inspect)      | Dump heartbeat + manifest hash + schedule state for one agent.        |
| [`reload`](#reload)        | Send SIGHUP to the running daemon.                                    |
| [`run-now`](#run-now)      | Force-run an agent immediately, ignoring schedule windows.            |
| [`completions`](#completions) | Print a shell completion script (bash / zsh / fish / elvish / powershell). |

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

## `api`

Bridge raw JSON Lines between stdin/stdout and the daemon's local Unix socket.
The command does not parse, render, or persist messages, so it can be used as a
backend for scripts or a TUI.

```bash
dotagent api [--socket <PATH>]
```

By default it connects to `$DOTAGENT_HOME/api.sock` (normally
`~/.config/dotagent/api.sock`). `--socket` overrides the path for tests or an
explicit endpoint. Every input frame is forwarded with its original bytes and
line ending; every socket frame is forwarded to stdout the same way.

When stdin reaches EOF, the client half-closes its request side and continues
reading responses/events until the daemon closes the socket. Diagnostics go to
stderr. There is no prompt or TUI rendering.

**Example**:

```bash
printf '%s\n' '{"id":"status","method":"status.get"}' | dotagent api
```

See [`Local Client API`](local-api.md#raw-cli-bridge) for the wire contract.

---

## `status`

Textual health dashboard. Read-only — never writes to audit, never
dispatches. Takes no flags.

```bash
dotagent status
```

**Output**:

```text
═══ Agent Health · 2026-08-06 17:51 ═══

  ✅ ok       2/5
  ⚠️  degraded 1
  ❌ failing  1
  🕑 stale    1

AGENT/SCHEDULE                       STATE       LAST RUN                   REASON
────────────────────────────────────────────────────────────────────────────────────────────────────
disk-alert/every-15min               ❌ failing   2026-08-06T16:00:04-0300   2 attempts, will retry
hn-digest/weekday-morning            ⚠️  degraded  2026-08-06T08:12:41-0300   recovered after 1 attempt
backup-offsite/nightly               🕑 stale     2026-08-01T11:40:23-0300   window missed 7330min ago (stale)
inbox-triage/every-90min             ✅ ok        2026-08-06T17:42:58-0300   ok
inbox-triage/weekly-cleanup          ✅ ok        2026-08-03T10:44:53-0300   no window today · last success ok

Logs:    ~/.config/dotagent/logs/
State:   ~/.config/dotagent/state/agents/
Audit:   ~/.config/dotagent/audit.log
```

One row per `(agent, schedule)` pair — `inbox-triage` above declares
two schedules and gets two rows, each with its own health. The counter
at the top counts rows, not agents: `2/5` there is five schedules
across four agents.

Rows are ordered **most-urgent-first**: `failing` → `degraded` →
`stale` → `ok`. Within a bucket, discovery order.

| Column           | Meaning                                                                    |
|------------------|----------------------------------------------------------------------------|
| `AGENT/SCHEDULE` | `agent.name` + the schedule's `id`.                                        |
| `STATE`          | One of the four [health states](agent-spec.md#health-states).              |
| `LAST RUN`       | `finished_at_iso` from the heartbeat — when the run **ended**, success or not. `never` if there is no heartbeat yet. |
| `REASON`         | Why that state, see below.                                                 |

Agents with `monitor = false` in their manifest are excluded (those are
typically one-shot/manual examples).

### Reading the `REASON` column

The state comes from the heartbeat *and* from the window file for the
window currently due — the same file the daemon writes retries against.
The reason tells you which of the two decided:

| State      | Reason                             | What produced it                                                 |
|------------|------------------------------------|------------------------------------------------------------------|
| `ok`       | `ok`                               | The due window succeeded on the first try.                       |
| `ok`       | `no window today · last success ok` | Nothing is due — a weekday-only schedule on a Sunday, an hour outside `hours` — and the last run succeeded. |
| `degraded` | `recovered after N attempts`       | The window did succeed, but burned `N` **failed** attempts first. |
| `failing`  | `N attempts, will retry`           | The window passed without success; the retry budget is not spent yet. |
| `failing`  | `gave up after N attempts`         | Retry budget exhausted — the window is marked `given_up`.         |
| `failing`  | `window due Nmin ago, no attempt`  | The window is overdue and **no window file exists** — the daemon never dispatched. Usually means the daemon is not running. |
| `stale`    | `window missed Nmin ago (stale)`   | Older than `stale_after_minutes`; retrying is no longer useful.   |
| `stale`    | `never ran`                        | No heartbeat and no window due — the schedule has never fired.    |

The count is singular-aware: `1 attempt`, `2 attempts`.

`N` in `recovered after N attempts` counts **failed** attempts, not
dispatches. The window file's `attempts` counter is bumped after every
dispatch including the one that succeeded, so a first-try success lands
on disk as `attempts: 1` and still reports `ok`. See
[Health states](agent-spec.md#health-states).

For `stale`, an `interval` schedule is judged against the **first**
window it missed after its last success, not the rolling window
dispatch uses — so a schedule broken for weeks reports the full age.

### Live subprocesses

When the daemon is running and has children alive, `status` appends a
second table read from the supervisor snapshot
(`state/supervisor.json`). It is omitted entirely when the snapshot is
missing or empty.

```text
─── Live subprocesses (2 running) ───
KIND       PID      OWNER          LABEL                          AGE / DEADLINE
────────────────────────────────────────────────────────────────────────────────────────────────────
⚠️  agent    41207    hn-digest      hn-digest.weekday-morning      246s / 300s (82%)
   sink     41219    hn-digest      sink-file.invoke               3s / 30s (10%)
```

`KIND` is one of `agent`, `persistent`, `preflight`, `sink`, `notify`,
`skill`, `plugin_info`, `plugin_validate`. Rows sort by deadline
pressure, highest first, and are flagged `⚠️` at ≥80% of the deadline
and `🔴` at ≥100%.

A `persistent` row never gets a warning icon: its clock is the idle
window before recycling, not a run running late, so reaching it is the
pool working as designed. Those rows mark the clock `idle`, and their
`AGE` is time since the last answer. See
[Lifecycle](../concepts/lifecycle.md).

Agents launched by `dotagent mcp` or `run-now` run in **that** process,
not under the daemon, so they never appear here.

The snapshot carries two fields this table deliberately does not show:
`pgid` and `command`. They are identity proof for the next daemon's
[boot orphan reap](../guides/daemon-lifecycle.md#boot-orphan-reap), not
information an operator reads at a glance. Full schema in
[`paths.md`](paths.md#statesupervisorjson).

---

## `daily-summary`

Send the end-of-day health summary. The daemon delivers it once a day
at `[daily_summary].time` (default `22:45` local) and schedules its own
wake-up for that moment. `grace_minutes` (default `30`) is the tail
that still counts as on-time when the wake-up could not happen at all —
laptop closed, machine off. Both are configurable; see
[`[daily_summary]`](../guides/config-reference.md#daily_summary).

```bash
dotagent daily-summary [--dry-run]
```

| Flag        | Meaning                                                                        |
|-------------|--------------------------------------------------------------------------------|
| `--dry-run` | Print the message and its destinations to stdout instead of delivering it.     |

Typing the command delivers even when `enabled = false`. That flag
governs the daemon's scheduled delivery; someone who ran this asked for
this one.

**Destinations** come from `[[daily_summary.notifiers]]`, which takes
the same entries a manifest's `[[notifiers]]` takes. With none
configured the summary goes to the `desktop` driver — the only one that
needs no address and sends nothing off the machine.

**Output** — the same classification `status` renders, folded into a
message short enough for a phone. Healthy schedules collapse into the
count; only the unhealthy ones are named, and empty buckets are
dropped:

```text
📊 Agents · 2026-08-06
2/5 ok

❌ Failing:
  · disk-alert/every-15min — 2 attempts, will retry

⚠️ Degraded:
  · hn-digest/weekday-morning — recovered after 1 attempt

🕑 Stale:
  · backup-offsite/nightly — window missed 7330min ago (stale)
```

An all-green day is three lines: the header, `N/N ok`, and nothing
else.

`--dry-run` appends where it would have gone:

```text
→ would deliver to: telegram, desktop
```

Each delivery is audited as `plugin_invoked` with
`plugin: "notifier:<driver>"`, on failure as well as on success — a
summary that reached nobody is answerable from `dotagent audit` instead
of from silence.

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

`doctor` also reports inbound Telegram when `[telegram]` is present in
`config.toml` — whether the ingress is on, and the two ways it can be
half-configured:

```
telegram ingress: on — 1 allowed user(s), dispatcher 'telegram-assistant'
telegram ingress: OFF — bot_token set but allowed_user_ids is empty
    ⚠ an empty allowlist means nobody, never everybody. Add your numeric user id.
```

It stays silent when the section is absent, since the ingress is off by
default. A `dispatcher_agent` that does not resolve is a warning: every
accepted message would fail after passing the allowlist.

It also prints where long-term memory lives:

```
memory: /Users/me/.config/dotagent/outl (default)
```

A path set in `[memory] workspace` that holds no outl workspace is a warning —
the default path is scaffolded automatically, a configured one is not.

And how many skills it found, across every search root:

```
skills: 8 found, including ~/.claude/skills
```

A `SKILL.md` that fails to parse is a warning, not an error — nothing stops
running, an assistant just answers without a procedure it should have had. Two
skill names that sanitize to the same tool name are also warned about: only the
first is callable, and a skill you wrote and cannot call is otherwise a mystery.

Commands get the same treatment, with one extra check. They carry two derived
names — the MCP tool and the Telegram menu entry — and Telegram's
`[a-z0-9_]{1,32}` has no hyphens, so two commands can be distinct in the catalog
and collide in the menu:

```text
commands: 4 found
    ⚠ weekly-numbers (…/a/weekly-numbers.md), weekly_numbers (…/b/weekly_numbers.md)
      all want /weekly_numbers — only the first is in the menu
```

The files are named, not just the commands: two names differing only by `-`
versus `_` are near-identical on screen, and the useful question is which one to
rename. See [Commands](../concepts/commands.md#two-names-two-collision-rules).

Manifests that fail to parse are listed with `✗` and counted as errors. They
no longer abort the scan — the healthy agents are still reported below them.

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

## `audit verify`

Verify the hash chain in `$DOTAGENT_HOME/audit.log` and print what
verification could actually establish.

```bash
dotagent audit verify           # the live audit.log only
dotagent audit verify --full    # follow the seams back through every segment
dotagent audit verify --json    # one JSON line, for scripts
```

The daemon runs the same check at boot, but it only asks yes/no. This
asks *how far back*, which is the question a rotated log makes real: a
history the operator pruned must not read the same as one somebody
beheaded.

**Verdicts:**

| Output | Meaning | Exit |
|---|---|---|
| `chain intact from GENESIS` | Every entry still on disk was checked, back to the first line ever written | `0` |
| `chain intact since <ts>` | Every entry checked links, and the oldest is a **seam** naming where the rest went. The text says whether that segment is still on disk (re-run with `--full`) or gone (retention — or evidence removed; the chain cannot tell those apart) | `0` |
| `unexplained truncation` | The oldest entry links to a hash nothing accounts for: no `GENESIS`, no seam. The head of the log was removed | `1` |
| `chain broken at position N` | A link mismatch, or a line nobody can parse, inside the data that is present. Names the segment and both hashes | `1` |

```text
✗ chain broken at position 42
  in:       audit.log.20260806T101500
  expected: c8d2f1…
  actual:   9a3b07…
  file:     ~/.config/dotagent/audit.log
  scope:    full — following seams back through every segment on disk
  segments: 2 on disk (audit.log.20260806T101500, audit.log.20261104T093000)
```

Without `--full` the walk stops at the first seam, which is what the
daemon does at boot — the live file is the only one that changes, so it
is the cheap check worth running every time. `--full` is the one to run
when you actually want the guarantee to reach `GENESIS`.

What the chain does and does not prove (short version: it catches partial
edits, never a total rewrite) is in
[`security/threat-model.md`](../security/threat-model.md).

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
manifest_dir: /Users/me/.config/dotagent/agents/hello
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
dotagent run-now <NAME> [--schedule ID] [--json]
```

| Arg/flag          | Meaning                                                                                |
|-------------------|----------------------------------------------------------------------------------------|
| `NAME`            | The agent's `agent.name`.                                                              |
| `--schedule ID`   | Which schedule's `args` to use. If omitted, uses the first schedule declared.          |
| `--json`          | Emit one machine-parseable JSON line instead of the human report.                      |

**Example**:

```bash
dotagent run-now finops-weekly --schedule weekly
```

```text
✓ finops-weekly/weekly  ok  47s

stdout:
  wrote 12 rows to /Users/me/reports/weekly.md
```

A preflight abort names the plugin that stopped it instead of a run
result:

```text
⊘ finops-weekly/weekly  aborted by preflight  1s
  plugin: preflight-warp
  suggest: warp-cli connect
```

Colour follows the TTY and honours `NO_COLOR`. Truncated output says so
explicitly rather than trailing off.

Use this to:

- Trigger an agent after fixing a problem (don't wait for the next
  window).
- Manually exercise the full plugin chain (preflight → spawn → sink →
  notify).

---

## `mcp`

Serve every discovered agent as an MCP tool over stdio. JSON-RPC 2.0, one
object per line.

```bash
dotagent mcp
```

Takes no flags — the catalog comes from the manifests already on disk. Point
any MCP client at it:

```json
{ "mcpServers": { "dotagent": { "command": "dotagent", "args": ["mcp"] } } }
```

**Example** — list the catalog by hand:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | dotagent mcp
# → {"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"run-disk-alert",...}]}}
```

The catalog is not only agents: `skill-*` tools carry the procedures found
under `~/.config/dotagent/skills/` and `~/.claude/skills/`, `command-get` and
`command-list` resolve the commands under `~/.config/dotagent/commands/`, and
`memory-*` tools appear when `[memory]` is on. See
[Skills](../concepts/skills.md) and [Commands](../concepts/commands.md).

Agents run in **this** process, like `run-now` — not through the daemon — so
the subprocess tree does not appear in `dotagent status`. State is keyed off
the `trigger-mcp` slug so an on-demand run never overwrites the scheduled
history of the same agent.

stdout carries protocol only; logging goes to stderr, so `RUST_LOG=debug
dotagent mcp` stays safe to run under a client.

Full protocol reference: [MCP server](mcp.md).

---

## `completions`

Print a shell completion script. The script wires **dynamic** completion
of agent names: pressing `<TAB>` after `dotagent run`, `inspect`,
`run-now`, `logs`, `install`, or `uninstall` runs `dotagent _list-agents`
(a hidden helper) and lists every manifest currently on disk — no stale
baked-in list to refresh.

```bash
dotagent completions <SHELL>
```

| Arg     | Values                                          |
|---------|-------------------------------------------------|
| `SHELL` | `bash` · `zsh` · `fish` · `elvish` · `powershell` |

> Dynamic agent-name completion is wired for `bash`, `zsh`, and `fish`.
> `elvish` / `powershell` get subcommand + flag completion only.

**Install**:

```bash
# fish — eval in current shell, or save to ~/.config/fish/completions/dotagent.fish
dotagent completions fish | source

# zsh — drop into a fpath dir
dotagent completions zsh > ~/.zfunc/_dotagent

# bash — drop into the bash-completion dir
dotagent completions bash > ~/.local/share/bash-completion/completions/dotagent
```

Example:

```bash
$ dotagent run hel<TAB>
hello-fish  hello-go  hello-python  hello-rust
```

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
