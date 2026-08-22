# MCP server

> `dotagent mcp` — every agent on this machine, as a callable tool.

```bash
dotagent mcp
```

Speaks JSON-RPC 2.0 over stdio, one object per line. Point any MCP client at it:

```json
{ "mcpServers": { "dotagent": { "command": "dotagent", "args": ["mcp"] } } }
```

The catalog is built from the manifests already on disk. Claude Code, Claude Desktop, an aggregating proxy, or a headless `claude -p --mcp-config` all see the same tools.

## Why this shape

A model asked to run something has two options: compose a command, or pick from a list. Composing means somebody has to validate the result, and that check is exactly the one that gets forgotten.

As tools, the list is the protocol. `tools/list` is the catalog, a name that is not in it is not callable, and "only run what the operator declared" stops being a rule anyone has to remember.

## Methods

| Method | Behavior |
|---|---|
| `initialize` | handshake; echoes the client's `protocolVersion` when it sent one, otherwise `2024-11-05` |
| `tools/list` | one tool per discovered agent |
| `tools/call` | run an agent, return its output |
| `ping` | liveness, empty result |

Requests without an `id` are notifications and get no response. Unparseable input answers with `-32700` and a null id.

## Introspection

| Tool | Arguments | Behavior |
|---|---|---|
| `dotagent-status` | — | Health of every scheduled agent, same as `dotagent status`. |
| `dotagent-logs` | `agent`, `lines` | Tail an agent's captured output. Default 40 lines, capped at 400. |
| `dotagent-inspect` | `agent` | Heartbeat, window state, manifest hash. |
| `dotagent-doctor` | — | Manifest validation, plugin resolution, secrets / Telegram / memory status. |
| `dotagent-next-runs` | — | What the scheduler would dispatch now. A preview; nothing runs. |

Always present, regardless of `[memory]`.

**Read-only by selection, not by accident.** The CLI has commands that write —
`install`, `uninstall`, `reload`, `daily-summary` — and none are exposed.
`bootstrap` is refused most deliberately: it marks every window as ok, which
silences a failure rather than fixing it, and an assistant that "resolves" an
alert by deleting the signal is worse than one that cannot help.

`run` and `run-now` are absent because they already exist as the `run-*` tools.

Agent names are resolved through discovery rather than concatenated into a
path, so `../` in a name is refused rather than followed.

These re-execute the `dotagent` binary instead of reimplementing each command.
Rendering lives in the CLI, and a second copy would drift — an assistant
quoting a status that no longer matches `dotagent status` is worse than a
subprocess.

## Memory tools

Five more tools sit beside the agent catalog when `[memory]` is enabled
(the default). They are served in-process — nothing is spawned:

| Tool | Arguments | Behavior |
|---|---|---|
| `memory-remember` | `text`, `topics[]` | Store a durable fact, linked to its topics. Restating a fact already stored reinforces it instead of duplicating it. |
| `memory-recall` | `query` or `topic`, `limit` | Rank facts by shared words then recency, or return everything linked to a topic. Each result leads with its id. |
| `memory-topics` | — | List the subjects memory knows about. |
| `memory-supersede` | `id`, `text`, `topics[]` | Replace a fact that stopped being true. The old one stays readable, stops being recalled. |
| `memory-forget` | `id` | Delete a fact outright. |

The ids in `memory-recall` output are what `memory-supersede` and
`memory-forget` take — a model that finds a wrong fact can fix it in the next
call instead of asking which one.

Facts live in an [outl](https://github.com/avelino/outl) workspace you can open
and edit. See [Memory](../concepts/memory.md) for what belongs in it and why
ranking is lexical rather than semantic.

The same verbs are available from a shell as
[`dotagent memory`](cli.md#memory) — no daemon required, which is what lets a
consolidation pass be an ordinary scheduled agent.

Turn them off with `[memory] enabled = false` in `config.toml`.

## Remediation tools

One tool per `[[preflight]] remediation` declared in a manifest, named
`remediate-<agent>-<plugin>`:

| Tool | Arguments | Behavior |
|---|---|---|
| `remediate-<agent>-<plugin>` | — | Run the declared command, return its output. |

The point is an alert you can answer. "preflight aborted by plugin
preflight-warp: warp-cli connect" names the fix, and until now the fix was
something you got up and typed.

**It takes no arguments**, deliberately: the command is fixed by the manifest,
so the model chooses *which* remediation, never *what* runs. And it is
declared by the operator rather than taken from the plugin's `suggest` string
— running a command a plugin wrote, triggered from a chat, is arbitrary
execution over an inbound path. See [threat model
V12](../security/threat-model.md#v12--remediation-from-a-chat-message).

Supervised with a 120-second deadline, audited as `remediation_invoked` at
`Critical`, and it does **not** re-run the agent: clearing the check and
dispatching a run stay two decisions.

## Skill tools

One tool per installed skill, plus two verbs that reach inside one:

| Tool | Arguments | Behavior |
|---|---|---|
| `skill-<name>` | — | Return the procedure, plus an index of the files beside it. |
| `skill-read` | `skill`, `path` | Return one supporting file from that skill. |
| `skill-run` | `skill`, `script`, `args[]` | Execute one `scripts/` entry, return its output. |

Loading a skill returns **text**. The tool description says so explicitly,
because these are usually written in the imperative ("Cut a new release") and a
model could otherwise read the call as performing the procedure.

`skill-read` exists because a skill is often more than one file. A procedure
that says "see `references/glossary.md`" assumes a filesystem tool the caller
may not have; without a way to fetch it, the model follows half a procedure and
never notices. The index returned alongside the body is what makes the request
possible.

Skills come from `~/.config/dotagent/skills/` **and** `~/.claude/skills/` — the
format is Anthropic's Agent Skills layout, so one written for Claude Code is
already installed. See [Skills](../concepts/skills.md), including what does not
port.

A broken `SKILL.md` does not fail `tools/list` the way a broken manifest does.
The consequences differ: an unparseable manifest means an agent silently never
runs, while an unparseable skill means one procedure is missing. It is logged
and reported by `doctor`; the catalog still serves.

Turn them off with `[skills] enabled = false`.

## Command tools

Two more when `[commands]` is enabled (the default) and at least one command is
installed:

| Tool | Arguments | Behavior |
|---|---|---|
| `command-get` | `name`, `args` | Resolve an invoked command into the prompt to follow, arguments substituted. |
| `command-list` | — | Every installed command, with what it does and takes. |

**Two tools, not one per command** — deliberately unlike `skill-*`. There, the
catalog is a menu the model picks from. A command was already picked by a human
before the model saw anything, so publishing N command tools would re-open that
decision and let a model call the wrong one. Pass `command.name` and
`command.args` from the trigger payload through unchanged.

Note `args` is a **string** here and an array for `run-*`: an agent's arguments
are argv, a command's are whatever the sender typed after the name.

See [Commands](../concepts/commands.md).

## Tool naming

MCP restricts tool names to `[a-zA-Z0-9_-]`, which agent names are not bound by. Each agent becomes `run-<sanitized-name>`:

| Agent | Tool |
|---|---|
| `disk-alert` | `run-disk-alert` |
| `hn.digest` | `run-hn-digest` |
| `ops/cost-report` | `run-ops-cost-report` |

Skills use the same sanitizer under a different prefix, `skill-<name>`, which is
what keeps a skill from ever colliding with an agent. Two *skills* can still
collide, and the same rule applies: first discovered wins, the loser is dropped
with a warning, and `doctor` names both.

The mapping is lossy — `a.b` and `a/b` both produce `run-a-b`. On a collision the first agent discovered wins and the later one is dropped from the catalog with a warning in the log. Shadowing silently would mean `tools/call` running an agent nobody chose.

The reverse direction is never reconstructed: a tool name is resolved by rebuilding the catalog and matching, not by transforming the string back.

## Tool description

`agent.description` from the manifest. Without one, the description falls back to the agent name plus its schedule ids, which still helps a model choose. Writing a real `description` is the single highest-leverage thing you can do for dispatch quality.

## Arguments

```json
{
  "schedule": "daily",
  "args": ["--verbose"]
}
```

Both optional. `schedule` picks which schedule's args to borrow, defaulting to the agent's first. `args` is appended after the manifest's own `run.args`. An agent with no schedules gets the synthetic id `trigger`.

## Results

Success returns the agent's stdout as text content:

```json
{ "content": [{ "type": "text", "text": "..." }], "isError": false }
```

An agent that fails is **not** a JSON-RPC error. It comes back as a result with `isError: true` and the exit code plus stderr in the text, so the model can read the failure and react:

```json
{ "content": [{ "type": "text", "text": "disk-alert exited 1\n..." }], "isError": true }
```

JSON-RPC errors are reserved for protocol faults — unknown method, unknown tool, malformed params. The distinction matters: a model told "that tool does not exist" behaves differently from one told "the tool ran and failed".

## Process model

Agents run **in the `dotagent mcp` process**, the same way `run-now` does. Not through the daemon.

Consequences, all shared with `run-now`:

- The subprocess tree does not appear in `dotagent status`, which reflects only the daemon's supervisor.
- Nothing reaps it when the daemon stops.

State is keyed off the `trigger-mcp` slug rather than the schedule's args, so an MCP-initiated run can never overwrite `last_success_at` for a cron window that never fired. See [Triggers](../concepts/triggers.md#what-a-triggered-run-keeps).

That isolates MCP runs from the **daemon**, not from **each other**. The slug is a constant, so two MCP clients running the same agent at the same moment write the same heartbeat file. Writes are serialized — the state store holds an exclusive lock — so the file never tears, but the later finisher wins and the earlier run's `finished_at` is lost. Scheduling is unaffected (the scheduled slug is a different file); the visible effect is `dotagent inspect` showing one of the two runs. A single `dotagent mcp` process handles requests one at a time, so this needs two clients to happen at all.

## Streams

stdout carries protocol only. All logging goes to stderr, so `RUST_LOG=debug dotagent mcp` stays safe to run under a client.

## Security

Enabling an MCP client to reach dotagent means that client can run any installed agent. That is the point, and it is also the whole blast radius — worth stating plainly:

- Over stdio the server inherits the trust of whatever spawned it. A local MCP client already runs as you.
- The `dotagent mcp` subcommand itself uses stdio only. It does not open a port
  or provide a TCP/HTTP endpoint, and nothing from this MCP transport is
  reachable from another machine.
- The catalog is the boundary. An agent that is not installed cannot be run, and arguments never reach a shell.

This statement is scoped to the MCP subcommand. A daemon may separately expose
the user-local Unix-socket [Local Client API](local-api.md) at
`$DOTAGENT_HOME/api.sock`; that API is not an MCP transport and has its own
limits and threat model.

See the [threat model](../security/threat-model.md).

## Checking it

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | dotagent mcp
```

One tool per installed agent. To exercise a call:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run-hello-fish","arguments":{}}}' \
  | dotagent mcp
```

A malformed manifest anywhere in the search path fails discovery, and `tools/list` answers with an error naming the parse failure rather than an empty catalog. A model told it has no tools would answer confidently and wrongly; an error is something you can see and fix.

## See also

- [Triggers](../concepts/triggers.md) — the general concept
- [Local Client API](local-api.md) — the daemon's Unix-socket trigger transport
- [Telegram](../concepts/telegram.md) — a chat front end built on this
- [CLI](cli.md#mcp)
