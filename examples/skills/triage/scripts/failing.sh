#!/usr/bin/env bash
#
# failing.sh — the agents that are not ok, one per line.
#
# `dotagent status` prints a summary block and then every agent. On a machine
# with thirty agents and one broken, the table is the noise and this is the
# signal. Called through `skill-run`, so stdout is what the model reads.

set -euo pipefail

command -v dotagent >/dev/null || {
  echo "dotagent is not in PATH" >&2
  exit 1
}

# Summary lines are indented; table rows start at column 0 with the agent name.
# Anything carrying a non-ok icon and starting at column 0 is a row worth
# reporting.
rows="$(dotagent status | grep -E '❌|⚠️|🕑' | grep -v '^ ' || true)"

if [[ -z "$rows" ]]; then
  echo "All agents ok."
  exit 0
fi

printf '%s\n' "$rows"
