#!/usr/bin/env fish

# disk-alert — example agent for dotagent.
#
# Exits 0 when free space is healthy. Exits 1 when it drops below
# DISK_FREE_MIN_PCT (read from env, set by agent.toml).
#
# dotagent reads stderr's last 5 lines into the on_failure message, so we
# write the human summary there.

set -l mount $DISK_CHECK_MOUNT
set -l threshold $DISK_FREE_MIN_PCT

if test -z "$mount"
    set mount "/"
end
if test -z "$threshold"
    set threshold 20
end

# `df -P` gives a portable single-line output:
#   Filesystem  1024-blocks  Used  Available  Capacity  Mounted on
# We want column 4 (Available) and column 2 (1024-blocks).
set -l line (df -Pk $mount | tail -1)
set -l total (echo $line | awk '{print $2}')
set -l avail (echo $line | awk '{print $4}')

if test -z "$total" -o -z "$avail"
    echo "disk-alert: could not parse df output for $mount" >&2
    exit 2
end

# Avoid floating-point: integer percent = avail * 100 / total.
set -l pct_free (math "round($avail * 100 / $total)")

# Human-readable sizes for the message body.
set -l avail_gb (math --scale=1 "$avail / 1024 / 1024")
set -l total_gb (math --scale=1 "$total / 1024 / 1024")

echo "mount: $mount"
echo "free:  $avail_gb GB / $total_gb GB ($pct_free%)"
echo "threshold: $threshold%"

if test $pct_free -lt $threshold
    # This message ends up in the dotagent on_failure notification.
    echo "" >&2
    echo "🚨 disk-alert: $pct_free% free on $mount" >&2
    echo "    avail: $avail_gb GB / $total_gb GB" >&2
    echo "    threshold: $threshold%" >&2
    echo "    host: "(hostname) >&2
    exit 1
end

echo "ok"
exit 0
