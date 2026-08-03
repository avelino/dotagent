#!/usr/bin/env bash
#
# hn-digest — example LLM agent for dotagent.
#
# The shape every LLM agent takes here: deterministic shell collects the
# data, one `claude -p` call does the single thing that needs judgment,
# deterministic shell renders the result. dotagent schedules it, retries
# it and notifies when it breaks.
#
# The Hacker News API needs no key. `claude -p` uses whatever auth the
# Claude Code CLI already has, so no API key is exported here either.
#
# See docs/guides/llm-agents.md for why it's built this way.

set -euo pipefail

# ---- dotagent preamble --------------------------------------------------
# Every AGENT_* var is injected by dotagent. The `:-` fallbacks let you run
# `bash agent.sh` by hand, outside the daemon, to debug.
AGENT_NAME="${AGENT_NAME:-hn-digest}"
AGENT_HOME="${AGENT_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
AGENT_TMPDIR="${AGENT_TMPDIR:-$(mktemp -d)}"
AGENT_DRY_RUN="${AGENT_DRY_RUN:-false}"

log() { printf '[%s] %s\n' "$AGENT_NAME" "$*" >&2; }
err() { printf '[%s] error: %s\n' "$AGENT_NAME" "$*" >&2; }

# Tunables from [env.extra] in agent.toml, so you change behaviour without
# touching the script.
STORY_COUNT="${HN_STORY_COUNT:-20}"
CLAUDE_MODEL="${HN_CLAUDE_MODEL:-sonnet}"
CLAUDE_TIMEOUT="${HN_CLAUDE_TIMEOUT:-300}"

# macOS ships no `timeout` (it's GNU coreutils). Perl's alarm is built in
# everywhere, and `exec` keeps the PID so the supervisor's kill-tree still
# reaches the real process.
run_with_timeout() {
  local seconds="$1"
  shift
  perl -e 'alarm shift; exec @ARGV' "$seconds" "$@"
}

command -v claude >/dev/null || {
  err "claude CLI not found in PATH"
  exit 1
}

# ---- Collect: HN top stories --------------------------------------------
# Plain curl + jq. Asking a model to do this would cost tokens and seconds
# for something that has one correct answer.
log "fetching top $STORY_COUNT HN stories"
mkdir -p "$AGENT_TMPDIR/raw"

curl -sf --max-time 20 "https://hacker-news.firebaseio.com/v0/topstories.json" |
  jq ".[0:$STORY_COUNT]" >"$AGENT_TMPDIR/raw/ids.json"

# One request per story, all in flight at once. Each writes its own file:
# parallel appends to a shared file interleave once a line crosses the pipe
# buffer.
while read -r id; do
  curl -sf --max-time 20 "https://hacker-news.firebaseio.com/v0/item/$id.json" |
    jq -c '{
      title,
      url: (.url // "https://news.ycombinator.com/item?id=\(.id)"),
      score: (.score // 0),
      comments: (.descendants // 0)
    }' >"$AGENT_TMPDIR/raw/story-$id.json" &
done < <(jq -r '.[]' "$AGENT_TMPDIR/raw/ids.json")
wait

jq -s '[.[] | select(.title != null)] | sort_by(-.score)' \
  "$AGENT_TMPDIR"/raw/story-*.json >"$AGENT_TMPDIR/stories.json"

story_count=$(jq 'length' "$AGENT_TMPDIR/stories.json")
log "collected $story_count stories"

if [[ "$story_count" -eq 0 ]]; then
  err "no stories collected, HN API unreachable?"
  exit 1
fi

# ---- Dry run stops here -------------------------------------------------
# `dotagent run <name> --dry-run` should never spend tokens. Check the
# collected data, skip the model.
if [[ "$AGENT_DRY_RUN" == "true" ]]; then
  log "dry run, skipping claude"
  jq . "$AGENT_TMPDIR/stories.json"
  exit 0
fi

# ---- Build the user prompt ----------------------------------------------
# Two separate channels. The system prompt (prompt.md) is static and holds
# the output contract. The user prompt is this run's data. Keeping them
# apart means you can review the contract in git and diff a bad run's
# input on its own.
prompt_file="$AGENT_TMPDIR/prompt.txt"
{
  echo "Today's top $story_count Hacker News stories, as JSON:"
  echo
  jq . "$AGENT_TMPDIR/stories.json"
  echo
  echo "Pick the 3 most interesting and summarize each one."
  echo "Follow the output format in your system prompt exactly."
} >"$prompt_file"

# ---- Run claude ---------------------------------------------------------
# `-p -` reads the user prompt from stdin, which sidesteps argv length
# limits and shell quoting entirely.
#
# --allowedTools is the safety belt: headless has no human to approve a
# tool call, so the run gets exactly the tools it needs and nothing else.
system_prompt=""
[[ -f "$AGENT_HOME/prompt.md" ]] && system_prompt=$(cat "$AGENT_HOME/prompt.md")

raw="$AGENT_TMPDIR/digest_raw.txt"
stderr_file="$AGENT_TMPDIR/claude_stderr.txt"
log "running claude (model=$CLAUDE_MODEL, timeout=${CLAUDE_TIMEOUT}s)"

claude_rc=0
run_with_timeout "$CLAUDE_TIMEOUT" claude \
  --model "$CLAUDE_MODEL" \
  --allowedTools "WebFetch" \
  --system-prompt "$system_prompt" \
  -p - <"$prompt_file" >"$raw" 2>"$stderr_file" || claude_rc=$?

if [[ "$claude_rc" -ne 0 ]]; then
  err "claude exited $claude_rc"
  [[ -s "$stderr_file" ]] && err "$(tail -5 "$stderr_file")"
  exit 1
fi

# Empty output is the most common LLM failure and it does NOT come with a
# non-zero exit. Guard it, or you publish a blank digest.
if [[ ! -s "$raw" ]]; then
  err "claude returned empty output"
  exit 1
fi

# ---- Render for the sink ------------------------------------------------
# Hierarchy convention shared by every sink plugin (sink-file, sink-roam,
# sink-outl):
#   line 1, no indent  → root block
#   indent ≤ 3 spaces  → L1
#   indent > 3 spaces  → L2, child of the previous L1
#
# The root line comes from the script, not the model. Anything the output
# depends on for correctness stays deterministic.
#
# prompt.md tells the model not to emit blank lines, but the parser counts
# indentation strictly and one stray blank line flattens the tree, so strip
# them here too. Never let the contract live only in the prompt.
echo "#hn-digest $(date +%Y-%m-%d)"
sed '/^[[:space:]]*$/d' "$raw"
