# Troubleshooting

> Sintoma → diagnostic → fix. Top-down by where the problem lives.

The decision tree:

```mermaid
flowchart TD
    A[Something's wrong] --> B{Daemon running?}
    B -->|no| C[Section: Daemon won't start]
    B -->|yes| D{Doctor passes?}
    D -->|no| E[Section: Doctor errors]
    D -->|yes| F{Agent firing?}
    F -->|no| G[Section: Agent never runs]
    F -->|yes| H{Run succeeds?}
    H -->|no| I[Section: Agent runs but fails]
    H -->|yes| J{Notify / sink working?}
    J -->|no| K[Section: Notifier / sink not working]
```

When in doubt, start with `dotagent doctor` + `dotagent status` —
they're read-only and give the most signal per second.

---

## Daemon won't start

### Symptom: `dotagent reload` says "daemon not running"

```bash
ls -l ~/.config/dotagent/state/daemon.pid
```

- **File missing** → daemon was never started (or was bootouted). Start it:
  - macOS: `launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/run.avelino.dotagent.plist`
  - Linux: `systemctl --user enable --now run.avelino.dotagent`
- **File present** but `ps -p $(cat …)` returns nothing → stale pidfile
  from a crashed daemon. Delete it and start again:
  ```bash
  rm ~/.config/dotagent/state/daemon.pid
  ```

### Symptom: launchd / systemd loads the unit but the daemon dies immediately

This is exactly what the captured stderr file is for — a daemon that dies
before or during startup has nowhere else to say why:

```bash
tail -50 ~/.config/dotagent/logs/daemon/run.avelino.dotagent-error.log
```

> Only crashes land there. Under launchd / systemd the daemon does not mirror
> its `tracing` stream to stderr (stderr is an unrotated file there, and the
> stream already goes to `dotagent.log`). So an **empty** `…-error.log` on a
> daemon that keeps dying means it is not crashing at all — check
> `logs/daemon/dotagent.log` and `dotagent status` instead. To force the
> mirror on anyway, set `DOTAGENT_LOG_STDERR=1` in the unit's environment.

Common causes:

| Error in stderr                                | Fix                                                                                      |
|------------------------------------------------|------------------------------------------------------------------------------------------|
| `telemetry init failed: ...`                   | Bad `[telemetry]` config. Either fix `config.toml` or delete the section.                |
| `config.toml parse error: ...`                 | TOML syntax issue. Validate with `toml-cli get config.toml .` or remove the file.        |
| `No such file or directory: dotagent`          | The unit's `ExecStart` points at a binary that was moved. Re-run `dotagent install`.    |
| `permission denied`                            | The dotagent binary lost the executable bit. `chmod +x` it.                              |

### Symptom: macOS — unit loads but `launchctl print` shows `state = exited`

ThrottleInterval (10s) is in play — launchd waits 10s before respawning.
After many crashes in quick succession, launchd will back off further. The
real diagnosis is in the error log above.

### Symptom: Linux — `systemctl --user status` shows `Active: inactive (dead)` despite `enable --now`

```bash
journalctl --user -u run.avelino.dotagent -n 50
```

If you see `User lingering not enabled`, the user session is killed at
logout. Enable lingering:

```bash
loginctl enable-linger $USER
```

---

## Doctor errors

### `agent.name is empty`

The `[agent].name` field is missing or `""`. Open the manifest and add it.

### `run.command is empty`

`[run].command` field missing. dotagent needs the command to invoke
(e.g., `"fish"`, `"python3"`, `"./agent"`).

### `duplicate schedule id: <id>`

Two `[[schedules]]` blocks have the same `id`. Schedule ids must be
unique **within a single manifest** — across different manifests they
can repeat.

### `✗ plugin <name> not found`

A `[[preflight]]`, `[[on_success]]`, `[[on_failure]]`, or
`[[notifiers]] driver = "plugin"` references a plugin binary that
isn't on `$PATH`.

```bash
# Where would dotagent look?
echo $DOTAGENT_PLUGIN_PATH                                    # custom (if set)
ls ~/.config/dotagent/plugins/                                 # user-local
ls /usr/local/lib/dotagent/plugins/                            # system-wide
which dotagent-plugin-sink-roam                                # $PATH
```

If absent on all four:

- Brew install: `brew install dotagent` (when tap publishes) ships
  every first-party plugin.
- Cargo install: `cargo install --path plugins/sink-roam` (or whichever).
- Release binaries: re-extract the tarball into a `$PATH` directory.

### `⚠ <name>: no [security] section`

Warning, not error. v0 is schema-only. Add a minimal block to silence
it:

```toml
[security]
# Document intent — enforcement comes in a future release.
network = "allow"
```

See [`security/threat-model.md`](../security/threat-model.md).

### `⚠ <name>: manifest drift since last daemon run`

The on-disk manifest's sha256 doesn't match what's cached in
`state/known_manifests.json` from the last daemon load. Cause is one of:

- **You edited it intentionally.** Run `dotagent reload`. The new
  hash gets cached.
- **You DIDN'T edit it.** Drift detection is doing its job — investigate.
  Compare timestamps:
  ```bash
  stat ~/.config/dotagent/agents/<name>/agent.toml
  git log -p ~/dotfiles/agents/<name>/agent.toml   # if version-controlled
  ```
  See [V1 in the threat model](../security/threat-model.md#v1--manifest-hijack).

---

## Agent never runs

### Symptom: `dotagent doctor` says manifest is fine but the daemon never fires the agent

Start with the dry-run:

```bash
dotagent tick --dry-run
# → (dry-run) scanned N agent(s); would dispatch M; next event: <ts>
```

If `dispatched = 0` and you expected dispatch:

- The current window has already succeeded — heartbeat says
  `last_success_at >= expected_at`. Run `dotagent inspect <name>` to
  confirm.
- The agent's `monitor = false` excludes it from the daemon. Set
  `monitor = true` (or remove the line — default is true).
- The schedule's window is in the future. `next event` shows when.

### Symptom: `monitor = false` agents fire when I don't want them to

`monitor = false` excludes the agent from `tick` / daemon dispatch, but
NOT from `run` / `run-now`. The example `hello-*` agents use this so
they only run when you explicitly invoke them.

### Symptom: agent fires but with the wrong `args`

`AGENT_ARGV` is the schedule's `args`, NOT `[run].args`. The full argv
the agent receives is `[run].command + [run].args + schedule.args`.

```toml
[run]
command = "python3"
args = ["./agent.py", "--mode", "prod"]      # always present

[[schedules]]
id = "weekly"
args = ["--period", "last-week"]              # appended at runtime
```

The Python script is invoked as `python3 ./agent.py --mode prod --period last-week`.
Inside the script, `AGENT_ARGV = ["--period","last-week"]`.

### Symptom: schedule type "interval" doesn't fire on a fresh install

Interval schedules with no previous run start "from now". A
`interval_minutes = 60` agent installed at 14:32 next fires at 15:32.
Set a smaller interval (or `dotagent run-now`) to verify.

### Symptom: cron-style schedule never matches

```toml
[[schedules]]
id = "daily"
type = "cron"
weekdays = [1, 2, 3, 4, 5]
hours = [8]
minute = 30
```

`weekdays`: **0 = Sunday, 6 = Saturday** (matches launchd Weekday). Many
folks expect 0 = Monday — that's cron(8), not us. dotagent matches launchd.

Quick sanity:

```bash
dotagent tick --dry-run
# Compare "next event" with your expectation.
```

---

## Agent runs but fails

### Symptom: `exit_code = 124`

The agent was killed for exceeding `agent.timeout_seconds`. dotagent
sends SIGTERM, waits 5 seconds, then SIGKILL. Either:

- Bump `timeout_seconds` in the manifest.
- Fix the agent (most timeouts are an external CLI hanging — wrap
  with `--timeout`).

`timeout_seconds` is the **backstop**, not the first line of defense.
It kills a wedged run; it does nothing to stop one slow network call
from eating the whole budget and leaving the agent no time to finish
the work it already did. Put a ceiling on the call itself — `curl
--max-time 20`, an HTTP client timeout, `timeout 30 <cmd>` — so a
single unresponsive endpoint degrades one step instead of the run.

### Symptom: agent succeeded once but the daemon keeps re-firing it

The previous run failed and you fixed it without updating state. The
window doesn't yet show `succeeded_at`. Run `dotagent run-now <name>` to
force a fresh success, OR delete the window file:

```bash
rm ~/.config/dotagent/state/windows/<name>-<slug>-<ts>.json
```

The daemon will recompute on the next tick.

### Symptom: `dotagent run` works but the agent fails when the daemon fires it

Almost always an **environment difference**. The interactive shell has
`$PATH` / `$HOME` / `$LANG` you take for granted; the daemon inherits
launchd / systemd's much-poorer env.

Check what the daemon sees:

```bash
# macOS — what env launchd hands to the daemon
launchctl print "gui/$(id -u)/run.avelino.dotagent" 2>&1 | grep -A 20 environment

# Linux
systemctl --user show-environment
```

Set explicit `$PATH` in the agent's manifest:

```toml
[env.extra]
PATH = "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin"
```

Or `inherit = true` plus tighten the unit-level env vars in the plist.

### Symptom: preflight aborts the run but I want to see why

```bash
tail ~/.config/dotagent/audit.log | jq 'select(.event.event_type == "preflight_failed")'
```

The `suggest` field is the message the plugin returned ("warp-cli
connect", etc.).

To run the preflight manually:

```bash
echo '{
  "kind": "preflight",
  "agent": "test",
  "schedule": "test",
  "event": "preflight",
  "config": { "connect_command": "warp-cli connect" }
}' | dotagent-plugin-preflight-warp invoke
```

---

## Notifier / sink not working

### Desktop notifier doesn't show banners (macOS)

System Settings → Notifications → make sure the **terminal app
running the daemon** (and possibly `dotagent` itself) is allowed.
notify-rust uses `NSUserNotification`, which inherits the parent
process's notification entitlements.

For launchd-managed daemons, the entitlement lives with the daemon
binary itself — TCC may show `dotagent` as the requestor first time.

### Desktop notifier doesn't show banners (Linux)

`notify-rust` uses D-Bus. Verify the daemon process can talk to your
notification daemon:

```bash
notify-send "test" "hello"
```

If `notify-send` shows nothing, no D-Bus session is reachable. Common
causes:

- Headless / SSH session with no `$DBUS_SESSION_BUS_ADDRESS`.
- systemd user unit started before the graphical target — set
  `Wants=graphical-session.target` in the unit.

### iMessage notifier doesn't send

`imessage` runs `osascript` to talk to Messages.app. Verify manually:

```bash
osascript -e 'tell application "Messages" to send "test" to buddy "+5511..."'
```

If that fails, automation isn't permitted: System Settings → Privacy &
Security → Automation → Terminal (or `dotagent`) → toggle Messages on.

The `imessage` driver also rate-limits via
`$DOTAGENT_HOME/state/notify/imessage/<slug>.json`. If you're sending
within `rate_limit_minutes` of a previous successful send, the call is
**skipped** (not failed). Look for it in the audit log:

```bash
tail ~/.config/dotagent/audit.log \
  | jq 'select(.event.plugin == "notifier:imessage")'
```

### Slack / ntfy / Pushover / Telegram notifier doesn't fire

Native HTTPS — check connectivity from the daemon's environment:

```bash
curl -v -X POST https://hooks.slack.com/services/T0000/B0000/SECRET \
    -H 'Content-Type: application/json' \
    -d '{"text":"test"}'
```

If that succeeds but dotagent's notifier doesn't, check the audit log
for a `Backend(...)` error:

```bash
tail ~/.config/dotagent/audit.log \
  | jq 'select(.event.event_type == "plugin_invoked" and (.event.plugin | startswith("notifier:")))'
```

Three causes account for most of it:

**`<field>: env var ${X} is unset`.** A `${VAR}` in a credential field did not
resolve. dotagent fails the send rather than posting the literal placeholder,
so nothing arrives. Check that the key is in `~/.config/dotagent/secrets.env`,
that the file is mode `0600` (otherwise the daemon refuses the whole file), and
that you sent `SIGHUP` after editing it — the store is read at startup and on
reload, not per send.

```bash
dotagent doctor                 # reports the secrets file's state
dotagent reload                 # SIGHUP: re-read secrets.env + config.toml
```

**`<driver> transport error (...)`.** Network or TLS. The message carries a
kind and, when there was one, an HTTP status — deliberately **not** the URL,
because for Slack and Telegram the URL is the credential. Reproduce with the
`curl` above from the daemon's own environment (a proxy set in your shell but
not in the plist is a classic).

**Nothing arrived, no error.** Check the event filter first —
`events = ["given_up"]` on an agent that keeps recovering never fires. Then
check size: an over-limit body is now trimmed with a `[truncated]` marker
rather than rejected, so if you see a truncated message that is the cap doing
its job (ntfy counts **bytes**, not characters — see
[notifications](../concepts/notifications.md#message-size-limits)).

For Telegram specifically, an API refusal now carries the Bot API's own
`description` (`"Bad Request: message is too long"`, `"chat not found"`), which
is usually the whole answer.

### Sink plugin appears not to run

Sinks fire **only on agent exit 0**. If the agent failed, only the
notifier path runs.

```bash
# Did the agent succeed?
cat ~/.config/dotagent/state/agents/<name>/<slug>.heartbeat.json | jq .exit_code
# Did the sink get invoked?
tail ~/.config/dotagent/audit.log \
  | jq 'select(.event.event_type == "plugin_invoked" and .event.plugin == "sink-roam")'
```

### `sink-roam` writes but the previous block doesn't get replaced

Your `marker_regex` doesn't match the old root block. See
[`plugins/sink-roam.md`](../plugins/sink-roam.md#block-keeps-duplicating-after-re-runs).

---

## A manifest is broken

### One agent stopped running and nothing said why

Check for a parse failure:

```bash
dotagent doctor 2>&1 | grep '✗'
grep manifest_invalid ~/.config/dotagent/state/audit.log | tail -5
```

A manifest that fails to parse or validate is skipped, audited as
`manifest_invalid` (Critical, so it fires out-of-band notification), and
the other agents keep running.

> Before this behavior existed, one broken file aborted the entire scan:
> the daemon saw **zero** agents, dispatched nothing, and the only trace
> was a single `warn!` line. If you are on an older build and everything
> stopped at once, look for a manifest you edited recently.

---

## Inbound Telegram

### The bot never answers

Check whether the ingress even started:

```bash
dotagent doctor | grep -i telegram
```

`telegram ingress: OFF — bot_token set but allowed_user_ids is empty` is
the most common cause. An empty allowlist means **nobody**, never
everybody. Add your numeric user id (from
[@userinfobot](https://t.me/userinfobot)) — a `@username` will not work,
it is not an authorization input.

If the ingress is on, your message may be getting refused:

```bash
grep trigger_rejected ~/.config/dotagent/state/audit.log | tail -5
```

`reason: "user id not in allowed_user_ids"` means the id in
`config.toml` is not the one that sent the message. `reason: "rate limit
exceeded"` means you passed `rate_limit_per_minute` (default 10).

### `telegram poll failed` repeating in the log

Transport problem — network down, or a revoked/wrong token. The backoff
doubles to a 60-second ceiling, so this will not spin. Verify the token
resolves:

```bash
dotagent doctor | grep -A3 "secrets file"
```

An unresolved `${TELEGRAM_BOT_TOKEN}` shows up there as an unresolved
reference.

### The run happens but no reply arrives

The dispatcher printed nothing on stdout, or logged to stdout instead of
stderr and the agent then failed. stdout **is** the reply:

```bash
dotagent logs telegram-assistant -n 50
```

### The reply is cut off mid-sentence

Telegram caps a message at 4096 characters. Longer output is trimmed
with a `[truncated]` marker; the full text is in `dotagent logs`.

### `dispatcher agent '<name>' not found`

`doctor` says this when `dispatcher_agent` in `config.toml` does not
match any installed agent. Every accepted message would fail after
passing the allowlist.

---

## MCP server

### `tools/list` returns an error instead of tools

One or more `agent.toml` files failed to load. The healthy agents are
fine — the daemon still runs them — but the MCP catalog refuses to be
served incomplete, because a model handed a list that quietly lost
entries would answer confidently and wrongly about what it can do.

The error names each failing path. `dotagent doctor` shows the same
list with the parse messages.

### An agent is missing from `tools/list`

Two agent names can sanitize to the same tool name (`a.b` and `a/b` both
become `run-a-b`). The first one discovered wins and the log says:

```
tool name collides with an earlier agent — skipping
```

Rename one of them.

### Runs started via MCP don't show in `dotagent status`

Expected. `dotagent mcp` runs agents in its own process, like `run-now`;
`status` reflects only the daemon's supervisor. Their heartbeats live
under the `trigger-mcp` slug, visible via `dotagent inspect`.

---

## Logs

### `dotagent logs <name>` says "no logs found"

The agent has never run, or the log directory was deleted. Run:

```bash
dotagent run-now <name>
ls ~/.config/dotagent/logs/agents/<name>/
```

### Logs are noisy / full of `tracing` debug spam

```toml
# config.toml
[logging]
level = "warn"
```

Or transient:

```bash
RUST_LOG=warn dotagent daemon
```

### Disk filling despite retention being set

- `retention_days` in `config.toml` only kicks in during the 03:00
  sweep. If the daemon hasn't been alive at 03:00 since the last
  rollover, nothing got swept yet.
- The sweep needs write permission on `logs/agents/<name>/`. If your
  agent script chowns its log directory (don't), the sweep fails
  silently.
- The audit log is **never** swept. It rotates at 32MB into
  `audit.log.<YYYYMMDDTHHMMSS>` segments, and those segments stay
  forever — it is the only forensic artifact dotagent keeps, so pruning
  it is your call, not a sweeper's. Plan for ~1KB per event, roughly a
  hundred events/day.
- `state/windows/` is swept by the same 03:00 pass, on
  `[state] window_retention_days` (default 30). A 15-minute agent writes
  ~96 files a day there, so a daemon that never sees 03:00 accumulates
  thousands. A window whose `.lock` is currently held is skipped — it
  gets collected on the next pass.

---

## Audit log

### Ask the log what it can still prove

```bash
dotagent audit verify --full
```

Start here for anything in this section. It prints one of four verdicts —
intact from `GENESIS`, intact since a named rotation, unexplained
truncation, or broken at a position — and exits non-zero for the last
two. `--json` for scripts. Without `--full` it checks only the live
`audit.log`, which is what the daemon does at boot.

Full output reference: [`reference/cli.md`](../reference/cli.md#audit-verify).

### `audit_chain_broken` event in the log

The daemon detected tampering (or a partial write) of `audit.log`. The
event is itself a chained entry — investigation:

```bash
grep audit_chain_broken ~/.config/dotagent/audit.log | jq .
# {"event":{"event_type":"audit_chain_broken","position":42,...}}
```

`position` is the line number where the chain broke. Read context:

```bash
sed -n '40,45p' ~/.config/dotagent/audit.log | jq .
```

Cause is almost always:

- Someone (or you) edited the file by hand.
- A crash mid-write left a half-line, or bytes cut inside a multibyte
  character. dotagent steps over that garbage on the next append rather
  than refusing to write — the line stays on disk and verification keeps
  reporting it, which is the loud half of the trade.
- Disk corruption.
- The head of the file was removed — see the next section, which is the
  one case that is easy to cause by accident.

dotagent continues operating — the new chain is anchored to the broken
position. Forensics is on you; `dotagent audit verify --full` names the
position and the segment.

### `position: 0` with `expected_prev_hash: "GENESIS"`

The first line of `audit.log` does not chain to `GENESIS`, and no
**seam** explains why. Verification calls this *unexplained truncation*:
somebody removed the head of the log.

The legitimate way for the file to start somewhere other than `GENESIS`
is rotation, which leaves a seam as line 1:

```bash
head -1 ~/.config/dotagent/audit.log | jq .
# {"event":{"event_type":"audit_log_rotated","rotated_to":"audit.log.20260806T101500",
#           "entries":38513,"tail_hash":"c8d2..."},"prev_hash":"c8d2..."}
```

If that line is missing, someone `sed`'d or `tail`'d the file. Deleting
a whole **segment** is fine and stays quiet — the seam lives in the
current file and keeps explaining the gap. Deleting lines from the
current file takes the seam with them, and that is what fires here.

### Rotated segments

```bash
ls -la ~/.config/dotagent/audit.log*
```

`audit.log` is live; `audit.log.<YYYYMMDDTHHMMSS>` are sealed segments,
oldest first by name. To read the whole history in order:

```bash
cat $(ls ~/.config/dotagent/audit.log.* 2>/dev/null) ~/.config/dotagent/audit.log | jq .
```

Removing old segments is supported and does not break verification —
it reports "intact since `<ts>`" instead of "intact from GENESIS". The
reasoning is in
[`security/threat-model.md`](../security/threat-model.md#what-the-hash-chain-guarantees-and-what-it-does-not).

---

## Plugin protocol

### Plugin always returns `ok=false`

```bash
# Run the exact payload dotagent would send.
echo '{
  "kind": "sink",
  "agent": "test",
  "schedule": "test",
  "event": "success",
  "message": "smoke",
  "config": <whatever your manifest sets>
}' | dotagent-plugin-<name> invoke
```

Stderr has the human-readable error. Stdout has the JSON response.

### Plugin works manually but not from the daemon

Cause is almost always **environment**:

- `$PATH` differences (covered in
  [Agent runs but fails](#symptom-dotagent-run-works-but-the-agent-fails-when-the-daemon-fires-it)).
- `$DOTAGENT_PLUGIN_PATH` not inherited by launchd. Set it in the
  plist's `EnvironmentVariables`.
- HOME differences (the plugin reads `~/.config/<x>` but the daemon's
  HOME is somewhere unexpected).

### Plugin info JSON is malformed — `dotagent doctor` won't parse it

Run the plugin directly:

```bash
dotagent-plugin-<name> info | jq .
```

If `jq` errors out, the plugin is printing log lines to stdout
(forbidden — stdout is reserved for JSON). Patch the plugin to use
stderr for logs.

---

## Performance

### Daemon CPU usage is high

The daemon **should** be near-zero CPU outside of a tick. If
`top`/`htop` shows persistent CPU:

- A plugin is spinning. `dotagent plugin list` then check each.
- A schedule with `interval_minutes = 0` or similar — fix it (manifest
  validation should catch this; file an issue if not).
- Verbose tracing flooding the file. `RUST_LOG=info` and retry.

### Agent timeout fires every run

Profile the agent outside dotagent (`time fish ./agent.fish`). If it
genuinely takes longer than `timeout_seconds`, bump the manifest. If
it's fast in your shell but slow under the daemon — that's an
environment difference (see env section above).

---

## When all else fails

```bash
# 1. State of the daemon
dotagent status
dotagent tick --dry-run
ps -p $(cat ~/.config/dotagent/state/daemon.pid 2>/dev/null) -o command= 2>/dev/null
launchctl print "gui/$(id -u)/run.avelino.dotagent" 2>&1 | head -30      # macOS
systemctl --user status run.avelino.dotagent                              # Linux

# 2. State of the configured agents
dotagent doctor
dotagent plugin list
for a in ~/.config/dotagent/agents/*/; do
    dotagent inspect "$(basename $a)"
done

# 3. Recent events
tail -100 ~/.config/dotagent/audit.log | jq -c
tail -100 ~/.config/dotagent/logs/daemon/dotagent.log | jq -c

# 4. Resource sanity
df -h ~/.config/dotagent       # disk full?
ls -la ~/.config/dotagent/logs/agents/                   # rogue agent filling logs?
```

If you still can't pin it down, open an issue with the output of (1)
through (4) and a redacted copy of the affected `agent.toml`.

---

## Related

- [`cli.md`](../reference/cli.md) — every subcommand at a glance
- [`daemon-lifecycle.md`](daemon-lifecycle.md) — start / stop / reload
- [`observability.md`](observability.md) — log streams + jq recipes
- [`paths.md`](../reference/paths.md) — where every file lives
- Plugin-specific troubleshooting under [`docs/plugins/`](../plugins/README.md)
