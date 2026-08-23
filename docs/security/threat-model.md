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

### V10 — Skill script execution

A skill ([`concepts/skills.md`](../concepts/skills.md)) may package executables
under `scripts/`, and `skill-run` executes one on request from an MCP client.
Two things are new. Code runs **outside any manifest**, so nothing in
`agent.toml` describes or bounds it. And the search path includes
`~/.claude/skills/`, a directory that exists for another tool and that the
operator did not create for dotagent.

**Mitigations:**
- **Path containment.** Absolute paths and `..` are refused before touching the
  disk; the canonicalized target must still sit under the canonicalized skill
  directory, which is what catches a symlink pointing outside.
- **`scripts/` only, executable bit required.** A `references/` document is not
  runnable, and dropping a helper beside an entry point does not silently make
  it callable — the author names entry points by chmod.
- **Supervised.** Every execution goes through `dotagent-supervisor`: enforced
  deadline (`timeout_seconds`, default 300) and kill-tree on expiry, so a
  script that spawns children cannot leave orphans — and the guarantee now
  survives the daemon itself. A daemon killed mid-run used to leave its
  children with nobody holding their deadline; the next boot re-reads the
  supervisor snapshot and kill-trees what it can prove was left behind. See
  [Boot orphan reap](../guides/daemon-lifecycle.md#boot-orphan-reap).

  The reap is **deliberately incomplete**, and that is the security position.
  Killing the wrong process is a worse outcome than an orphan surviving, so a
  record is only signalled when group leadership, start time and command line
  all still match what was recorded. A recycled pid fails at least one of them
  and is refused. Unreadable snapshot, missing identity fields, another daemon
  alive: all abort without signalling. Bounding an escaped script is a
  best-effort mitigation here, never a licence to signal a pid read off disk.
- **Arguments are a token list**, never a shell string — same rule as V5.
- **Audited.** Each run appends `skill_invoked` (skill, script, exit code,
  whether it timed out). Without it, "what ran on this machine" would have a
  hole exactly where a manifest cannot answer.

**Not mitigated:** anyone who can write to a skill directory can put a script
there and have it run. That is the same statement V1 makes about manifests, and
it now applies to `~/.claude/skills/` too — a directory whose contents may have
been installed for a different tool with different expectations. Narrow the
search path if that matters:

```toml
[skills]
claude_skills = false
paths = ["/Users/me/.config/dotagent/skills"]
```

Turning skills off entirely (`[skills] enabled = false`) removes both the tools
and the execution path.

### V11 — Commands

A command ([`concepts/commands.md`](../concepts/commands.md)) is a prompt with
no `scripts/` equivalent. Installing one grants **no execution**: everything it
can cause is already reachable through the tools the dispatcher holds. That is
the whole difference from V10, and it is why this entry is short.

What remains is **influence** — a command file is text a model is told to
follow, so anyone who can write the directory can steer an assistant that has
real tools. Two smaller edges come from Telegram itself.

**Mitigations:**
- **No new execution vector.** The daemon resolves nothing; `command-get`
  returns text. Arguments are substituted textually and never reach a shell.
- **`~/.claude/commands/` is off by default**, the reverse of `claude_skills`.
  A directory maintained for another tool does not become a published menu
  because dotagent happened to find it.
- **Per-chat menu scope.** `setMyCommands` is registered against each
  allowlisted chat rather than `BotCommandScopeDefault`, so command names and
  descriptions are not published to anyone who finds the bot. The allowlist
  already gates execution; this keeps the *catalog* from leaking too.
- **Screening order.** The allowlist and rate limit are checked before any
  command is parsed or answered, so an unlisted sender cannot enumerate the
  catalog with `/help`, and a sender looping `/typo` spends the same budget as
  one looping prose.
- **Audited by name only.** `command_dispatched` records the command and
  whether it was `known`; arguments are content, the same reason
  `trigger_received` never records a message body. Repeated `known: false` from
  one sender is what probing looks like.

**Not mitigated:** a command file is trusted input to the model, exactly as a
skill body is. Write access to the commands directory is write access to what
the assistant is told. `[commands] enabled = false` removes the menu and both
tools.

### V12 — Remediation from a chat message

A `[[preflight]] remediation` ([`concepts/agents.md`](../concepts/agents.md))
is a command an assistant can run, and the request to run it arrives over
Telegram. That is V8's shape with a sharper edge: the point is to change the
machine, not to read from it.

The rejected design is worth naming, because it is the obvious one. A
preflight plugin already returns `suggest` — the command that would clear the
check — and letting an assistant run that string is one line of code. It is
also arbitrary execution: the string is written by a plugin, and any plugin
could then have anything run by asking an assistant nicely.

**Mitigations:**
- **Declared, not suggested.** The command comes from the operator's manifest.
  `suggest` is never executable; it stays a message.
- **The catalog is the boundary**, as with agents: `tools/list` publishes one
  entry per declared remediation, a name that is not there is not callable,
  and the tool takes no arguments — so the model chooses *which*, never *what*.
- **argv, not a shell.** `remediation = "x && curl … | sh"` runs a program
  named `x` with those literal arguments and fails. There is no shell to
  interpret the operators.
- **Supervised**, with a 120-second deadline and kill-tree on expiry.
- **Audited** as `remediation_invoked` at `Critical`, recording the command as
  declared. It is the one event where a chat message changed the machine.
- **It does not re-run the agent.** Clearing the check and dispatching a run
  stay two decisions.

**Not mitigated:** an operator who declares a dangerous command has declared
it. This moves the trust to the manifest, where V1 already puts it — someone
who can write your `agent.toml` can already set `run.command`.

### V13 — State carried between senders by a persistent agent

`[lifecycle] mode = "persistent"` ([`concepts/lifecycle.md`](../concepts/lifecycle.md))
keeps an agent process alive between runs. That process holds whatever it
holds — a conversation, a cached credential, a decision someone authorized —
and every later request lands in the same memory.

The failure is not a bug in the pool. It is a manifest that declares
`mode = "persistent"` without a `key` on an agent that serves more than one
person: every conversation shares one process, so what one sender said is
context for the next one's answer. Nothing about it looks wrong until two
people use the bot, which is exactly the shape of a leak that ships.

**Mitigations:**
- **`key` shards by an attested field.** `key = "chat_id"` gives one process
  per conversation. The value is a *selector over the payload the daemon
  already attested* — the sender names nothing.
- **The key never reaches a label raw.** It is reduced to `[A-Za-z0-9_-]`, and
  anything else — path separators, whitespace, 4 KB of chat text — becomes a
  stable digest instead. Two distinct values never collapse to one process, so
  sanitizing cannot merge two conversations.
- **`doctor` warns** when the Telegram `dispatcher_agent` is persistent with no
  key, because that is the configuration where this actually bites.
- **`max_invocations` bounds how long any one process accumulates**, and
  `max_instances` bounds how many exist. Both default to finite values.
- **Recycling is audited** as `persistent_agent_recycled` with the reason, so
  "why did it forget" has an answer.
- **One pool, inside the daemon.** There is no second instance to race it,
  which is what an external pool needs `flock` for.

**Not mitigated:** an agent that writes its own state to disk, keyed however it
likes. dotagent isolates the *process*, not the filesystem — `[security]
filesystem_writable` is still schema-only (V-deferred, below).

### V16 — Assistant harness: registry and memory as new state

`[assistant]` moves conversation bookkeeping into the daemon, which makes two
new files interesting to an attacker with user-level access:

**The registry** (`state/assistant/<source>-<session>.json`, 0600, atomic
writes) holds pointers only — model session id, generation, toolkit hash,
transcript size. It deliberately has no field that can hold chat text, so
reading it yields "which session served chat X", never what was said. The
deeper risk is *writing* it: a poisoned pointer makes the next reply resume
an attacker-chosen model session. That is a same-user attack surface the
premise already accepts (the attacker could edit the manifest and get
arbitrary execution directly); the registry adds no privilege.

**The memory workspace** (`outl/`) accumulates `MEMO:` facts — flushed from
assistant replies, and from the stdout of any agent whose manifest declares
`[memory]`. Trust boundary: the facts are *agent-produced text*, not attested
input — a compromised or prompt-injected agent can poison its own memory
with false facts that later color future replies. It cannot write another
conversation's registry, and `memory-recall` output is bounded (2 KiB per
turn), but "the assistant believes wrong things" is a real failure mode.

Mitigations: the workspace is a plain outl graph the user can open, read and
edit (a bad memory is visible and correctable by hand); every fact records
which agent, source and session wrote it as block properties, so one poisoned
run's output can be found and removed as a group; `dotagent memory forget`
and `memory-supersede` make that correction a command rather than a
hand-edit; and recall failure degrades to an empty block, never to a blocked
conversation.

Capture from a plain agent is **opt-in per manifest and successful runs
only**. Both bounds are about trust rather than tidiness: an agent that never
declared `[memory]` cannot write facts at all, and a failing run — the state
an attacker is most likely to be able to force — cannot file its output as
durable truth.

**Not mitigated:** prompt injection through message content that ends up in
a `MEMO:` line verbatim. The memory stores what the agent claimed. Recorded
provenance narrows *which* run to distrust, and editing or forgetting the
fact is the correction path — but nothing validates a claim at write time.

### V14 — Notifier credentials written to the daemon's own log

V6 is about a *plugin* leaking secrets outward. This is the inverse, and it
does not need a misbehaving anything: dotagent leaking its own notifier
credentials into a file it writes itself.

`reqwest::Error`'s `Display` appends ` for url (…)`. For two built-in drivers
the URL **is** the credential —
`hooks.slack.com/services/T…/B…/<secret>` and
`api.telegram.org/bot<token>/sendMessage`. Any `?` that converted such an error
into a `NotifyError` reached `warn!(driver, error = %e, "notifier failed")` and
landed in `~/.config/dotagent/logs/daemon/dotagent.log.*` — live credentials,
in a file with a 30-day retention window, from a failure as mundane as a DNS
blip. Nothing was compromised to cause it; the notifier just had a bad day.

**Mitigations:**

- **The leak is unrepresentable, not merely unwritten.** `NotifyError` has no
  `From<reqwest::Error>` — the variant was **removed from the enum**, so `?` on
  a `reqwest` call does not compile. A driver has to route transport failures
  through `redact::sanitize_reqwest_err`, which keeps only the failure kind and
  the HTTP status (`slack transport error (timeout, status 503)`). Deleting the
  conversion is what stops the next driver from reintroducing this by accident;
  fixing the log line would not have.
- **API error bodies are scrubbed before logging.** Telegram's own error text
  happily echoes the request URL back, so the token is blanked in the
  description — both as a whole string and as its trailing high-entropy
  segment, since a truncated echo can carry the half that matters.
- **ntfy `base_url` userinfo is redacted**, because a self-hosted URL may
  legitimately embed HTTP basic credentials.
- **Credentials do not have to be in the manifest at all.** Every
  credential-bearing field takes `${VAR}`, resolved at send time from
  `secrets.env` — see [`concepts/secrets.md`](../concepts/secrets.md). The
  expansion error names the field and the variable only; it never echoes a
  resolved value or the literal input, because the input is a credential
  template and the store holds sibling credentials.

**Not mitigated:** an operator who writes a literal credential into
`agent.toml` and commits it. dotagent cannot un-publish a git history. The
`${VAR}` path exists so this never has to happen; `doctor` does not currently
flag literals that look like credentials.

### V17 — Installed binaries reachable from a chat message

`[os]` lets an assistant run binaries that are already on the machine. It is
the widest inbound path in this document, and the only one where the operator
declares what *may* run while a model chooses the arguments. Everything else
here either fixes the whole command in a manifest (V12) or ships the code
being run inside the repo (V10).

The honest framing: an allowlist of whole binaries is not a sandbox. `git log`
cannot write to the repo, but `git` with the right flags can run a pager, and
a binary that takes a `--exec` style flag will do what that flag says. What
the allowlist buys is that the set of programs is finite, declared, and
auditable — not that every invocation inside it is harmless.

**Mitigations:**
- **Off by default, and empty by default when on.** `enabled = true` with no
  `allow` list runs nothing. Reaching "on" by accident is a state that should
  do nothing rather than everything.
- **The catalog is the boundary**, as everywhere else: with `[os]` off, the
  `os-run` and `os-list` tools are absent from `tools/list`, so a client sees
  no such capability rather than one that refuses.
- **Whole-token matching.** `kubectl get` admits `kubectl get pods` and
  refuses `kubectl delete` and `kubectl getsecrets`. Granularity is per entry,
  so a binary that only reads can be listed bare and one that can change
  production can be pinned to its safe subcommands.
- **A name, never a path.** `/bin/sh`, `./sh` and anything carrying `..` are
  refused before the list is consulted, so a path cannot impersonate a listed
  name. Resolution goes through `PATH`.
- **argv, not a shell.** Arguments reach the program as a token list. `|`,
  `&&` and `$(…)` in an argument are literal characters, the same rule as V5.
- **Supervised**, with the configured deadline and kill-tree on expiry.
- **Audited** as `os_command_invoked` at `Critical`, recording the binary and
  **the full argument list** — the half that no manifest declared.

**The `!` prefix.** A message starting with `!` is run directly: `!rg foo`
spawns `rg` with one argument and never reaches the dispatcher. It is the same
policy through a different door, and it is *narrower* than the model's door in
the way that matters — a typed line cannot be steered by content, so the
prompt-injection surface that makes `os-run` risky does not exist here.

- **Screened first.** The prefix is read after the Telegram allowlist and the
  rate limit, never before. Reading it earlier would make `!` a way past both.
- **The same `[os] allow` list.** A typed line is not trusted further than a
  chosen one, because the allowlist authenticates an account and not a person
  (V8): a stolen session types `!` just as well. Without this, one compromised
  session would go from "runs what the operator allowed" to "unrestricted
  shell".
- **No session, no model, no memory.** Nothing is stored and no reply is
  paraphrased. Errors come back raw, which is the point of typing it yourself.
- **Not a shell.** Quotes group an argument; `|`, `&&`, `;` and `$(…)` are
  ordinary characters in an argument list. `!ls; rm x` asks for a binary
  literally named `ls;` and fails to find it.

**`deny` and `confirm`.** `deny` refuses always and beats `allow`, `*`
included — a list that could be widened past its own refusals would not be
one. `confirm` runs only after a person answers `!!` in the same conversation,
and it defaults to a non-empty list precisely because `allow = ["*"]` with no
brake is the configuration people reach for first.

Matching is per binary rather than per pattern. `confirm = ["rm"]` covers
`rm -rf /`, `rm -r -f /`, `rm -fr /` and `rm --recursive --force /`; a textual
`"rm -rf"` would cover the first and miss three. The shells are on the default
list for the same reason: with `*`, `sh -c` reaches every binary a
binary-name guard would otherwise catch.

**A model may not confirm.** `os-run` refuses anything on the `confirm` list
outright and tells the caller to have the person type it. The confirmation
exists so a human decides; a tool that could both request and grant it would
be theatre. This is the one asymmetry between the two doors, and it is the
point of having two.

Confirmations are held in memory, one slot per conversation, with a TTL. A
restart forgets them, which fails closed: the cost is retyping a command, not
a `!!` from yesterday landing on a machine whose operator has moved on.

**`allow = ["*"]`** admits every binary on `PATH`. It is a supported
configuration and it is the widest one: with a shell in reach, the mitigations
above stop bounding *what* runs and bound only *who can ask* and *what gets
recorded*. The inbound channel's own gate (Telegram's `allowed_user_ids`, the
socket's uid check) becomes the whole boundary, and `doctor` says so on every
run rather than letting it be configured once and forgotten.

**Residual risk, stated plainly:** an allowlisted binary is trusted with
whatever that binary can do. Listing `kubectl` bare on a machine with
production credentials means a chat message can reach production. That is a
choice the operator makes per entry, which is why the granularity exists and
why the default is an empty list.

### V15 — Local Unix-socket API

The daemon can expose a local client API at `$DOTAGENT_HOME/api.sock` when
`telegram.dispatcher_agent` resolves to a discovered manifest. A local process
can send messages to that socket and cause the configured dispatcher agent to
run. This is intentionally a same-machine transport, not a network endpoint.

**Mitigations:**

- **Socket permissions.** The listener is created with mode `0600`, so another
  uid cannot open it through the normal Unix-socket permission check.
- **Bind hygiene.** The daemon uses `lstat`-style metadata checks, refuses a
  non-socket file or symlink at the path, refuses a live listener, and removes a
  stale socket only when its uid is the current uid. A stale socket owned by
  another uid is left in place and reported as an error.
- **Peer attribution.** Linux `SO_PEERCRED` and Darwin local peer credential
  APIs provide uid and, when available, pid. The daemon carries that identity
  into the trigger actor used for audit entries. If the kernel cannot provide it,
  the actor is honestly recorded as `local` rather than guessed.
- **Bounded input and work.** The API caps connections at 16, requests at 30 per
  minute per connection, text at 32 KiB, session ids at 64 ASCII identifier
  characters, gateway conversations at 4, and each conversation queue at 64.
  A pending event queue is capped at 1 MiB per connection; a slow client is
  disconnected instead of pinning daemon memory.
- **Audit without transcript capture.** Gateway admission, rejection, and
  agent execution are auditable, with actor/session attribution. Message text
  is not copied into the audit log because it is content, not identity.
- **No public listener.** The current implementation has no HTTP or TCP mode.
  Mobile clients, remote clients, and token-based authentication are future
  transport decisions, not capabilities of this socket.

**Trade-offs and not mitigated:**

- `0600` is an OS-user boundary, not authentication. A process already running
  as the dotagent user can open the socket, submit arbitrary dispatcher input,
  and read the replies it requested. That is user-equivalent capability, which
  this threat model explicitly does not claim to prevent.
- Rate limits and gateway caps reduce accidental floods and resource pressure;
  they do not distinguish a trusted process from another process running as the
  same user. `session_id` is an opaque routing key, not an access-control
  credential.
- Local clients receive raw streamed stdout lines. A client with socket access
  can therefore see the output of the work it triggered; agents that persist
  transcripts or other sensitive state must enforce their own data boundary.
- A daemon crash can leave `api.sock` behind. The next daemon applies ownership
  and liveness checks before removing it, rather than blindly unlinking a path.

## Defenses shipped in v0 (with the daemon engine)

| Defense | Status | Scope |
|---|---|---|
| Audit log (hash-chained, append-only) | ✅ v0 | All `agent_run`, `agent_failed`, `agent_recovered`, `manifest_*`, `plugin_*` events. Rotates at 32MB across a hash **seam**; segments are never deleted automatically. See [what the chain guarantees](#what-the-hash-chain-guarantees-and-what-it-does-not). |
| Out-of-band notification on critical events | ✅ v0 | `given_up`, `phantom_agent_detected`, `manifest_drift_detected`, `audit_chain_broken` (which now also covers a log whose head was removed with no seam accounting for it) |
| `[security]` schema in manifest | ✅ v0 schema-only | Parses + `doctor` warns on inconsistency. **Enforcement is post-v0** — see below. |
| Manifest drift detection | ✅ v0 | sha256 cache + notify on mismatch |
| Phantom agent detection | ✅ v0 | first-seen detection + notify |
| Broken manifest does not hide healthy agents | ✅ | A failed parse is skipped and audited as `manifest_invalid` (Critical). Previously one bad file aborted the whole scan, leaving the daemon with zero agents and nothing but a log line. |
| Trigger env cannot shadow runner env | ✅ | Per-invocation variables are applied before the `AGENT_*` block, so an untrusted payload cannot redefine `AGENT_NAME` or `AGENT_HEARTBEAT_FILE`. |
| Notifier credentials cannot reach `tracing` | ✅ | `NotifyError` has no `From<reqwest::Error>`, so a `?` that would log a webhook URL does not compile. Transport errors are reduced to kind + status; API error bodies are token-scrubbed. See [V14](#v14--notifier-credentials-written-to-the-daemons-own-log). |
| `${VAR}` for every credential-bearing notifier field | ✅ | `slack.webhook_url`, `ntfy.token`/`base_url`/`topic`, `pushover.token`/`user`, `telegram.bot_token`. Resolved at send time from `secrets.env` (0600-enforced), env as fallback. Unresolved = failed send, never the literal placeholder. |
| Local client API socket hygiene and backpressure | ✅ v0 | Unix socket `0600`, stale-path ownership checks, peer uid/pid attribution when available, connection/request/text/session/event-queue limits. This is not same-user authentication. See [V15](#v15--local-unix-socket-api). |
| Alerts that repeat while a failure holds | ✅ | `stale` and `given_up` re-notify on a rising ladder (entry, 1h, 6h, daily) rather than once. A monitoring channel that goes quiet while the failure persists is indistinguishable from one where nothing is wrong. |

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

## What the hash chain guarantees (and what it does not)

Worth stating plainly, because rotation made the question concrete and the
answer is easy to overstate.

**The chain detects partial edits. It has never detected a total rewrite.**

Each line carries `prev_hash = sha256(previous line)`. Change one line in the
middle and every hash downstream stops reproducing, so verification names the
position. Delete a line and the same thing happens. That is the guarantee, and
it is a real one: it turns "somebody edited the log" from invisible into loud.

But an attacker with write access to `audit.log` can recompute the whole file —
strip the entries they dislike, rewrite every `prev_hash` forward, and hand
back a file that verifies from `GENESIS` to the end. Nothing in dotagent stops
that, and nothing could without a key the attacker cannot reach (see
*Defenses deferred*: TPM / Secure Enclave anchoring). Per the premise at the
top of this document, an attacker with user-equivalent capability is out of
scope, and this is one of the places that shows.

### Rotation preserves exactly that property — no more, no less

Rotation renames the live file to `audit.log.<stamp>` and starts a new one
whose first line is a **seam** (`audit_log_rotated`) recording the segment
name, its entry count, and its tail hash. The seam's own `prev_hash` equals
that tail hash, so the chain crosses the rename with no gap.

The seam is what makes retention legible:

| On disk | Verification says | Why |
|---|---|---|
| all segments present | intact from `GENESIS` | every link checked |
| old segment deleted, seam present | **intact since `<ts>`**, naming the missing segment and its entry count | the seam survived in the current file, still covered by the chain, and explains the orphan |
| head of the live file cut off | **unexplained truncation** → `audit_chain_broken`, critical | the seam went with the deleted lines; nothing accounts for the remaining `prev_hash` |
| a line edited anywhere present | **broken at position N**, naming the segment | hashes stop reproducing |
| segment truncated at its end | **broken**, seam's `tail_hash` vs. actual | the seam pinned the tail before the segment left |
| a line nobody can parse | **broken at position N**, naming the segment | an entry that will not deserialize is as much a hole as one that will not hash |
| a seam graph that loops back on itself | **broken**, naming the segment | rotation only ever writes seams forward; a cycle was assembled |

You read that table with:

```bash
dotagent audit verify --full        # exits 1 on the rows that say "broken"
dotagent audit verify --json        # same verdict, one machine-parseable line
```

The daemon runs the same check at boot, but only over the live file and
only for yes/no — after the first rotation it never re-reads a rotated
segment. Everything below the first two rows needs `--full` to be seen.
Flags and output shapes: [`reference/cli.md`](../reference/cli.md#audit-verify).

Could an attacker forge a seam — write an `audit_log_rotated` entry pointing at
a segment that never existed, to explain away entries they deleted? Yes. But
that costs them exactly what a total rewrite already costs: write access plus
recomputing the chain forward. **Rotation does not move the boundary between
what the chain catches and what it doesn't.** It only adds a way to say
"history was pruned here, on purpose" that is as trustworthy as everything
else in the file — no more, and importantly no less.

Two smaller hardening notes, since the seam is a value read off disk and disk
is attacker-writable:

- `rotated_to` is validated as a bare sibling filename matching
  `<log>.<YYYYMMDDTHHMMSS>[-N]`. A seam naming `../../etc/passwd` is not
  followed; it is treated as an orphan, i.e. suspicious.
- A seam whose `prev_hash` disagrees with its own declared `tail_hash` is not a
  seam. It is an entry shaped like one, which is what a clumsy forgery looks
  like, and it reads as unexplained truncation.

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
