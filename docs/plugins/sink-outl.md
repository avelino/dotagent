# sink-outl

> Publish an agent's stdout to [Outl](https://github.com/avelino/outl) as
> a hierarchical block tree (root + L1 sections + L2 children).
> Idempotent: re-running a schedule replaces the previous block via a
> single batched call instead of duplicating.

| Property        | Value                                            |
|-----------------|--------------------------------------------------|
| Kind            | `sink`                                           |
| Platforms       | `darwin`, `linux`                                |
| Binary          | `dotagent-plugin-sink-outl`                      |
| External deps   | `mcp` CLI with `outl` server configured          |
| Network         | Whatever the `mcp outl` server hits internally   |

## What it does

Twin of [`sink-roam`](sink-roam.md). Same config schema (`page`,
`marker_regex`, `mcp_binary`) so a single agent can ship the same
payload to both backends by declaring two `[[on_success]]` blocks.

The pipeline:

```mermaid
flowchart LR
    in[agent stdout] --> sanitize["sanitize<br/>(strip fences,<br/>normalize indent)"]
    sanitize --> parse["parse hierarchy<br/>(L1/L2)"]
    parse --> resolve["resolve target<br/>(daily / page / namespaced)"]
    resolve --> batch["outl_batch:<br/>N delete + 1 append_tree"]
```

Unlike `sink-roam` (which makes several round-trips to Roam's API),
Outl exposes `outl_batch` — a sequence of ops applied in one call.
The plugin composes one batch with:

1. `block_delete` for every existing block matching `marker_regex` (idempotency).
2. One `block_append_tree` writing the new root + sections in a single call.

Each MCP call is `mcp outl <tool> '<json>'`.

## When to use

- Your agent generates **hierarchical markdown** (not arbitrary prose)
  destined for Outl.
- You want re-runs to be idempotent — yesterday's block is replaced,
  not appended to — and atomic (delete + write happens server-side in
  one batch).
- You use Outl as the human-facing dashboard or want to mirror an agent
  output to Outl alongside Roam.

## Config schema

| Field          | Type    | Required | Default            | Description                                                                |
|----------------|---------|----------|--------------------|----------------------------------------------------------------------------|
| `page`         | string  | **yes**  | —                  | Target page reference. `today` / `April 22nd, 2026` / `2026-06-01` / `acme/tech/aws` |
| `marker_regex` | string  | no       | (matches nothing)  | Regex used to find and delete an existing block (idempotency)              |
| `mcp_binary`   | string  | no       | `~/.cargo/bin/mcp` | Override the `mcp` CLI path                                                |

Verify the schema at runtime:

```bash
dotagent-plugin-sink-outl info | jq .schema
```

### `page` reference forms

| Form                         | Resolves to                                                       |
|------------------------------|-------------------------------------------------------------------|
| `today`                      | Local-clock daily journal (ISO slug, e.g. `2026-06-01`)           |
| `yesterday` / `tomorrow`     | Daily journal offset by one day from local clock                  |
| `2026-06-01`                 | A specific daily journal (ISO `YYYY-MM-DD`)                       |
| `April 22nd, 2026`           | A specific daily journal (Roam-native ordinal — note `nd`)        |
| `acme/tech/infra/aws/X`      | A namespaced page (lowercased, auto-created via `outl_page_create`) |

> Unlike Roam, Outl's daily slug is **ISO** (`2026-06-01`). The plugin
> accepts the Roam-native ordinal form too and normalizes it to ISO
> before talking to Outl — so the same `agent.toml` works against both
> backends.

### Content hierarchy expected on stdin

The parser is shared with `sink-roam`:

```
#TAG First line is the ROOT (no indentation).
  L1 header — direct child of root (indent ≤ 3 spaces)
    L2 child of the previous L1 (indent > 3 spaces)
    Another L2 of the same L1
  Another L1 header
    L2 of that L1
```

Code fences (` ``` `) are stripped. Everything after a `---` separator
is cut. Leading `- ` per line is removed (Outl renders bullets itself).

## Examples

### Mirror an agent to both Roam and Outl

```toml
[agent]
name = "team-standup"

# Roam (legacy)
[[on_success]]
plugin = "sink-roam"
config = { page = "today", marker_regex = "#DORA.*dia-anterior" }

# Outl (new) — identical config shape, just swap the plugin name
[[on_success]]
plugin = "sink-outl"
config = { page = "today", marker_regex = "#DORA.*dia-anterior" }
```

The same stdout payload reaches both backends; idempotency holds on
each side independently.

### Namespaced page (created on first run)

```toml
[[on_success]]
plugin = "sink-outl"
config = {
  page = "acme/tech/infra/aws/finops-2026",
  marker_regex = "#weekly-report"
}
```

### Specific past daily

```toml
[[on_success]]
plugin = "sink-outl"
config = { page = "2026-05-27", marker_regex = "#postmortem" }
```

### Custom mcp binary path

```toml
[[on_success]]
plugin = "sink-outl"
config = {
  page = "today",
  marker_regex = "#summary",
  mcp_binary = "/opt/dev/mcp-staging"
}
```

## Response shape

### Success

```json
{ "ok": true, "root_id": "abc123XYZ" }
```

`root_id` is the id of the newly appended root block — useful for
chasing the result manually with `mcp outl outl_block_get '{"id":"abc123XYZ"}'`.

### Validation failed

```json
{ "ok": false, "error": "page is required" }
```

### Runtime failure

```json
{ "ok": false, "error": "<underlying error string>" }
```

Possible causes:

- `mcp` CLI not found (set `mcp_binary`)
- Outl MCP server misconfigured (`mcp outl outl_daily_today '{}'` fails)
- Stdin was empty / unparseable
- `outl_batch` returned an `error` envelope (the plugin propagates it
  verbatim along with `failed_at`)

## Behavior details

### Single round-trip via `outl_batch`

The whole publish is **one** `mcp outl outl_batch '{"ops":[...]}'` call.
The ops are emitted in order:

1. N × `block_delete` (one per match found by `marker_regex`).
2. 1 × `block_append_tree` (the new root + sections).

If any op fails server-side, the batch stops; the response carries
`error` + `failed_at` and the plugin returns `ok=false` with that
detail. Earlier `block_delete` ops that already ran are **not** rolled
back — Outl batches are best-effort sequential.

### Idempotency

When `marker_regex` matches existing children of the target page, the
plugin deletes them *before* writing the new tree. If no match, the
plugin writes a new root regardless — running an un-markered config
twice WILL produce two roots. **Always set `marker_regex` for production.**

The marker search uses `outl_search` with a cheap FTS probe (first
contiguous run of `#`/alphanumeric/`_`/`-` from the pattern), then
re-filters client-side with the full regex.

### Sanitization

Same convention as `sink-roam`:

- Strips opening/closing ` ``` ` code fences.
- Cuts at the first `---` line.
- Removes `- ` literals at the start of lines.
- Normalizes whitespace.

### Hierarchy threshold

Indent ≤ 3 spaces → L1. Indent > 3 spaces → L2. Orphan L2 lines (no
preceding L1) are silently dropped.

### Resolution order for `page`

1. `today` / `yesterday` / `tomorrow` → ISO slug from local clock.
2. ISO `YYYY-MM-DD` → daily target as-is.
3. Roam-native ordinal `Month <D><suffix>, YYYY` → converted to ISO.
4. Everything else → namespaced page (lowercased, `/` preserved).
   The plugin issues an `outl_page_create` first (idempotent in Outl).

## External dependencies

- **`mcp` CLI** (in Rust, separate project): https://github.com/avelino/mcp
- **Outl server configured** under the mcp client config
  (`~/.config/mcp/`). Test with:

```bash
mcp outl outl_daily_today '{}' | head
```

You should see JSON containing today's daily journal id.

## Manual testing

```bash
# 1) Info
dotagent-plugin-sink-outl info | jq .

# 2) Validate
echo '{"page":"today"}' | dotagent-plugin-sink-outl validate

# 3) Real invoke
echo '{
  "kind": "sink",
  "agent": "test",
  "schedule": "test",
  "event": "success",
  "message": "#TEST root from sink-outl\n  L1 header\n    L2 child",
  "config": {"page":"today","marker_regex":"#TEST"}
}' | dotagent-plugin-sink-outl invoke
```

Run the same invoke twice — the previous block is deleted and replaced
in the same batch, so you end with one block, not two.

## Troubleshooting

### `mcp outl outl_daily_today '{}'` fails

Fix the upstream config:

```bash
ls ~/.config/mcp/                # outl.json should be here
mcp outl outl_daily_today '{}' 2>&1 | head
```

### "no JSON in mcp output"

The mcp CLI is printing tracing INFO lines before the JSON envelope.
The plugin already filters from the first `{` or `[`, but extremely
verbose log levels can corrupt the output. Lower verbosity:

```bash
RUST_LOG=warn mcp outl outl_daily_today '{}'
```

### `outl_batch failed at op N`

A `block_delete` or `block_append_tree` op was rejected by the server.
`failed_at` is the 0-indexed position in the `ops` list. The first
`block_append_tree` is always last; numbers below it are deletes.
Common causes: stale block id (someone else deleted it between search
and batch), or the target page slug doesn't exist and the
`outl_page_create` step failed silently.

### Block keeps duplicating after re-runs

Your `marker_regex` doesn't match the root block's text. The plugin
uses Rust's `regex` crate (PCRE-ish syntax). Anchor with `^`/`$` if
needed, escape `.`/`+`/`*`.

Inspect what's there:

```bash
mcp outl outl_search '{"query":"#TAG","in":"blocks","limit":10}'
```

### Orphan L2 lines disappear

Every L2 line needs an L1 line above it.

### Content was cut at `---`

Intentional. Either remove the separator from your prompt's output, or
move the kept content above it.

## See also

- [Concept guide](../concepts/plugins.md)
- [`sink-roam`](sink-roam.md) — twin plugin for Roam Research
- [`sink-file`](sink-file.md) — flat output to a single file
- Source: [`plugins/sink-outl/`](../../plugins/sink-outl/)
- Upstream: [outl](https://github.com/avelino/outl), [mcp](https://github.com/avelino/mcp)
