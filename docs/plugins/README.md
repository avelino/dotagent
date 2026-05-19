# Built-in plugins

dotagent ships with first-party plugins, installed alongside the
`dotagent` binary by the Homebrew formula. Each has its own page below
with full config schema, behavior details, examples, and troubleshooting.

For the **concept** (what a plugin is, how to use one, how to write one),
see [`docs/plugins.md`](../concepts/plugins.md). For the **protocol spec** (verbs,
JSON shape, exit codes), see [`docs/reference/plugin-protocol.md`](../reference/plugin-protocol.md).

> **Note on notifications.** Notifications **used to be plugins** (`notify-imessage`,
> `notify-desktop`, etc). They are now **built into the daemon** under the
> `[[notifiers]]` array — no plugin subprocess, native APIs (NSUserNotification
> on macOS, D-Bus on Linux, HTTPS for Slack/ntfy/Pushover). See
> [`docs/concepts/notifications.md`](../concepts/notifications.md) for the new shape.

## Preflight plugins (`kind = "preflight"`)

Run BEFORE the agent. If any returns `ok=false`, the agent is aborted and
`on_failure` fires with `event = "preflight"`.

| Plugin                                | Platforms       | When to pick                                                       |
|---------------------------------------|-----------------|--------------------------------------------------------------------|
| [`preflight-warp`](preflight-warp.md) | macOS + Linux   | Agent depends on a corporate VPN routed via Cloudflare WARP.       |
| [`preflight-cmd`](preflight-cmd.md)   | macOS + Linux   | Generic: run any shell command, gate on exit code + stdout match.  |

## Sink plugins (`kind = "sink"`)

Run AFTER a successful agent run. Persist the captured stdout somewhere.

| Plugin                          | Platforms       | When to pick                                                            |
|---------------------------------|-----------------|-------------------------------------------------------------------------|
| [`sink-roam`](sink-roam.md)     | macOS + Linux   | Output is hierarchical markdown destined for Roam Research.             |
| [`sink-file`](sink-file.md)     | macOS + Linux   | Output is a single file (overwrite or append).                          |

## Discovery

dotagent finds these binaries via `$PATH` (Homebrew installs them in
`/opt/homebrew/bin/` on Apple silicon / `/usr/local/bin/` on Intel / Linux).
Custom plugins go in `~/.config/dotagent/plugins/` or anywhere listed in
`$DOTAGENT_PLUGIN_PATH`.

Verify what's available:

```bash
dotagent plugin list
dotagent doctor                                 # validates every reference in your manifests
dotagent-plugin-<name> info | jq .              # raw metadata for any plugin
```
