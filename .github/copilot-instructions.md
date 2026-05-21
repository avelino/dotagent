# Copilot / AI Code Review Instructions

> Read this **before** generating any code, completion, or pull-request
> review on this repository. These are not style preferences — they are
> hard constraints derived from the project's architecture
> (`CLAUDE.md`), the published manifest schema
> (`docs/reference/agent-spec.md`), the public plugin protocol
> (`docs/reference/plugin-protocol.md`), and the threat model
> (`docs/security/threat-model.md`).
>
> When acting as a reviewer, **flag every violation explicitly**, with
> file and line, and **block** the merge until it is resolved or
> consciously waived by a human maintainer. Silent acceptance is the
> failure mode this file exists to prevent.

## 1. What this project is (and is not)

dotagent is a **polyglot agent orchestrator**: a single daemon that
schedules, supervises, and notifies on user-written agents (Fish,
Python, Go, Rust, anything). It is not an AI runtime, not an SDK, not
an MCP proxy. The `mcp` CLI is a separate project; do not pull it in,
embed it, or replace it.

This framing matters: any patch that turns dotagent into "a thing that
also does X" should be rejected unless X is genuinely scheduling,
supervision, notification, or audit.

## 2. Crate ownership

Each kind of change has exactly one home. Cross-crate boundary
violations need to be relocated before merge. The mapping (canonical
version in `CLAUDE.md`):

| Change                                  | Goes where                                            |
| --------------------------------------- | ----------------------------------------------------- |
| Shared type                             | `crates/dotagent-core/`                               |
| Time / scheduling math                  | `crates/dotagent-scheduler/` — **pure, no IO**        |
| Filesystem state                        | `crates/dotagent-state/`                              |
| Subprocess lifecycle                    | `crates/dotagent-runner/`                             |
| launchd / systemd unit emission         | `crates/dotagent-unit-gen/`                           |
| Logs / OTel / retention                 | `crates/dotagent-telemetry/`                          |
| CLI subcommand                          | `crates/dotagent/src/commands/`                       |
| Built-in notifier driver                | `crates/dotagent-notify/src/drivers/`                 |
| Preflight / sink plugin                 | new crate in `plugins/`                               |
| Third-party notifier (escape hatch)     | new crate in `plugins/` (`kind = "notify"`)           |

**Flag and request changes** whenever you see:

- Any `use std::fs`, `std::env`, `chrono::Local::now()`,
  `tokio::fs`, `tokio::time::sleep` (anything reading the wall clock or
  the filesystem) appearing inside `crates/dotagent-scheduler/`.
  Scheduler functions must take `now: DateTime<Local>` as a parameter.
  This is what keeps the test suite fast and deterministic.
- A notifier driver added under `plugins/` instead of
  `dotagent-notify/src/drivers/`. Built-in notifiers are in-process for
  a reason (notify is the hot path on failure).
- Logic placed in `crates/dotagent/` that should belong in a library
  crate (the binary should be thin: parse args, wire up, dispatch).

## 3. Manifest, plugin protocol, heartbeat — public contracts

These three shapes are **public API**. They must not break without an
explicit major-version bump and a CHANGELOG entry.

### `agent.toml` schema (`crates/dotagent-core/src/manifest.rs`)

- **Additive changes only** by default. New optional fields are fine.
- Renaming or removing a field requires a deprecation note in
  `docs/reference/agent-spec.md` and a parser that still accepts the
  old name for at least one minor release.
- Every schema change must also touch `docs/reference/agent-spec.md`
  and `docs/llms.txt` in the same PR. PRs that change the struct
  without touching both docs **must be requested to fix before merge**.

### Plugin protocol (`crates/dotagent-plugin/src/lib.rs`)

- Three verbs: `info`, `validate`, `invoke`. JSON over stdio.
- Adding a new verb is a major-version bump of the
  `dotagent-plugin` crate.
- Changing the JSON shape of an existing verb is a breaking change.
  Request the version bump (and a CHANGELOG entry) before approving.
- The protocol is implemented by binaries in Go, Python, Bash, etc.
  outside this repo. Assume external implementations exist and **do
  not** break them.

### Heartbeat (`crates/dotagent-core/src/heartbeat.rs`)

- File format is **wire-compatible with the legacy Fish framework**
  (`lib/agent.fish` in the user's dotfiles repo) so the two can coexist
  during migration.
- New optional fields are fine. Renaming, removing, or changing the
  type of any existing field requires a documented migration path and a
  reader capable of consuming the old shape.

## 4. Defaults must work out of the box

A feature that requires the user to write a config file before it does
anything is a misdesigned feature.

- New `config.toml` fields must have a sensible default. The default
  path is documented in `docs/guides/config-reference.md`; only
  customisation lives there.
- `dotagent doctor` should never say "you must create config.toml
  before using X". If it does, the default is wrong or missing.
- New CLI commands should work against the example agents in
  `examples/` without environment setup beyond
  `DOTAGENT_ROOT=$PWD/examples`.

When reviewing, **flag** any new required config field (no default, no
fallback) and request a default unless the contributor can articulate
why one cannot exist (the canonical example: a notifier's `to`
address — there is no universal default).

## 5. User-facing output

The CLI talks to humans and to scripts. It must do both well.

- **Never `{:?}` (Debug formatting) in `println!`/`eprintln!` that
  reach the terminal.** Debug leaks Rust type names, escapes newlines
  as literals, and produces unreadable walls of text.
- All execution outcomes (success, failure, preflight abort, plugin
  error) render through
  `crates/dotagent/src/commands/output.rs::render_outcome`. New code
  paths must go through it.
- Respect TTY detection and `NO_COLOR`. Colour is not allowed when
  stdout is not a terminal, or when `NO_COLOR` is set.
- Support `--json` whenever a command's output might be consumed by a
  script. Human-readable text on stdout, structured JSON behind the
  flag, never both interleaved.
- Audit on every PR touching `crates/dotagent/src/commands/`:
  ```
  rg -n '\{:\?\}' crates/dotagent/src/commands
  ```
  Any hit needs justification.

## 6. Dependencies and Cargo hygiene

- **All workspace dependencies are pinned in the root `Cargo.toml`**
  under `[workspace.dependencies]`. Member crates use
  `<dep>.workspace = true`. If a member crate re-declares a version,
  ask the contributor to move it to the root.
- **No `default-features = true` on TLS-adjacent dependencies.** We
  ship `rustls`, not OpenSSL. The canonical example is `reqwest`:
  ```toml
  reqwest = { version = "0.13", default-features = false, features = ["rustls-tls", "json"] }
  ```
  A PR that flips `default-features = true` or adds `native-tls`
  pulls OpenSSL into the build — flag it and ask for the rustls
  feature flags instead.
- **Use `anyhow` in binaries** (`crates/dotagent/`, `plugins/*`).
  **Use `thiserror` in libraries** (`crates/dotagent-*` except the
  binary). Mixing the two within a single crate is a smell.
- Time and dates: **`chrono`**, not `time`. The codebase is consistent;
  do not split the dependency tree.
- Async runtime: **`tokio`** with `features = ["full"]` in binaries,
  minimal features in libraries.

## 7. Security review checklist

Apply this list to any patch touching parsing, spawning, plugin
invocation, env injection, secrets, audit, or trust boundaries.

- **No shell interpolation.** `Command::new("sh").arg("-c").arg(x)`
  with `x` coming from a manifest field is a hard stop — request the
  contributor switch to a tokenised `args` list.
- **`unsafe` blocks** must carry a `// SAFETY:` comment explaining the
  invariants and why the safe API was insufficient. The existing
  `kill(2)` call in the runner is the model.
- **Input validation tests** are required when the change adds or
  modifies a parser. Happy path is not enough. Required:
  - Malformed input rejected with a clear error.
  - Unicode / case sensitivity covered.
  - Boundary tests (empty, whitespace, oversize) covered.
  - At least as many deny tests as allow tests.
- **Secrets handling**:
  - A secret value must never appear in a log line, stdout, audit
    entry, or notifier payload unless the user has explicitly opted
    in.
  - `Debug` impls on structs that contain secrets must redact them.
  - Use the helpers in `crates/dotagent-secrets/` for any new path
    that touches credentials.
- **Audit log invariants**:
  - Every consequential action emits an event. New event types are
    additive and documented in `docs/reference/plugin-protocol.md` and
    `docs/concepts/plugins.md`.
  - The hash chain must not be broken by a refactor. If you touch the
    chain, add a test that asserts continuity across a synthetic
    sequence.
- **Path containment**: any new field that takes a path must be
  validated against the same rules as `command` and `args` — no
  relative paths, no `..` traversal, no expansion to outside
  `DOTAGENT_ROOT` / `~/.config/dotagent`. The recent commit `3e9f374`
  ("Tighten parser, reject relative paths, align docs") is the
  reference.

When in doubt, cross-check
[`docs/security/threat-model.md`](../docs/security/threat-model.md) and
the report-channel guidance in [`SECURITY.md`](../SECURITY.md).

## 8. Documentation drift

A user-visible change without a doc update is a half-finished change.
**Hold the merge** until the docs land in the same PR.

User-visible categories that always require doc updates:

| Change                                | Docs that move with it                                       |
| ------------------------------------- | ------------------------------------------------------------ |
| New / renamed env var                 | `docs/reference/env-vars.md` + every plugin doc that mentions it + `docs/reference/agent-spec.md` |
| New / renamed manifest field          | `docs/reference/agent-spec.md` + `docs/llms.txt` + relevant `docs/concepts/*` |
| New plugin in `plugins/`              | `docs/plugins/<name>.md` + `docs/plugins/README.md` + `Cargo.toml` workspace `members` + `Formula/dotagent.rb` |
| New CLI command                       | `docs/reference/cli.md` + `README.md` if user-visible        |
| New event type                        | `docs/reference/plugin-protocol.md` + `docs/concepts/plugins.md` event table |
| Change to `~/.config/dotagent/...`    | `docs/reference/paths.md` + every doc that quotes the path   |
| New default value                     | `docs/guides/config-reference.md` + relevant guide           |

Mermaid is the only diagram format we use. If a PR introduces ASCII
box-drawing (`┌─┐│└┘`, `+--+`, `──▶`) in any `.md`, ask the contributor
to convert it — GitHub renders Mermaid natively, ASCII degrades in
non-monospace contexts and is hostile to diff review.

## 9. Validation pipeline — must pass locally before review

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

CI runs the same three on macOS and Linux. A new clippy warning is a
build failure. Do not propose `#[allow(...)]` to suppress a warning
without an inline justification comment.

For end-to-end smoke after a build:

```bash
cargo build --release -p dotagent
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent doctor
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent run hello-fish \
  --schedule manual --dry-run
```

## 10. Testing conventions

- **Behavior, not implementation.** A test that breaks when an
  internal helper is renamed without behavior change is a bad test.
- **Bug fixes start with a failing regression test.** PR description
  must reference the test name.
- **Integration > excessive mocking** for glue code. The scheduler is
  the exception (pure, unit-tested); the runner and state crates
  prefer integration tests with `tempfile`.
- **Property tests** (`proptest`, `quickcheck`) welcome for parsers and
  scheduler math.
- **Async tests**: `#[tokio::test]`, not bespoke runtimes.

## 11. Trade-offs already decided — do not re-litigate

These were debated and decided. Reopening requires a new issue with new
justification — a PR that changes one of these without referencing the
prior decision should be flagged so the discussion happens in the issue
tracker, not in the diff.

- **One daemon per system**, not one launchd plist per agent.
- **Subprocess + JSON** for plugins, not WASM, not dylib.
- **Notifiers in-process** (`dotagent-notify`), with the plugin
  protocol kept as an escape hatch for third-party services.
- **TOML for manifests**, not YAML, not JSON.
- **Heartbeat shape clones the Fish framework** for migration.
- **`mcp` CLI stays separate** — do not vendor or embed.

When you spot one of these in a diff, point the contributor to this
section so the conversation can move to where the trade-off was
originally weighed.

## 12. AI-specific guardrails

When generating code or review comments:

- **Do not invent file paths, crates, env vars, or doc URLs.** If you
  are not sure a thing exists, search the repo or say so. Hallucinated
  references waste reviewer time and erode trust in AI suggestions.
- **Cite file and line** for every finding (`path/to/file.rs:42`) so a
  human can navigate to it.
- **Do not commit, push, tag, or create releases.** All git
  state-changing operations belong to the human maintainer. You may
  prepare diffs and draft messages.
- **No `Co-Authored-By: <AI tool>` trailers** in commit messages. The
  contributor is the responsible author.
- **Quote the rule you are enforcing.** "This violates `CLAUDE.md` §X"
  is better than "this looks wrong". Specificity is what makes a review
  actionable.
- **When in doubt, flag rather than approve.** This file exists because
  silent acceptance is the failure mode.
