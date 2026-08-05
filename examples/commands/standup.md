---
description: What ran overnight, in three lines
argument-hint: "[agent]"
allowed-tools: dotagent-status, dotagent-logs, memory-recall
---

Report what the orchestrator did while I was away.

Scope: $ARGUMENTS — if that is empty, every agent.

1. Call `dotagent-status`.
2. For anything that is not `ok`, call `dotagent-logs` for that agent and read
   the tail.
3. Answer in exactly three lines:
   - what ran and finished clean, as a count
   - what broke, one agent per clause, with the reason
   - what I should do about it, or "nothing" when that is the honest answer

No preamble. If everything is `ok`, the whole answer is one line.
