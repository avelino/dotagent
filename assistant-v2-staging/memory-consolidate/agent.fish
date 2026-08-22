#!/usr/bin/env fish
# memory-consolidate — nightly distillation.
#
# Reads TODAY's journal .md from the dotagent memory workspace (the outl
# projection the dispatcher's MEMO flush writes), asks haiku to distill
# durable facts + TODOs, and re-ingests them through the same MCP door the
# dispatcher uses (JSON-RPC to `dotagent mcp`, one call per fact) so topic
# pages and backlinks stay consistent.
#
# Failure posture: best-effort. Empty journal, unreachable model or a failed
# write logs to stderr and exits 0 — tomorrow retries on a fuller journal.

set -l script_dir (dirname (status filename))
set -l memroot ~/.config/dotagent/outl
set -l today (date +%Y-%m-%d)
set -l journal $memroot/journals/$today.md

if not test -f "$journal"
    echo "no journal for $today — nothing to consolidate" >&2
    exit 0
end

set -l facts (cat "$journal" | string collect)
if test -z "$facts"
    echo "journal for $today is empty" >&2
    exit 0
end

# haiku is enough: reshape-and-tag, no judgment worth sonnet.
set -l distilled (printf '%s\n' "$facts" | claude -p \
    --model haiku \
    --strict-mcp-config \
    --settings '{"hooks":{}}' \
    --system-prompt-file $script_dir/prompt.md \
    -p - 2>/dev/null)

if test $status -ne 0; or test -z "$distilled"
    echo "distillation produced nothing (claude exit $status)" >&2
    exit 0
end

# Re-ingest each `MEMO: <fact> | topics: a, b` line via the MCP door.
printf '%s\n' $distilled | python3 "$script_dir/mcp_remember.py"
echo "consolidated $today"
