# sink-file

> Persist an agent's stdout to a file. Two modes: overwrite (default) or
> append. Creates parent directories on demand.

| Property        | Value                                            |
|-----------------|--------------------------------------------------|
| Kind            | `sink`                                           |
| Platforms       | `darwin`, `linux`                                |
| Binary          | `dotagent-plugin-sink-file`                      |
| External deps   | none                                             |

## What it does

Writes `payload.message` to the configured `path`. With `mode = "overwrite"`
(default) the file is replaced. With `mode = "append"` the message is
appended to the end (no implicit newline beyond whatever's in the message).
Parent directories are created with `mkdir -p` semantics.

## When to use

- You want the agent's output **in a file you `grep`/`cat` later**, not
  pushed to a service.
- You're feeding another tool (Hugo, mkdocs, your local search) that
  watches a directory.
- You want a quick audit trail without standing up Roam / Notion / etc.

## Config schema

| Field    | Type    | Required | Default       | Description                                                  |
|----------|---------|----------|---------------|--------------------------------------------------------------|
| `path`   | string  | **yes**  | —             | Destination file path (absolute or relative to daemon CWD)   |
| `mode`   | string  | no       | `overwrite`   | `overwrite` (default) or `append`                            |

Verify schema at runtime:

```bash
dotagent-plugin-sink-file info | jq .schema
```

## Examples

### Daily snapshot (overwritten each run)

```toml
[[on_success]]
plugin = "sink-file"
config = { path = "/Users/avelino/reports/hn-today.md" }
```

After today's run, the file contains exactly today's output. No
accumulation.

### Append-only log (one entry per run)

```toml
[[on_success]]
plugin = "sink-file"
config = { path = "/Users/avelino/dotagent-history/standup.log", mode = "append" }
```

Each run grows the file. Useful for "did this agent ever generate the
phrase X?" type retrospective greps.

### Date-stamped filename via shell

`sink-file` doesn't template `path`. To get a per-day filename, have
the agent itself build the path and use a different sink. Simplest is
to use both `sink-file` AND let the agent write its own dated copy:

```fish
# In agent.fish — write the dated copy yourself.
set -l dated /Users/avelino/reports/(date +%Y-%m-%d)-summary.md
cp $AGENT_TMPDIR/output.txt $dated
```

And use `sink-file` for the "latest" alias:

```toml
[[on_success]]
plugin = "sink-file"
config = { path = "/Users/avelino/reports/latest-summary.md" }
```

## Response shape

### Success

```json
{ "ok": true, "written_to": "/Users/avelino/reports/hn-today.md" }
```

### Failed validation

```json
{ "ok": false, "error": "path is required" }
```

### Runtime failure

Exit non-zero. Common causes:

- Parent directory exists but isn't writable (permission denied)
- `path` collides with an existing directory of the same name
- Disk full

Stderr carries the raw `io::Error`.

## Behavior details

- **Newlines**: the plugin writes exactly `payload.message` bytes. If
  your message ends without a trailing newline, the file doesn't get
  one either. Tools that expect newline-terminated lines should append
  `\n` to the agent's stdout.
- **Atomic-ish**: `overwrite` mode uses `std::fs::write` which truncates
  + writes. There's a brief window where the file is empty if the daemon
  crashes mid-write. Use `mode = "append"` if you need stricter
  durability (each entry is an `O_APPEND` write).
- **Encoding**: bytes-through. If your message is UTF-8 (almost always
  is), the file is UTF-8. No BOM, no transformation.
- **Permissions**: written with the daemon's umask (`0644` by default).
  Change your shell's umask before launching the daemon if you need
  tighter perms.

## External dependencies

None. Pure stdlib I/O.

## Manual testing

```bash
# 1) Info
dotagent-plugin-sink-file info | jq .

# 2) Validate
echo '{"path":"/tmp/test.txt"}' | dotagent-plugin-sink-file validate

# 3) Overwrite
echo '{
  "kind": "sink",
  "agent": "test",
  "schedule": "test",
  "event": "success",
  "message": "hello\nworld\n",
  "config": {"path":"/tmp/dotagent-sink-test.txt"}
}' | dotagent-plugin-sink-file invoke

cat /tmp/dotagent-sink-test.txt
# hello
# world

# 4) Append
echo '{
  "kind": "sink",
  "agent": "test",
  "schedule": "test",
  "event": "success",
  "message": "second line\n",
  "config": {"path":"/tmp/dotagent-sink-test.txt","mode":"append"}
}' | dotagent-plugin-sink-file invoke

cat /tmp/dotagent-sink-test.txt
# hello
# world
# second line
```

## Troubleshooting

### File doesn't appear

- Check the daemon's audit log to confirm `sink-file` was invoked:

```bash
tail ~/.config/dotagent/audit.log \
  | jq 'select(.event.event_type == "plugin_invoked" and .event.plugin == "sink-file")'
```

- Was `payload.message` empty? `dotagent run <agent>` first; the
  message is the captured stdout.

### Permission denied

Two flavors:

1. **Parent dir not writable** — fix with `chmod` or pick a different
   path.
2. **macOS TCC** — if `path` is under `~/Documents`, `~/Downloads`,
   `~/Desktop`, the daemon needs Full Disk Access. Settings → Privacy
   → Full Disk Access → add the dotagent daemon binary.

### Append mode silently overwrote my file

It didn't — but it might have inherited the prior overwrite if you
changed `mode` after one overwrite-mode run. Read the file's actual
length before / after:

```bash
wc -c /tmp/dotagent-sink-test.txt
```

If it grew, append worked.

### Concurrent runs of the same agent corrupt the file

dotagent ensures only one run per `(agent, slug)` at a time, so this
shouldn't happen organically. If you're invoking the plugin manually
in parallel, use `mode = "append"` — `O_APPEND` is atomic on POSIX up
to PIPE_BUF (4KiB on Linux, 512B on macOS). For larger writes, you'll
need a different strategy (write to tmp + rename).

## See also

- [Concept guide](../concepts/plugins.md)
- [`sink-roam`](sink-roam.md) — for hierarchical / Roam destinations
- Source: [`plugins/sink-file/`](../../plugins/sink-file/)
