#!/usr/bin/env bash
#
# telegram-assistant — turn a chat message into an agent run.
#
# Reads the message dotagent handed over, gives it to `claude -p` with
# dotagent's MCP server attached, and streams the answer back on stdout as
# assistant-v1 frames — one JSON object per line:
#
#   {"type":"delta","text":...}                      chunk as it is composed
#   {"type":"reply","text":...}                      the final answer
#   {"type":"session","claude_session":...,...}      bookkeeping, best-effort
#
# A failed run prints no reply and exits non-zero; the daemon reports the
# failure through its own notifiers. Every line this script prints on stdout
# is a valid frame of one of those three types.
#
# The session belongs to this agent, not to Telegram. dotagent is a harness:
# it hands over an opaque AGENT_SESSION_ID and the conversation keyed by it
# stays whatever this script makes of it. The chat id survives only as a
# legacy fallback so in-flight Telegram conversations keep their history
# across the upgrade.
#
# The model never composes a command. It picks a tool from `tools/list`,
# which is one entry per agent already on disk — a name that does not exist
# is not callable. That property comes from the protocol, not from a check
# in this script.
#
# Bash here is an example, not a contract. Rewrite it in anything that reads
# env vars, exits with a code, and prints assistant-v1 frames.
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

for bin in claude jq dotagent python3; do
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

# ---- Conversation key -----------------------------------------------------
# One conversation per SESSION_ID, so "sim" refers to whatever was just
# proposed. The daemon owns the id and it is opaque — this script never
# parses it, only uses it as a key. The chat id fallback keeps Telegram
# conversations started before the upgrade continuous; "default" covers
# running the script by hand with neither set.
SESSION_ID="${AGENT_SESSION_ID:-}"
if [[ -z "$SESSION_ID" ]]; then
	SESSION_ID="$(printf '%s' "$AGENT_TRIGGER_PAYLOAD" | jq -r '.chat_id // empty')"
fi
if [[ -z "$SESSION_ID" ]]; then
	SESSION_ID="default"
fi
# Opaque does not mean filesystem-safe: keep the key inside one path
# segment even if a future daemon puts slashes in it.
SESSION_KEY="$(printf '%s' "$SESSION_ID" | tr -c 'A-Za-z0-9._-' '_')"

session_dir="${DOTAGENT_HOME:-$HOME/.config/dotagent}/state/telegram-assistant"
mkdir -p "$session_dir"

# ---- claude capabilities --------------------------------------------------
# Probed per run, cheap against a model call, and the honest way to treat
# the CLI as an dependency: `--session-id` buys deterministic session ids
# (uuid5 below), `--include-partial-messages` buys per-token deltas.
claude_help="$(claude --help 2>&1 || true)"
if grep -q -- '--session-id' <<<"$claude_help"; then
	has_session_id_flag=1
else
	has_session_id_flag=0
fi
if grep -q -- '--include-partial-messages' <<<"$claude_help"; then
	has_partial_flag=1
else
	has_partial_flag=0
fi

# Session uuid, derived not stored, so it survives daemon restarts.
# The seed is "dotagent-assistant-<SESSION_ID>-g<gen>": the rename from the
# older "dotagent-telegram-<chat_id>-..." means the first run after this
# upgrade starts a fresh conversation per session (new uuid, marker
# mismatch, `--session-id` create). One-time cost, accepted and noted in
# the CHANGELOG by the daemon-side slice.
seed_uuid() {
	printf '%s' "dotagent-assistant-$SESSION_ID-g$1" |
		python3 -c 'import sys,uuid; print(uuid.uuid5(uuid.NAMESPACE_DNS, sys.stdin.read()))'
}

# A generation counter, bumped when a session is retired for length.
gen_file="$session_dir/$SESSION_KEY.gen"
marker="$session_dir/$SESSION_KEY.started"
generation="$(cat "$gen_file" 2>/dev/null || echo 0)"
current_session=""
if [[ "$has_session_id_flag" -eq 1 ]]; then
	current_session="$(seed_uuid "$generation")"
fi

# `--resume` replays the whole transcript as model input, and nothing trims
# it, so a chat gets slower the more you use it. Measured on a real bot: a
# ~90 KB transcript answers in 8-10s, a 977 KB one in 26-141s.
#
# The ceiling is a compromise, not a fix — retiring costs the recent back and
# forth. That is why an assistant like this should store durable facts with
# `memory-remember`: continuity that matters belongs there, not in a
# transcript that grows until it is unusable. See docs/concepts/memory.md.
max_transcript_kb=400
# claude names the project directory after the PHYSICAL path, so `pwd -P`.
project_slug="$(pwd -P | tr '/.' '-')"
transcript_for() {
	printf '%s/.claude/projects/%s/%s.jsonl' "$HOME" "$project_slug" "$1"
}

# Which session would this run continue? Deterministic mode knows its uuid
# up front; legacy mode only knows the id the previous run left in the marker.
retire_candidate="$current_session"
if [[ -z "$retire_candidate" ]]; then
	retire_candidate="$(cat "$marker" 2>/dev/null || true)"
fi
if [[ -n "$retire_candidate" ]]; then
	transcript="$(transcript_for "$retire_candidate")"
	if [[ -f "$transcript" ]]; then
		kb=$(($(wc -c <"$transcript") / 1024))
		if ((kb > max_transcript_kb)); then
			generation=$((generation + 1))
			printf '%s' "$generation" >"$gen_file"
			rm -f "$marker"
			if [[ "$has_session_id_flag" -eq 1 ]]; then
				current_session="$(seed_uuid "$generation")"
			fi
			log "transcript at ${kb}KB — retiring session, generation $generation"
		fi
	fi
fi

# `--session-id` creates, `--resume` continues, and using the wrong one is
# an error. The marker records which state this conversation is in; legacy
# mode resumes whatever id the previous run captured from the stream.
session_flag=()
if [[ "$has_session_id_flag" -eq 1 ]]; then
	if [[ -f "$marker" && "$(cat "$marker")" == "$current_session" ]]; then
		session_flag=(--resume "$current_session")
	else
		session_flag=(--session-id "$current_session")
	fi
elif [[ -f "$marker" ]]; then
	session_flag=(--resume "$(cat "$marker")")
fi

# ---- Stream shape ---------------------------------------------------------
# claude's stream-json, translated to assistant-v1 by one long-lived jq
# (not one jq per line — the stream can carry hundreds of partial-message
# frames and per-line process spawns would cost seconds).
#
# With partial messages, deltas come from text_delta events as tokens land.
# Without, each assistant message's text blocks are the finest granularity
# available. The `result` line closes the run: its `result` field is the
# final answer, its `session_id` is the conversation, `is_error` the health.
# Everything else in the stream — system, hooks, rate limits — is dropped.
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

# The `done` frame is internal bookkeeping ferried out of the pipeline
# through a file (the pipeline's last stage is a subshell); it is never
# printed. Deltas pass through untouched, as claude+jq wrote them.
meta="$AGENT_TMPDIR/done.frame"
raw_stream="$AGENT_TMPDIR/claude-stream.jsonl"
: >"$meta"

# The message goes in on stdin, never interpolated into the prompt string —
# an argv-borne body would be one quoting bug away from a shell problem.
set +e
printf '%s' "$message" | run_with_timeout "$CLAUDE_TIMEOUT" \
	claude \
	--model "$MODEL" \
	--append-system-prompt "$(cat "$AGENT_HOME/prompt.md")" \
	--mcp-config "$AGENT_TMPDIR/mcp.json" \
	--strict-mcp-config \
	--allowedTools "mcp__dotagent" \
	"${stream_flags[@]}" \
	${session_flag[@]+"${session_flag[@]}"} \
	-p - |
	tee "$raw_stream" |
	jq -c "$frame_filter" |
	{
		while IFS= read -r frame; do
			case "$frame" in
			'{"type":"delta",'*)
				printf '%s\n' "$frame"
				;;
			'{"type":"done",'*)
				printf '%s\n' "$frame" >"$meta"
				;;
			esac
		done
	}
status=$?
set -e

# Every failure from here on is the same contract: say why on stderr, print
# no reply, exit non-zero. The daemon owns failure reporting.
if [[ $status -ne 0 ]]; then
	err "claude exited $status"
	if [[ $status -eq 142 ]]; then
		err "gave up after ${CLAUDE_TIMEOUT}s"
	fi
	exit "$status"
fi

done_frame="$(cat "$meta" 2>/dev/null || true)"
if [[ -n "$done_frame" ]]; then
	reply_text="$(printf '%s' "$done_frame" | jq -r '.text')"
	ran_session="$(printf '%s' "$done_frame" | jq -r '.session')"
	is_error="$(printf '%s' "$done_frame" | jq -r '.error')"
	if [[ "$is_error" == "true" ]]; then
		err "model run reported an error (result.is_error)"
		exit 1
	fi
	# Legacy mode learns the session id only now, from the stream.
	if [[ -z "$ran_session" ]]; then
		ran_session="$current_session"
	fi
else
	# claude succeeded but printed nothing this filter recognizes — an older
	# CLI, or an output shape this script has never seen. The whole stdout
	# becomes one reply frame so the run still answers.
	err "no result frame in stream — falling back to raw stdout as reply"
	reply_text="$(cat "$raw_stream")"
	ran_session="$current_session"
fi

# Only after a run that actually created it. Writing the marker up front
# would leave a conversation permanently broken if the first call failed:
# every later message would `--resume` a session that never existed.
if [[ -n "$ran_session" ]]; then
	printf '%s' "$ran_session" >"$marker"
fi

# Bookkeeping frame, best-effort: emit when the session and its transcript
# are known, stay silent otherwise. The daemon ignores it either way.
if [[ -n "$ran_session" ]]; then
	transcript="$(transcript_for "$ran_session")"
	if [[ -f "$transcript" ]]; then
		bytes="$(wc -c <"$transcript" | tr -d '[:space:]')"
		jq -cn --arg s "$ran_session" --argjson b "$bytes" \
			'{type:"session",claude_session:$s,transcript_bytes:$b}'
	fi
fi

if [[ -z "${reply_text// /}" ]]; then
	reply_text="I had nothing to say about that."
fi

# Exactly one reply frame ends every successful run.
jq -cn --arg t "$reply_text" '{type:"reply",text:$t}'
