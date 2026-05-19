---
name: dotagent:doc-review
description: Audit dotagent docs for drift after ANY code/behavior change — new plugin, renamed field, moved file, changed default, new env var, new command, new event type, schema bump, path change. Always invoke before reporting a task as "done" if the change is user-visible. Returns a punch list of doc files that need updating with the specific lines/sections that are now stale, plus any links pointing to moved/renamed files. Never silently passes — when in doubt, flag.
---

# Skill: doc-review

Docs in this repo are **contract**, not afterthought. Every user-visible
change has a doc consequence somewhere. This skill is the gate that
catches drift before it lands.

## When you MUST invoke this

Before reporting "done" on any change that touches:

- **Manifest schema** (`agent.toml` fields, defaults, validation) →
  `docs/reference/agent-spec.md` + every `docs/plugins/<name>.md` that
  uses a field that changed.
- **Plugin protocol** (verbs, JSON shape, exit codes, env vars passed)
  → `docs/reference/plugin-protocol.md` + `docs/concepts/plugins.md`
  ("Creating a plugin" section).
- **A new built-in plugin** (or rename / removal) → new file under
  `docs/plugins/<name>.md`, plus index updates in:
  - `docs/plugins/README.md` (table per kind)
  - `docs/concepts/plugins.md` ("Built-in plugins" table)
  - `Formula/dotagent.rb` (binary list in the header comment)
  - `Cargo.toml` workspace members
  - `.github/workflows/release.yml` if it enumerates plugins.
- **A new CLI subcommand** (or renamed / removed) → `README.md`
  quickstart if relevant, `docs/concepts/agents.md` if it's how users
  install/interact, `.claude/skills/new-command/SKILL.md` if the pattern
  for adding commands changed.
- **A new env var dotagent injects** → `docs/reference/agent-spec.md`
  ("Environment variables dotagent injects" table) +
  `docs/concepts/agents.md` ("Extending agents").
- **A new event type** (`given_up`, `recovered`, etc.) →
  `docs/concepts/plugins.md` ("The events filter" table) +
  `docs/reference/plugin-protocol.md`.
- **Filesystem paths** (where state, logs, audit, config live) →
  `docs/reference/agent-spec.md` + `docs/security/threat-model.md` +
  `docs/concepts/notifications.md` (imessage rate-limit state path),
  every plugin doc that mentions the path (`sink-roam` references `mcp`
  config dir, etc.).
- **Security posture** (new event, new threat vector, new defense) →
  `docs/security/threat-model.md`.
- **A new convention or pattern** (e.g., "always use Mermaid, never
  ASCII") → `CLAUDE.md` (root).

## Audit procedure

Run these checks in order. Stop at the first failure and report — don't
batch.

### 1. Stale path references

```bash
# After any path change, these MUST return zero hits.
grep -rn "\.local/state/dotagent\|\.local/share/dotagent" docs/ CLAUDE.md README.md
grep -rn "dev\.dotagent\." docs/ CLAUDE.md README.md   # old per-agent plist label
```

### 2. Stale count / list references

```bash
# Plugin count drifts every time a plugin is added.
grep -rn "eight\|seven\|nine\|ten" docs/concepts/plugins.md docs/plugins/README.md
# Compare against:
ls plugins/ | grep -v target | wc -l
```

### 3. New plugin not yet documented

```bash
# Each plugin directory MUST have a matching doc.
for p in plugins/*/; do
  name=$(basename "$p")
  test -f "docs/plugins/${name}.md" \
    || echo "MISSING: docs/plugins/${name}.md"
done

# And it must appear in the index tables:
for p in plugins/*/; do
  name=$(basename "$p")
  grep -q "$name" docs/plugins/README.md \
    || echo "MISSING from docs/plugins/README.md: $name"
  grep -q "$name" docs/concepts/plugins.md \
    || echo "MISSING from docs/concepts/plugins.md: $name"
done
```

### 4. Broken markdown links

```bash
# After any file move, links break. Find references to moved files.
grep -rn "\](.*\.md)" docs/ CLAUDE.md README.md \
  | while IFS=: read -r src line content; do
      # Extract the path between ]( and ) — naive but catches >95% of cases.
      target=$(echo "$content" | sed -n 's|.*\](\([^)]*\.md\)).*|\1|p')
      [ -z "$target" ] && continue
      # Resolve relative.
      dir=$(dirname "$src")
      case "$target" in
        /*) full="$target" ;;
        *)  full="$dir/$target" ;;
      esac
      # Strip ../ etc.
      if ! test -f "$(realpath -m "$full" 2>/dev/null)"; then
        echo "BROKEN: $src:$line → $target"
      fi
    done
```

### 5. Schema / behavior consistency

For each docs/plugins/<name>.md compare its `Config schema` table
against the plugin's `info | jq .schema` output. They should agree.

```bash
for p in plugins/*/; do
  name=$(basename "$p")
  bin="./target/release/dotagent-plugin-${name}"
  test -x "$bin" || continue
  # Compare required fields:
  in_doc=$(grep -A20 "^## Config schema" "docs/plugins/${name}.md" \
           | grep -E "\*\*yes\*\*" | awk -F'|' '{print $2}' | tr -d '` ')
  in_info=$("$bin" info 2>/dev/null | jq -r '.schema.required[]?')
  diff <(echo "$in_doc" | sort) <(echo "$in_info" | sort) \
    || echo "SCHEMA DRIFT: docs/plugins/${name}.md vs $bin info"
done
```

### 6. CLAUDE.md repo structure section

Whenever a new top-level directory (or `crates/<x>`, `plugins/<x>`)
appears, `CLAUDE.md`'s "Arquitetura do repo" diagram must show it.

```bash
# Crates listed in CLAUDE.md vs actual.
grep -oE "dotagent-[a-z-]+" CLAUDE.md | sort -u > /tmp/claude-crates
ls -d crates/dotagent-*/ | sed 's|crates/||; s|/||' | sort -u > /tmp/actual-crates
diff /tmp/claude-crates /tmp/actual-crates || echo "Update CLAUDE.md repo layout"
```

## What to report

A punch list, file-by-file, with the specific change needed. Example:

```
✗ docs/plugins/README.md:3
  "ships with N first-party plugins" — bump count when adding sink/preflight

✗ docs/concepts/notifications.md:42
  driver table missing a newly-added built-in driver

✗ CLAUDE.md:54
  crate tree missing new `dotagent-<area>` crate

✗ docs/plugins/sink-file.md:59
  example path still uses old `~/.local/share/dotagent-history` —
  centralization to `~/.config/dotagent/` not propagated to this example
```

Then fix them. Don't ask permission for trivially mechanical edits
(bumping counts, adding rows to existing tables, updating paths).

## Anti-patterns

- ❌ Reporting "done" on a code change while doc grep finds drift.
- ❌ Skipping doc-review because "it's a small change". Small code
  changes routinely break large doc surfaces (a renamed env var
  invalidates every doc that mentions it).
- ❌ Updating only the index but not the per-plugin page (or vice
  versa). Always check both.
- ❌ Updating the English doc but not CLAUDE.md (which is pt-BR for
  Avelino + LLM agents). Both are canonical.

## When to skip

- Pure refactor inside a crate that doesn't change ANY public API,
  manifest field, CLI flag, env var, file path, or behavior visible
  to a user or plugin author.
- Code-comment-only changes.
- Test additions that don't change covered behavior.

If in doubt, run the audit. Cost is seconds. The cost of stale docs is
hours, days later.
