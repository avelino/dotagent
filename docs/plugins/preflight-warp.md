# preflight-warp

> Gate an agent's run on Cloudflare WARP being connected. Returns
> `ok: false` (and optionally a `suggest` hint) when WARP is not in
> `Connected` state, which aborts the run and fires `on_failure` with
> `event = "preflight"`.

| Property        | Value                                            |
|-----------------|--------------------------------------------------|
| Kind            | `preflight`                                      |
| Platforms       | `darwin`, `linux`                                |
| Binary          | `dotagent-plugin-preflight-warp`                 |
| External deps   | `warp-cli` (Cloudflare's WARP client)             |

## What it does

Runs `warp-cli status`, parses stdout for the literal substring
`Connected`, and returns `ok: true` only on a match. On failure, the
response includes:

- `error` — short summary (`WARP not connected`)
- `suggest` — the manifest's configured `connect_command` (so a
  notification can show the user *how* to fix it)
- `status` — the raw `warp-cli status` stdout for context

dotagent aborts the run when the response is `ok: false`. The agent
script never starts.

## When to use

- Your agent depends on internal services routed through a corporate
  VPN (Cloudflare's WARP / Zero Trust).
- The first symptom of WARP being off is a long sequence of timeouts /
  401s from inside the agent — much louder to fail fast at preflight.
- You want the failure notification to *tell you* the exact command to
  reconnect, so you don't have to remember.

## Config schema

| Field             | Type    | Required | Default      | Description                                                       |
|-------------------|---------|----------|--------------|-------------------------------------------------------------------|
| `binary`          | string  | no       | `warp-cli`   | Override the binary name (e.g., `/usr/local/bin/warp-cli`)        |
| `connect_command` | string  | no       | —            | Hint reported in the response so a notification plugin can echo it |

Both fields are optional — the plugin works with zero config when
`warp-cli` is on `$PATH`.

Verify schema at runtime:

```bash
dotagent-plugin-preflight-warp info | jq .schema
```

## Examples

### Minimal — abort the run unless WARP is connected

```toml
[[preflight]]
plugin = "preflight-warp"
```

### With reconnect hint

```toml
[[preflight]]
plugin = "preflight-warp"
config = { connect_command = "warp-cli connect" }
```

When this fails and `on_failure` fires, the message includes
`preflight aborted by plugin preflight-warp: warp-cli connect` — so the
iMessage / Slack alert hands you the command to run.

### Combined with retry to wait for the user

```toml
[agent]
name = "team-standup"
timeout_seconds = 1200

[defaults]
max_retries = 20                  # tries every backoff window…
retry_backoff_minutes = [30]      # …every 30 minutes…
stale_after_minutes = 720         # …until the window is 12 hours old.

[[preflight]]
plugin = "preflight-warp"
config = { connect_command = "warp-cli connect" }

[[notifiers]]
driver = "imessage"
to     = "+5511..."
rate_limit_minutes = 60
events = ["preflight"]
```

This is the production pattern from the legacy `team-standup`:
the agent keeps retrying every 30 min while WARP is down, but the
iMessage alert is rate-limited to one per hour so you don't get spammed
during a long disconnection.

## Response shape

### WARP connected

```json
{
  "ok": true,
  "status": "Status update: Connected\nSuccess"
}
```

### WARP disconnected

```json
{
  "ok": false,
  "error": "WARP not connected",
  "suggest": "warp-cli connect",
  "status": "Status update: Disconnected\nSuccess"
}
```

### `warp-cli` not installed / not on PATH

```json
{
  "ok": false,
  "error": "could not run warp-cli",
  "suggest": "warp-cli connect"
}
```

## Behavior details

- **Match is substring `Connected`.** The plugin doesn't parse the full
  status structure — `warp-cli status` reliably emits `Status update:
  Connected` when the tunnel is up.
- **No retry inside the plugin.** Retrying is dotagent's job — the
  manifest controls how often the daemon re-fires the agent.
- **Exit code is always 0** (the plugin spoke successfully), but `ok:
  false` blocks the agent. dotagent's audit log differentiates them.

## External dependencies

- **Cloudflare WARP client** with `warp-cli`:
  - macOS: download from https://1.1.1.1
  - Linux: `apt install cloudflare-warp` or Cloudflare's RPM repo
- The user must have already run `warp-cli registration new` once.

## Manual testing

```bash
# 1) Info
dotagent-plugin-preflight-warp info | jq .

# 2) Validate (no config needed)
echo '{}' | dotagent-plugin-preflight-warp validate

# 3) Real invoke
echo '{
  "kind": "preflight",
  "agent": "test",
  "schedule": "test",
  "event": "preflight",
  "config": {"connect_command":"warp-cli connect"}
}' | dotagent-plugin-preflight-warp invoke

# 4) Confirm WARP state directly
warp-cli status
```

Force a `false` response by disconnecting:

```bash
warp-cli disconnect
echo '{"config":{"connect_command":"warp-cli connect"}}' \
  | dotagent-plugin-preflight-warp invoke
# → {"ok":false,"error":"WARP not connected","suggest":"warp-cli connect","status":"..."}
warp-cli connect
```

## Troubleshooting

### Plugin returns `ok: false` even when WARP is connected

`warp-cli status` output varies by version. Confirm what your version
prints:

```bash
warp-cli status | head -2
```

If it doesn't contain the literal word `Connected`, file an issue —
we'll widen the match.

### `could not run warp-cli`

Either:

- `warp-cli` isn't installed: install Cloudflare WARP.
- It's installed but not on the daemon's `$PATH`: pass the full path:

```toml
[[preflight]]
plugin = "preflight-warp"
config = { binary = "/usr/local/bin/warp-cli", connect_command = "warp-cli connect" }
```

### Agent runs even with WARP off

Confirm the `[[preflight]]` block is in the manifest:

```bash
dotagent doctor | grep preflight
```

If the plugin is unresolved (`plugin preflight-warp not found`), the
preflight is silently skipped. Install with:

```bash
brew install dotagent          # bundles all first-party plugins
```

## See also

- [Concept guide](../concepts/plugins.md)
- [`preflight-cmd`](preflight-cmd.md) — generic version (any command + exit check)
- Source: [`plugins/preflight-warp/`](../../plugins/preflight-warp/)
- Cloudflare WARP: https://1.1.1.1
