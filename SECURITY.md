# Security Policy

dotagent is a daemon that spawns user-defined commands on a schedule. We
take security reports seriously and respond on a published timeline.

## Supported versions

We support **the latest released minor version** with security fixes.

| Version  | Supported          |
| -------- | ------------------ |
| `0.1.x`  | Yes (current line) |
| `< 0.1`  | No                 |

"Current line" means the highest minor version published on the
[Releases page](https://github.com/avelino/dotagent/releases). If you're
unsure whether you're on it, `dotagent --version` and the Releases page
together resolve the question.

Once `1.0.0` ships, this table will be updated to reflect a longer
support window for the most recent two minor lines.

## What is in scope

- The `dotagent` daemon, CLI, and all crates in `crates/`.
- The official plugins in `plugins/` (preflight, sinks, notifiers
  marked as official).
- The Homebrew formula and release artifacts distributed from this
  repository's GitHub Releases.
- The documentation in `docs/` — if a doc instructs users to take an
  action that compromises their system, that's a security report too.

## What is out of scope

Read [`docs/security/threat-model.md`](docs/security/threat-model.md)
first. The premise: dotagent runs as the user. An attacker with the
same user-level capability the daemon has can already do everything
dotagent does, by editing `~/.config` directly or replacing the binary.
**We do not try to defend against a local attacker with user-equivalent
privileges**; doing so would require a hardware-rooted privilege
boundary dotagent cannot provide.

The following kinds of reports won't lead to a code change — we list
them here so you don't spend time writing one up, and so the reasoning
is transparent:

- "I have write access to `~/.config/dotagent/` and I can change which
  command runs." — Correct, and the threat model is explicit about
  this being out of scope. An attacker who can write your config can
  already run anything as you.
- "I have root on the machine and can modify the daemon binary." —
  Same boundary. Root on the host bypasses every userland defense.
- Vulnerabilities in third-party plugins not maintained in this
  repository — please reach out to the plugin's author directly;
  they're better positioned to fix it.
- Vulnerabilities in `mcp` (a separate project at
  <https://github.com/avelino/mcp>). dotagent shells out to it but
  doesn't vendor it, so the fix belongs there.

If you're not sure whether a finding fits one of these categories,
send the report anyway — we'd rather receive a borderline one and
explain than miss something real.

In-scope, and the kind of report we want:

- A way to make the daemon execute commands **not declared** in any
  `agent.toml`.
- A way to bypass the audit log (an action that should emit an event
  but doesn't).
- A way to read or write files outside `DOTAGENT_ROOT` and
  `~/.config/dotagent/` from a manifest field that should be sandboxed.
- A way to leak secrets injected via `[env]` into log lines, audit
  entries, or notifier payloads that the user did not opt into.
- A way for a malicious plugin response to crash, hang, or corrupt the
  daemon's persistent state.
- A way for input flowing through `command` / `args` / `extra_env` to
  reach a shell interpreter (we explicitly never `sh -c` user input).
- A way to escalate privileges from a plugin subprocess back into the
  daemon process.
- Cryptographic weaknesses in the audit log hash chain or the
  secrets-file parser.
- Any vulnerability in our build / release pipeline that could be used
  to ship a tampered binary via the official channels.

## How to report

**Please do not open public GitHub issues for security reports.**

Use one of these two channels:

1. **GitHub Private Vulnerability Reporting** — preferred.
   <https://github.com/avelino/dotagent/security/advisories/new>
2. **Email** — `avelinorun@gmail.com` with the subject prefix
   `[dotagent security]`.

A useful report includes:

- The dotagent version (`dotagent --version`).
- The operating system and version.
- A minimal reproduction. A short `agent.toml` plus the exact command
  is ideal.
- The impact — what an attacker gains, what assumptions are required.
- Suggested remediation, if you have one.

If your report contains live secrets or PII, please redact them before
sending. We do not need real credentials to validate a class of
vulnerability.

## Response timeline

| Step                                  | Target time           |
| ------------------------------------- | --------------------- |
| Acknowledgement of receipt            | Within **72 hours**   |
| Initial triage + severity assessment  | Within **7 days**     |
| Status update cadence during fix      | At least every **14 days** |
| Coordinated disclosure window         | Up to **90 days** from receipt |

For **critical** issues (remote unauthenticated impact, secret
exfiltration, complete daemon compromise), we will aim for a release
within 7 days of confirmation.

If we miss a deadline we owe you, that is a bug in our process — please
escalate by replying to the original thread.

## Coordinated disclosure

We follow coordinated disclosure:

1. You report privately.
2. We acknowledge, investigate, prepare a fix and a release.
3. We publish the release, a GitHub Security Advisory, and request a CVE
   if appropriate.
4. You are credited in the advisory unless you prefer to remain
   anonymous.

We do not run a bug bounty program at this time. We are happy to
acknowledge researchers publicly in the advisory and in release notes.

## Defensive baseline (what dotagent does today)

For context — these are mitigations already in the codebase, documented
in the threat model:

- **Manifest drift detection** — sha256 of every loaded manifest is
  cached in `~/.config/dotagent/state/known_manifests.json`; changes
  emit `manifest_drift_detected` and notify out-of-band.
- **Phantom-agent detection** — newly appearing agent names emit
  `phantom_agent_detected` on first sight.
- **No shell interpolation** — `RunConfig::args` is a token list passed
  to `Command::new(...).args(...)`; we never `sh -c` user input.
- **Append-only audit log** with per-event hash chain.
- **TLS via `rustls`** — every HTTP-speaking crate in the workspace is
  configured with `default-features = false` and an explicit
  `rustls-tls` feature.

Planned but not yet shipped: daemon self-hash check, plugin hash
registry, resource-bound limits on retry storms. These are tracked in
the threat model document and will accept patches.
