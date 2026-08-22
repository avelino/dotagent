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
webhook_url = "${SLACK_WEBHOOK_URL}"
events = ["given_up", "recovered"]
```

`events` is optional. Empty (or absent) means "all events".

The same entry shape is reused at daemon level by
`[[daily_summary.notifiers]]` in `config.toml`, which decides where the
end-of-day health roll-up goes — see
[`[daily_summary]`](../guides/config-reference.md#daily_summary). The
one difference: `events` is ignored there, since that list is already
scoped to a single event.

> **Credentials go in `${VAR}`, not in the manifest.** An `agent.toml` lives
> in a versioned repo; a webhook URL written literally into one is a webhook
> URL in git history forever. Every credential-bearing field on every HTTP
> driver accepts `${VAR}`, resolved at send time from
> `~/.config/dotagent/secrets.env` — see [Secrets and credentials](#secrets-and-credentials)
> below and [`secrets.md`](./secrets.md).

| Event           | When it fires                                            |
|-----------------|----------------------------------------------------------|
| `attempt_failed`| The agent exited non-zero (a retry may still happen)     |
| `timed_out`     | The agent exceeded `agent.timeout_seconds`               |
| `given_up`      | All retries exhausted — operator action expected          |
| `stale`         | The schedule stopped running at all (see below)          |
| `recovered`     | A previously-failing schedule passed                     |
| `success`       | Every successful run (use sparingly)                     |
| `preflight`     | A preflight plugin blocked the run                       |

### `stale`, and why alerts repeat

Every other event fires from something that *happened*. An agent that quietly
stops being scheduled does nothing at all: no run, no failure, no event. Its
window ages past `stale_after_minutes`, the daemon stops even attempting it,
and the last thing you heard was a success weeks ago. Silence reads as fine.

`stale` fires from the *condition* instead of an event, on every daemon tick
while it holds. The same applies to `given_up`, which used to be said once and
never again — an agent broken for a week keeps asking.

Because a condition is true continuously, dotagent spaces re-notifications on a
rising ladder: **on entry, then after 1h, 6h, and once a day** for as long as it
lasts. The state survives daemon restarts (`state/notify/alerts.json`) and is
forgotten the moment the schedule succeeds again — so the next failure is loud
from its first second instead of inheriting yesterday's silence.

You do **not** have to add `"stale"` to an existing `events` list. A notifier
that asked for `given_up` asked to be told the agent is broken, and stale is the
same news only worse, so it is delivered on the `given_up` channel when nothing
lists `stale` explicitly. Listing it makes the routing explicit.

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
webhook_url = "${SLACK_WEBHOOK_URL}"  # the URL *is* the credential
channel     = "#alerts"       # optional
username    = "dotagent"      # optional
icon_emoji  = ":robot_face:"  # optional
```

### `ntfy`

```toml
[[notifiers]]
driver  = "ntfy"
topic   = "${NTFY_TOPIC}"       # on public ntfy.sh the topic name is the secret
base_url = "https://ntfy.sh"    # default; set to self-hosted URL if needed
token    = "${NTFY_TOKEN}"      # optional bearer auth
priority = 4                    # 1..5
title    = "disk-alert"         # default: agent name
tags     = ["warning", "skull"]
```

`title` and `tags` travel as the `X-Title` / `X-Tags` **HTTP headers**, and a
header value is bytes, not text. Raw UTF-8 there is `obs-text` — RFC 9110 says
a sender should not generate it, and ntfy's own docs warn it can arrive as `?`.
So anything that is not plain visible ASCII is emitted as an
[RFC 2047](https://datatracker.ietf.org/doc/html/rfc2047) `=?UTF-8?Q?…?=`
encoded word, which the ntfy server decodes before reading the header. You
write `title = "Falha na execução 🚨"`; it survives the wire intact. Control
characters are flattened to spaces first — a newline is not a header value at
all, and `HeaderValue` would reject the whole request over it.

### `pushover`

```toml
[[notifiers]]
driver   = "pushover"
token    = "${PUSHOVER_TOKEN}"
user     = "${PUSHOVER_USER}"
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
  `Debug` redacts the token explicitly. A literal token also works but
  committing it to the manifest is **not** recommended. See
  [secrets concept](./secrets.md) for the loader's posture
  (0600-enforced, never echoed, audit-logged by key count only).
- `chat_id` is **not** `${VAR}`-expanded. It is an address, not a
  credential — write it literally.
- When `parse_mode = "MarkdownV2"`, dotagent escapes the **19** characters
  Telegram reserves (``\_*[]()~`>#+-=|{}.!``) — the backslash included,
  since an unescaped one is itself an escape opener that desynchronizes
  everything after it. The body **always** goes through the escaper, so
  pre-escaping it produces doubled backslashes rather than formatting. If
  you want live markup, build the message with `parse_mode = "HTML"`
  instead, or leave `parse_mode` unset for plain text.
- Escaping runs **before** the length cut, because escaping grows the text:
  a body that fits under 4096 characters plain can exceed it once every `.`
  and `-` has a backslash in front. If the cut lands between a backslash and
  the character it escapes, the orphaned backslash is dropped — Telegram
  rejects the entire message over one dangling escape.
- When the Bot API refuses a send, the log line carries the API's own
  `description` (`"Bad Request: message is too long"`) rather than a bare
  status code, with the bot token scrubbed out of it first.
- Outbound only, by default. Receiving Telegram updates is a separate,
  explicitly-enabled ingress — see [`telegram.md`](./telegram.md).

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

## Message size limits

Every backend rejects the **whole request** when the body is over its limit,
so an alert that is too long is an alert that never arrives. dotagent trims
instead, turning "silently undelivered" into "delivered, minus the tail". The
cut is marked: `[truncated]` on bodies, `…` on titles.

| Driver     | Body limit         | Title limit       |
|------------|--------------------|-------------------|
| `slack`    | 40,000 characters  | —                 |
| `telegram` | 4,096 characters   | —                 |
| `pushover` | 1,024 characters   | 250 characters    |
| `ntfy`     | 4,096 **bytes**    | 250 **bytes**     |
| `desktop`  | (OS-dependent)     | (OS-dependent)    |
| `imessage` | (none applied)     | —                 |

Characters versus bytes is not a pedantic distinction here. ntfy counts
bytes, and pt-BR alert text is not ASCII: `ç` and `ã` cost 2 bytes each, an
emoji costs 4. A character-based trim can hand ntfy four times its limit
while believing it stayed under. The byte cutter also walks the cut back to
a UTF-8 character boundary, so a truncated message may land a few bytes below
the cap — under is correct, over is a rejected request.

ntfy's title cap applies to the **raw** text, before RFC 2047 encoding, because
encoding multiplies: a 4-byte emoji becomes 12 characters of `=XX`, and the cap
has to bite before the encoder runs rather than after.

## Empty bodies

The mirror image of the size limits: too *little* body is also a rejected
request. Telegram answers an empty `sendMessage` with `400 Bad Request:
message text is empty`, and the drivers that accept it deliver a blank line.

What dotagent does depends on why the body is empty, and the two cases pull
in opposite directions:

| Event | Empty body | Why |
|---|---|---|
| `success` | nothing is sent | The agent had nothing to report. A sweeper that finds no follow-ups is *working* — the run is already in `dotagent status` and in the agent's log. |
| everything else | body synthesized as `agent/schedule: event (no output)` | The state change **is** the news. Losing a `given_up` because the process died too fast to print anything is the worst outcome available. |

A skipped `success` is logged at debug level, not warn — it is the expected
outcome for an agent that reports by exception. The decision happens before
any driver runs, so there is no `plugin_invoked` audit entry either: nothing
was invoked.

This is a floor, not a substitute for a good message. An agent that always
prints something useful never reaches either branch.

## Secrets and credentials

Every credential-bearing field accepts `${VAR}`, resolved at send time against
`~/.config/dotagent/secrets.env` first and the process environment second:

| Driver     | `${VAR}`-expanded fields              | Left literal          |
|------------|---------------------------------------|-----------------------|
| `slack`    | `webhook_url`                         | —                     |
| `ntfy`     | `token`, `base_url`, `topic`          | —                     |
| `pushover` | `token`, `user`                       | —                     |
| `telegram` | `bot_token`                           | `chat_id`             |
| `imessage` | —                                     | `to`                  |

`chat_id` and `to` are addresses. Routing them through a secrets resolver
would turn a typo into "env var unset" instead of a message delivered to the
wrong place, which is the failure you actually want to see.

An unresolved reference **fails the send** rather than falling back to the
literal `"${SLACK_WEBHOOK_URL}"` — sending that string is a request
authenticated as a placeholder, which answers `404` and looks like an outage
rather than a typo. The error names the field and the variable and nothing
else.

Credentials also never reach `tracing`. `reqwest::Error`'s `Display` appends
` for url (…)`, and for Slack and Telegram that URL *is* the secret — a single
`?` on an HTTP call used to write a live webhook into
`~/.config/dotagent/logs/daemon/dotagent.log` from a failure as mundane as a
DNS blip. `NotifyError` has no `From<reqwest::Error>` conversion at all, so
that `?` no longer compiles: transport failures are reduced to a kind plus a
status (`slack transport error (timeout)`) at the call site, and the leak is
unrepresentable rather than merely unwritten. See
[`threat-model.md`](../security/threat-model.md).

> **Network allow-list.** If you declare `[security] network = [...]`,
> include `"api.telegram.org"` so the (future) sandbox lets the bot
> reach the API. v0 is schema-only, so it's a no-op today, but the
> declaration documents intent.

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
webhook_url = "${SLACK_WEBHOOK_URL}"
events = ["recovered"]
```

## Failure semantics

A notifier failing is **logged but does not fail the run** — the run already
happened. Each invocation lands in the audit log
(`$DOTAGENT_HOME/audit.log`) as a `plugin_invoked` event with
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
