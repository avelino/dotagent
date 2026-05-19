#!/usr/bin/env python3
"""hello-python — minimal example agent for dotagent."""

import os
import sys

vars_of_interest = [
    "AGENT_NAME",
    "AGENT_HOME",
    "AGENT_TMPDIR",
    "AGENT_DRY_RUN",
    "AGENT_SCHEDULE_ID",
    "AGENT_START_EPOCH",
    "AGENT_ARGV",
    "AGENT_HEARTBEAT_FILE",
]

print("=== hello from python agent ===")
for k in vars_of_interest:
    print(f"{k:<22} = {os.environ.get(k, '')}")
print(f"sys.argv               = {sys.argv}")
