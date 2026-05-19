#!/usr/bin/env fish

# hello-fish — minimal example agent for dotagent.
#
# dotagent injects the env vars; the agent just reports what it received.

echo "=== hello from fish agent ==="
echo "AGENT_NAME        = $AGENT_NAME"
echo "AGENT_HOME        = $AGENT_HOME"
echo "AGENT_TMPDIR      = $AGENT_TMPDIR"
echo "AGENT_DRY_RUN     = $AGENT_DRY_RUN"
echo "AGENT_SCHEDULE_ID = $AGENT_SCHEDULE_ID"
echo "AGENT_START_EPOCH = $AGENT_START_EPOCH"
echo "AGENT_ARGV        = $AGENT_ARGV"
echo "AGENT_HEARTBEAT_FILE = $AGENT_HEARTBEAT_FILE"
echo "Args received     = $argv"

# Optional: write something into the tmpdir so callers can confirm cleanup.
echo "ran at $(date)" >$AGENT_TMPDIR/hello.txt
