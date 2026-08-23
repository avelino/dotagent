//! Running an installed binary, for the two paths that may ask.
//!
//! `[os] allow` is the policy. Two things consult it:
//!
//! - `os-run`, where a **model** chose the binary and the arguments.
//! - a `!` message, where a **person** typed them.
//!
//! The second is not the looser case. A model composes from whatever entered
//! its context, so a forwarded message or a page title can steer it; a typed
//! `!` line cannot be steered by content at all. What it can be is somebody
//! else's fingers — the allowlist authenticates an account, never a person
//! (see `docs/security/threat-model.md` V8) — so both go through the same
//! list. One policy, two doors.
//!
//! Nothing here reaches a shell. Arguments are a token list from the first
//! character to the last.

use dotagent_core::config::OsConfig;
use dotagent_state::AuditLog;

/// The `[os]` section as it stands on disk right now.
///
/// Read per invocation rather than cached, so narrowing the allowlist takes
/// effect on the next message instead of the next daemon restart.
pub fn config() -> OsConfig {
    dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .os
}

/// Cap on what one invocation returns. `rg` over a large tree does not get to
/// evict the conversation it was called from.
const OUTPUT_MAX_BYTES: usize = 8_000;

/// What running one binary produced.
pub struct Outcome {
    pub text: String,
    pub is_error: bool,
}

/// The `!` prefix, split into a binary and its arguments.
///
/// `!` and `! ` are both accepted, so `!ls` and `! ls` mean the same thing.
/// Returns `None` when the text is not a bang line, which is the common case
/// and must stay cheap.
pub fn parse_bang(text: &str) -> Option<(String, Vec<String>)> {
    let rest = text.strip_prefix('!')?.trim();
    if rest.is_empty() {
        return None;
    }
    let mut tokens = split_tokens(rest);
    if tokens.is_empty() {
        return None;
    }
    let bin = tokens.remove(0);
    Some((bin, tokens))
}

/// Split on whitespace, honoring single and double quotes.
///
/// Quotes are the one shell habit worth keeping: `! rg "hello world"` is what
/// a person types, and splitting it into two arguments would search for the
/// wrong thing. Nothing else from a shell is interpreted — no escapes, no
/// substitution, no operators — because there is no shell to interpret them.
fn split_tokens(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;

    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started || !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => current.push(ch),
        }
    }
    if started || !current.is_empty() {
        out.push(current);
    }
    out
}

/// A command waiting for someone to say yes.
///
/// Held in memory only. A daemon restart forgets every pending confirmation,
/// which is the safe direction to fail: the worst case is retyping the
/// command, not a `!!` from yesterday landing on a machine whose operator has
/// moved on.
struct Pending {
    bin: String,
    args: Vec<String>,
    asked_at: std::time::Instant,
}

/// Pending confirmations, one per conversation.
///
/// Keyed by session so a `!!` in one chat can never confirm what was asked in
/// another. One slot per session, because a queue of pending destructive
/// commands answered by a bare `!!` is a way to confirm the wrong one.
#[derive(Default)]
pub struct Confirmations {
    inner: std::sync::Mutex<std::collections::HashMap<String, Pending>>,
}

impl Confirmations {
    /// Record what `session` will run if it confirms, replacing anything it
    /// had pending.
    pub fn park(&self, session: &str, bin: &str, args: &[String]) {
        let mut guard = self.inner.lock().expect("confirmations poisoned");
        guard.insert(
            session.to_string(),
            Pending {
                bin: bin.to_string(),
                args: args.to_vec(),
                asked_at: std::time::Instant::now(),
            },
        );
    }

    /// Take what `session` had pending, if it has not aged out.
    pub fn take(&self, session: &str, ttl_seconds: u64) -> Option<(String, Vec<String>)> {
        let mut guard = self.inner.lock().expect("confirmations poisoned");
        let pending = guard.remove(session)?;
        if pending.asked_at.elapsed() > std::time::Duration::from_secs(ttl_seconds) {
            return None;
        }
        Some((pending.bin, pending.args))
    }
}

/// Is this the confirmation token?
///
/// `!!` and nothing else. A yes has to be unambiguous, and "sim" arriving in
/// the middle of a conversation is not — the sender may be answering the
/// assistant's last question rather than a parked `rm`.
pub fn is_confirmation(text: &str) -> bool {
    text.trim() == "!!"
}

/// How the parked command is quoted back before it runs.
pub fn confirmation_prompt(bin: &str, args: &[String], ttl_seconds: u64) -> String {
    let rendered = if args.is_empty() {
        bin.to_string()
    } else {
        format!("{bin} {}", args.join(" "))
    };
    format!(
        "This will run:\n\n    {rendered}\n\nSend `!!` to confirm. It expires in {}s.",
        ttl_seconds
    )
}

/// Refusal text for a binary the allowlist does not admit.
///
/// Worded so that an assistant relaying it does not sound like it decided.
pub fn refusal(bin: &str) -> String {
    format!(
        "`{bin}` is not allowed here. Call os-list for what is, and tell whoever asked \
         that it is the machine's config that says no — not you."
    )
}

/// Same refusal for someone who typed the line themselves. They are not
/// asking an assistant to relay anything, so it names the file to edit.
pub fn refusal_typed(bin: &str) -> String {
    format!("`{bin}` is not allowed. Add it to `[os] allow` in config.toml to change that.")
}

/// Run `bin` with `args` unless the policy denies the pair.
///
/// Checked here rather than by the caller, so a new caller cannot forget it.
/// The test is `decide() != Deny` rather than `== Allow`, because a caller
/// reaching this point with a `Confirm` verdict has already collected the
/// confirmation — and `deny` must still refuse a command someone confirmed.
///
/// `refused` supplies the wording, which is the only thing the paths phrase
/// differently.
pub async fn run_allowed(
    cfg: &OsConfig,
    bin: &str,
    args: &[String],
    refused: impl Fn(&str) -> String,
) -> Outcome {
    if cfg.decide(bin, args) == dotagent_core::config::OsDecision::Deny {
        return Outcome {
            text: refused(bin),
            is_error: true,
        };
    }

    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let plugins = dotagent_plugin::PluginClient::from_environment();
    let spec = dotagent_supervisor::SpawnSpec {
        kind: dotagent_supervisor::ProcessKind::Skill,
        owner: dotagent_supervisor::ProcessOwner {
            agent: "os".to_string(),
            ..Default::default()
        },
        deadline: std::time::Duration::from_secs(cfg.timeout_seconds),
        label: format!("os:{bin}"),
    };

    let handle = match plugins.supervisor().spawn_supervised(cmd, spec).await {
        Ok(h) => h,
        Err(e) => {
            return Outcome {
                text: format!("Could not start `{bin}`: {e}"),
                is_error: true,
            }
        }
    };

    let (text, is_error, exit_code, timed_out) = match handle.wait_with_output().await {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if code == 0 {
                let body = if stdout.is_empty() {
                    format!("`{bin}` exited 0 and printed nothing.")
                } else {
                    cap(&stdout, OUTPUT_MAX_BYTES)
                };
                (body, false, code, false)
            } else {
                let detail = if stderr.is_empty() { &stdout } else { &stderr };
                (
                    format!("`{bin}` exited {code}.\n{}", cap(detail, OUTPUT_MAX_BYTES)),
                    true,
                    code,
                    false,
                )
            }
        }
        Err(dotagent_supervisor::SupervisorError::TimedOut { elapsed, .. }) => (
            format!(
                "`{bin}` was still running after {}s and was killed.",
                elapsed.as_secs()
            ),
            true,
            -1,
            true,
        ),
        Err(e) => (format!("`{bin}` could not run: {e}"), true, -1, false),
    };

    // Audited because an inbound message started a process. The arguments are
    // recorded in full: they are the half no manifest declared.
    if let Ok(audit) = AuditLog::from_home() {
        let _ = audit.append(dotagent_core::AuditEvent::OsCommandInvoked {
            bin: bin.to_string(),
            args: args.to_vec(),
            exit_code,
            timed_out,
        });
    }
    Outcome { text, is_error }
}

/// Truncate on a char boundary, saying so.
fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… truncated at {max} bytes.", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_bang_line_splits_into_a_binary_and_arguments() {
        assert_eq!(parse_bang("!ls -la"), Some(("ls".into(), v(&["-la"]))));
        assert_eq!(parse_bang("! ls -la"), Some(("ls".into(), v(&["-la"]))));
        assert_eq!(parse_bang("!   ls"), Some(("ls".into(), v(&[]))));
    }

    #[test]
    fn text_without_the_prefix_is_not_a_bang_line() {
        assert_eq!(parse_bang("ls -la"), None);
        assert_eq!(parse_bang("what does ! mean"), None);
        assert_eq!(parse_bang(""), None);
    }

    #[test]
    fn a_bare_bang_is_not_a_command() {
        assert_eq!(parse_bang("!"), None);
        assert_eq!(parse_bang("!   "), None);
    }

    #[test]
    fn quotes_hold_a_phrase_together() {
        assert_eq!(
            parse_bang(r#"!rg "hello world" src"#),
            Some(("rg".into(), v(&["hello world", "src"])))
        );
        assert_eq!(
            parse_bang("!rg 'hello world'"),
            Some(("rg".into(), v(&["hello world"])))
        );
    }

    #[test]
    fn an_empty_quoted_argument_survives() {
        assert_eq!(
            parse_bang(r#"!rg "" x"#),
            Some(("rg".into(), v(&["", "x"])))
        );
    }

    #[test]
    fn shell_operators_are_ordinary_characters() {
        // No shell runs this, so these are arguments to one program and not
        // a second command. The test pins that they stay in one invocation.
        assert_eq!(
            parse_bang("!rg foo && rm -rf /"),
            Some(("rg".into(), v(&["foo", "&&", "rm", "-rf", "/"])))
        );
        assert_eq!(
            parse_bang("!echo hi | sh"),
            Some(("echo".into(), v(&["hi", "|", "sh"])))
        );
        assert_eq!(
            parse_bang("!echo $(whoami)"),
            Some(("echo".into(), v(&["$(whoami)"])))
        );
    }

    #[test]
    fn a_semicolon_does_not_start_a_second_command() {
        assert_eq!(
            parse_bang("!ls; rm x"),
            Some(("ls;".into(), v(&["rm", "x"])))
        );
    }

    #[test]
    fn only_a_bare_double_bang_confirms() {
        assert!(is_confirmation("!!"));
        assert!(is_confirmation("  !!  "));
        // Anything carrying more is a command, not an answer. `!!rm` must not
        // release a parked command *and* mean something else.
        assert!(!is_confirmation("!!rm"));
        assert!(!is_confirmation("!! rm"));
        assert!(!is_confirmation("sim"));
        assert!(!is_confirmation("yes"));
        assert!(!is_confirmation("!"));
        assert!(!is_confirmation(""));
    }

    #[test]
    fn a_parked_command_comes_back_once() {
        let c = Confirmations::default();
        c.park("chat-1", "rm", &["-rf".to_string(), "/tmp/x".to_string()]);
        let taken = c.take("chat-1", 120).expect("should be pending");
        assert_eq!(taken.0, "rm");
        // Taken means gone: a second `!!` must not re-run it.
        assert!(c.take("chat-1", 120).is_none());
    }

    #[test]
    fn a_confirmation_cannot_cross_conversations() {
        let c = Confirmations::default();
        c.park("chat-1", "rm", &["/tmp/x".to_string()]);
        assert!(c.take("chat-2", 120).is_none());
        assert!(c.take("chat-1", 120).is_some());
    }

    #[test]
    fn an_expired_confirmation_is_not_answerable() {
        let c = Confirmations::default();
        c.park("chat-1", "rm", &["/tmp/x".to_string()]);
        // Zero ttl: anything already parked is older than that.
        assert!(c.take("chat-1", 0).is_none());
    }

    #[test]
    fn parking_again_replaces_rather_than_queues() {
        // One slot per conversation. Two pending commands answered by a bare
        // `!!` is a way to confirm the wrong one.
        let c = Confirmations::default();
        c.park("chat-1", "rm", &["/tmp/first".to_string()]);
        c.park("chat-1", "rm", &["/tmp/second".to_string()]);
        let (_, args) = c.take("chat-1", 120).unwrap();
        assert_eq!(args, vec!["/tmp/second".to_string()]);
        assert!(c.take("chat-1", 120).is_none());
    }

    #[test]
    fn the_prompt_quotes_the_command_it_will_run() {
        let p = confirmation_prompt("rm", &["-rf".to_string(), "/tmp/x".to_string()], 120);
        assert!(p.contains("rm -rf /tmp/x"));
        assert!(p.contains("!!"));
        assert!(p.contains("120"));
    }

    #[test]
    fn cap_marks_what_it_removed() {
        let long = "x".repeat(100);
        let out = cap(&long, 10);
        assert!(out.starts_with("xxxxxxxxxx"));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn cap_leaves_short_output_alone() {
        assert_eq!(cap("hi", 10), "hi");
    }

    #[test]
    fn cap_does_not_split_a_multibyte_character() {
        let s = "áááááá";
        let out = cap(s, 5);
        assert!(out.contains("truncated"));
    }
}
