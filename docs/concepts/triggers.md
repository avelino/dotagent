# Triggers

> A run that no clock asked for.

Every run used to come from a `[[schedules]]` window: cron, interval, or expression. A trigger is the other cause — a chat message arrived, an MCP client called a tool, an agent finished and wants to start the next one.

```mermaid
flowchart LR
    clock([clock]) --> daemon
    msg([a message]) --> daemon
    tool([an MCP tools/call]) --> daemon
    daemon --> run[run an agent]
    run --> back([the answer reaches whoever asked])
```

## What a triggered run keeps

Everything a scheduled run has. Same supervisor, same heartbeat, same audit trail, same `[[on_success]]` / `[[on_failure]]` hooks, same built-in notifiers. A trigger changes *why* a run started, not *how* it is run.

Two things are deliberately different.

**No window state.** A triggered run has no attempts counter, never consumes a retry, and can never mark a cron window as given up. Retry policy exists to answer "did the 08:30 window succeed"; a message at 14:00 is not an answer to that question.

**Its own slug.** State files are keyed by `trigger-<source>` instead of the schedule's args, so `~/.config/dotagent/state/agents/<name>/trigger-telegram.heartbeat.json` sits beside the scheduled history rather than on top of it. Without this, asking an agent to run on demand would write `last_success_at` for a window that never fired, and the scheduler would stop retrying a genuinely missed run.

## Sources

| Source | Who produces it | Answers back? |
|---|---|---|
| `telegram` | the daemon's inbound poller — see [Telegram](telegram.md) | yes, in the same chat |
| `mcp` | `dotagent mcp`, one tool per agent — see [MCP server](../reference/mcp.md) | yes, as the tool result |
| `cli` | reserved | no |

## Serialization

Triggers arrive on a channel the daemon reads from the same `select!` that handles its sleep and signals. A triggered run therefore never overlaps the tick loop.

That is a correctness requirement, not a performance choice. Two concurrent runs of the same `(agent, slug)` would race the heartbeat's read-modify-write and the state store's lockfile handling. Serial execution keeps both dormant.

The practical consequence: a long agent delays the next trigger. A ten-minute run means a message arriving at minute two waits. If that matters for your setup, keep the dispatcher fast and let it hand slow work to something else.

`[lifecycle] mode = "persistent"` does not change this. It removes the startup cost of each run, not the queue.

## What the agent receives

Trigger context arrives as environment variables, alongside the usual `AGENT_*` block:

| Variable | Meaning |
|---|---|
| `AGENT_TRIGGER_SOURCE` | `telegram`, `mcp`, `cli` |
| `AGENT_TRIGGER_ACTOR` | who asked, as the source can attest it |
| `AGENT_TRIGGER_REPLY_TO` | opaque handle for the conversation to answer |
| `AGENT_TRIGGER_PAYLOAD` | source-specific JSON body |

`AGENT_TRIGGER_PAYLOAD` carries the message text, plus a `command` object when the sender invoked one — see [Commands](commands.md#the-payload). It rides in the environment rather than argv on purpose: a body that reached argv would be one quoting bug away from a shell problem, and every current producer is bounded well under `ARG_MAX` (a Telegram message caps at 4096 characters). A source with unbounded payloads should write a file into `AGENT_TMPDIR` rather than grow this variable.

These variables are applied *before* the `AGENT_*` block, so a payload can never redefine `AGENT_NAME` or `AGENT_HEARTBEAT_FILE`.

A [persistent](lifecycle.md) agent gets none of them. An environment is fixed at spawn and that process answers many different messages, so the same context arrives in the `trigger` field of each request frame instead — see [the persistent protocol](../reference/persistent-protocol.md).

An agent with no `[[schedules]]` at all is legal and gets the synthetic schedule id `trigger`. That is the natural shape for an agent that only ever runs because someone asked.

## Answering

Whatever the agent prints on stdout goes back to whoever asked, when the source supports it. Log to stderr — it is captured for `dotagent logs` and stays out of the reply.

The agent that a dispatcher *calls* never learns a chat exists. Only the triggered agent has `AGENT_TRIGGER_REPLY_TO`, and it is the one whose stdout gets delivered.

## Security

A trigger can originate from outside the machine. The full posture is in the [threat model](../security/threat-model.md); the short version:

- `agent` in a trigger is a **selector, not a command**. It resolves against manifests already on disk, and anything else is refused.
- Nothing in a trigger reaches a shell.
- Every accepted trigger writes `trigger_received` and `agent_triggered` to the audit log; every refused one writes `trigger_rejected` at `Critical`.
- Message bodies are never written to the audit log. It records attribution, not transcripts.

## See also

- [Lifecycle](lifecycle.md) — keeping a dispatcher alive between messages
- [Telegram](telegram.md) — the inbound chat source
- [MCP server](../reference/mcp.md) — agents as tools
- [`examples/telegram-assistant`](https://github.com/avelino/dotagent/tree/main/examples/telegram-assistant) — the whole loop end to end
