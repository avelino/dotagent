# Health states

Fetched on demand — the four states and what each one is actually telling you.

| State | Meaning | Is it a problem? |
|---|---|---|
| `ok` | Last run in the current window succeeded. | No. |
| `degraded` | Last run failed, retries remain in this window. | Not yet. Worth naming, not worth waking anyone. |
| `failing` | The window is exhausted; dotagent gave up on it. | Yes. This is the one to lead with. |
| `stale` | No run recorded for a window that should have had one. | **Depends.** See below. |

## Why `stale` is the one people get wrong

`stale` means dotagent expected a run and does not see one. Two very different
causes produce it:

- **The daemon was not running** — laptop asleep, unit not loaded, machine off.
  Nothing is wrong with the agent.
- **The schedule never fired** when it should have. That is a real problem.

Check the agent's schedule before calling it broken. An agent that runs on
weekdays at 08:00 is legitimately stale from Friday evening until Monday
morning, and reporting that as a failure trains whoever reads it to ignore you.

## Reading a failure

`degraded` and `failing` both carry the exit code and the tail of stderr. Quote
it. A paraphrase of an error message loses the one detail — a path, a status
code, a missing binary — that makes the difference between fixing it and
guessing.
