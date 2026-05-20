//! `dotagent completions <shell>` — emit a shell completion script.
//!
//! Strategy:
//! 1. Generate the boilerplate (subcommands + flags) via `clap_complete`.
//! 2. Append a shell-specific tail that wires dynamic completion of agent
//!    names. The tail calls `dotagent _list-agents` (a hidden subcommand) at
//!    completion time, so the candidate set always reflects the manifests
//!    discovered on disk *right now* — no stale baked-in lists.
//!
//! Subcommands that take an agent name positional:
//!   `run`, `inspect`, `run-now`, `logs`, `install`, `uninstall`.

use std::io;

use clap::Command;
use clap_complete::{generate, Shell};

/// Subcommands whose first positional arg is an agent name (or optional one).
const AGENT_NAME_SUBCOMMANDS: &[&str] =
    &["run", "inspect", "run-now", "logs", "install", "uninstall"];

pub fn print(shell: Shell, cmd: &mut Command) {
    // Generate the base script via clap_complete.
    let bin_name = cmd.get_name().to_string();
    let mut buf = Vec::<u8>::new();
    generate(shell, cmd, &bin_name, &mut buf);

    // Strip lines that *suggest* `_list-agents` as a subcommand candidate.
    // `hide = true` on the clap subcommand keeps it out of `--help` but
    // clap_complete still emits it as a tab-completion candidate. The
    // helper is internal — users should never see it offered.
    let script = String::from_utf8_lossy(&buf);
    let filtered: String = script
        .lines()
        .filter(|line| !is_list_agents_suggestion(line))
        .map(strip_list_agents_from_opts)
        .collect::<Vec<_>>()
        .join("\n");

    // Write boilerplate first.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    use io::Write;
    let _ = out.write_all(filtered.as_bytes());
    let _ = out.write_all(b"\n");

    // Append dynamic agent-name completion tailored to the shell.
    let tail = match shell {
        Shell::Fish => fish_tail(&bin_name),
        Shell::Zsh => zsh_tail(&bin_name),
        Shell::Bash => bash_tail(&bin_name),
        // Elvish / PowerShell: not wired for dynamic completion. The base
        // script still works for subcommand + flag completion.
        _ => String::new(),
    };
    let _ = out.write_all(tail.as_bytes());
}

/// True when this line is a *candidate suggestion* for the hidden
/// `_list-agents` subcommand. Matches the patterns clap_complete emits
/// across fish/zsh/bash:
///
/// - fish: `... -a "_list-agents" -d '...'`
/// - zsh: `'_list-agents:(internal) ...' \`
/// - bash: `_list-agents) ...` inside a case (left intact — the dispatch
///   block routes manual invocations, suggestion stripping for bash
///   happens via `strip_list_agents_from_opts` on `opts="..."` lines).
fn is_list_agents_suggestion(line: &str) -> bool {
    let trimmed = line.trim_start();
    // fish suggestion: `complete -c <bin> ... -a "_list-agents" ...`
    if trimmed.starts_with("complete ") && trimmed.contains(r#"-a "_list-agents""#) {
        return true;
    }
    // zsh `_arguments`/`_values` candidate line.
    if trimmed.starts_with("'_list-agents:") || trimmed.starts_with("\"_list-agents:") {
        return true;
    }
    false
}

/// Bash emits subcommand lists as `opts="run tick ... _list-agents help"`.
/// Strip the helper from those candidate strings; leave it elsewhere so the
/// generated case-dispatch still routes manual invocations correctly.
fn strip_list_agents_from_opts(line: &str) -> String {
    if !line.contains("opts=") {
        return line.to_string();
    }
    line.replace(" _list-agents", "")
}

/// Fish: completions are additive. `__fish_seen_subcommand_from X` matches
/// after the subcommand keyword is present anywhere on the line.
fn fish_tail(bin: &str) -> String {
    let mut s = String::new();
    s.push_str("\n# --- dotagent: dynamic agent-name completion ---\n");
    s.push_str(&format!(
        "function __{bin}_list_agents\n    command {bin} _list-agents 2>/dev/null\nend\n",
        bin = bin
    ));
    for sub in AGENT_NAME_SUBCOMMANDS {
        s.push_str(&format!(
            "complete -c {bin} -n \"__fish_seen_subcommand_from {sub}\" -f -a \"(__{bin}_list_agents)\"\n",
            bin = bin,
            sub = sub
        ));
    }
    s
}

/// Zsh: register a `compdef` override that runs *after* the clap-generated
/// `_dotagent` function. We wrap the original handler: when the completion
/// state lands on a positional for one of our agent-name subcommands,
/// inject the list from the helper.
fn zsh_tail(bin: &str) -> String {
    let subs = AGENT_NAME_SUBCOMMANDS.join("|");
    format!(
        r#"
# --- dotagent: dynamic agent-name completion ---
_{bin}_dynamic_agents() {{
    local -a agents
    agents=("${{(@f)$({bin} _list-agents 2>/dev/null)}}")
    _describe 'agent' agents
}}

_{bin}_dynamic_wrap() {{
    local prev_func="_{bin}"
    # Call the clap-generated completer first.
    "$prev_func" "$@"
    # If we are positioned on the first positional of an agent-name
    # subcommand, layer agent names on top.
    local words=( ${{(z)BUFFER}} )
    local subcmd="${{words[2]}}"
    case "$subcmd" in
      {subs})
        _{bin}_dynamic_agents
        ;;
    esac
}}

compdef _{bin}_dynamic_wrap {bin}
"#,
        bin = bin,
        subs = subs,
    )
}

/// Bash: wrap the clap-generated `_dotagent` function. After running it,
/// if the active subcommand expects an agent name, splice in our names.
fn bash_tail(bin: &str) -> String {
    let subs = AGENT_NAME_SUBCOMMANDS.join("|");
    format!(
        r#"
# --- dotagent: dynamic agent-name completion ---
_{bin}_dynamic_agents() {{
    local cur="${{COMP_WORDS[COMP_CWORD]}}"
    local subcmd="${{COMP_WORDS[1]}}"
    case "$subcmd" in
        {subs})
            local agents
            agents="$({bin} _list-agents 2>/dev/null)"
            local additions
            additions=($(compgen -W "$agents" -- "$cur"))
            if [ "${{#additions[@]}}" -gt 0 ]; then
                COMPREPLY+=("${{additions[@]}}")
            fi
            ;;
    esac
}}

_{bin}_orig=$(declare -f _{bin})
if [ -n "$_{bin}_orig" ]; then
    eval "_{bin}_clap() ${{_{bin}_orig#*\(\) }}"
    _{bin}() {{
        _{bin}_clap "$@"
        _{bin}_dynamic_agents
    }}
    complete -F _{bin} -o bashdefault -o default {bin}
fi
"#,
        bin = bin,
        subs = subs,
    )
}
