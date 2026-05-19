# Migrating from the Fish agent-orchestrator

If you came from the Fish framework in [avelino/dotfiles](https://github.com/avelino/dotfiles)
(`lib/agent.fish`, `agents/agent-orchestrator/`), this guide maps every concept
you already use to its dotagent equivalent. The plan is **no big bang** — old
and new run side by side until you migrate the last agent.

## Mapping at a glance

| Fish framework concept              | dotagent equivalent                                  |
|-------------------------------------|------------------------------------------------------|
| `meta.json`                         | `agent.toml` (richer, with comments and TOML types)  |
| `lib/agent.fish` — `agent_init`     | env vars dotagent injects + tmpdir auto-cleanup      |
| `lib/agent.fish` — heartbeat        | dotagent writes the same shape, before+after the run |
| `lib/run-with-timeout.fish`         | built into `dotagent run` (`agent.timeout_seconds`)  |
| `agents/agent-orchestrator` (tick)  | `dotagent daemon` (adaptive, single plist) + `dotagent tick` (one-shot) |
| `agents/agent-orchestrator --status`| `dotagent status` (read-only dashboard)              |
| `agents/agent-orchestrator --daily-summary` | `dotagent daily-summary` + daemon's internal 22:45 fire |
| `cron.nix` launchd config           | `dotagent install` (single `run.avelino.dotagent` plist) |
| `agent_output_imessage`             | built-in notifier `[[notifiers]] driver = "imessage"` (with `rate_limit_minutes`) |
| `lib/roam.fish` (`roam_publish`)    | plugin `sink-roam` (full port: sanitize + parse + idempotent replace) |
| inline WARP check                   | plugin `preflight-warp`                              |
| `IMESSAGE_TO="+55..."` hardcoded    | per-plugin `config.to`, or profile defaults          |
| retry loop in `agent-orchestrator`  | `WindowState`-backed retry with `max_retries` + `retry_backoff_minutes` |
| (no equivalent)                     | `dotagent logs`, `dotagent inspect`, `dotagent reload`, `dotagent run-now` |

## Step-by-step

### 1. Install dotagent

```bash
git clone https://github.com/avelino/dotagent
cd dotagent
cargo install --path crates/dotagent
cargo install --path plugins/preflight-warp
cargo install --path plugins/sink-roam
# (whichever plugins your agents use — notifiers are built in)
```

> Notifiers (`desktop`, `imessage`, `slack`, `ntfy`, `pushover`) ship
> inside the `dotagent` binary. No extra `cargo install` needed.

### 2. Convert `meta.json` → `agent.toml`

> If you're managing your dotfiles with **Nix / home-manager** (as
> avelino/dotfiles does), the conversion is mechanical and there's a
> reference Nix module that does the work for every agent in one
> place — see
> [`modules/home-manager/dotagent/`](https://github.com/avelino/dotfiles/tree/main/modules/home-manager/dotagent)
> in avelino/dotfiles. Copy `default.nix` from there, adjust the
> `builtinAgents` list, set `dotagent.enable = true`, and rebuild.
> Skip the manual conversion below if so.

For the `team-standup` agent (example), the Fish manifest…

```json
{
  "name": "team-standup",
  "monitor": true,
  "timeout_seconds": 1200,
  "schedules": [
    {
      "id": "daily",
      "weekdays": [1, 2, 3, 4, 5],
      "hours": [8],
      "minute": 30,
      "args": ["--period", "dia-anterior"]
    }
  ],
  "max_retries": 20,
  "retry_backoff_minutes": [30]
}
```

…becomes:

```toml
[agent]
name = "team-standup"
description = "DORA metrics daily standup"
monitor = true
timeout_seconds = 1200

[run]
command = "fish"
args = ["./agent.fish"]    # the original entrypoint, unchanged

[defaults]
max_retries = 20
retry_backoff_minutes = [30]

[[schedules]]
id = "daily"
type = "cron"
weekdays = [1, 2, 3, 4, 5]
hours = [8]
minute = 30
args = ["--period", "dia-anterior"]

[[preflight]]
plugin = "preflight-warp"
config = { connect_command = "warp-cli connect" }

[[notifiers]]
driver = "imessage"
to     = "+5511999999999"
rate_limit_minutes = 60
events = ["given_up"]
```

### 3. Trim the Fish entrypoint

You can drop everything dotagent now owns. The reduced agent.fish:

- Keeps your data collection + Claude invocation.
- **Removes** `agent_init` (env vars are already injected).
- **Removes** `agent_heartbeat_start` / `_agent_heartbeat_finish` (dotagent
  writes the same heartbeat file shape).
- **Removes** any inline WARP check (now done by `preflight-warp`).
- **Removes** `agent_output_imessage` calls (now done by the built-in
  `imessage` notifier — declared under `[[notifiers]]`).
- Continues to use `roam_publish` / `lib/roam.fish` while `sink-roam` is in
  progress. Once `sink-roam` is feature-complete you can drop the Roam
  helpers too.

A thin shim (`lib/agent-shim.fish`) that reads the env vars and re-exports
the legacy names (`$AGENT_TMPDIR`, `$AGENT_HEARTBEAT_FILE`, ...) lets you
migrate without touching the agent body. Source it instead of the original
`lib/agent.fish`.

### 4. Install the dotagent daemon

dotagent uses **one** plist for the entire system (the daemon manages
every agent internally — no per-agent plist):

```bash
dotagent install
# wrote ~/Library/LaunchAgents/run.avelino.dotagent.plist
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/run.avelino.dotagent.plist
```

Now remove each migrated agent's entry from
`modules/home-manager/cron.nix` and run `darwin-rebuild switch`. As
soon as the entry is gone from `cron.nix`, the dotagent daemon picks
up scheduling for that agent.

### 5. Verify the monitor

```bash
dotagent status                  # health dashboard for every discovered agent
dotagent tick --dry-run          # show what the daemon would dispatch right now
tail -F ~/.config/dotagent/logs/run.avelino.dotagent.log
```

The Fish `agent-orchestrator` can be left running during the migration —
both read/write the same heartbeat shape, so they don't fight (note the
new path: `~/.config/dotagent/state/agents/...` for dotagent vs.
`~/.local/state/agents/...` for the Fish framework). When you're
confident, remove its launchd entry and delete `agents/agent-orchestrator/`.

### 6. (Optional) move the agent out of dotfiles

Since each agent is now self-contained (`agent.toml` + script + assets), you
can lift it into its own repo or share it without dragging the rest of your
dotfiles along.

## Things that intentionally differ

- **No iMessage hardcoding.** The phone number lives in the manifest's
  `[[notifiers]] driver = "imessage"` block (or in a profile default).
  dotagent has no opinion about your notification target — pick the
  driver you want.
- **No `agent_mcp` wrappers.** dotagent isn't an MCP proxy; the `mcp` CLI
  you already use stays where it is. Agents continue to call `mcp` directly.
- **`tick` is read-mostly.** dotagent's `tick` doesn't depend on `claude`,
  `osascript`, or `jq` — it only does scheduling math. Notifications go
  through the daemon's built-in drivers (`[[notifiers]]`) or, for
  third-party services, through plugins.
