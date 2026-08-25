# Changelog

All notable changes to dotagent are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/) and the project
adheres to [Semantic Versioning](https://semver.org/).

Pre-1.0: minor bumps may include breaking changes; both `agent.toml`
schema and the plugin protocol are flagged in each entry.

## [0.6.1] - 2026-08-25

### Fixed

- **Memory extractor deadlines.** The extractor now applies its deadline to
  stdin and process completion, preventing a stalled extractor from blocking
  the daemon indefinitely.
- **Memory persistence after dispatch.** The daemon waits for the scheduled
  flush and runs extractor commands from the manifest directory, so extracted
  facts are persisted with the expected working directory.
- **`run-now` memory capture.** Manual runs merge captured facts before one
  flush instead of racing multiple persistence operations.

## [0.6.0] - 2026-08-22

### Added

- **Dates in a fact are linked automatically.** `TODO até 2026-08-24` is
  stored as `TODO até [[2026-08-24]]`, so the day's journal backlinks to the
  pendency instead of the date being plain text nobody can traverse. Applied
  in the store rather than asked of a model, so it holds for every writer:
  the assistant, the extractor, a scheduled agent's `MEMO:` line and the CLI.
  ISO only, idempotent, and bounded on both sides so a version string is not
  mistaken for a date. A linked day never becomes a topic page — the journal
  for it already exists.
- **`[assistant.extractor]` — memory that does not depend on the model
  remembering.** Capture used to happen only when the dispatcher's prompt told
  the model to end replies with `MEMO:` lines and the model complied, which
  put the memory of the whole machine inside one agent's prompt file. A model
  that forgot left no error, just a journal that stopped growing. The daemon
  now hands each turn to a declared command after the reply is delivered and
  files what comes back, parsed by the same `MEMO:` parser. The model's own
  lines stay a shortcut and merge with the extractor's. Off the reply path, so
  a slow extractor costs no latency; best-effort, so a broken one costs a
  turn's memory rather than the turn. Inference stays in a process the
  operator names — the daemon decides when to extract, never what a
  conversation meant.
- **`[os]` — installed binaries an assistant may run.** Off by default, and
  empty by default when on: `enabled = true` with no `allow` list runs
  nothing. An entry is a binary name, optionally followed by the leading
  arguments that must match, so `rg` allows the whole binary while
  `kubectl get` allows that subcommand and refuses `kubectl delete`. Matching
  is on whole tokens; a path is refused where a name is expected; arguments
  reach the program as a token list and never a shell. With the section off,
  `os-run` and `os-list` are absent from the MCP catalog rather than present
  and refusing. Every invocation is supervised, deadlined, and audited as
  `os_command_invoked` at `Critical` with the full argument list — the half no
  manifest declared. See `docs/security/threat-model.md` V17 for the residual
  risk, which is real: an allowlisted binary is trusted with whatever that
  binary can do.
- **`!` runs a typed command directly.** A message beginning with `!` skips
  the dispatcher entirely: `!rg foo src` spawns the binary and returns its
  output, with no model call, no session and no stored state. It obeys the
  same `[os] allow` list, and the prefix is read *after* the Telegram
  allowlist and rate limit so it can never be a way past them. Quotes group an
  argument; every other shell construct is ordinary text. Available over
  Telegram and over the local socket.
- **`allow = ["*"]`** opens every binary on `PATH` in one entry, instead of an
  enumerated list that would go stale on the next install. `doctor` warns
  while it is set, and `os-list` reports it as what it is.
- **`deny` and `confirm` for `[os]`.** `deny` refuses always and beats `allow`
  including `*`. `confirm` parks the command and answers with what it will
  run; a `!!` in the same conversation releases it, one slot per conversation,
  in memory with a TTL. `confirm` defaults to a non-empty list — the
  destructive classics plus every shell, since a guard on `rm` that lets
  `sh -c 'rm -rf /'` through guards nothing. Matching is per binary, so
  `rm -rf /`, `rm -fr /` and `rm --recursive --force /` are all caught where a
  textual pattern would catch one. `os-run` refuses confirm-class commands
  outright: a model may not answer its own confirmation.
- **`[[os.tool]]` publishes a binary under its own name.** `os-run` makes
  every allowed binary reachable, but a model has to already know `outl`
  exists to reach it. An entry becomes an MCP tool (`os-outl`,
  `os-kubectl-get`) carrying the operator's description, which is what the
  model reads when deciding to use it. A fixed `args` prefix cannot be
  replaced by the model, so `kubectl get` publishes a read-only view of a
  binary that can also delete. `doctor` reports entries the allowlist would
  refuse, empty descriptions, and colliding names.
- **Ranked recall.** `memory-recall` scores facts by shared terms first and
  recency second instead of asking whether the query was a substring of a
  stored fact. It almost never was — a question like "o que ficou pendente do
  databricks?" matched nothing — so recall silently degraded into "the last
  few facts written", regardless of subject. Ranking is lexical, never
  semantic: a fact competes only if it shares a real word, so a miss still
  returns nothing rather than the closest thing.
- **Dedup and supersede on the write path.** Restating a stored fact
  reinforces it (`seen::` goes up, new topics merge in) instead of filing a
  duplicate; a fact that stopped being true is marked `superseded-by::` and
  stops being recalled while staying readable in its journal. New
  `memory-supersede` and `memory-forget` tools, and `memory-recall` output
  now leads with each fact's id so they are addressable.
- **Provenance on every fact**, written as outl block properties
  (`agent::`, `source::`, `session::`, `seen::`, `last-seen::`) under the
  block rather than inside its text. One poisoned run's output can be found
  and removed as a group.
- **`[memory]` in `agent.toml`** — any agent, not just a conversational one,
  can file facts by printing `MEMO: <fact> | topics: a, b` on stdout. Opt-in
  per manifest and successful runs only. Topics declared in the section are
  added to every fact the agent files.
- **`dotagent memory`** — `recall`, `remember`, `supersede`, `forget`,
  `topics` and `stats` from a shell, with no daemon required, so a
  consolidation pass can be an ordinary scheduled agent instead of logic
  buried in the daemon. `--json` emits one object per line, provenance
  included.
- **Accent and plural folding in recall.** Query and fact are normalized the
  same way before they meet, so "pendência" finds a fact tagged `pendencias`
  and "custos" finds "custo". Found against a real store, where the question
  "tem alguma pendência com prazo?" returned nothing while the answer sat
  there under a slug the agent had coined without the accent.
- **Topic vocabulary in the recall block.** `AGENT_ASSISTANT_MEMORY` now ends
  with the topics already in use, so an assistant tags a new fact with a name
  the store already knows instead of coining `reunioes` next to `reuniao`.

### Fixed

- **A link inside a sentence no longer eats the sentence.** `split_links`
  removed every `[[…]]` from the statement while keeping it as a topic, so a
  fact edited by hand in the desktop app as "o [[dotagent]] roda no launchd"
  came back as "o roda no launchd". Only the trailing run — the links the
  store itself appended — is removed now; a link written into the prose keeps
  its text and still registers as a topic.

- **A notification with an empty body is no longer sent as-is.** Telegram
  rejects it (`400 message text is empty`), so an agent that reports by
  exception — a sweeper with nothing to sweep — produced a failed notifier
  on every clean run. `success` with no output now sends nothing; every
  other event synthesizes `agent/schedule: event (no output)`, because a
  state change still has to reach whoever is on the hook for it.
- `memory-recall` accepted a `topic` argument in its schema and ignored it,
  always running a text search.
- "Most recent facts" sorted every page by slug, so a topic page whose name
  started with a letter ranked ahead of every dated journal.
- Recall handed the assistant each fact with its `[[topic]]` markup inline;
  the injected block now carries the sentence, with topics rendered
  separately.

## [0.5.0] - 2026-08-18

### Added

- **Local client API** — the daemon serves path-independent JSON Lines over
  `$DOTAGENT_HOME/api.sock` when the configured dispatcher is discovered. The
  current transport is Unix-socket only; requests support `message.send`,
  `commands.list`, and `status.get`, with streamed `typing`, `run.started`,
  `reply.delta`, and final `reply` events. The socket is 0600, peer
  credentials are kernel-verified (same-uid), and a bind/accept failure is
  fatal to the daemon rather than silently disabling the endpoint.
- **`dotagent api` — the smallest built-in local client.** A raw JSONL bridge:
  stdin to the socket, socket to stdout, no rendering and no session state.
  Scripts and future TUIs speak the wire contract directly;
  `--socket <PATH>` overrides the default path. After stdin EOF the bridge
  half-closes and keeps reading, so late `reply` events still arrive.
- **`TriggerSource::Local` and session-scoped trigger state.** One-shot
  triggered runs keep the legacy `trigger-<source>` slug without a session and
  use `trigger-<source>-<sanitized-session>` when one is present. The opaque
  session id is exposed to the agent as `AGENT_SESSION_ID`.
- **`assistant-v1` stdout protocol.** A manifest may declare
  `[run] protocol = "assistant-v1"` for `delta`, `reply`, and optional
  `session` JSON Lines frames. The daemon uses the final `reply` frame for
  delivery shaping and does not own assistant transcript state. The protocol
  is opt-in: plain stdout is never parsed as assistant frames.

### Changed

- **Persistent trigger frames carry `session_id` per request.**
  `AGENT_TRIGGER_*` and `AGENT_SESSION_ID` are no longer fixed in the
  persistent process environment.
- **Trigger admission is now gateway-based.** Conversations are FIFO per
  `(source, session)` and different conversations can run concurrently up to
  a default cap of four. Telegram retains its allowlist and ingress rate
  limit; local clients receive raw streaming output while Telegram remains
  final-only. Gateway stdout streaming uses a bounded delta channel — a slow
  sink drops intermediate deltas (logged, with a count) instead of growing
  without limit.
- **Ambiguous persistent delivery is a terminal, counted failure.** A request
  whose bytes crossed the process boundary but never got an answer
  (`RequestLost`) consumes the attempt, closes the heartbeat with a dedicated
  exit code, writes an `agent_run` audit event, fires `given_up`, and is never
  redispatched — a side effect that may have run must not run twice. A
  supervisor-deadline kill is classified as `timed_out`, not as a lost
  request.
- **Telegram reply correlation keys on the canonical chat id.** Outbound
  notifications register the numeric `chat.id` returned by `sendMessage`, so
  an allowlist configured with `@channel_username` resolves `reply_to_run`
  the same way a numeric id always did.
- **Reload keeps one local socket owner.** Telegram ingress restarts on
  SIGHUP after aborting and joining the previous poller — two pollers never
  overlap on the offset file — while the local API listener remains on the
  same socket until daemon restart.

### Fixed

- **The local API answers before it narrates.** `accepted` is enqueued before
  `run.started`, `typing`, `reply.delta` and `reply` can reach the same
  connection, on a multi-thread runtime. A client that half-closes after
  sending still receives its final `reply`: the writer stays alive until the
  request's producers finish instead of being cut by a fixed drain window.
  An oversized frame without a terminating newline now ends the connection
  instead of pinning one of the connection slots, and the Darwin
  peer-credential constants match the SDK (`0x001`/`0x002`).
- **The persistent pool cannot run two instances of one key.** Slot states
  are explicit (`Starting`, `Live`, `Retiring`, `Stopped`): spawn
  reservations survive the idle sweep, an LRU eviction holds its key until
  the old process is actually gone, and a request arriving mid-retirement
  waits instead of forking a second instance.
- **Cancelling a run kills the process group.** Aborting a gateway worker
  (forced shutdown) now terminates the agent's process group and cleans the
  supervisor registry — a dropped `SupervisedHandle` no longer leaves a
  one-shot agent running unsupervised.
- **The persistent protocol parser only accepts answers.** Frames such as
  `{"kind":"progress","id":"1"}` or a `ready` handshake carrying an `id` are
  ignored instead of completing a request with an empty response; a manifest
  combining `assistant-v1` with `lifecycle.mode = "persistent"` is rejected
  at load rather than failing mid-handshake at runtime.
- **Post-run audit failure no longer corrupts retry accounting.** An audit
  append error after the agent already ran is logged and the run's outcome,
  window state and hooks proceed — a completed execution cannot be
  redispatched because observability write failed.

## [0.4.0] - 2026-08-08

Both entries below address [#47](https://github.com/avelino/dotagent/issues/47)
— dotagent cost battery on laptops in two unrelated ways, neither visible
as CPU percentage.

### Added

- **`[power]`: agents no longer have to run on battery.** A laptop pays
  for a scheduler twice — once for the CPU, once for being kept out of
  idle — and dotagent had no notion of where the electricity was coming
  from. A 15-minute interval agent fired 96 times a day whether the
  machine was plugged in at a desk or in a bag at 12%.

  ```toml
  [power]
  on_battery = "run"        # run | defer   (default: run)
  min_battery_percent = 0   # defer below this charge; 0 = never
  ```

  The two rules are independent: `min_battery_percent` on its own means
  "run on battery, but not when there's nearly none left". Any schedule
  can override `on_battery` in its `agent.toml`, because the cost is
  per-schedule — a cheap hourly check and an expensive 15-minute sync
  don't deserve the same policy. `min_battery_percent` is global by
  design.

  **Deferring does not queue.** Both schedule kinds resolve to the
  current window rather than a backlog, so an agent deferred across four
  hours of battery runs once when the charger goes in, not sixteen
  times. The check sits after the staleness check and writes no window
  state, so a deferred run burns no retry and leaves no trace.

  Detection is `pmset -g batt` on macOS and `/sys/class/power_supply` on
  Linux; anything else, or a failed probe, reads as mains power. A
  machine whose battery cannot be read must keep running its agents —
  failing to run is the worse failure. **Defaults change nothing**: an
  untouched install dispatches exactly as before and never probes at
  all.

  `dotagent tick` applies the same gate as the daemon — a tick that
  dispatched what the daemon would have held back would misreport the
  thing it exists to reproduce.

  `agent.toml` schema: additive (`on_battery` on a schedule).

### Changed

- **The daemon holds no timers while it supervises nothing.** Two
  background tasks polled to observe that nothing had happened: the
  reaper swept on a fixed 5-second tick (~17k wake-ups/day) and the
  snapshot writer rewrote a byte-identical `[]` every 2 seconds (~43k
  writes/day). On an idle daemon that was the entirety of its cost, and
  the disk writes kept the SSD from settling.

  The reaper is now deadline-driven — it sleeps until the nearest
  deadline in the registry — and the snapshot writer skips writes whose
  payload is unchanged. Both park outright on an empty registry and are
  woken by a registry change: a spawn, a `retime` that pulls a deadline
  in, or the deregistration of the entry that owned the deadline being
  slept on. Without that last one a process finishing early left its
  original deadline armed behind it, and "an idle daemon holds no
  timers" stayed false until that timer fired on nothing.

  Losing the tick made the reaper *more* precise: it fires on the
  deadline rather than up to one tick late.

  **Breaking (public API), `dotagent-supervisor`:**
  `Supervisor::start_reaper` no longer takes a `tick` argument — a
  deadline-driven loop has no interval to tune, and leaving the
  parameter in place would have implied one. `DEFAULT_REAPER_TICK` is
  removed; `SNAPSHOT_TICK` replaces it as the one interval that remains
  meaningful (how often the snapshot file is refreshed *while* something
  is running).

### Fixed

- **A deferred run could raise a false `stale` alert.** The power gate
  leaves window state untouched on purpose, so plugging in later still
  dispatches the current window. The health sweep in the same tick had no
  visibility into that decision and reads staleness from window age
  alone, so a weekend spent unplugged turned every deferred schedule into
  an outage alert for behavior the operator had asked for. The sweep now
  skips schedules the policy is currently holding back, without clearing
  the escalation ladder: an agent already failing before the charger came
  out resumes its episode rather than restarting it.

- **On Linux, an unreadable `online` file read as "on battery".** The
  probe could not tell a confirmed `0` from a read that failed or
  returned something unparseable, so under `on_battery = "defer"` a
  broken sysfs entry suppressed every run indefinitely — the opposite of
  the documented fail-open behavior. Charger state that cannot be
  established is now `Unknown`, which counts as mains. A peripheral's
  battery (`scope = Device`, as a wireless mouse reports) is also no
  longer mistaken for the machine's, which would have judged
  `min_battery_percent` against the charge of the mouse.

- **`dotagent tick --dry-run` ignored the power policy**, reporting runs
  as dispatchable that the daemon would have held back.

- **`retime` to a shorter deadline could be ignored.** Re-pointing a
  supervised entry's clock — which the persistent pool does on every
  request, to swap the idle window for the request deadline — did not
  wake the reaper. With the deadline-driven loop this became load-
  bearing: a reaper asleep on a 60-second deadline would sleep through a
  50ms one set under it. Regression test included.

## [0.3.0] - 2026-08-07

### Security

- **A failing notifier could write its own credential to disk.**
  `reqwest::Error`'s `Display` appends ` for url (…)`, and for Slack the URL
  *is* the webhook secret (Telegram's carries the bot token in the path). A
  `?` on any HTTP call converted that into a `NotifyError`, which reached
  `warn!(driver, error = %e, "notifier failed")` and landed in
  `~/.config/dotagent/logs/daemon/dotagent.log.*` — live credentials on disk,
  for the whole retention window, from a failure as mundane as a DNS blip.

  **Breaking (public API):** `NotifyError::Http(#[from] reqwest::Error)` is
  **removed from the enum**. Deleting the variant rather than just editing the
  log line is the point — without the `From` impl, `?` on a `reqwest` call no
  longer compiles, so the next driver cannot reintroduce the leak by accident.
  Transport failures are reduced at the call site to a kind plus a status
  (`slack transport error (timeout)`) and surface as `NotifyError::Backend`.
  Anything matching on `NotifyError::Http` must be updated; anything matching
  `_` or using `Display` is unaffected.

- **Every credential-bearing notifier field now accepts `${VAR}`.** Previously
  only `telegram.bot_token` did, so a Slack webhook, an ntfy token or a
  Pushover key had to be written literally into an `agent.toml` — a file that
  lives in a versioned repo, which makes it a credential in git history
  forever. Now expanded, at send time, against `~/.config/dotagent/secrets.env`
  first and `std::env::var` second: `slack.webhook_url`, `ntfy.token` /
  `base_url` / `topic`, `pushover.token` / `user`, `telegram.bot_token`.

  `ntfy.base_url` and `ntfy.topic` are in the list because they are not merely
  addresses: a self-hosted base URL can embed HTTP basic credentials, and on a
  public `ntfy.sh` the topic name is the only thing standing between an alert
  stream and anyone who guesses it. `telegram.chat_id` and `imessage.to` are
  deliberately **not** expanded — they identify *where* a message goes, and
  routing them through a secrets resolver would turn a typo into "env var
  unset" instead of a message delivered to the wrong place.

  An unresolved reference fails the send. There is no fallback to the literal
  `"${SLACK_WEBHOOK_URL}"`: that is not a degraded credential, it is a request
  authenticated as a placeholder, which answers 404 and reads like an outage
  rather than a typo. The error names the field and the variable and nothing
  else — the input is a credential template and the store holds sibling
  credentials, so echoing either would turn a config typo into a disclosure.

### Fixed

- **One slow agent made the bot stop answering for twenty minutes.** Inbound
  triggers were an arm of the daemon's main `select!`. That arm is only polled
  *between* ticks, and a tick awaits every scheduled run inline — so for as
  long as one scheduled agent was running, no queued message was even looked
  at. Observed in production: a message sent at 19:45:03 was dispatched at
  20:04:48, one second after the scheduled run holding the loop hit its
  1200-second timeout. Nineteen minutes and forty-five seconds of silence, from
  a component that had nothing to do with the agent that was running. The
  sender's only signal is silence, so the behaviour is indistinguishable from a
  dead bot, and the natural response — resend — makes it worse.

  Triggers are now drained by a worker task of their own. They stay serialized
  **against each other**: one worker consuming the channel FIFO, awaiting each
  handler to completion, so request N+1 is not started before N answers and
  per-conversation order survives by construction rather than by luck. One
  worker rather than one task per message on purpose — a task per message would
  hand ordering to the scheduler and to whichever task reached the persistent
  pool's per-key mutex first, and would turn a burst of N messages into N
  concurrent agent processes.

  What is new is exactly one concurrent pair, a triggered run beside a
  scheduled one, and it is safe on everything the two share: a triggered run's
  slug is namespaced by source (`trigger-telegram`) so it never touches the
  scheduled heartbeat file, the state store `flock`s per file regardless,
  triggered runs never write window state, and every audit append takes an
  exclusive lock. Scheduled runs remain serialized against each other.

  The worker is watched. Its `JoinHandle` is one arm of the loop's `select!`,
  so a handler that panics stops the daemon — loud, and restarted by
  launchd/systemd — instead of ending the task, closing the channel, and
  leaving a permanently deaf bot on a daemon that keeps ticking and reports
  itself healthy. That is the behaviour a trigger running inline used to have
  for free.

  SIGHUP retires persistent instances on a task of its own. Draining the pool
  waits for the slot each instance holds, and a slot busy with a request stays
  held for that request's whole deadline — awaiting that inline would stop the
  scheduler ticking, dispatching and summarizing for up to `timeout_seconds`
  (1200 in one production manifest). Reloads therefore no longer overlap with
  the loop at all; a request in flight finishes and its instance is retired
  after. Lock order is always `slots` → `slot`, so there is no deadlock.

  A scheduled run and a triggered one no longer share a persistent instance.
  The pool key is now `<scope>:<slice>`, where scope is `scheduled` or
  `trigger-<source>` and slice is the resolved `[lifecycle] key` (or
  `default`). Without the scope both collapsed to `default` for any agent that
  declares no `key` — a scheduled run carries no payload to resolve one from —
  so a 1200-second scheduled run held the chat behind the same per-key mutex,
  which is the blocking this whole entry is about. `AGENT_PERSIST_KEY` and the
  `key` field of `persistent_agent_*` audit entries carry the composed value.

- **A daemon that died without shutting down orphaned its children forever.**
  The supervisor holds every deadline in memory. `SIGKILL`, a panic, or
  `launchctl kickstart -k` takes it down with the daemon and leaves the
  subprocesses running with nobody enforcing anything — observed as a
  persistent agent alive for 26 minutes against a 600-second timeout, and
  nothing in the system was going to end it.

  A starting daemon now sweeps what the last one left behind. The mere presence
  of `state/supervisor.json` at boot is the signal that the previous exit was
  not clean, because a clean exit deletes it. The sweep runs before the
  snapshot writer starts, whose first tick would otherwise overwrite the only
  record of those processes.

  **It refuses to kill on doubt.** pids get recycled, and a stale record is not
  a licence to signal whatever holds the number now. A record is reaped only
  when every axis agrees: pid is not init and not ours, the record carries
  `pgid` and `command` and its `pgid` equals its `pid` (every supervised spawn
  does `setpgid(0, 0)`), the OS still reports the pid, the observed pgid still
  equals the pid, the observed start time is within five seconds of the
  recorded one, and the observed command line is consistent with the recorded
  one. Anything missing, ambiguous or unparseable is a skip. An unreadable
  snapshot signals nothing and is left on disk as evidence; a live peer daemon
  in the pidfile aborts the sweep entirely. Confirmed orphans get the existing
  kill-tree: `killpg(SIGTERM)`, grace, `killpg(SIGKILL)`.

  `ProcessInfo` gained `pgid` and `command`, both `#[serde(default)]` — a
  snapshot written by an older build deserializes fine and classifies as
  un-confirmable, so nothing is killed. The reap therefore becomes effective
  from the **second** restart after upgrading: the first one is what writes a
  snapshot the next boot can check.

- **Every non-ASCII character in a trigger payload reached the agent
  double-encoded.** launchd and systemd start a daemon with no `LANG` and no
  `LC_ALL`, and agents inherited that gap. A process in the resulting `C`
  locale has `MB_CUR_MAX == 1`: it reads each **byte** of an environment
  variable as one character — Latin-1 — and writes it back out as UTF-8. A
  Telegram message saying `quem é thiago avelino?` arrived in
  `AGENT_TRIGGER_PAYLOAD` and reached the agent as `quem Ã© thiago avelino?`.

  It stayed invisible for months because the result is still *valid* UTF-8, so
  nothing anywhere errored — and because only the environment is affected.
  Command-substitution output is not, so the same run logged a mangled prompt
  directly beside a clean answer, which reads like a model problem rather than
  a transport one. In one production log **57 of 57** payload-derived lines
  were corrupted and none were clean.

  `dotagent-runner` now names a UTF-8 locale (`en_US.UTF-8` on macOS,
  `C.UTF-8` elsewhere) when the agent inherits **no** locale at all — none of
  `LC_ALL`, `LC_CTYPE` or `LANG`. It is set as **`LC_CTYPE`**, not `LANG`:
  `MB_CUR_MAX` is the character-type category alone, while `LANG` is the
  fallback for every category, and moving `LC_COLLATE` to `en_US.UTF-8` on
  macOS would swap byte ordering for ICU collation — silently changing `sort`,
  `[[ a < b ]]` and `[a-z]` ranges for every shell agent, on macOS only. An
  inherited locale is never overridden (POSIX precedence is `LC_ALL` >
  `LC_CTYPE` > `LANG`, and an `ssh` session forwards `LC_CTYPE` alone), and a
  manifest naming any locale variable in `[env.extra]` stands the injection
  down entirely. Reproduced against fish 3.7.1: `c3 a9` round-trips as
  `c3 83 c2 a9` under `C`, unchanged under `en_US.UTF-8`.

- **An interval agent that gave up stayed dead forever.** `expected_at` for
  `type = "interval"` returned a frozen `last_success + interval`. Once that
  single window aged past `stale_after_minutes` every tick skipped it, so no
  run happened, so `last_success` never advanced, so the window could never
  move — a closed loop with no exit. One agent observed sat dead for **55
  days**.

  The window is now a rolling one: the tick sequence anchored on
  `last_success` (`+iv`, `+2·iv`, …) keeps advancing while runs fail, exactly
  like the calendar keeps producing new cron windows, and `expected_at`
  returns the greatest tick at or before now. Dispatch stays alive no matter
  how long the agent has been broken.

  Staleness is measured against a *different* window — the **first** one missed
  after the last success, not the rolling one. Judging health against a window
  that rolls forward would report a chronically failing agent as a fresh
  failure forever, which trades one silence for another.

- **`attempts` was read as a retry count and it is a dispatch count.** The
  daemon bumps it on every dispatch, the successful one included, so a window
  that worked on the first try lands on disk as `attempts: 1` — and
  `health_state` called that `degraded`. Every healthy agent looked slightly
  sick, which is the fastest way to teach someone to ignore the column.
  `degraded` now requires at least one attempt that actually *failed*, with
  the successful dispatch discounted only when the last recorded attempt is
  the one that exited 0.

- **`interval_minutes = 0` panicked the daemon** with a divide-by-zero. A
  zero interval has no meaningful cadence, so the schedule is now simply never
  due — it does not fire every tick and it does not take the process down.

- **The daily summary reached its window by coincidence, not by design.** The
  daemon never scheduled a wake-up for it. It landed inside the window only
  because `MAX_SLEEP_MINUTES` happened to equal the window's width — two
  unrelated constants with no code connecting them. The coincidence broke the
  moment a tick overran its own sleep budget, since the next sleep then fell
  back to 60s and pushed the following cycle past the window, and it never held
  at all once the time became configurable. The summary is now a wake-up reason
  in its own right, folded into the same `min()` as the next agent window.

- **A summary window crossing midnight could be marked delivered by the wrong
  day.** The once-per-day guard keyed on *today's* date rather than the date
  the window opened on. With a `time` late enough that `grace_minutes` runs
  past midnight the two differ, so a 00:10 delivery recorded itself against the
  new day and left the 23:50 window that produced it looking undelivered — or,
  read the other way, silenced the next one. The guard now keys on the window's
  own date.

- **`dotagent reload` left most of `config.toml` pinned at boot.** SIGHUP
  re-read the file for secrets and the Telegram ingress but the tick loop kept
  the config it started with, so retention thresholds and the daily summary's
  time and destinations only changed on a full restart. Editing the file and
  reloading looked like it took effect and did not — the worst shape a reload
  can have. The whole config is now replaced.

### Added

- **The daily summary now has somewhere to go.** It has shipped since the
  beginning and has never delivered anything to anyone: the window was a
  hardcoded `[22:45, 23:15)`, the notifier was a hardcoded plugin, and the
  recipient was a placeholder phone number nobody owned. It fired every night
  into that constant and left no trace when the send went nowhere, which is why
  the defect survived this long — a feature that silently does nothing looks
  exactly like a feature with nothing to report.

  It is configurable now, under **`[daily_summary]`** in `config.toml`: `time`
  (default `22:45` local, `HH:MM` or `HH:MM:SS`), `grace_minutes` (default
  `30`, clamped to `[1, 1440]`), `enabled` (default `true`), and
  `[[daily_summary.notifiers]]`, which takes the same entries a manifest's
  `[[notifiers]]` takes.

  **An empty notifier list means `desktop`, not silence.** Every other driver
  needs a chat id, a phone number or a webhook, and there is no universal
  default for those; `desktop` is the only one with nothing to fill in and
  nothing to leak. `[telegram]` is deliberately *not* reused as a destination —
  that section is ingress, and wiring it to egress would send a nightly report
  to everyone who set up a bot to talk to their agents and never asked for one.

  The failure modes lean the same way. A `time` that does not parse falls back
  to the default instead of disabling delivery, because a typo should cost the
  wrong hour and not a silent month; `grace_minutes = 0` becomes `1`, since a
  zero-width window is an empty one. `events` inside a summary notifier is
  **ignored** — the list is already scoped to a single event, so a filter there
  could only subtract, and an entry copied out of a manifest carrying
  `events = ["given_up"]` would drop the whole summary without a word.
  `enabled = false` stops the daemon's fire but not `dotagent daily-summary`
  typed by hand, which is a different request. Every delivery is audited as
  `plugin_invoked` with `plugin: "notifier:<driver>"`, failures included, so a
  summary that reached nobody is answerable from `dotagent audit` rather than
  from silence — the exact thing missing for its whole life so far.
  `--dry-run` now also prints `→ would deliver to: <drivers>`. Docs:
  [`guides/config-reference.md`](docs/guides/config-reference.md#daily_summary).

- **`stale` — the failure that had no way to tell you.** Every other event
  fires from something that happened. An agent that quietly stops being
  scheduled does nothing at all: no run, no failure, nothing to notify on. Its
  window ages out, the daemon stops even attempting it, and the last thing
  anyone heard was a success weeks ago. Silence reads as fine, which is how a
  55-day outage goes unnoticed.

  `stale` fires from the *condition* instead, swept on every daemon tick by
  `sweep_health_notifications`. `given_up` joins it: it used to be said once
  and never again, so an agent broken for a week asked exactly once.

  Because a condition is true continuously, re-notification runs on a rising
  ladder — **on entry, then after 1h, 6h, and once a day** for as long as it
  holds. Both extremes end in the same silence: notify once and the alert is
  buried by whatever else arrived that hour; notify every tick and the reader
  is trained to dismiss dotagent on sight. Rising spacing puts the density in
  the first hours, when a fix is most likely to actually happen.

  The episode is deleted the moment the schedule succeeds again, so the *next*
  failure is loud from its first second instead of inheriting yesterday's
  spacing. State lives in `state/notify/alerts.json` and survives restarts;
  losing it costs at most one duplicate alert, which is the right side to be
  wrong on.

  **No manifest edit required.** `stale` is delivered on the `given_up`
  channel when no notifier lists it explicitly — someone who wrote
  `events = ["given_up"]` asked to be told their agent is broken, and stale is
  the same news only worse. Listing `"stale"` makes the routing explicit.

- **The audit log rotates, and the chain crosses the rename.** It was
  append-only and unbounded, which is a promise that ends in a file nobody can
  open. Past **32MB** the live file is sealed as `audit.log.<YYYYMMDDTHHMMSS>`
  and a fresh one opens with an `AuditLogRotated` **seam** as its first line,
  whose `prev_hash` is the sealed segment's tail hash — so there is no gap in
  the chain at the rename, and rotation verifies clean.

  Segments are **never deleted automatically**; the `[logs]` sweeper does not
  touch them. This is the only forensic artifact dotagent keeps, so pruning it
  is the operator's call.

  The seam is what makes that pruning legible. `verify_chain_status()` returns
  a four-state `ChainStatus` that distinguishes *"intact since `<ts>`, and here
  is the segment that is gone"* from *"the head of the live file was cut off
  and nothing accounts for it"* — the second fires `audit_chain_broken`, the
  first does not. `verify_chain()` keeps its signature. Rotation does not move
  the boundary between what the chain catches and what it does not; the
  reasoning is in
  [`security/threat-model.md`](docs/security/threat-model.md#what-the-hash-chain-guarantees-and-what-it-does-not).

- **`[state] window_retention_days`** (default `30`, `0` disables) — the 03:00
  sweep now covers `state/windows/`, the one state directory that grows without
  bound. A schedule writes one window file per fired window and never revisits
  it, so a 15-minute agent leaves ~96 files a day behind, each with a `.lock`
  beside it. Windows are deleted, never gzipped — the daemon reads them as
  JSON, so a compressed window is a corrupted one — and a window whose lock is
  currently held is skipped until the next pass. Heartbeats are deliberately
  not covered: there is exactly one per `(agent, schedule)` and it is rewritten
  in place.

### Changed

- **Health reasons and summary headers are in English.** The strings
  `dotagent status` prints in its `REASON` column, and the section headers of
  the daily summary, were written in Portuguese while every other user-facing
  surface in the project was not. They are the most-read output dotagent
  produces, so they were also the most visible place for the project to
  contradict itself: `2 tentativas, vai retentar` → `2 attempts, will retry`,
  `desisti após N tentativas` → `gave up after N attempts`, `janela perdida há
  Nmin (stale)` → `window missed Nmin ago (stale)`, `nunca rodou` →
  `never ran`, `sem janela hoje · último sucesso ok` →
  `no window today · last success ok`, and `❌ Falhando` / `⚠️ Degradado` →
  `❌ Failing:` / `⚠️ Degraded:`.

  The count is singular-aware now, which it was not: a first-failure window
  read `recuperou após 1 tentativas`. Anything grepping these strings — a
  dashboard, an alert rule — needs updating; they are output, not API, and this
  is the moment to say so.

- **The daemon's stderr is a crash channel now, not a second log.** The
  `tracing` stderr layer is installed **only when stderr is a terminal**. Under
  launchd / systemd stderr is an appended plain file that no rotation policy
  covers, so mirroring there duplicated an already-rotated log into one that
  grew forever. `run.avelino.dotagent-error.log` now receives panics and
  startup failures that happen before logging is up, and nothing else — an
  empty file on a healthy daemon is the intended state. Watch
  `logs/daemon/dotagent.log` for activity. `DOTAGENT_LOG_STDERR=1` forces the
  mirror on (units rewired to journald, which does rotate), `0` forces it off.
  ANSI colour follows the same TTY rule and honours `NO_COLOR`, in the daemon
  **and** in every subcommand.

- **`tick_started` / `tick_completed` are no longer emitted.** A tick is
  telemetry, not an auditable event — "woke up and looked at 17 agents" tells a
  forensic reader nothing, and on one real 38,510-entry log the two variants
  were **64%** of every line. Because each append re-read the log to find the
  tail hash, that noise also made every event worth recording more expensive to
  write; appends are now O(1) via a seek to the end. The daemon emits `tracing`
  at `debug` instead. The enum variants are **kept** so existing logs stay
  parseable: removing them would break `dotagent status --audit` and chain
  verification the moment either reached a historic tick entry, and a chain you
  can no longer verify is worse than one carrying dead weight.

- **An over-long alert is trimmed instead of dropped.** Every backend rejects
  the whole request when the body exceeds its limit, so a too-long alert was an
  alert that never arrived. Caps are now applied before sending, with the cut
  marked (`[truncated]` on bodies, `…` on titles): slack 40,000 characters,
  telegram 4,096 characters, pushover 1,024 characters (title 250), ntfy 4,096
  **bytes** (title 250 bytes).

  Bytes versus characters is not pedantry here. ntfy counts bytes and alert
  text is not ASCII — `ç` and `ã` cost 2 bytes each, an emoji costs 4 — so a
  character-based trim could hand ntfy four times its limit while believing it
  stayed under. The byte cutter also walks back to a UTF-8 character boundary:
  a sliced codepoint would be rejected for a different reason than the one the
  cap exists to avoid.

- **ntfy sends `X-Title` / `X-Tags` as RFC 2047 encoded words.** A header value
  is bytes, not text; raw UTF-8 there is `obs-text`, which RFC 9110 says a
  sender should not generate and which ntfy's own docs warn can arrive as `?`.
  Non-ASCII values are now emitted as `=?UTF-8?Q?…?=`, which the ntfy server
  decodes before reading any header param, so `title = "Falha na execução 🚨"`
  survives the wire. Control characters are flattened to spaces first — a
  newline is not a header value at all, and it would kill the whole request in
  the builder. The title cap bites on the **raw** text, before encoding, since
  a 4-byte emoji becomes 12 characters of `=XX`.

- **Telegram MarkdownV2 escaping covers `\`** — 19 reserved characters, not 18.
  An unescaped backslash is itself an escape opener, so one in the body
  desynchronized everything after it and Telegram rejected the message.
  Truncation also moved to the **outbound** path (it was only applied inbound),
  and escaping runs *before* the cut because escaping grows the text: a body
  that fits under 4,096 characters plain can exceed it once every `.` and `-`
  carries a backslash. A backslash orphaned by the cut is dropped — Telegram
  refuses the whole message over one dangling escape.

  Consequence for anyone hand-escaping: the body always goes through the
  escaper, so pre-escaped input now comes out with doubled backslashes. Use
  `parse_mode = "HTML"` if you need live markup.

- **A refused Telegram send says why.** The Bot API answers
  `{"ok":false,"description":"…"}` and that description was discarded in favour
  of a bare status code. It is now carried into the log line, capped, with the
  bot token scrubbed out of it first — the API is happy to echo the request URL
  back inside its own error text.

### Added

- **An agent can stay alive between runs.** `[lifecycle] mode = "persistent"`
  keeps the process up and delivers requests to it over JSON lines, one object
  per line each way. The problem it solves is a dispatcher paying for startup
  and for reloading its conversation on every single message: measured on a
  chat bot, 4.94s per forked message against **1.90s** on a live process. Every
  message after the first is a second message.

  It is deliberately not a second supervisor. Instances are spawned through
  `dotagent-supervisor` like every other subprocess, so they appear in
  `dotagent status` under `persistent`, the reaper enforces their deadline with
  a real kill-tree, and daemon shutdown reaps them. The idle timeout is not a
  new timer either — it is the reaper's existing clock, re-pointed after each
  answer with the new `Supervisor::retime`. One clock, one kill path.

  `key = "chat_id"` gives one process per conversation; without it every
  sender shares one process and whatever it remembers. `max_invocations`
  recycles before a long conversation degrades it, `max_instances` evicts the
  least recently used, and a crash costs a respawn rather than the message —
  a death in the window between setting the deadline and writing the request
  is retried once. Recycles are audited as `persistent_agent_recycled` with
  the reason. New threat vector
  [V13](docs/security/threat-model.md#v13--state-carried-between-senders-by-a-persistent-agent);
  the protocol is at
  [`docs/reference/persistent-protocol.md`](docs/reference/persistent-protocol.md)
  and the why at [`docs/concepts/lifecycle.md`](docs/concepts/lifecycle.md).

  `mode = "oneshot"` is the default and stays the default. An agent with no
  `[lifecycle]` behaves exactly as before.
- **A reply knows which run it answers.** Notifications are the messages you
  actually want to answer, and answering one used to arrive as prose someone
  had to parse. dotagent now records what each outbound Telegram message was
  about and resolves a reply back to it, handing the dispatcher
  `AGENT_TRIGGER_PAYLOAD.reply_to_run` with the agent, schedule and event.
  Resolved from the message id rather than the text, because the text is not
  an interface: one alert reads `disk-alert/every-15min gave up after 3
  attempts` and another reads only `preflight aborted by plugin
  preflight-warp`. The table lives at `state/notify/telegram/sent.json`,
  capped at 500 entries — losing it costs correlation on old alerts, never
  delivery.
- **`[[preflight]] remediation`** — declare what clears a check, and it
  becomes a `remediate-<agent>-<plugin>` tool an assistant can offer instead
  of only reporting the failure. The plugin's own `suggest` string stays
  unexecutable on purpose: running a command a *plugin* wrote, triggered by a
  chat message, is arbitrary execution over an inbound path. Declared in the
  manifest it is the operator's command, in a file under review, published as
  a closed catalog entry that takes no arguments — so a model picks *which*,
  never *what*. Split into argv with no shell, supervised with a 120s
  deadline, audited as `remediation_invoked` at `Critical`, and it does not
  re-run the agent. New threat vector
  [V12](docs/security/threat-model.md#v12--remediation-from-a-chat-message).

### Changed

- **`examples/telegram-assistant` holds a conversation.** It used to answer
  each message from nothing, which makes confirm-then-act impossible — the
  assistant asks "shall I?" and then cannot know what it offered. Each chat
  now gets its own `claude` session, with a 400 KB ceiling on the transcript
  because `--resume` replays all of it as input and nothing trims it: measured
  on a real bot, ~90 KB answers in 8-10s and 977 KB in 26-141s. Retiring costs
  the recent history, which is the argument for keeping what matters in
  [memory](docs/concepts/memory.md) instead.

### Added

- **Latency guidance for conversational agents** in
  [`guides/llm-agents.md`](docs/guides/llm-agents.md#where-the-latency-actually-is),
  with measurements rather than advice: an aggregating MCP proxy spawned per
  run costs 8s that connecting to a running one does not, `dotagent mcp`
  itself costs nothing, a forked `claude -p` answers the second message in
  4.94s against 1.90s for a process kept alive, and a tool catalog past a few
  hundred entries turns one answer into several round trips. Also the failure
  that is easy to miss: a proxy listing its backends from a hash map returned
  them in a different order each call, which changed the session fingerprint
  and silently threw away the warm session on every message.

## [0.2.1] - 2026-08-04

### Added

- **Commands** — procedures *you* pick, published as a Telegram menu. A command
  is one markdown file in [Claude Code slash command][cc-slash] format under
  `~/.config/dotagent/commands/`, so a catalog you already keep is reusable
  rather than transcribed. Type `/` in the chat and the client renders
  everything installed; pick one and it runs. Skills cover the case where a
  model decides what applies — commands cover the case where you already know,
  and paying for a dispatch decision that was already made is latency plus a
  chance of picking something else. `$ARGUMENTS` and `$1`…`$N` substitute what
  was typed after the name; arguments given to a body with no placeholder are
  appended rather than silently dropped. Configurable under `[commands]`,
  including `claude_commands` — **off** by default, unlike its skills
  counterpart, because a command becomes a published menu entry and a Claude
  Code catalog is usually full of things that assume a shell. Docs:
  [`concepts/commands.md`](docs/concepts/commands.md).
- Commands carry **two derived names**, and they collide under different rules.
  Telegram allows `[a-z0-9_]{1,32}` — no hyphens — so `commit-message`
  registers as `/commit_message` while the MCP tool is
  `command-commit-message`. The catalog is deduped twice, a name Telegram would
  refuse fails validation instead of being truncated into a menu entry that
  resolves to nothing, and `dotagent doctor` names the *files* in a collision so
  you know which to rename.
- `dotagent mcp` gains `command-get` and `command-list` — **two tools for the
  whole catalog, not one per command**, deliberately unlike `skill-*`. There the
  catalog is a menu a model picks from; here a human already picked, and
  publishing N tools would re-open that decision and let a model call the wrong
  one. The daemon parses `/name args` lexically and publishes the catalog via
  `setMyCommands`; it never resolves a command to its body, which keeps
  "dotagent itself interprets nothing" true.
- The Telegram menu registers per allowlisted chat rather than globally. The
  allowlist already gates execution, so a global menu would not be an
  authorization hole — but it would publish every command name and description
  to anyone who finds the bot. New audit event `command_dispatched` records the
  name and whether it was known, never the arguments.
- Built-in `/help` lists what is installed, and an unknown `/typo` is answered
  from the catalog instead of falling through to a model that would improvise.
  Writing your own `help.md` replaces the built-in.

[cc-slash]: https://docs.claude.com/en/docs/claude-code/slash-commands

- **Skills** — procedures an assistant loads when they apply. A skill is a
  directory with a `SKILL.md` (frontmatter + markdown), and `dotagent mcp`
  exposes one `skill-<name>` tool per installed skill: the description is the
  trigger a model matches on, the body arrives only when it calls. Agents are
  verbs and memory is facts; this is the third thing, *how to do something* —
  which previously had to bloat a system prompt or be forced into a script.
  Two more tools reach inside one: `skill-read` for the reference files a
  procedure points at (without it, "see references/x.md" is a dead end for a
  caller with no filesystem tool) and `skill-run` for executables under
  `scripts/`, supervised with a deadline and kill-tree, audited as
  `skill_invoked`. `~/.claude/skills/` is searched by default — the format is
  Anthropic's Agent Skills layout, so a skill written for Claude Code needs no
  copy, with the honest caveat that a skill whose steps assume that harness
  (Bash, Read, nested skills) does not port. Configurable under `[skills]`.
  Docs: [`concepts/skills.md`](docs/concepts/skills.md), threat vector V10.

- **Long-term memory** — new crate `dotagent-memory` embeds
  [outl](https://github.com/avelino/outl) (`outl-ws` + `outl-actions`) as an
  agent memory backend, and `dotagent mcp` exposes `memory-remember` /
  `memory-recall`. Facts land as one block per journal entry in a workspace
  at `~/.config/dotagent/outl`, scaffolded when the daemon starts. It is a
  normal outl workspace on purpose: open it, read what an agent kept, fix a
  wrong memory, delete a page. Configurable under `[memory]`, and a
  configured path is never scaffolded so a typo fails loudly. Recall is
  substring rather than semantic — a near-miss an assistant states as fact
  is worse than no match. Docs: [`concepts/memory.md`](docs/concepts/memory.md).
- **Replies carry what they answer.** When a Telegram message is a reply, the
  quoted text rides in `AGENT_TRIGGER_PAYLOAD.reply_to_text`. A bare "sim"
  means nothing on its own, and inferring it from conversation history breaks
  the moment someone answers an older message out of order.
- Memory is a **graph**, not a list. `memory-remember` takes topics, each
  becoming a `[[link]]` to a page that outl's backlinks fill in, so a fact
  stored once on the day it was learned is reachable both chronologically and
  by subject. `memory-recall` accepts a `topic` to ask the graph instead of
  guessing which words the fact used, and `memory-topics` lists what exists —
  reusing a topic is what makes the gathering work, and inventing a near
  duplicate splits a subject in two.
- Topic names are normalized (`Roam Research`, `roam research` → `roam-research`)
  so capitalization alone cannot fragment the graph. `/` survives, since
  hierarchy is a real page path in outl.
- **Introspection tools** on the MCP server: `dotagent-status`,
  `dotagent-logs`, `dotagent-inspect`, `dotagent-doctor` and
  `dotagent-next-runs`. An assistant asked "did it run?" can now quote the log
  or the health table instead of reasoning about what probably happened.
  Read-only by selection: `install`, `uninstall`, `reload` and `daily-summary`
  are deliberately absent, and `bootstrap` most of all — it marks every window
  as ok, silencing a failure instead of fixing it. Agent names resolve through
  discovery, so `../` is refused rather than followed.
- `dotagent doctor` reports where memory lives, or that it is off.

### Changed

- `reqwest` 0.13.1 → 0.13.4. The `webpki-roots` feature was dropped upstream
  and `rustls` now carries the trust anchors; keeping the old feature name
  pinned the lock and silently blocked every patch release. Verified with a
  real TLS handshake against `api.telegram.org` before and after.

### Fixed

- `tools/call` parsed the agent argument schema before dispatching, so a tool
  whose `args` is not a list — `command-get`, whose `args` is the string the
  sender typed — was rejected with the *agent* schema's error before its handler
  ran. The typed parse now happens once the tool is known to be an agent.

## [0.2.0] - 2026-08-04

### Breaking (library consumers only)

The `agent.toml` schema and the heartbeat file shape are **unchanged** — no
manifest needs editing and the Fish-framework compatibility holds. These
break only code that depends on the crates as libraries:

- `RunSpec` gained `slug_override` and `extra_env`. Struct literals must add
  both fields.
- `AuditEvent` gained `manifest_invalid`, `trigger_received`,
  `trigger_rejected` and `agent_triggered`. The enum is not
  `#[non_exhaustive]`, so exhaustive matches need new arms.
- `discovery::discover_all` no longer returns `Err` for an unparseable
  manifest; it skips it. Use `discovery::discover` to see what failed.

### Added

- **Inbound triggers** ([#40](https://github.com/avelino/dotagent/issues/40)) —
  a run can now start because something asked, not only because a window
  came due. Triggered runs keep the supervisor, heartbeat, audit trail and
  hooks of a scheduled run, but carry no window state (no attempts counter,
  no retry consumed) and use their own slug (`trigger-<source>`) so they
  can never overwrite `last_success_at` for a cron window that never fired.
  Docs: [`concepts/triggers.md`](docs/concepts/triggers.md).
- **`dotagent mcp`** — serves every discovered agent as an MCP tool over
  JSON-RPC 2.0 on stdio. A model picks from `tools/list` instead of
  composing a command, so "only run what the operator declared" becomes a
  property of the protocol. Reachable from Claude Code, Claude Desktop or
  any MCP proxy. New crate `dotagent-mcp` holds the wire types.
  Docs: [`reference/mcp.md`](docs/reference/mcp.md).
- **Inbound Telegram** — the daemon can long-poll `getUpdates` and hand
  accepted messages to a dispatcher agent, whose stdout is sent back to the
  same chat. Configured under a new daemon-level `[telegram]` section
  (`bot_token`, `allowed_user_ids`, `dispatcher_agent`,
  `poll_timeout_seconds`, `rate_limit_per_minute`). Off unless both a token
  and a non-empty numeric allowlist are set — an empty allowlist means
  nobody, never everybody. Docs:
  [`concepts/telegram.md`](docs/concepts/telegram.md).
- Four env vars on triggered runs: `AGENT_TRIGGER_SOURCE`,
  `AGENT_TRIGGER_ACTOR`, `AGENT_TRIGGER_REPLY_TO`, `AGENT_TRIGGER_PAYLOAD`.
  Applied *before* the `AGENT_*` block so a payload can never redefine
  `AGENT_NAME` or `AGENT_HEARTBEAT_FILE`. The message body travels here,
  never in argv.
- Audit events `trigger_received`, `trigger_rejected` (`Critical`) and
  `agent_triggered`. Message bodies are never recorded — the log carries
  attribution, not transcripts.
- Threat model gains **V8** (untrusted inbound message causes a local run)
  and **V9** (MCP client reaches the agent catalog). V8 is the first vector
  where someone with no access to the machine can cause local execution,
  and it is called out as an explicit exception to the document's premise.
- `examples/telegram-assistant/` — the whole loop: message → `claude -p`
  with the MCP server attached → agent → reply.
- Replies quote the message they answer (`reply_parameters`, Bot API 7.0+).
  Runs are asynchronous and a chat can have several questions in flight, so
  an unquoted answer leaves you guessing which one it belongs to. Sent with
  `allow_sending_without_reply`, so deleting the original mid-run still
  delivers the answer instead of failing with a 400.

### Fixed

- **One broken `agent.toml` no longer stops all scheduling.** Discovery
  aborted the whole scan on the first parse failure, and the daemon turned
  that error into an empty agent list — so a single typo silently stopped
  every agent, with one `warn!` line as the only evidence. Broken manifests
  are now skipped, reported by `doctor`, and audited as `manifest_invalid`
  (`Critical`, so it fires out-of-band notification).
- **The state store deleted its own lock file while still holding the
  lock.** Another process opening the same path afterwards got a fresh
  inode and acquired its own "exclusive" lock, so two writers could believe
  they had the file to themselves. The lock file now persists, matching
  what `AuditLog::append` already did.
- **`.expect()` on the heartbeat could panic the runner.** If the file went
  missing mid-run (another process, an operator with `rm`), finishing a run
  killed the daemon. It now rebuilds the record and logs, without inventing
  a `last_success_at` that never happened.
- **`give_up` re-derived the state root** instead of using the store it was
  given, so a daemon running against a non-default `DOTAGENT_HOME` wrote
  the give-up marker where nothing else would look for it.
- **One malformed Telegram update no longer drops the whole batch.** A
  required field missing from a single update failed the deserialization of
  the entire `getUpdates` response, which in a long-poll means the offset
  never advances and Telegram redelivers that batch forever. One bad message
  would have wedged the bot permanently.

### Changed

- `[[schedules]]` is no longer required. An agent that only ever runs from
  a trigger may declare none and receives the synthetic schedule id
  `trigger`.
- `dotagent doctor` reports inbound Telegram status: whether the ingress is
  on, and the two ways it can be half-configured (empty allowlist, missing
  dispatcher agent).
- The threat model now states the `[security]` enforcement gap explicitly
  rather than leaving it as a "post-v0" footnote. It reads differently once
  an untrusted message can pick which declared agent runs.
- The Telegram driver now shares one `reqwest::Client` with a timeout
  instead of building one per send. Previously every notification rebuilt
  the TLS stack and no timeout was configured at all.
- `CLAUDE.md` trade-off #4 amended: dotagent now **serves** MCP while still
  not **consuming** it. The `mcp` proxy CLI remains separate and `sink-*`
  plugins still shell out to it.

## [0.1.4] - 2026-06-01

### Added

- **`sink-outl` plugin** — twin of `sink-roam` targeting
  [Outl](https://github.com/avelino/outl). Same config shape
  (`page`, `marker_regex`, `mcp_binary`) so an existing
  `[[on_success]]` block migrates by changing the plugin name. The
  whole publish is a single `outl_batch` call (N `block_delete` ops
  matched by `marker_regex` + one `block_append_tree`), removing the
  round-trips `sink-roam` makes against the Roam API.
- Daily slug resolution accepts the Roam-native ordinal form
  (`April 22nd, 2026`) and normalizes it to ISO (`2026-04-22`) before
  talking to Outl — same `agent.toml` works against both backends.
- Docs: [`docs/plugins/sink-outl.md`](docs/plugins/sink-outl.md), entry
  added to the plugin index, SUMMARY, `concepts/plugins.md`,
  `getting-started/installation.md`, `getting-started/next-steps.md`
  and `llms.txt`.

### Changed

- Homebrew formula comment lists the new `dotagent-plugin-sink-outl`
  binary; `bin.install Dir["bin/*"]` already shipped it automatically.

## [0.1.3] - 2026-05-22

### Added

- **`dotagent-supervisor` crate** — single subprocess lifecycle manager
  covering deadlines, POSIX process-group kill-tree, and a live
  registry. Every orchestrated subprocess (agent, plugin, hook) now
  passes through the supervisor; ad-hoc helpers inside notifier
  drivers (e.g. `osascript`) remain unchanged.

### Fixed

- `shutdown_signals_every_live_entry` flaky test on macOS CI.

## [0.1.2] - 2026-05-21

### Added

- **`dotagent-secrets` crate** — `secrets.env` loader with `op://`
  reference support, fed into the agent environment alongside the
  manifest's `[env]` block.
- Telegram notifier driver.
- Shell completion with dynamic agent-name autocomplete.

### Changed

- CLI run-now output pretty-prints the outcome instead of dumping
  `Debug` and tightens the renderer test suite.

[0.5.0]: https://github.com/avelino/dotagent/releases/tag/v0.5.0
[0.4.0]: https://github.com/avelino/dotagent/releases/tag/v0.4.0
[0.3.0]: https://github.com/avelino/dotagent/releases/tag/v0.3.0
[0.2.1]: https://github.com/avelino/dotagent/releases/tag/v0.2.1
[0.2.0]: https://github.com/avelino/dotagent/releases/tag/v0.2.0
[0.1.4]: https://github.com/avelino/dotagent/releases/tag/v0.1.4
[0.1.3]: https://github.com/avelino/dotagent/releases/tag/v0.1.3
[0.1.2]: https://github.com/avelino/dotagent/releases/tag/v0.1.2
