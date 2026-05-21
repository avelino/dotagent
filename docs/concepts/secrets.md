# Secrets

dotagent reads a single secrets file at daemon startup so notifier
configs (and, eventually, agent env injection) can reference
`${VAR}` without the operator having to wire env vars into the
launchd plist or systemd unit.

> If you're here because a notifier config says
> `bot_token = "${TELEGRAM_BOT_TOKEN}"` and you're wondering where that
> variable should live — this is the page.

## Where the file lives

```
~/.config/dotagent/secrets.env       # default
```

Override (in order of precedence):

1. `[secrets] file = "..."` in `~/.config/dotagent/config.toml`
2. `DOTAGENT_SECRETS_FILE` env var (absolute path)
3. The default above

The override is useful when the file is mounted from a secret manager
(Vault, AWS Secrets Manager, sops-nix, kubernetes secret volume) into
somewhere like `/run/secrets/dotagent.env`.

## File format

```env
# ~/.config/dotagent/secrets.env
# Comments start with `#`. Blank lines are fine.

TELEGRAM_BOT_TOKEN=1234:abcdef...
SLACK_WEBHOOK_URL="https://hooks.slack.com/services/..."
PUSHOVER_TOKEN='literal token with $no expansion'
export DIRENV_STYLE=ok

# 1Password CLI reference — resolved at daemon startup
TELEGRAM_BOT_TOKEN_2=op://Personal/dotagent/telegram-token
```

Roughly dotenv-compatible:

- One `KEY=VALUE` per line. The first `=` is the separator; subsequent
  `=` characters are part of the value.
- Lines may start with `export ` — the prefix is stripped (handy for
  copy-paste from a `.envrc`).
- **Double-quoted** values strip the quotes and process the escapes
  `\n`, `\r`, `\t`, `\\`, `\"`. Anything else after `\` is an error,
  on purpose, so typos don't pass silently.
- **Single-quoted** values strip the quotes and are otherwise literal
  (no escapes, no `${VAR}`) — useful for tokens with backslashes or
  dollar signs.
- **Unquoted** values: leading/trailing whitespace trimmed, interior
  preserved. No shell expansion: `$HOME` stays the literal string.
- Keys must match `[A-Za-z_][A-Za-z0-9_]*`.

### Secret references (1Password)

A value matching `op://vault/item[/section]/field` is recognised as a
**1Password CLI reference**. At daemon startup the loader shells out
to `op read --no-newline <ref>` and stores the plaintext result in
memory. The literal `op://…` string is **never** kept — if `op` fails,
the key is removed from the store so any notifier needing it fails
loud (`env var ${…} is unset`) instead of sending the placeholder
over the wire.

Requirements:

- `op` (the 1Password CLI) on `PATH`.
- An active 1Password session — desktop app biometric unlock or
  `op signin` before starting the daemon. The loader does not prompt.

```env
TELEGRAM_BOT_TOKEN=op://Personal/dotagent/telegram-token
SLACK_WEBHOOK_URL=op://Work/dotagent/slack-webhook-url
```

> **Why CLI, not the SDK?** Issue #34 explicitly framed remote secret
> managers as out-of-scope; the CLI sits in the same posture as
> populating the file from `vault read`, just with the lookup
> happening at startup instead of being a separate pre-step. The
> 1Password Service Account SDK could replace the fork later (issue
> not yet filed) but it requires a Business-plan service account
> token, which most laptop-dev users don't have.

### Other secret managers

For anything that isn't `op`, **populate the file before starting the
daemon**. The file is the single source of truth at startup time.

```bash
# AWS Secrets Manager
aws secretsmanager get-secret-value \
  --secret-id dotagent/prod \
  --query SecretString --output text \
  > ~/.config/dotagent/secrets.env
chmod 600 ~/.config/dotagent/secrets.env

# HashiCorp Vault
vault kv get -format=json secret/dotagent \
  | jq -r '.data.data | to_entries[] | "\(.key)=\(.value)"' \
  > ~/.config/dotagent/secrets.env
chmod 600 ~/.config/dotagent/secrets.env

# sops + age (decrypt at boot)
sops -d secrets.enc.env > ~/.config/dotagent/secrets.env
chmod 600 ~/.config/dotagent/secrets.env
```

Wire it into your launchd plist / systemd unit's
`ExecStartPre` and you get the same posture as Kubernetes secret
volumes: a normal file on disk, refreshed by an out-of-band tool.

## Permission posture

The file **must** be mode `0600` (read/write owner-only). Any group
or world bits and the daemon refuses to load it — same posture as
`ssh` refusing to use a world-readable private key.

```bash
chmod 600 ~/.config/dotagent/secrets.env
```

Refusal is non-fatal: the daemon still starts so scheduled runs keep
firing. But notifiers that depended on the rejected file will fail
loudly (`env var ${TELEGRAM_BOT_TOKEN} is unset`) — there is no
silent fall-through to "send the literal placeholder". Run
`dotagent doctor` to see the file's state in one place.

## How values get used

At send time, every notifier config string that contains `${VAR}`
walks through this resolver:

1. **Secrets store** — the in-memory copy of the file loaded at
   daemon startup.
2. **`std::env::var`** — fallback for operators who already wired
   the variable into the plist / systemd unit.

If both miss, the notifier fails with a clear error rather than
sending a request authenticated as the literal string `${…}`.

```mermaid
flowchart LR
    A["telegram bot_token = \"${TELEGRAM_BOT_TOKEN}\""] --> B{secrets store has key?}
    B -- yes --> C[use value from secrets.env]
    B -- no --> D{std::env::var has key?}
    D -- yes --> E[use value from process env]
    D -- no --> F[fail: env var unset]
```

Today only `telegram.bot_token` honors `${VAR}` interpolation.
`slack.webhook_url`, `pushover.token`/`user`, and `ntfy.token` will
follow in a future change — open an issue if you need one urgently.

## Reload

The file is read once at daemon startup. To pick up changes:

- `dotagent reload` (sends `SIGHUP` — preferred), or
- restart the unit (`launchctl kickstart` / `systemctl --user restart`).

There is no automatic file-watch reload. The reasoning is the same
as for `sshd`: secrets rotation is rare, surprises are expensive,
and a deliberate signal is a feature.

### What happens when reload fails

If the file becomes unreadable between startup and SIGHUP (insecure
mode, parse error, file deleted), the daemon **drops the previously
loaded store** rather than keeping it as a fallback. Rationale: an
operator who just ran `op signin && op item rotate <id>` would be
worse off if dotagent kept serving the old token. Once dropped:

- Notifier `${VAR}` lookups fall through to `std::env::var`.
- Anything that has no env fallback fails loud with
  `env var ${…} is unset` rather than silently using a revoked
  credential.
- The audit log records `secrets_refused` with reason ending in
  `; previous store dropped`, so the chain explains itself.

Restore the file (fix permissions, re-decrypt, etc) and SIGHUP again
to re-populate the store.

## What the audit log shows

dotagent's append-only `audit.log` records the load outcome — never
the values themselves.

| Event              | Severity | Payload                                                                        |
|--------------------|----------|--------------------------------------------------------------------------------|
| `secrets_loaded`   | Notice   | `path`, `key_count`, `unresolved_references`                                   |
| `secrets_refused`  | Critical | `path`, `reason` (e.g., "insecure permissions … mode 640; previous store dropped") |

### What never leaks vs. what may appear

- **Values never appear** anywhere — not in `audit.log`, not in
  `tracing` output, not in `Debug` impls (`SecretsStore`'s `Debug`
  redacts to `len` + `source`).
- **Key names may appear in operational `tracing` warnings** —
  duplicate-key warnings and notifier `env var ${KEY} is unset`
  errors both include the key name on purpose, because they're
  unactionable without it. Key names also appear in failed
  `op://...` warnings (along with the reference path), since the
  vault/item identifiers are operator-visible metadata, not the
  secret itself.
- **The audit log itself never includes key names** — only counts.

If you treat key names as sensitive, route the daemon's `tracing`
output to a sink you trust (the daemon's `logs/daemon/*.log` inherits
the umask of whoever started it — set `umask 077` in the launchd /
systemd unit's environment if you want owner-only).

## Agent subprocess isolation

The secrets file is read by the daemon **only**. Agent subprocesses
do not get the whole file dumped into their env — they only see
what their own manifest explicitly opts into.

> v0 ships with notifier-side resolution working. The opt-in
> mechanism for `agent.toml` (`[env].from_secrets = ["KEY1", ...]`)
> will land in a follow-up issue.

## Non-goals

- **GPG / age decryption.** Tracked separately.
- **Remote secret managers** (Vault, AWS Secrets Manager, Doppler).
  Out of scope. Tools like
  [`vault read -format=table`](https://developer.hashicorp.com/vault/docs/commands/read)
  or
  [`aws secretsmanager get-secret-value`](https://docs.aws.amazon.com/cli/latest/reference/secretsmanager/get-secret-value.html)
  can populate the file from upstream.
- **Automatic file-watch reload.** SIGHUP / restart only.

## Troubleshooting

| Symptom                                     | Fix                                                          |
|---------------------------------------------|--------------------------------------------------------------|
| `env var ${TELEGRAM_BOT_TOKEN} is unset`    | Add the key to `secrets.env`, then `dotagent reload`.        |
| `dotagent doctor` flags `insecure permissions` | `chmod 600 ~/.config/dotagent/secrets.env`.               |
| `dotagent doctor` says `(not present)`      | Create the file. Missing is OK — only an issue if a notifier needs `${VAR}`. |
| Edited file, change didn't take effect      | `dotagent reload`. The daemon caches the file in memory.     |

## See also

- [`docs/concepts/notifications.md`](./notifications.md) — notifier drivers and where `${VAR}` matters today.
- [`docs/reference/agent-spec.md`](../reference/agent-spec.md) — manifest schema.
- [`docs/security/threat-model.md`](../security/threat-model.md) — overall posture.
