# FAQ

Quick answers to recurring questions. If yours isn't here, try
[Troubleshooting](guides/troubleshooting.md) first.

---

## Installation & support

### Does dotagent run on Windows?

**No.** dotagent uses `kill(2)`, `osascript`, launchd, systemd, and Unix
signal handling. WSL works (Linux paths apply). Native Windows is out
of scope.

### Does it need root / sudo?

**No.** dotagent runs as your user. The daemon is registered as a
**user agent** (launchd `gui/<uid>` domain) / **systemd user unit**.
There's no system-wide install, no setuid binary, no privileged
helper.

The only step that might surface a prompt is macOS Full Disk Access /
Automation entitlement when a `sink-file` writes into protected dirs
(`~/Documents`, `~/Downloads`) or the `imessage` notifier sends through
Messages.app. Those are per-user TCC grants, not root.

### Is there a stable release?

**Pre-release** as of this writing. The CHANGELOG / GitHub Releases
page is authoritative.

What's stable:

- The `agent.toml` manifest schema (additive changes only)
- The plugin protocol (verbs + JSON payload shape)
- The heartbeat file shape (compatible with the legacy Fish framework)

What might still move:

- `config.toml` may grow new sections.
- `dotagent daily-summary` currently has a hardcoded default target —
  that becomes `config.toml`-driven before 1.0.
- A couple of CLI subcommands are stubs (`bootstrap`, `plugin invoke`).

### Will it run on a server (headless)?

**Yes**, with caveats:

- `desktop` notifier needs a graphical session (`NSUserNotification` on
  macOS, D-Bus on Linux). Headless? Skip that driver — `slack`, `ntfy`,
  `pushover` are pure HTTPS and work fine.
- `imessage` is macOS GUI-only.
- On Linux, enable lingering so the systemd user session survives
  logout: `loginctl enable-linger $USER`.

### Multi-user / multi-tenant?

dotagent runs **per-user**. The state directory is
`$DOTAGENT_HOME` (default `~/.config/dotagent`). Two users on the same
machine have completely independent daemons, manifests, audits — none
shared.

If you need cross-user orchestration, run one daemon per user OR build
a thin shared layer (queue, shared filesystem). dotagent isn't a
multi-tenant scheduler.

---

## Concepts

### Why a daemon and not just cron?

`cron` runs an entry. `dotagent daemon` runs **and monitors** an entry.
The difference is whether you can answer "did the 08:30 run succeed?"
without writing your own bookkeeping. Specifically, cron can't:

- Retry on failure with backoff.
- Tell you a window passed without being satisfied.
- Notify on `attempt_failed` / `given_up` / `recovered` semantics.
- Detect manifest tampering or phantom agents.
- Run preflight checks before the body.
- Spawn one process per agent without forking the scheduler.

If your task is "run this thing every Monday at 9am, fire-and-forget,
don't care what happens" — `cron` is enough. If you care whether it
*actually ran*, you wanted dotagent (or kubernetes cronjobs, or
nomad jobs — same family).

### Why not launchd / systemd timers directly?

Same answer as cron, plus a deeper one: dotagent uses launchd / systemd
to keep the **daemon** alive, then does adaptive scheduling itself.
That's intentional:

- Generates **one** unit, not one-per-agent. Adding an agent is a `.toml`
  drop, not a unit-install + reload + boot dance.
- Adaptive sleep — wake at the next event, not "tick every minute".
  100 schedules cost the same as 10.
- Per-(agent, schedule) retry policy. launchd has `RetryDelay` for the
  whole unit; systemd has `Restart=on-failure` for the whole unit. No
  notion of "this window failed".
- Cross-platform — `agent.toml` is identical on macOS + Linux. The
  launchd plist / systemd unit is a generated artifact, not your
  source of truth.

### Why is this not just kubernetes cronjobs?

It is, ish. If you're already on a k8s cluster, k8s cronjobs are
fine. dotagent is for the **laptop / single-user / on-device** version
of the same problem — orchestrating personal automations without
running k8s on your laptop. Different deployment target, similar
shape.

### Why is the agent a subprocess and not a library?

- **Crash isolation.** A panic in your agent doesn't take down the
  daemon.
- **Polyglot.** Fish, Python, Go, Rust, bash, a compiled binary — all
  first-class. No SDK to import.
- **No ABI.** No `cdylib` versioning headaches, no FFI breakage.

Trade-off: fork+exec per run (~5-10ms on Apple silicon). dotagent fires
agents on **discrete events** (a schedule window), not in hot loops, so
the cost is invisible.

### Why is each notifier in-process but plugins are subprocess?

Different fire frequency:

- **Notifiers** fire on **every** failure attempt. A 20-retry agent
  fires the notifier 20 times. fork+exec × 20 + JSON marshal/unmarshal
  × 20 was real overhead. In-process is faster and removes the
  dependency on extra binaries on `$PATH`.
- **Sinks / preflight** fire **once per run**. The fork+exec cost is
  invisible.

The plugin protocol stays alive for third-party notifiers (Discord,
Teams, etc.) via `driver = "plugin"` — escape hatch when the built-in
drivers don't cover your case.

### Why isn't there an LLM SDK?

dotagent runs scripts. The script decides whether and when to call an
LLM. This is intentional:

- Most "agent" loops don't need an LLM 80% of the time. Pay tokens
  only when judgment is actually required.
- Picking the model / prompt / cost / retry shape is **your call**, not
  dotagent's. Lock-in to a vendor SDK is not what you want from an
  orchestrator.

See [`concepts/agents.md`](concepts/agents.md#the-dotagent-definition)
for the philosophy.

---

## Workflow

### Can I debug an agent without the daemon?

Yes — three options:

```bash
# 1. Foreground run, no plugins fire.
dotagent run <name> --schedule <id>

# 2. Foreground run, plugins fire (preflight / sink / notify).
dotagent run-now <name>

# 3. Direct invocation (no dotagent involvement at all).
cd ~/.config/dotagent/agents/<name>
fish ./agent.fish        # or python3 ./agent.py, etc.
```

For (3) you won't get the `AGENT_*` env vars — write your script to
handle empty / unset gracefully if you want this to work.

### How do I trigger an agent immediately?

```bash
dotagent run-now <name> [--schedule <id>]
```

This fires the agent now, ignoring schedule windows. Preflight, sinks,
and notifiers all run as usual.

### How do I see what the daemon is about to do?

```bash
dotagent tick --dry-run
# → (dry-run) scanned 4 agent(s); would dispatch 1; next event: 2026-05-19T08:30:00-0300
```

That `next event` timestamp is when the daemon will next wake.

### How do I tail logs?

```bash
dotagent logs <name>            # last 50 lines, one agent
dotagent logs <name> --follow   # tail -F
dotagent logs --follow           # tail -F across every agent at once
dotagent logs <name> -n 200     # last 200 lines
```

For the daemon's own structured logs:

```bash
tail -F ~/.config/dotagent/logs/daemon/dotagent.log | jq .
```

### How do I clear state for a single agent?

```bash
rm -rf ~/.config/dotagent/state/agents/<name>/
rm  ~/.config/dotagent/state/windows/<name>-*
```

dotagent will re-run from scratch on the next dispatch. Your manifests
and agent scripts aren't touched.

### How do I version-control my agents?

Two patterns:

**Pattern A — agents in dotfiles**:

```bash
mkdir -p ~/dotfiles/agents/<name>
# … develop there, version with git …
ln -s ~/dotfiles/agents/<name> ~/.config/dotagent/agents/<name>
```

dotagent follows the symlink. Your agent stays under git; only the
symlink lives in `~/.config/dotagent/`.

**Pattern B — agents in their own repo**:

```bash
git clone git@github.com:you/my-agents.git ~/agents
# … one directory per agent, each with an agent.toml …
ln -s ~/agents/<name> ~/.config/dotagent/agents/<name>
```

If you use Nix / home-manager, there's a reference module in
[avelino/dotfiles](https://github.com/avelino/dotfiles) that handles
discovery + symlinking automatically. See
[`migrating-from-fish.md`](guides/migrating-from-fish.md).

---

## Security

### Is dotagent sandboxed?

**Not in v0.** The `[security]` block in `agent.toml` is parsed and
`dotagent doctor` warns on inconsistency, but the runner doesn't
enforce `allowed_commands` / `filesystem_writable` / network policy.

Sandbox integration (sandbox-exec on macOS / bwrap / firejail on Linux)
is on the roadmap. See [`security/threat-model.md`](security/threat-model.md)
for the full posture and what *is* defended in v0 (audit log, drift
detection, phantom-agent detection, out-of-band notify).

### Does dotagent need disk encryption / outbound firewall / etc.?

**Yes** — see [the user-expected layers in the threat model](security/threat-model.md#what-the-user-is-expected-to-handle).
dotagent is **one layer** of defense; we assume FileVault / LUKS,
Little Snitch / nftables, and a passworded SSH key are in place. Without
those, dotagent's audit log lets you reconstruct what happened, but
doesn't prevent the attacker from doing it.

### What gets sent to third parties?

By default: **nothing**.

- Slack / ntfy / Pushover notifiers: **only** when configured per agent.
  Each delivery POSTs the agent name + schedule + event + message body
  to the configured webhook.
- OpenTelemetry export: opt-in via `[telemetry] otlp_endpoint`. The
  configured endpoint receives span metadata only (no log bodies yet).
- iMessage notifier: shells out to `osascript` — Apple's privacy policy
  applies.
- `mcp` CLI (if your agent uses it): whatever your `mcp` config says.

There's no telemetry pinging an Anthropic/Avelino-owned endpoint. No
update-check call-home. No analytics.

### Can I run plugins I didn't write?

Treat them like any binary you put on `$PATH` — same trust
assumptions. The audit log records every invocation with `ok` / `error`
and the resolved binary path, so misbehaving plugins are at least
**detectable** after the fact. See
[V4 in the threat model](security/threat-model.md#v4--plugin-swap).

---

## Limits

### How many agents can I run?

Soft answer: hundreds, easily, on a modern laptop. The scheduler is
adaptive — sleep until the next event, not poll. The bottleneck is
fork+exec, which happens **only** when an agent fires, not on every
tick.

Hard answer: there's no internal cap. dotagent has been tested with
9 production agents firing 14+ times/day each.

### Can I run a sub-second schedule?

No, and you shouldn't. `interval_minutes` minimum is 1 minute, and
spawn overhead is real (5-10ms × language + runtime). If you need
high-frequency work, write a long-running service and don't use
dotagent.

### What's the max agent timeout?

`timeout_seconds: u64` — practical limit is "however long you're
willing to wait". Default is 1800 (30 min). Set higher for slow agents
(some FinOps reports take 20+ min). dotagent sends SIGTERM, waits 5
seconds, then SIGKILL.

---

## Trivia

### Why "dotagent"?

Two reasons:

1. "dotfiles for agents" — the same convention that turned shell
   config into version-controllable text turns scheduled automations
   into version-controllable text.
2. `.` (dot) is the smallest unit of file naming. The whole point of
   dotagent is staying out of the way: agent.toml is small, the binary
   is one, the daemon is one process. Minimal noise.

### Why launchd label "run.avelino.dotagent"?

Reverse-DNS label, same convention Apple uses internally for system
agents. `avelino` because the project was originally personal to one
user; the label may genericize before 1.0. The daemon's behavior is
unchanged regardless.

### Will dotagent run my LLM agents?

It'll run **scripts that call** your LLM agents. There's no LLM in
dotagent itself. See [`concepts/agents.md`](concepts/agents.md) for
the "script controls, AI analyzes" philosophy.

### Is there a UI?

No. `dotagent status` is the textual dashboard. The structured logs
are JSON — pipe to whatever (Grafana / Honeycomb / your dashboard
tool). A web UI is **not** on the roadmap.

---

## Related

- [Installation](getting-started/installation.md) — install paths
- [First agent](getting-started/first-agent.md) — zero → daemon
- [CLI reference](reference/cli.md) — every command
- [Troubleshooting](guides/troubleshooting.md) — sintoma → fix
- [Threat model](security/threat-model.md) — what's defended, what isn't
