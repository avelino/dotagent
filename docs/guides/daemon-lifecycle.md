# Daemon Lifecycle

> Install, start, stop, reload, diagnose. macOS launchd + Linux
> systemd, side-by-side.

The dotagent daemon is **one process per user**. The OS (launchd on
macOS / systemd on Linux) keeps it alive. dotagent never installs
itself — `dotagent install` only generates the unit file; you load it
into the OS scheduler with one extra command.

If anything below isn't behaving as expected, the
[Diagnostics](#diagnostics) section at the bottom has the canonical
"is this thing on?" check.

```mermaid
flowchart LR
    A[dotagent install] -->|writes file| U["unit file<br/>(.plist or .service)"]
    U -->|loaded into| OS["OS scheduler<br/>(launchd / systemd)"]
    OS -->|spawn + supervise| D[dotagent daemon]
    D -->|writes| PID["state/daemon.pid"]
    PID -.->|read by| R[dotagent reload]
    R -->|SIGHUP| D
```

---

## TL;DR

```bash
# 1. Generate the unit file.
dotagent install

# 2. Load it into the OS scheduler (one of these two).
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/run.avelino.dotagent.plist          # macOS
systemctl --user daemon-reload && systemctl --user enable --now run.avelino.dotagent          # Linux

# 3. Verify.
dotagent status
cat ~/.config/dotagent/state/daemon.pid
```

---

## Generate the unit file

```bash
dotagent install
```

Writes a single unit, regardless of how many agents you have:

| Platform | File                                                            |
|----------|-----------------------------------------------------------------|
| macOS    | `~/Library/LaunchAgents/run.avelino.dotagent.plist`             |
| Linux    | `~/.config/systemd/user/run.avelino.dotagent.service`           |

The unit's `ExecStart` / `ProgramArguments` points at the **currently-running
`dotagent` binary** (resolved via `std::env::current_exe`). If you move the
binary after install, re-run `dotagent install` so the unit refreshes.

Both unit files set:

| Property           | macOS (`launchd`)                | Linux (`systemd`)         |
|--------------------|----------------------------------|---------------------------|
| Auto-restart       | `KeepAlive=true`                 | `Restart=always`          |
| Start at login     | `RunAtLoad=true`                 | (enable with `--now`)     |
| Throttle           | `ThrottleInterval=10`            | `RestartSec=10`           |
| stdout capture     | `StandardOutPath=…run.avelino.dotagent.log` | `StandardOutput=append:…` |
| stderr capture     | `StandardErrorPath=…-error.log`  | `StandardError=append:…`  |

Templates live in
[`crates/dotagent-unit-gen/templates/`](../../crates/dotagent-unit-gen/templates/).

> **Note**: `dotagent install` accepts `--all` and a positional `NAME`
> for backwards compatibility with the legacy per-agent install flow.
> Both are **no-ops** now — the daemon manages every discovered manifest
> internally. The CLI prints a notice to remind you.

---

## Start

### macOS (launchd)

```bash
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/run.avelino.dotagent.plist
```

What this does: registers the plist with launchd's `gui/<uid>` domain
(the user session domain) and immediately spawns the daemon since
`RunAtLoad=true`.

Verify:

```bash
launchctl print "gui/$(id -u)/run.avelino.dotagent" 2>&1 | head -20
# Should print "state = running" plus the spawned PID.
```

> **`brew services start dotagent`** (coming soon, once the tap
> publishes) is a friendlier alternative — it runs `launchctl bootstrap`
> for you.

### Linux (systemd user units)

```bash
systemctl --user daemon-reload
systemctl --user enable --now run.avelino.dotagent
```

`daemon-reload` makes systemd re-scan `~/.config/systemd/user/`.
`enable --now` does two things: marks the unit to auto-start at the
next login AND starts it immediately.

For the unit to **actually survive logout**, you need lingering
enabled (otherwise systemd kills user sessions when you log out):

```bash
loginctl enable-linger $USER
```

Verify:

```bash
systemctl --user status run.avelino.dotagent
# Should print "Active: active (running)".
```

---

## Stop

### macOS

```bash
launchctl bootout "gui/$(id -u)/run.avelino.dotagent"
```

`bootout` is the inverse of `bootstrap` — unloads the plist from
launchd, sends SIGTERM, and removes the daemon from the user domain.
The plist file stays put on disk; the daemon just isn't running.

> Don't `kill -9` the daemon — `KeepAlive=true` will respawn it within
> 10s. Either bootout (above) or use `launchctl stop`:
>
> ```bash
> launchctl stop run.avelino.dotagent      # one-shot stop; bootstrap stays loaded → will restart
> ```

### Linux

```bash
systemctl --user stop run.avelino.dotagent
```

Doesn't disable auto-start at next login. To prevent re-spawn at the
next boot, also:

```bash
systemctl --user disable run.avelino.dotagent
```

---

## Restart

After upgrading the `dotagent` binary itself, the running daemon is
still pointing at the **old in-memory binary**. Replacing the file
doesn't change the running process. Restart it explicitly:

### macOS

```bash
launchctl kickstart -k "gui/$(id -u)/run.avelino.dotagent"
```

`kickstart -k` sends SIGTERM, waits for exit, then re-spawns.

### Linux

```bash
systemctl --user restart run.avelino.dotagent
```

> If you only changed **manifests** (`agent.toml` files) or
> **`config.toml`**, use `dotagent reload` (SIGHUP) instead — it's
> cheaper.

---

## Reload (SIGHUP)

```bash
dotagent reload
```

Reads `~/.config/dotagent/state/daemon.pid`, sends `SIGHUP` to that
process. The daemon picks up changes on its next tick:

- New manifests in `~/.config/dotagent/agents/`
- Modified manifests (drift is detected and audited)
- Updated `config.toml`
- Updated plugin binaries (resolution is re-done per tick)

What `reload` does NOT do:

- **Doesn't swap the binary.** A SIGHUP'd process keeps its
  in-memory code. For binary swaps, use `restart` (above).
- **Doesn't drain in-flight runs.** Currently-spawned agents keep
  running until they exit.
- **Doesn't reset the audit chain.** The new tick continues from the
  current `prev_hash`.

Errors:

| Error                                  | Meaning                                                                  |
|----------------------------------------|--------------------------------------------------------------------------|
| `reading ... (daemon not running?)`    | `state/daemon.pid` is missing. Daemon isn't running.                     |
| `sending SIGHUP: No such process`      | PID file is stale (daemon crashed without `Drop` cleanup). Start it again. |

---

## Uninstall

Stop the daemon first, then remove the unit:

```bash
# 1. Stop the running daemon (so the file we delete isn't actively in use).
launchctl bootout "gui/$(id -u)/run.avelino.dotagent"        # macOS
systemctl --user disable --now run.avelino.dotagent          # Linux

# 2. Remove the unit file.
dotagent uninstall
# → removed ~/Library/LaunchAgents/run.avelino.dotagent.plist
# (or systemd unit on Linux)
```

`dotagent uninstall` is idempotent — running it twice doesn't error,
the second call just prints "nothing to remove".

**`dotagent uninstall` does NOT delete your data.** Manifests,
heartbeats, audit log, config — all stay in `~/.config/dotagent/`.
See [`installation.md`](../getting-started/installation.md#uninstall)
for a full wipe.

---

## Diagnostics

### "Is the daemon running?"

```bash
# 1. PID file exists?
ls -l ~/.config/dotagent/state/daemon.pid
# → -rw-r--r-- ... daemon.pid

# 2. PID is alive?
ps -p $(cat ~/.config/dotagent/state/daemon.pid) -o command= 2>/dev/null
# → dotagent daemon
```

If either step fails:

- File missing → daemon never started (or was stopped cleanly).
- PID exists but `ps` empty → stale pidfile (daemon crashed without
  the `Drop` guard firing). Just start the daemon again.

### Platform-native checks

**macOS**:

```bash
launchctl print "gui/$(id -u)/run.avelino.dotagent" 2>&1 | head -30
# state = running           ← the line that matters
# program = .../dotagent
# arguments = .../dotagent → daemon
```

**Linux**:

```bash
systemctl --user status run.avelino.dotagent
# Active: active (running)  ← the line that matters
```

### "What's the daemon doing right now?"

Tail the structured log:

```bash
tail -F ~/.config/dotagent/logs/daemon/dotagent.log | jq -c .
# {"timestamp":"...","level":"INFO","fields":{"message":"daemon started"}}
# {"timestamp":"...","level":"INFO","fields":{"message":"dispatching run","agent":"hello","schedule":"every-2min"}}
```

Or the launchd / systemd captured stdout:

```bash
tail -F ~/.config/dotagent/logs/daemon/run.avelino.dotagent.log
tail -F ~/.config/dotagent/logs/daemon/run.avelino.dotagent-error.log
```

Health dashboard:

```bash
dotagent status
```

### "What will the daemon do next?"

```bash
dotagent tick --dry-run
# (dry-run) scanned 4 agent(s); would dispatch 1; next event: 2026-05-19T08:30:00-0300
```

That `next event` timestamp is when the daemon will next wake.

### "Did the audit log break?"

The daemon verifies the chain on startup and emits
`AuditChainBroken` (with notify) if it fails. Manual check:

```bash
# Walk the file; each line's prev_hash must be sha256 of the previous line.
# In practice, just look for the audit_chain_broken event:
grep audit_chain_broken ~/.config/dotagent/audit.log
# (no output = chain intact)
```

---

## Signal reference

The daemon process responds to:

| Signal     | Effect                                                                             |
|------------|------------------------------------------------------------------------------------|
| `SIGHUP`   | Wake immediately; re-read manifests + plugins on the next tick. `dotagent reload`. |
| `SIGTERM`  | Graceful shutdown. Drops `daemon.pid`. Audit gets `DaemonStopped`.                 |
| `SIGINT`   | Same as SIGTERM.                                                                   |
| `SIGKILL`  | Immediate kill — no `Drop` runs → stale pidfile. Auto-restart via launchd/systemd. |

Don't `kill -9` unless you have to — let `launchctl bootout` / `systemctl --user stop`
do the work.

---

## Common patterns

### Run the daemon manually (development)

For debugging the daemon itself, bypass launchd/systemd:

```bash
RUST_LOG=debug dotagent daemon
# Foreground; Ctrl+C to stop. Same code path as the supervised daemon.
```

Don't do this while the supervised daemon is also running — they'll
both try to write `daemon.pid` and step on each other.

### Run on a non-standard root

```bash
DOTAGENT_HOME=/tmp/sandbox dotagent install
DOTAGENT_HOME=/tmp/sandbox launchctl bootstrap "gui/$(id -u)" \
    ~/Library/LaunchAgents/run.avelino.dotagent.plist
```

The unit file inherits the env var of the shell that ran `install` —
**not great** for permanence. Better: set the env var inside the unit
file directly (`EnvironmentVariables` for launchd / `Environment=` for
systemd).

### Make config / env changes survive restart

If you need persistent overrides (custom `DOTAGENT_HOME`, custom
`OTEL_EXPORTER_OTLP_HEADERS`, etc.) edit the unit file:

**macOS** (`~/Library/LaunchAgents/run.avelino.dotagent.plist`):

```xml
<key>EnvironmentVariables</key>
<dict>
    <key>DOTAGENT_HOME</key>
    <string>/var/lib/dotagent</string>
    <key>OTEL_EXPORTER_OTLP_HEADERS</key>
    <string>x-honeycomb-team=YOUR_KEY</string>
</dict>
```

After editing, reload:

```bash
launchctl bootout "gui/$(id -u)/run.avelino.dotagent"
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/run.avelino.dotagent.plist
```

**Linux** (`~/.config/systemd/user/run.avelino.dotagent.service`):

```ini
[Service]
Environment=DOTAGENT_HOME=/var/lib/dotagent
Environment=OTEL_EXPORTER_OTLP_HEADERS=x-honeycomb-team=YOUR_KEY
```

After editing:

```bash
systemctl --user daemon-reload
systemctl --user restart run.avelino.dotagent
```

> The unit file is **regenerated** by `dotagent install` — your manual
> edits are lost if you re-run it. Long-term, treat the unit file as a
> generated artifact and keep custom envvars in `config.toml` instead
> where possible.

---

## Related

- [`installation.md`](../getting-started/installation.md) — install
  paths (brew, release, cargo, source)
- [`cli.md`](../reference/cli.md) — `install`, `uninstall`, `reload`
- [`troubleshooting.md`](troubleshooting.md) — sintoma → diagnostic
- [`observability.md`](observability.md) — log streams + OTel
- [`paths.md`](../reference/paths.md) — every file the daemon touches
