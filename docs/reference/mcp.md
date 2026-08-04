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

## Tool naming

MCP restricts tool names to `[a-zA-Z0-9_-]`, which agent names are not bound by. Each agent becomes `run-<sanitized-name>`:

| Agent | Tool |
|---|---|
| `disk-alert` | `run-disk-alert` |
| `hn.digest` | `run-hn-digest` |
| `buser/finops` | `run-buser-finops` |

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
- There is no network listener. No port, no auth to configure, nothing reachable from another machine.
- The catalog is the boundary. An agent that is not installed cannot be run, and arguments never reach a shell.

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
- [Telegram](../concepts/telegram.md) — a chat front end built on this
- [CLI](cli.md#dotagent-mcp)
