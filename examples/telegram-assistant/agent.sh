#!/usr/bin/env bash
#
# telegram-assistant — turn a chat message into an agent run.
#
# Reads the message dotagent handed over, gives it to `claude -p` with
# dotagent's MCP server attached, and prints the answer. The daemon sends
# stdout back to the conversation that asked.
#
# The model never composes a command. It picks a tool from `tools/list`,
# which is one entry per agent already on disk — a name that does not exist
# is not callable. That property comes from the protocol, not from a check
# in this script.
#
# Bash here is an example, not a contract. Rewrite it in anything that reads
# env vars and exits with a code.
#
# See docs/concepts/telegram.md and docs/guides/llm-agents.md.

set -euo pipefail

# ---- dotagent preamble --------------------------------------------------
# The `:-` fallbacks let you run `bash agent.sh` by hand to debug.
AGENT_NAME="${AGENT_NAME:-telegram-assistant}"
AGENT_TMPDIR="${AGENT_TMPDIR:-$(mktemp -d)}"
AGENT_HOME="${AGENT_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"

log() { printf '[%s] %s\n' "$AGENT_NAME" "$*" >&2; }
err() { printf '[%s] error: %s\n' "$AGENT_NAME" "$*" >&2; }

MODEL="${TELEGRAM_ASSISTANT_MODEL:-sonnet}"
CLAUDE_TIMEOUT="${TELEGRAM_ASSISTANT_CLAUDE_TIMEOUT:-540}"

# macOS ships no `timeout` (GNU coreutils). Perl's alarm is built in
# everywhere, and `exec` keeps the PID so the supervisor's kill-tree still
# reaches the real process.
run_with_timeout() {
  local seconds="$1"
  shift
  perl -e 'alarm shift; exec @ARGV' "$seconds" "$@"
}

for bin in claude jq dotagent; do
  command -v "$bin" >/dev/null || {
    err "$bin not found in PATH"
    exit 1
  }
done

# ---- The message --------------------------------------------------------
# AGENT_TRIGGER_PAYLOAD is JSON, set by the daemon for triggered runs. It
# arrives in the environment rather than argv so quoting and shell
# metacharacters in the message body are never interpreted.
if [[ -z "${AGENT_TRIGGER_PAYLOAD:-}" ]]; then
  err "no AGENT_TRIGGER_PAYLOAD — this agent only runs from a trigger"
  err "try: dotagent run-now telegram-assistant  (after exporting a payload)"
  exit 1
fi

message="$(printf '%s' "$AGENT_TRIGGER_PAYLOAD" | jq -r '.text // empty')"
if [[ -z "$message" ]]; then
  err "payload carried no text"
  exit 1
fi
log "handling message from user ${AGENT_TRIGGER_ACTOR:-unknown}"

# ---- MCP wiring ---------------------------------------------------------
# `dotagent mcp` exposes one tool per discovered agent. Spawned fresh per
# run: the catalog is read from disk at startup, so a manifest added since
# the last message shows up without restarting anything.
cat >"$AGENT_TMPDIR/mcp.json" <<'JSON'
{
  "mcpServers": {
    "dotagent": {
      "command": "dotagent",
      "args": ["mcp"]
    }
  }
}
JSON

# The message goes in on stdin, never interpolated into the prompt string —
# an argv-borne body would be one quoting bug away from a shell problem.
set +e
answer="$(
  printf '%s' "$message" | run_with_timeout "$CLAUDE_TIMEOUT" \
    claude \
    --model "$MODEL" \
    --system-prompt "$(cat "$AGENT_HOME/prompt.md")" \
    --mcp-config "$AGENT_TMPDIR/mcp.json" \
    --allowedTools "mcp__dotagent" \
    -p -
)"
status=$?
set -e

if [[ $status -ne 0 ]]; then
  err "claude exited $status"
  # Say something useful in the chat rather than leaving the sender waiting
  # on a message that silently never comes.
  if [[ $status -eq 142 ]]; then
    echo "That took longer than ${CLAUDE_TIMEOUT}s and I gave up."
  else
    echo "I could not reach the model (exit $status)."
  fi
  exit "$status"
fi

if [[ -z "${answer// /}" ]]; then
  echo "I had nothing to say about that."
  exit 0
fi

# stdout is the reply. Everything above logged to stderr precisely so this
# is the only thing that reaches the chat.
printf '%s\n' "$answer"
