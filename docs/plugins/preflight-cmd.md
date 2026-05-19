# preflight-cmd

> Generic preflight: run any command, check its exit code (and optionally
> match a substring in stdout/stderr). If the check fails, the agent run
> is aborted.

| Property        | Value                                            |
|-----------------|--------------------------------------------------|
| Kind            | `preflight`                                      |
| Platforms       | `darwin`, `linux`                                |
| Binary          | `dotagent-plugin-preflight-cmd`                  |
| External deps   | whatever command you configure                   |

## What it does

Spawns the configured command, captures stdout + stderr + exit code, and
compares against the expected values. Returns `ok: true` when both:

1. Exit code equals `expect_exit` (default `0`)
2. If `expect_contains` is set, that substring appears in either stdout
   or stderr

## When to use

- You need a preflight gate but no specialized plugin exists yet.
- The check is a one-off command that doesn't justify a dedicated
  plugin (`gh auth status`, `aws sts get-caller-identity`, `ping -c1 ...`).
- You're prototyping — get the gate working, then upstream a specialized
  plugin later if it sticks.

## Config schema

| Field              | Type           | Required | Default | Description                                                          |
|--------------------|----------------|----------|---------|----------------------------------------------------------------------|
| `command`          | string         | **yes**  | —       | Binary to execute (resolved via `$PATH` unless absolute)             |
| `args`             | array<string>  | no       | `[]`    | Arguments to pass                                                    |
| `expect_exit`      | integer        | no       | `0`     | Expected exit code                                                   |
| `expect_contains`  | string         | no       | (none)  | Substring required in stdout OR stderr                               |

Verify schema at runtime:

```bash
dotagent-plugin-preflight-cmd info | jq .schema
```

## Examples

### gh auth check before an agent that uses the GitHub CLI

```toml
[[preflight]]
plugin = "preflight-cmd"
config = {
  command = "gh",
  args = ["auth", "status"],
  expect_exit = 0
}
```

### AWS credentials valid before a FinOps agent

```toml
[[preflight]]
plugin = "preflight-cmd"
config = {
  command = "aws",
  args = ["sts", "get-caller-identity"],
  expect_exit = 0
}
```

### Network reachability check

```toml
[[preflight]]
plugin = "preflight-cmd"
config = {
  command = "ping",
  args = ["-c", "1", "-W", "2", "api.github.com"],
  expect_exit = 0
}
```

### Combined exit + content check

Verify `gws` (Google Workspace CLI) is logged in by checking specific
text in the output:

```toml
[[preflight]]
plugin = "preflight-cmd"
config = {
  command = "gws",
  args = ["gmail", "users", "labels", "list", "--params", "{\"userId\":\"me\"}"],
  expect_exit = 0,
  expect_contains = "labels"
}
```

If `gws` is logged out, exit will be 0 but the output won't contain
"labels" — `expect_contains` catches the silent-fail case.

### Custom non-zero "success"

Some tools use non-zero for valid states (`grep` returns 1 on no-match):

```toml
[[preflight]]
plugin = "preflight-cmd"
config = {
  command = "grep",
  args = ["-q", "ServerAliveInterval", "~/.ssh/config"],
  expect_exit = 0      # 0 = match found = ssh keepalive configured = ok
}
```

## Response shape

### Passed

```json
{ "ok": true, "exit_code": 0 }
```

### Failed (wrong exit code)

```json
{ "ok": false, "error": "preflight command failed", "exit_code": 1 }
```

### Failed (substring not found)

```json
{ "ok": false, "error": "preflight command failed", "exit_code": 0 }
```

(Exit was OK but `expect_contains` didn't match. The `error` message is
generic — check the agent log for the `dotagent doctor` / daemon log to
see which condition failed.)

## Behavior details

- **No shell.** `command` is executed directly with `args` as argv. No
  pipes, no globs, no env-var expansion. If you need shell features,
  invoke `/bin/sh -c "..."` explicitly:
  ```toml
  config = { command = "/bin/sh", args = ["-c", "test -f /tmp/ready && grep -q ok /tmp/ready"] }
  ```
- **Substring match is case-sensitive.** Use the exact string you
  expect.
- **Stdout AND stderr are searched** for `expect_contains` — some tools
  log to stderr (e.g., `gh auth status` writes to stderr by design).
- **No timeout inside the plugin.** If your command hangs, the daemon's
  `agent.timeout_seconds` doesn't apply (that's the agent's timeout,
  not the preflight's). Keep preflight commands fast.

## External dependencies

Whatever your `command` is. The plugin itself has no runtime deps
beyond the OS.

## Manual testing

```bash
# 1) Info
dotagent-plugin-preflight-cmd info | jq .

# 2) Validate
echo '{"command":"gh","args":["auth","status"]}' \
  | dotagent-plugin-preflight-cmd validate

# 3) Real invoke
echo '{
  "kind": "preflight",
  "agent": "test",
  "schedule": "test",
  "event": "preflight",
  "config": {"command":"gh","args":["auth","status"],"expect_exit":0}
}' | dotagent-plugin-preflight-cmd invoke

# 4) Negative test — make it fail
echo '{
  "config": {"command":"false"}
}' | dotagent-plugin-preflight-cmd invoke
# → {"ok":false,"error":"preflight command failed","exit_code":1}
```

## Troubleshooting

### Plugin always returns `exit_code: -1`

The command couldn't be spawned (binary missing, permission denied). The
real error is on stderr:

```bash
echo '{"config":{"command":"nonexistent"}}' \
  | dotagent-plugin-preflight-cmd invoke 2>&1
```

### `expect_contains` not matching even though I see the text

- Case sensitivity: `Logged in` ≠ `logged in`.
- ANSI escape codes (some CLIs color their output): pipe through
  `--no-color` or `2>&1 | cat` to verify the raw bytes.
- The substring must be in stdout OR stderr **of the spawned process**
  — not in YOUR terminal after shell processing.

### I need conditional logic (run only on weekdays etc.)

Wrong tool. Conditional logic lives in the schedule (`weekdays`) or in
the agent script. Preflight is a binary pass/fail gate.

## See also

- [Concept guide](../concepts/plugins.md)
- [`preflight-warp`](preflight-warp.md) — specialized version for WARP
- Source: [`plugins/preflight-cmd/`](../../plugins/preflight-cmd/)
