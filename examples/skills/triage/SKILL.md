---
name: triage
description: How to investigate an agent that is failing or stale, and what to say about it. Use when asked "is everything ok?", "why did X fail?", "what's broken?", or when a health check comes back with anything other than ok.
---

# Triage

The goal is one useful sentence per broken agent, not a status dump. Anyone
asking already suspects something is wrong; they need to know **what** and
**whether it matters**.

## Steps

1. Call `dotagent-status`. If everything is `ok`, say so in one line and stop.
2. For each agent that is not `ok`, call `dotagent-logs` with that agent's name.
   Read the tail — the last failure is almost always the whole story.
3. Classify what you found using `references/health-states.md`
   (fetch it with `skill-read`). The four states mean different things and
   `stale` in particular is not a failure.
4. Answer in this shape, one line per agent:

   ```
   <agent> — <state>: <what actually went wrong, quoting the error>
   ```

5. Only if asked to fix something: propose the specific `run-<agent>` call and
   wait. Re-running an agent that failed for an environmental reason just
   produces a second identical failure.

## Shortcut

`scripts/failing.sh` prints just the agents that are not `ok`, one per line.
Run it with `skill-run` when the full status table would be noise — a machine
with thirty agents and one broken.

## What not to do

- Do not call `dotagent-doctor` for this. It validates configuration, which is
  a different question from "did the last run work".
- Do not guess at a cause from the agent's name. Quote the log or say you could
  not find one.
- Do not report a `stale` agent as broken. Check what its schedule is first —
  a weekday-only agent is stale all weekend by design.
