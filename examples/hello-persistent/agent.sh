#!/usr/bin/env bash
#
# hello-persistent — the smallest agent that stays alive.
#
# The whole protocol: read a JSON object per line on stdin, write a JSON object
# per line on stdout. stdout is the channel; anything you want to say to a
# human goes to stderr.
#
#   → {"v":1,"kind":"hello","agent":"...","key":"..."}
#   ← {"v":1,"kind":"ready","ok":true}
#   → {"v":1,"kind":"request","id":"1","trigger":{...}}
#   ← {"v":1,"kind":"response","id":"1","ok":true,"output":"..."}
#
# Bash and no jq on purpose — this is the floor, not a recommendation. See
# docs/reference/persistent-protocol.md.

set -euo pipefail

log() { printf '[hello-persistent] %s\n' "$*" >&2; }

# The counter is the point of the example: it survives between requests, which
# is the one thing a one-shot agent cannot do.
answered=0

# Nothing here parses JSON properly. A real agent should — but the value of
# showing it in pure bash is that nothing is hidden behind a dependency.
field() {
  local key="$1" line="$2"
  printf '%s' "$line" | sed -n "s/.*\"$key\":\"\\([^\"]*\\)\".*/\\1/p"
}

emit() { printf '%s\n' "$1"; }

log "starting up (pid $$, key=${AGENT_PERSIST_KEY:-none}, lifecycle=${AGENT_LIFECYCLE:-oneshot})"

while IFS= read -r line; do
  case "$line" in
  *'"kind":"hello"'*)
    # Say ready only once whatever you needed to warm up is warm. dotagent
    # waits for this before sending the first request, and gives up after
    # [lifecycle] startup_timeout_seconds.
    emit '{"v":1,"kind":"ready","ok":true}'
    log "handshake done"
    ;;
  *'"kind":"request"'*)
    id="$(field id "$line")"
    answered=$((answered + 1))
    # Echoing the id back is what lets dotagent drop a late answer instead of
    # handing it to the next question.
    emit "{\"v\":1,\"kind\":\"response\",\"id\":\"$id\",\"ok\":true,\"output\":\"pid $$ answered $answered request(s) for key ${AGENT_PERSIST_KEY:-none}\"}"
    log "answered request $id (total $answered)"
    ;;
  *)
    log "ignoring unknown frame: $line"
    ;;
  esac
done

# stdin closing means dotagent is done with this instance — usually because it
# is being recycled. Nothing to clean up here; a real agent would flush
# whatever it is holding.
log "stdin closed, exiting after $answered request(s)"
