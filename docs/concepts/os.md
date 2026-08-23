# Running installed binaries

An assistant that can read your calendar and your agents' logs, but cannot run
`rg`, spends its day asking you to paste things. `[os]` is the section that
lets it reach the binaries already on the machine — and the design work is
almost entirely about what it must *not* be able to reach.

It is **off by default**, and empty by default when on. Nothing in this page
happens until you write the section.

## The shape

```toml
[os]
enabled = true
allow = ["rg", "outl", "kubectl get"]
```

That is the whole minimum. `allow` is the policy; everything else refines it.

Two things consult that list:

```mermaid
flowchart LR
    M[assistant] -->|os-run| P{[os] policy}
    H[you, typing !rg foo] --> P
    P -->|deny| X[refused]
    P -->|confirm| C[parked, waits for !!]
    P -->|allow| R[argv → supervisor → output]
```

The model's door and yours land on the same policy. That is deliberate: two
implementations of one rule become two rules the first time someone edits only
one of them.

## Why an allowlist and not a sandbox

dotagent already runs whatever your manifests say, as you. The question here is
narrower: **what can a message cause to run?**

A message is not a manifest. It arrives from a chat, it may be a forwarded
article, and an assistant reading it composes tool calls from whatever is in
its context. So the boundary cannot be "whatever the model decided" — it has
to be something you wrote down before the conversation started.

An entry is a binary name, optionally followed by the leading arguments that
must match:

| Entry | Admits | Refuses |
|---|---|---|
| `rg` | `rg` with any arguments | — |
| `kubectl get` | `kubectl get pods` | `kubectl delete`, `kubectl getsecrets` |

Matching is on whole tokens, so a prefix never sneaks through. A path is
refused where a name belongs — `/bin/sh` is not a way to spell a listed `sh`.

Pick the granularity per binary. Bare for the ones that only read, pinned to a
subcommand for the ones that can change something you care about.

## Opening everything

```toml
allow = ["*"]
```

One entry, every binary on `PATH`, a shell included. It is supported, and it
is the widest thing in this document.

Prefer it over enumerating `PATH`. A list of a thousand names goes stale on the
next install and reads as though a decision was made about each one; `*` says
what is true. `doctor` prints a warning on every run while it is set, because
this is exactly the kind of thing that gets configured once and forgotten.

What it means concretely: whoever can send a message can run what you can run.
Whatever guards the inbound channel — Telegram's `allowed_user_ids`, the local
socket's uid check — is now guarding the machine rather than the agent catalog.

## Asking before it acts

Two more lists say *how* something runs, not *whether*:

```toml
deny = ["shutdown", "reboot"]          # never, whoever asks
confirm = ["rm", "dd", "sh", "bash"]   # only after a person says yes
```

`deny` wins over `allow`, `*` included. A list that could be widened past its
own refusals would not be a refusal.

**`confirm` does not default to empty.** With `allow = ["*"]`, an empty one
would mean a chat message can repartition a disk with nothing in between. The
default covers the destructive classics — `rm`, `rmdir`, `dd`, `mkfs`,
`shred`, `diskutil`, `fdisk`, `parted`, `shutdown`, `reboot`, `halt` — and
every shell: `sh`, `bash`, `zsh`, `fish`, `dash`, `ksh`.

The shells are the entry that makes the rest mean anything. A guard on `rm`
that lets `sh -c 'rm -rf /'` through guards nothing.

Matching per binary is also what makes it hold. `confirm = ["rm"]` catches all
of these:

```
rm -rf /
rm -r -f /
rm -fr /
rm --recursive --force /
```

A textual pattern like `"rm -rf"` catches the first and misses three.

## The `!` prefix

A message beginning with `!` runs the command directly. The dispatcher never
sees it:

```
!rg TODO src
!outl page history buser-cto
```

No model call, no session, nothing stored. Errors come back raw, because the
point of typing a command is that it is exact and a paraphrased exit code is
worse than the exit code.

For anything on the `confirm` list, it parks:

```
!rm -r /tmp/build

    This will run:

        rm -r /tmp/build

    Send `!!` to confirm. It expires in 120s.

!!

    `rm` exited 0 and printed nothing.
```

One slot per conversation, so `!!` can only release what *that* chat parked.
Pending confirmations live in memory: a restart forgets them, which fails in
the safe direction.

The prefix is read **after** the channel's allowlist and rate limit, never
before. Reading it earlier would make `!` a way past both.

Quotes group an argument (`!rg "hello world"`). Nothing else from a shell
applies, because no shell is involved: `!ls; rm x` looks for a binary named
`ls;` and does not find it.

Works over Telegram and over the local socket (`dotagent api`).

## A model may not confirm itself

This is the one asymmetry between the two doors, and it is the reason there
are two.

When the assistant asks for something on the `confirm` list, `os-run` refuses
and tells it to have you type the line. A tool that could both request and
grant a confirmation would be theatre.

The reasoning is not that typed commands are trusted more. A typed line cannot
be steered by content at all — there is nothing to inject. A model composes
from whatever entered its context, so the prompt-injection surface exists on
exactly one of the two paths, and that is the one where the brake has to be
absolute.

## Naming a binary so the model knows it exists

`os-run` makes every allowed binary reachable. Reachable is not discoverable:
a model has to already know `outl` exists and guess what it is for.

```toml
[[os.tool]]
bin = "outl"
description = "Personal outliner: search notes, read a page by slug, read a daily journal."

[[os.tool]]
bin = "kubectl"
args = ["get"]
description = "Read Kubernetes objects: pods, deployments, nodes. Read-only."
```

That publishes `os-outl` and `os-kubectl-get` in the MCP catalog, each carrying
your sentence. The description is the entire point — a name without one is what
`os-run` already offers.

`args` fixes the leading arguments and the model can only append. That is what
makes `kubectl get` a publishable read-only view of a binary that also deletes:
asking that tool for `delete pods` runs `kubectl get delete pods`, which fails
as it should.

**Keep the list short.** A normal machine carries around a thousand executables
on `PATH`. A tool each would bury the catalog and push the useful ones behind
tool search. Name the ones that come up by name in conversation; let the rest
fall through to `os-run`.

`doctor` reports an entry whose binary `allow` does not admit, one with an
empty description, and two entries that would resolve to the same name — a
published tool that is then refused is worse than a missing one, because a
model picks it from the description and the failure lands mid-conversation.

## What this is not

It is not a sandbox, and the allowlist does not pretend to bound capability.

An allowlisted binary is trusted with everything that binary can do. `git log`
cannot write to a repository, but `git` with the right flags runs a pager, and
any binary carrying an `--exec`-style flag will do what that flag says. What
the list buys is that the set of programs is finite, declared and auditable —
not that every invocation inside it is harmless.

Listing `kubectl` bare on a machine holding production credentials means a chat
message reaches production. That is a choice made per entry, which is why the
granularity exists and why the default is an empty list.

## What always holds

Whatever the lists say, these do not change:

- **argv, never a shell.** Arguments reach the program as a token list. `|`,
  `&&`, `;` and `$(…)` in an argument are literal characters.
- **A name, not a path.** Resolution goes through `PATH`; `/bin/sh`, `./sh`
  and anything carrying `..` are refused before any list is consulted.
- **Supervised**, with the configured deadline and kill-tree on expiry.
- **Audited** as `os_command_invoked` at `Critical`, recording the binary and
  the full argument list — the half that no config declared in advance.
- **Output capped** at 8 KB, so one `rg` over a large tree cannot evict the
  conversation it was called from.

## See also

- [Configuration reference](../guides/config-reference.md#os) — every key
- [MCP reference](../reference/mcp.md#os-tools) — the tool surface
- [Telegram](telegram.md#running-a-command-yourself) — the `!` prefix in a chat
- [Local client API](../reference/local-api.md) — the same prefix over the socket
- [Threat model](../security/threat-model.md) — V17, and what it does not bound
