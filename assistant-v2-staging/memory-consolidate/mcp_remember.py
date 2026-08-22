#!/usr/bin/env python3
"""Re-ingest MEMO lines through the dotagent MCP memory door.

Reads `MEMO: <fact> | topics: a, b` and `TODO até <date>: <action> | topics: ...`
lines from stdin and calls
`memory-remember` on the daemon's MCP server, one JSON-RPC call per fact
(same protocol memory.py in the old telegram-assistant spoke, kept as the
proven pattern). Failures are logged to stderr and skipped — best effort.
"""

import json
import subprocess
import sys

MCP_BIN = "dotagent"


def remember(fact: str, topics: list[str]) -> bool:
    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory-remember",
            "arguments": {"text": fact, "topics": topics},
        },
    }
    try:
        proc = subprocess.run(
            [MCP_BIN, "mcp"],
            input=json.dumps(request) + "\n",
            capture_output=True,
            text=True,
            timeout=30,
        )
        return proc.returncode == 0
    except Exception as exc:  # noqa: BLE001 — best effort by contract
        print(f"remember failed: {exc}", file=sys.stderr)
        return False


def main() -> int:
    written = 0
    for line in sys.stdin:
        line = line.strip()
        if line.startswith("MEMO: "):
            body = line[len("MEMO: ") :]
        elif line.startswith("TODO até "):
            # The due-date prefix stays in the fact text: follow-up-sweeper
            # greps the projected journal for "TODO até" to find what is due.
            body = line
        else:
            continue
        if " | topics: " in body:
            fact, _, tail = body.partition(" | topics: ")
            topics = [t.strip() for t in tail.split(",") if t.strip()]
        else:
            fact, topics = body, []
        if fact and remember(fact, topics):
            written += 1
    print(f"re-ingested {written} facts", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
