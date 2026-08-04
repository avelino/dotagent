# Threat Model

> What we defend against, what we don't, and why.

This document captures the security posture of dotagent — the assumptions, the
attack surface, the defenses we ship, and the defenses we explicitly chose to
defer. Read it before proposing security features; the goal is detection and
auditability, not impossible-to-break sandboxing.

## Premise

dotagent is a daemon running as the user that spawns arbitrary external
commands declared in `agent.toml` files on disk. Any attacker with the same
user privileges that the daemon has — write access to `~/.config`,
`~/Library/LaunchAgents`, the daemon binary path — can already do what dotagent
does, bypassing it entirely.

**We do not try to defend against a local attacker with user-equivalent
capability**. Doing so would require a privilege boundary dotagent cannot
provide (TPM / Secure Enclave / hardware key, kernel-level mandatory access
control, etc.) and would degrade the developer-experience that justifies
dotagent's existence.

What we DO target:

1. **Detectability**: every consequential action emits an event in an
   append-only, hash-chained audit log.
2. **Out-of-band notification**: critical events fire a notification
   plugin (iMessage / Pushover / etc.) so the user sees attacks from a
   different device.
3. **Documented intent**: manifests declare a `[security]` section so even
   if enforcement is partial, the *intent* is captured and reviewable.

## Threat vectors

### V1 — Manifest hijack

An attacker rewrites `~/.config/dotagent/agents/innocent/agent.toml` so that
`run.command` exfiltrates data the next time the agent fires.

**Mitigations:**
- **Manifest drift detection**: daemon caches sha256 of every loaded manifest
  in `~/.config/dotagent/state/known_manifests.json`. On the next load,
  compares hashes. Drift → audit event `manifest_drift_detected` → notify.
- **Audit log**: every `agent_run` records the manifest hash used. Forensic
  reconstruction of "what was the agent at time T".
- **Not mitigated**: if the attacker edits both `agent.toml` and the cache
  file before the next daemon tick, the change is invisible to dotagent.
  Sophisticated attacker → eventually detected only via out-of-band review.

### V2 — Phantom agent

An attacker drops a new directory `~/.config/dotagent/agents/spyware/` with
its own `agent.toml`. The daemon discovers it on the next tick and runs it.

**Mitigations:**
- **First-seen detection**: any new agent name not present in
  `known_manifests.json` triggers `phantom_agent_detected` → notify. The
  agent **still runs by default** because we chose not to require explicit
  registration (would block legitimate `git pull` workflows).
- **Not mitigated**: we don't gate execution on prior approval. This is a
  deliberate UX trade-off — the user gets a notification immediately, but
  the attacker gets one execution before the user can react.

> **Rationale for not implementing a registry**: a registry only works if
> the attacker respects it. An attacker with rwx on `~/.config` writes the
> registry along with the manifest. Defense is theatre.

### V3 — Daemon binary swap

An attacker replaces `/usr/local/bin/dotagent` with a trojan binary
(same CLI, additional behavior).

**Mitigations (planned, not in v0):**
- Self-hash check on daemon startup against
  `~/.config/dotagent/daemon.sha256`. Mismatch → notify, daemon refuses
  to start.
- Optional macOS code signing: notarized binary, gatekeeper verifies.

### V4 — Plugin swap

An attacker replaces a plugin binary (`dotagent-plugin-sink-roam`)
with one that exfiltrates payloads.

**Mitigations (planned, not in v0):**
- Plugin hash registry analogous to manifest tracking.
- `doctor` reports plugin resolution path; user can spot oddities.

### V5 — Input flowing into spawn

An attacker controls input that ends up in `Command::new(...).arg(...)`
without sanitization.

**Mitigations:**
- `RunConfig::args` is treated as token list, never passed through a shell.
  We do not `Command::new("sh").arg("-c").arg(...)` user-controlled strings.
- `EnvConfig::extra` keys/values are typed `String`, not interpreted.
- Plugin invocation uses subprocess with JSON stdin — plugin output is
  parsed as JSON, not eval'd.

### V6 — Secrets leak via stdout

A plugin logs its config (containing secrets) to stdout, which dotagent
captures in audit log entries.

**Mitigations (partial):**
- Convention: plugins MUST NOT log config to stdout. Only the JSON response
  goes to stdout; human logs go to stderr.
- Audit log policy: stderr tails (5 lines) appear in `attempt_failed` /
  `given_up` events. Plugins are responsible for not putting secrets in
  stderr either.
- **Not mitigated**: a misbehaving plugin can leak via the response JSON.
  Audit log will contain it. User is responsible for plugin trust.

### V7 — Resource exhaustion / fork bomb

A malicious manifest sets `max_retries = 1000000` with
`retry_backoff_minutes = [0]`.

**Mitigations (planned, not in v0):**
- Hard clamp `max_retries <= 32` in the runtime.
- Hard clamp `retry_backoff_minutes[i] >= 1` in the runtime.
- Concurrent run cap (default 10 daemon-wide, 1 per agent).

### V8 — Untrusted inbound message causes a local run

**This vector is different in kind from V1–V7, and it is worth saying so
plainly.** Every other threat here assumes an attacker who already has
user-equivalent access to the machine. Inbound Telegram
([`concepts/telegram.md`](../concepts/telegram.md)) is the first path where
someone with **no access at all** can cause a local process to run. The premise
above still holds for everything else; this section is the exception carved out
of it.

The exposure exists only when `[telegram]` is configured. It is off by default,
and dotagent opens no inbound path you did not ask for.

**What an attacker needs:** the bot token (leaked from `secrets.env`, a backup,
a screenshot) or the ability to message a bot they discovered. Telegram bots are
enumerable, so assume discovery is free.

**Mitigations:**

- **Numeric allowlist.** `allowed_user_ids` gates every message. `@username` is
  changeable and therefore never used for authorization.
- **Empty means nobody.** A token configured with an empty allowlist leaves the
  ingress off and logs why. Reading empty as "no restriction" would turn one
  forgotten line into an open remote-execution endpoint.
- **The message never supplies a command.** It selects among agents already
  installed on disk. An agent name that does not resolve is refused, and nothing
  from a trigger reaches a shell — the message body travels in
  `AGENT_TRIGGER_PAYLOAD`, not argv.
- **Trigger env cannot shadow runner env.** Per-invocation variables are applied
  before the `AGENT_*` block, so a payload cannot redefine `AGENT_NAME` or
  `AGENT_HEARTBEAT_FILE`.
- **Rate limit per sender**, default 10/minute, so one sender cannot occupy the
  daemon indefinitely.
- **Audit.** Accepted messages emit `trigger_received` then `agent_triggered`;
  refusals emit `trigger_rejected` at `Critical`, which fires out-of-band
  notification. An unlisted sender means somebody found your bot, and you should
  hear about it from a different device.
- **No message text in the audit log.** It records the sender id and chat id.
  Bodies are content, not attribution, and a chat can contain anything you
  pasted into it.

**Not mitigated:**

- An allowlisted account that is itself compromised (stolen Telegram session)
  can run any installed agent. The allowlist authenticates an account, not a
  person.
- A dispatcher agent that ignores the catalog and interprets message text
  itself re-opens everything this section closes. The
  [`telegram-assistant` example](https://github.com/avelino/dotagent/tree/main/examples/telegram-assistant)
  routes only through MCP tools for exactly this reason.

### V9 — MCP client reaches the agent catalog

`dotagent mcp` ([`reference/mcp.md`](../reference/mcp.md)) exposes every
installed agent as a callable tool.

**Mitigations:**
- stdio transport only. No port, no listener, nothing reachable from another
  machine. The server inherits the trust of whatever spawned it, and a local
  MCP client already runs as you.
- The catalog is the boundary: an agent that is not installed cannot be run.
- Arguments are a token list, never a shell string — same rule as V5.

**Not mitigated:** any local process that can spawn `dotagent mcp` can run any
installed agent. That is user-equivalent capability, which the premise above
already places outside the boundary.

## Defenses shipped in v0 (with the daemon engine)

| Defense | Status | Scope |
|---|---|---|
| Audit log (hash-chained, append-only) | ✅ v0 | All `agent_run`, `agent_failed`, `agent_recovered`, `manifest_*`, `plugin_*` events |
| Out-of-band notification on critical events | ✅ v0 | `given_up`, `phantom_agent_detected`, `manifest_drift_detected`, `audit_chain_broken` |
| `[security]` schema in manifest | ✅ v0 schema-only | Parses + `doctor` warns on inconsistency. **Enforcement is post-v0** — see below. |
| Manifest drift detection | ✅ v0 | sha256 cache + notify on mismatch |
| Phantom agent detection | ✅ v0 | first-seen detection + notify |
| Broken manifest does not hide healthy agents | ✅ | A failed parse is skipped and audited as `manifest_invalid` (Critical). Previously one bad file aborted the whole scan, leaving the daemon with zero agents and nothing but a log line. |
| Trigger env cannot shadow runner env | ✅ | Per-invocation variables are applied before the `AGENT_*` block, so an untrusted payload cannot redefine `AGENT_NAME` or `AGENT_HEARTBEAT_FILE`. |

### The `[security]` gap, stated plainly

`[security]` is **declared intent, not enforcement**. `allowed_commands`,
`filesystem_writable`, `network` and `env_passthrough` are parsed, reported by
`doctor`, and otherwise ignored by the runner. An agent that declares
`allowed_commands = ["jq"]` and then runs `curl` is not stopped.

That was a reasonable trade while every agent was code you wrote for yourself.
It reads differently since **V8**: there is now a path where a message from the
internet chooses *which* declared agent runs. The choice is still confined to
what you installed — that part is real — but the blast radius of the agent it
picks is bounded by nothing except what that agent's own code does.

Concretely, `examples/telegram-assistant` declares
`allowed_commands = ["bash", "jq", "claude", "dotagent", "perl"]`. Today that
list is a comment. Treat it as documentation of what the author intended to
need, not as a control that holds.

Until enforcement lands, the honest mitigations are the ones outside dotagent:
outbound firewall, disk encryption, and not installing an agent you have not
read.

## Defenses deferred (with rationale)

| Defense | Why deferred |
|---|---|
| Manifest signing (minisign / GPG / age) | Key rotation UX is hard pre-1.0. Will be opt-in. |
| Agent registry with explicit approval | Defense is theatre — attacker with rwx edits registry too. UX cost is high. |
| Real sandbox (sandbox-exec / bwrap / firejail) | Cross-platform sandboxing is its own product. The `[security]` schema lands first; enforcement lands as a follow-up tracked by issue. |
| Daemon binary self-hash | Useful but low-impact pre-1.0. Adds it when there's distribution channel beyond `cargo install`. |
| Plugin signing | Same reasoning as manifest signing. |
| TPM / Secure Enclave-backed signing | Out of scope for v0. Architectural note: the audit log + `dotagent approve` flow could anchor to a Secure Enclave key later. |
| Hard resource limits (max_retries clamp etc.) | Will land before 1.0. Currently we trust the manifest author. |

## What the user is expected to handle

dotagent is **one layer** of defense, not the only one. We assume the user
runs:

- **Disk encryption** (FileVault / LUKS) — kills offline manifest tampering.
- **SSH key passphrase** + 1Password/secrets manager — kills credential
  theft via filesystem.
- **Outbound firewall** (Little Snitch on macOS / nftables on Linux) — kills
  exfiltration even if dotagent is hijacked.
- **Regular backups / snapshots** — recovery, not prevention.
- **macOS TCC permissions reviewed yearly** — Full Disk Access creep.

dotagent's `doctor` command will eventually warn when these are not in
place (best-effort detection).

## Convention for adding new event types

When introducing a new audit event, decide:

1. **Severity**: `info` (run-of-the-mill), `notice` (worth grep'ing later),
   `critical` (notify out-of-band).
2. **Schema**: event fields are typed. Use existing fields where they fit
   (`agent`, `schedule`, `manifest_sha256`) before inventing new ones.
3. **`security-reviewer` agent**: run it against the change. The agent in
   `.claude/agents/security-reviewer.md` knows the threat model and will
   flag if the new event widens attack surface.

## Reporting issues

Security issues: open a GitHub issue with `security` label, or email the
maintainer privately. Do not include reproduction code in public issues
before discussion.
