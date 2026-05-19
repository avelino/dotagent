# Plugin Protocol

> **Note.** The five most common notifiers (`desktop`, `imessage`,
> `slack`, `ntfy`, `pushover`) are now **built into the daemon** — they
> do not use this protocol. See
> [`docs/concepts/notifications.md`](../concepts/notifications.md) for
> the `[[notifiers]]` shape. This protocol is the contract for
> **preflight checks**, **output sinks**, and **third-party notifiers**
> (Discord, Teams, custom relays) wired via `driver = "plugin"`.

Plugins extend dotagent without recompiling it. They handle preflight
checks, output sinks, and third-party notifiers. A plugin is **any binary**
named `dotagent-plugin-<name>` that speaks the protocol below — write it
in Rust, Go, Python, Bash, whatever.

## Discovery

dotagent searches for the binary in this order, returning the first match:

1. `$DOTAGENT_PLUGIN_PATH` (colon-separated list of directories)
2. `~/.config/dotagent/plugins/`
3. `/usr/local/lib/dotagent/plugins/`
4. `$PATH`

## CLI surface

A plugin must accept exactly one positional argument: the **verb**.

```
dotagent-plugin-<name> info
dotagent-plugin-<name> validate
dotagent-plugin-<name> invoke
```

For `validate` and `invoke`, the JSON payload arrives on **stdin**. The
plugin must write its JSON response to **stdout** and exit with `0` for
success, non-zero for failure. Human-readable logs go to **stderr**.

## Verbs

### `info`

No stdin. Print self-describing metadata.

```jsonc
{
  "name": "sink-roam",
  "version": "0.1.0",
  "kinds": ["sink"],              // "notify" | "preflight" | "sink"
  "platforms": ["darwin", "linux"], // optional; empty means cross-platform
  "schema": {                     // JSON Schema describing accepted `config`
    "type": "object",
    "required": ["page"],
    "properties": {
      "page":         { "type": "string" },
      "marker_regex": { "type": "string" }
    }
  }
}
```

### `validate`

Stdin: the same JSON object the manifest puts under `config`.

```jsonc
// stdin
{ "page": "today", "marker_regex": "#daily" }
```

Stdout: `{"ok": true}` or `{"ok": false, "error": "..."}`. dotagent calls
this when loading manifests so misconfigurations surface in `dotagent doctor`
rather than at firing time.

### `invoke`

Stdin: an `InvokePayload` (see schema below).

```jsonc
{
  "kind": "sink",                  // matches plugin kind
  "agent": "finops-weekly",
  "schedule": "weekly",
  "event": "success",              // "attempt_failed" | "given_up" | "recovered" | "success" | "preflight"
  "message": "captured stdout from the agent...",
  "config": { "page": "today", "marker_regex": "#finops" }
}
```

Stdout: minimum `{"ok": bool}`. Extra fields are forwarded into dotagent's
logs and `--verbose` output; they do not change orchestrator behavior.

## Kind-specific contracts

### Notify

- Receives `message` (string) and `event`.
- Returns `ok=true` after delivery, `ok=false` otherwise. Best-effort —
  dotagent does not retry notifications.

### Preflight

- Returns `ok=true` if the precondition holds, `ok=false` to abort the run.
- May include `suggest` in the response (string) — dotagent forwards it into
  any failure notification so the user knows what to fix.
- The agent will NOT be invoked if any preflight returns `ok=false`. dotagent
  emits an `attempt_failed` event with the suggestion attached.

### Sink

- Persists the agent's output somewhere (file, Roam, Notion, etc.).
- `message` carries the captured stdout.
- Sinks run after a successful run; failure of a sink raises
  `attempt_failed` but does not roll back the agent (the run already
  succeeded).

## Convention: keep plugins single-purpose

Cross-cutting concerns (auth refresh, retries with backoff for transient HTTP
errors, etc.) belong in the plugin, not in dotagent. dotagent treats the
plugin as a black box: "given this payload, did you succeed?".

## Testing a plugin

```bash
echo '{"page":"today","marker_regex":"#daily"}' | dotagent-plugin-sink-roam validate
echo '{"kind":"sink","agent":"x","schedule":"y","event":"success","message":"hi","config":{"page":"today","marker_regex":"#daily"}}' \
  | dotagent-plugin-sink-roam invoke
dotagent-plugin-sink-roam info | jq .
```

If your plugin passes the three commands above, it integrates with dotagent.
