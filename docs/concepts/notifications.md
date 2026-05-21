# Notifications

dotagent ships with **built-in notification drivers** baked into the daemon.
No plugin protocol, no subprocess fork, no extra binary on `$PATH`. The
most common path (notify on failure) is the cheapest.

> **What changed.** Earlier versions shipped five `dotagent-plugin-notify-*`
> binaries (desktop, imessage, slack, ntfy, pushover). Each notification
> forked a process and spoke JSON over stdio. That worked but cost ~5-20ms
> per fire and forced users to keep five extra binaries on `$PATH`.
> Notifications are now in-process. The plugin protocol stays alive for
> `sink` / `preflight` and third-party notifiers (`driver = "plugin"`).

## Shape

Declare notifiers as a top-level array on the manifest:

```toml
[[notifiers]]
driver = "desktop"
title  = "dotagent"
sound  = true
events = ["attempt_failed", "given_up"]

[[notifiers]]
driver = "slack"
webhook_url = "https://hooks.slack.com/services/..."
events = ["given_up", "recovered"]
```

`events` is optional. Empty (or absent) means "all events".

| Event           | When it fires                                            |
|-----------------|----------------------------------------------------------|
| `attempt_failed`| The agent exited non-zero (a retry may still happen)     |
| `timed_out`     | The agent exceeded `agent.timeout_seconds`               |
| `given_up`      | All retries exhausted — operator action expected          |
| `recovered`     | A previously-failing schedule passed                     |
| `success`       | Every successful run (use sparingly)                     |
| `preflight`     | A preflight plugin blocked the run                       |

## Drivers

| `driver`     | Transport                                              | Subprocess?              |
|--------------|--------------------------------------------------------|--------------------------|
| `desktop`    | `NSUserNotification` (macOS) / D-Bus (Linux)           | No (native FFI)          |
| `slack`      | HTTPS POST to Slack Incoming Webhooks                  | No (in-process reqwest)  |
| `ntfy`       | HTTPS POST to ntfy.sh (or self-hosted)                 | No (in-process reqwest)  |
| `pushover`   | HTTPS POST to api.pushover.net                         | No (in-process reqwest)  |
| `telegram`   | HTTPS POST to api.telegram.org (Bot API)               | No (in-process reqwest)  |
| `imessage`   | `osascript` Messages.app automation                    | **Yes** — Apple has no API |
| `plugin`     | Falls back to the plugin protocol (`kind = "notify"`)  | Yes (legacy escape hatch)|

### `desktop`

```toml
[[notifiers]]
driver = "desktop"
title    = "dotagent"      # default: agent name
subtitle = "free space low" # macOS only
sound    = true             # macOS only
urgency  = "critical"       # Linux only: low | normal | critical
icon     = "dialog-warning" # Linux only: icon name or absolute path
expire_ms = 5000            # Linux only: 0 = persistent
```

### `slack`

```toml
[[notifiers]]
driver = "slack"
webhook_url = "https://hooks.slack.com/services/..."
channel     = "#alerts"       # optional
username    = "dotagent"      # optional
icon_emoji  = ":robot_face:"  # optional
```

### `ntfy`

```toml
[[notifiers]]
driver  = "ntfy"
topic   = "dotagent-alerts"
base_url = "https://ntfy.sh"    # default; set to self-hosted URL if needed
token    = "tk_..."             # optional bearer auth
priority = 4                    # 1..5
title    = "disk-alert"         # default: agent name
tags     = ["warning", "skull"]
```

### `pushover`

```toml
[[notifiers]]
driver   = "pushover"
token    = "azGDORePK8gMaC0QOYAMyEEuzJnyUi"
user     = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG"
priority = 1
title    = "disk-alert"        # default: agent name
```

### `telegram`

```toml
[[notifiers]]
driver       = "telegram"
bot_token    = "${TELEGRAM_BOT_TOKEN}"   # env interpolation; failsafe if unset
chat_id      = "-1001234567890"          # or "@my_channel"
parse_mode   = "MarkdownV2"              # optional: MarkdownV2 | HTML | Markdown
disable_notification = false             # optional: silent send
```

- `bot_token` accepts `${VAR}` references — resolution happens at send
  time against the daemon-loaded secrets file
  (`~/.config/dotagent/secrets.env`), falling back to the process env.
  Values never reach `tracing` output, and `Debug` redacts the token
  explicitly. A literal token also works but committing it to the
  manifest is **not** recommended. See
  [secrets concept](./secrets.md) for the loader's posture
  (0600-enforced, never echoed, audit-logged by key count only).
- When `parse_mode = "MarkdownV2"`, dotagent auto-escapes the 18
  characters Telegram reserves (``_*[]()~`>#+-=|{}.!``). Pass an
  already-escaped body if you need formatting (asterisks, links, etc.) —
  in that case use raw HTML or build the body yourself before handing it
  to the notifier.
- One-way only: dotagent does not receive Telegram updates (no inline
  keyboards, no webhooks).

> **Network allow-list.** If you declare `[security] network = [...]`,
> include `"api.telegram.org"` so the (future) sandbox lets the bot
> reach the API. v0 is schema-only, so it's a no-op today, but the
> declaration documents intent.

### `imessage` (macOS only)

```toml
[[notifiers]]
driver = "imessage"
to     = "+5511999999999"        # phone or email iMessage handle
rate_limit_minutes = 60          # skip-if-recent; 0 disables
```

> Apple does not expose any public API to send iMessages. This driver
> spawns `osascript` per send — it is the **only** built-in driver that
> forks. Rate-limit state lives at
> `$DOTAGENT_HOME/state/notify/imessage/<slug>.json`.

### `plugin` (escape hatch)

For third-party notifiers (Discord, Teams, custom relays), use the legacy
plugin protocol:

```toml
[[notifiers]]
driver = "plugin"
name   = "notify-discord"
events = ["given_up"]
[notifiers.config]
webhook_url = "https://discord.com/api/webhooks/..."
```

The binary `dotagent-plugin-notify-discord` is resolved via
`$DOTAGENT_PLUGIN_PATH` and the standard discovery order (see
[`docs/reference/plugin-protocol.md`](../reference/plugin-protocol.md)).

## Tiered notifications pattern

Combine drivers + `events` filters to keep noisy channels cheap and pager
channels rare:

```toml
# Cheap: desktop banner on every failure
[[notifiers]]
driver = "desktop"
title  = "disk-alert"
events = ["attempt_failed", "given_up"]

# Loud: iMessage only when retries are exhausted
[[notifiers]]
driver = "imessage"
to     = "+5511999999999"
rate_limit_minutes = 60
events = ["given_up"]

# Audit: Slack thread when something recovered after pain
[[notifiers]]
driver = "slack"
webhook_url = "https://hooks.slack.com/..."
events = ["recovered"]
```

## Failure semantics

A notifier failing is **logged but does not fail the run** — the run already
happened. Each invocation lands in the audit log
(`$DOTAGENT_HOME/state/audit.jsonl`) as a `plugin_invoked` event with
`plugin = "notifier:<driver>"`.

If a notifier rate-limits or dedups, it returns `Skipped { reason }` which
is treated as a success outcome.

## Legacy `[[on_failure]]` / `[[on_success]]`

The legacy plugin-style hooks still work — they always meant "fire these
plugins on these events". They are now reserved for **sink-style** hooks
(persist output, publish to Roam, etc.). For notifications, prefer
`[[notifiers]]` — it's faster, has fewer moving parts, and ships with
the daemon.

Migration is a 1-to-1 rename:

```toml
# Before
[[on_failure]]
plugin = "notify-desktop"
config = { title = "x", sound = true }
events = ["given_up"]

# After
[[notifiers]]
driver = "desktop"
title  = "x"
sound  = true
events = ["given_up"]
```
