#!/usr/bin/env bash
# telegram-assistant v2 — thin dispatcher on the daemon's [assistant] harness.
#
# What the daemon now owns (see agent.toml):
#   AGENT_ASSISTANT_SESSION      recorded claude session pointer (--resume)
#   AGENT_ASSISTANT_MCP_CONFIG   assembled, content-addressed mcp.json
#   AGENT_ASSISTANT_MEMORY       recalled facts, prepended below as context
#   MEMO: strip + persist        daemon-side, after this run
#
# What this script still owns: composing the user message from the trigger
# payload, one claude call, translating stream-json to assistant-v1 frames.
# ~150 lines instead of the old 560-line fish + pool + memory.py.

set -euo pipefail

ASSISTANT_DIR="/Users/avelino/dotfiles/modules/home-manager/dotagent/agents/telegram-assistant"
cd "$ASSISTANT_DIR" # deterministic cwd ⇒ deterministic claude transcript dir

log() { printf '%s\n' "$*" >&2; }

MODEL="${TELEGRAM_ASSISTANT_MODEL:-sonnet}"
CLAUDE_TIMEOUT="${TELEGRAM_ASSISTANT_CLAUDE_TIMEOUT:-540}"

# ---- Harness contract ------------------------------------------------------
# Missing AGENT_ASSISTANT_* means an old daemon: fail loudly, not silently.
if [[ -z "${AGENT_ASSISTANT_MCP_CONFIG:-}" ]]; then
	log "telegram-assistant v2 requires the rebuilt daemon ([assistant] harness); AGENT_ASSISTANT_MCP_CONFIG is missing"
	exit 78 # EX_CONFIG: configuration error, not a model failure
fi
MCP_CONFIG="$AGENT_ASSISTANT_MCP_CONFIG"
SESSION_FLAG=()
if [[ -n "${AGENT_ASSISTANT_SESSION:-}" ]]; then
	SESSION_FLAG=(--resume "$AGENT_ASSISTANT_SESSION")
fi

# ---- Message composition ---------------------------------------------------
payload="${AGENT_TRIGGER_PAYLOAD:-}"
text="$(printf '%s' "$payload" | jq -r '.text // empty')"
if [[ -z "$text" ]]; then
	log "trigger payload carries no text"
	exit 66 # EX_NOINPUT
fi

reply_context=""
reply_text="$(printf '%s' "$payload" | jq -r '.reply_to_text // empty')"
if [[ -n "$reply_text" ]]; then
	reply_context="[Respondendo a esta mensagem sua:]
$(printf '%s' "$reply_text" | jq -Rs .)

"
fi

command_line=""
cmd_name="$(printf '%s' "$payload" | jq -r '.command.name // empty')"
if [[ -n "$cmd_name" ]]; then
	cmd_args="$(printf '%s' "$payload" | jq -r '.command.args // empty')"
	command_line="(comando invocado: /$cmd_name $cmd_args)
"
fi

memory_block=""
if [[ -n "${AGENT_ASSISTANT_MEMORY:-}" ]]; then
	memory_block="$AGENT_ASSISTANT_MEMORY

"
fi

message="${reply_context}${command_line}${memory_block}${text}"

# ---- Tool allowlist --------------------------------------------------------
# Same derivation the old agent used: everything the proxy exposes, minus
# ai-memory (two memories in one toolkit is a documented bug). Sorted for
# stable diffs; session invalidation is the daemon's job now, not ours.
allowed="mcp__dotagent"
if command -v mcp >/dev/null 2>&1; then
	proxy_list="$(mcp --list 2>/dev/null | jq -r '.[].name' | grep -v '^ai-memory$' | sort | xargs -I{} printf ' mcp__mcp__{}' || true)"
	if [[ -n "$proxy_list" ]]; then
		allowed="mcp__dotagent$proxy_list"
	fi
else
	log "mcp proxy not on PATH; allowing only the dotagent catalog"
fi

# ---- Stream shape ----------------------------------------------------------
# claude's stream-json → assistant-v1 in one long-lived jq.
has_partial_flag=0
if claude --help | grep -q -- '--include-partial-messages'; then
	has_partial_flag=1
fi
if [[ "$has_partial_flag" -eq 1 ]]; then
	delta_clause='if .type == "stream_event" and .event.type == "content_block_delta" and .event.delta.type == "text_delta" then {type:"delta",text:.event.delta.text}'
else
	delta_clause='if .type == "assistant" then ((.message.content // []) | map(select(.type == "text") | .text) | join("")) as $t | select($t != "") | {type:"delta",text:$t}'
fi
frame_filter="$delta_clause elif .type == \"result\" then {type:\"done\",text:(.result // \"\"),session:(.session_id // \"\"),error:(.is_error // false)} else empty end"

stream_flags=(--output-format stream-json)
if [[ "$has_partial_flag" -eq 1 ]]; then
	stream_flags+=(--include-partial-messages)
fi

# ---- The call --------------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

set +e
printf '%s' "$message" | perl -e 'alarm shift; exec @ARGV' "$CLAUDE_TIMEOUT" \
	claude \
	--model "$MODEL" \
	--append-system-prompt "$(cat "$ASSISTANT_DIR/prompt.md")" \
	--mcp-config "$MCP_CONFIG" \
	--strict-mcp-config \
	--allowedTools "$allowed" \
	"${stream_flags[@]}" \
	${SESSION_FLAG[@]+"${SESSION_FLAG[@]}"} \
	-p - |
	tee "$tmp/raw" |
	jq -c "$frame_filter" |
	{
		while IFS= read -r frame; do
			case "$frame" in
			'{"type":"delta",'*)
				printf '%s\n' "$frame"
				;;
			'{"type":"done",'*)
				printf '%s\n' "$frame" >"$tmp/meta"
				;;
			esac
		done
	}
status=$?
set -e

if [[ $status -ne 0 ]]; then
	log "claude exited $status"
	[[ $status -eq 142 ]] && log "gave up after ${CLAUDE_TIMEOUT}s"
	exit "$status"
fi

done_frame="$(cat "$tmp/meta" 2>/dev/null || true)"
if [[ -z "$done_frame" ]]; then
	log "no result frame in stream — falling back to raw stdout as reply"
	reply_text="$(cat "$tmp/raw")"
	printf '%s\n' "$(jq -cn --arg t "$reply_text" '{type:"reply",text:$t}')"
	exit 0
fi

reply_text="$(printf '%s' "$done_frame" | jq -r '.text')"
ran_session="$(printf '%s' "$done_frame" | jq -r '.session')"
is_error="$(printf '%s' "$done_frame" | jq -r '.error')"
if [[ "$is_error" == "true" ]]; then
	log "model run reported an error (result.is_error)"
	exit 1
fi

# Session frame: the transcript file's size is what retirement keys on.
# claude slugifies the cwd by turning every '/' into '-' (verified against
# the existing transcript dir for this agent).
project_dir="$HOME/.claude/projects/$(printf '%s' "$ASSISTANT_DIR" | sed 's:/:-:g')"
transcript="$project_dir/$ran_session.jsonl"
transcript_bytes=0
[[ -f "$transcript" ]] && transcript_bytes="$(stat -f%z "$transcript" 2>/dev/null || echo 0)"
printf '%s\n' "$(jq -cn --arg s "$ran_session" --argjson b "$transcript_bytes" '{type:"session",claude_session:$s,transcript_bytes:$b}')"

printf '%s\n' "$(jq -cn --arg t "$reply_text" '{type:"reply",text:$t}')"
