# Contributing to dotagent

Thanks for considering a contribution. We're early — the project shipped
v0.1.x recently — and we'd rather figure things out together with
contributors than gatekeep up front. Read the relevant section for what
you want to do; you don't need to absorb the whole document to send a
useful change.

## I just want to fix a typo / improve a doc

Go for it. Open a PR directly — no issue first, no full validation
pipeline, no architecture reading required. Be sure the change builds
the docs (`mdbook` or just GitHub preview) and you're done.

## I want to send a code change

For code, please:

1. **Communicate first** — open an issue (or a discussion) describing
   the problem in your own words before writing the patch. The next
   section explains why this matters more than ever; the short version
   is that an aligned issue is what makes a PR mergeable in a single
   review round.
2. Fork → branch → commit → PR against `main`, referencing the issue.
3. Run the full validation pipeline locally before pushing
   (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`).
4. Update documentation in the same PR — drift is the leading cause of
   review back-and-forth.

The architectural context (why crates are split the way they are, which
patterns we've already decided against) lives in [`CLAUDE.md`](CLAUDE.md).
You don't need to read it cover-to-cover for a small change, but if you
get review feedback like "this should live in crate X", that's the file
that explains why.

## Before the code: communicate

A bit of philosophy before the mechanics — it shapes everything below.

**Contribute what you use.** The contributions that age best come from
people who actually run the project in anger. If you use dotagent
daily and hit a sharp edge, that bug report or that patch is gold —
you understand the constraint the way no outside reviewer can. If
you've never used it, the most useful first step is usually to install
it, run an agent of your own, and *then* decide what to improve. See
[Contribute What You Use](https://avelino.run/contribute-what-you-use/)
for the longer version of this argument.

**Writing code is the cheap part now. Understanding is the expensive
part.** AI tooling has collapsed the cost of producing a diff that
compiles. It has not collapsed the cost of a reviewer figuring out
*what problem the diff is solving*, *why this approach over the
alternative*, and *what edge cases the author considered*. A PR that
arrives without that context — a generated patch with no issue
behind it, no reproduction, no rationale — moves the work from "write
the code" to "reverse-engineer the author's intent", which costs the
maintainer more than writing it from scratch would have.

So please, in this order:

1. **Open an issue (or a discussion) describing the problem in your own
   words** before you write code for anything non-trivial. One or two
   paragraphs is enough. Include how you hit it, what you expected,
   what happened instead.
2. **Wait for a short alignment exchange.** Sometimes the answer is
   "yes, please send a PR with approach X". Sometimes it's "we
   considered this and decided against it because Y" — and the issue
   thread becomes the record of that decision for the next person who
   wonders the same thing. Both outcomes save you from writing a PR
   that can't be merged.
3. **Then write the change** with the issue number in the PR
   description.

For typo fixes, doc improvements, obvious one-line bugs — skip step 1.
For everything else, the issue first is the contract that lets review
move at human speed instead of at "skim a 3000-line generated diff"
speed.

We're not anti-AI — every contributor uses whatever tools they want,
and we do too. The line is: **the human in the PR is the author**,
which means they understood the problem, made the trade-offs, and can
defend the choices in review. If you can't, the PR isn't ready yet.

## What kind of contributions we want

- **Bug fixes** with a regression test that fails before the fix and
  passes after.
- **Plugins** (preflight, sink, third-party notifier) following the
  protocol in [`docs/reference/plugin-protocol.md`](docs/reference/plugin-protocol.md).
- **Documentation improvements** — typos, clarifications, missing edge
  cases, broken links.
- **Performance work** with measurements — flamegraph, criterion bench,
  or before/after `cargo run --release` numbers.
- **Platform support** for additional OSes (currently macOS + Linux),
  provided the new platform gets its own CI matrix entry.

### Tips for getting your PR merged quickly

These aren't gates — they're the patterns that make review fast. A PR
that hits these usually merges with little back-and-forth:

- **Tie behavior to a test.** A fix or feature that lands with a test
  proving the new behavior is much easier to merge than the same diff
  without one.
- **Wait for the third duplication before abstracting.** We try to
  follow the Rule of Three — the first repeated pattern usually
  doesn't show its real shape until a third caller appears. If you're
  introducing a new abstraction, mentioning the three concrete callers
  in the PR description makes the case for it.
- **Put new notifier drivers in `dotagent-notify/src/drivers/`, not
  `plugins/`.** Built-in notifiers run in-process for performance —
  the plugin protocol exists as an escape hatch for third-party
  services we don't maintain here. (See [`CLAUDE.md`](CLAUDE.md) for
  the reasoning.)
- **Annotate `unsafe` blocks with `// SAFETY:`.** Even one or two
  sentences explaining the invariants and why a safe API was
  insufficient gets the review across the line. The existing `kill(2)`
  call in the runner is the model.
- **Cosmetic-only refactors are tough to review.** If you spot
  something worth cleaning up alongside a substantive change, that's
  great — bundling pure-cosmetic work that touches many crates is what
  makes review hard. When in doubt, open an issue first to align on
  scope.

## Project layout — where does my change go?

Use this table as the first filter. The same table (with longer
justifications) lives in [`CLAUDE.md`](CLAUDE.md).

| Change                                | Goes where                                              |
| ------------------------------------- | ------------------------------------------------------- |
| New shared type                       | `crates/dotagent-core/`                                 |
| Scheduling / time function            | `crates/dotagent-scheduler/` (**pure — no IO**)         |
| Filesystem state IO                   | `crates/dotagent-state/`                                |
| Subprocess spawn / lifecycle          | `crates/dotagent-runner/`                               |
| launchd / systemd unit emission       | `crates/dotagent-unit-gen/`                             |
| Logs / OTel / retention               | `crates/dotagent-telemetry/`                            |
| New CLI subcommand                    | `crates/dotagent/src/commands/`                         |
| New built-in notifier driver          | `crates/dotagent-notify/src/drivers/`                   |
| New preflight / sink plugin           | New crate under `plugins/`                              |
| Third-party notifier (escape hatch)   | New crate under `plugins/` (`kind = "notify"`)          |
| `agent.toml` shape change             | `crates/dotagent-core/src/manifest.rs` + `docs/reference/agent-spec.md` |
| Plugin protocol change                | `crates/dotagent-plugin/src/lib.rs` + `docs/reference/plugin-protocol.md` |
| New `config.toml` field               | `crates/dotagent-core/src/config.rs` + `docs/guides/config-reference.md` |

### Why these constraints exist

A few constraints look unusual at first glance. The reasoning matters
more than the rule — if you understand the why, the rule rarely needs
to be cited:

- **`dotagent-scheduler` is pure** (no `std::fs`, `std::env`, or
  `chrono::Local::now()`). Every scheduling function receives
  `now: DateTime<Local>` as a parameter. This keeps the test matrix
  fast and deterministic — a unit test for "weekday rollover at DST
  boundary" runs in milliseconds because nothing touches the clock.
- **Heartbeat shape is wire-compatible with the legacy Fish
  framework.** A user can migrate one agent at a time without
  invalidating execution history. Additive changes are fine; renames
  or removals need a documented migration path so the two frameworks
  can keep coexisting during the move.
- **The plugin protocol is public API.** External plugins in Go,
  Python, Bash, etc. depend on the JSON shape staying stable. A
  breaking change requires a major version bump of the
  `dotagent-plugin` crate and a CHANGELOG entry.
- **Workspace dependency versions live in the root `Cargo.toml`**
  under `[workspace.dependencies]`; member crates use
  `<dep>.workspace = true`. This keeps a single source of truth for
  dependency resolution. If you pin a version in a member crate,
  reviewers will ask you to move it to the root — same dependency,
  just one place to update.
- **No `default-features = true` on TLS-adjacent dependencies**
  (`reqwest`, etc.). We ship with `rustls`; enabling defaults pulls
  OpenSSL into the build, which contradicts the static-binary
  distribution model.

## Development setup

You need a working Rust **stable** toolchain (we target the version
pinned in `rust-toolchain.toml` when present, otherwise current stable).

```bash
git clone https://github.com/avelino/dotagent
cd dotagent
cargo build --workspace
```

To exercise the binary against the example agents:

```bash
cargo build --release -p dotagent
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent doctor
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent run hello-fish \
  --schedule manual --dry-run
```

The Homebrew formula in `Formula/` is regenerated by the release
pipeline — please don't edit it by hand.

## Self-verification — required before opening a PR

```bash
cargo fmt --all                                            # apply
cargo fmt --all -- --check                                 # what CI runs
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

`clippy -D warnings` is enforced in CI on both macOS and Linux. A new
warning is a build failure.

A `./scripts/validate.sh` (or `just validate`) target may be available;
when present, prefer it — it runs the same steps in the same order CI
uses.

### Doc-review obligation

User-visible changes (manifest schema, plugin protocol, new
plugin/command/env var, path changes, new event types, default changes)
have documentation consequences in multiple files. Before requesting
review:

- Update every doc that mentions the changed concept (use `git grep` to
  audit).
- Cross-check the four indexes that list things: `README.md`,
  `docs/llms.txt`, `docs/plugins/README.md`, `Cargo.toml` workspace
  members.
- Add an example to `examples/` when introducing a feature contributors
  will copy-paste.

If you use Claude Code, the `dotagent:doc-review` skill automates the
punch list — please run it.

## Commit & PR conventions

- **Branches**: short, kebab-case, problem-oriented. Good:
  `fix-heartbeat-window-rollover`, `add-pushover-driver`. Less helpful:
  `wip-3`, `feature-branch`, `patch-1` — these make the branch list
  hard to scan once a few are open at once.
- **Commits**: imperative, present tense (`Tighten parser, reject
  relative paths`, not `Tightened` or `Tightening`). Conventional
  Commits prefixes (`feat:`, `fix:`, `refactor:`) are welcome but not
  required.
- **One logical change per commit**. Mechanical reformats go in their
  own commit so the substantive diff is readable.
- **Don't add `Co-Authored-By: <AI tool>` trailers.** A human
  contributor is the responsible author for the code, regardless of
  which tools helped draft it — that's how license, copyright, and
  postmortem attribution work in practice. The PR description is the
  right place to disclose AI assistance if you want to.
- **Sign your commits** if your local setup supports it (`git commit
  -S`). Not enforced, but appreciated.

### PR description

The PR description should answer four questions:

1. **What problem does this solve?** Link the issue.
2. **What's the shape of the change?** Which crates, which docs.
3. **How did you verify?** Local commands run, manual smoke test,
   relevant test names.
4. **What did you choose not to do?** Trade-offs deferred, follow-ups
   filed.

Screenshots / terminal recordings for any change that affects CLI
output.

## Reviewing other people's PRs

PRs from external contributors are reviewed against the same checklist
as internal changes — see [`.github/copilot-instructions.md`](.github/copilot-instructions.md)
for the full rubric used by both human reviewers and AI assistants. The
short version:

- Does it respect crate ownership?
- Are user-visible changes mirrored in docs?
- Are new defaults sensible without configuration?
- Does it expand the security test surface (when touching parsing,
  spawning, or trust boundaries)?

## Security

Do **not** open public issues for security reports. See
[`SECURITY.md`](SECURITY.md) for the disclosure process.

## Code of Conduct

By participating in this project, you agree to abide by the
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).

## Licensing

By submitting a contribution, you agree that your work is licensed under
the project's [MIT License](LICENSE).
