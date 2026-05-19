// hello-go — minimal Go example agent for dotagent.
//
// Build: go build -o agent ./...
package main

import (
	"fmt"
	"os"
)

func main() {
	vars := []string{
		"AGENT_NAME",
		"AGENT_HOME",
		"AGENT_TMPDIR",
		"AGENT_DRY_RUN",
		"AGENT_SCHEDULE_ID",
		"AGENT_START_EPOCH",
		"AGENT_ARGV",
		"AGENT_HEARTBEAT_FILE",
	}
	fmt.Println("=== hello from go agent ===")
	for _, k := range vars {
		fmt.Printf("%-22s = %s\n", k, os.Getenv(k))
	}
	fmt.Printf("os.Args                = %v\n", os.Args)
}
