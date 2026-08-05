//! Commands — procedures **you** pick.
//!
//! A skill ([`crate::skill`]) is loaded because a model matched its
//! description. A command is loaded because a human named it. Same markdown,
//! opposite entry point, and the difference is the whole feature: when you type
//! `/commit_message` there is nothing to infer, so routing it through a model
//! that reads descriptions and *chooses* is latency and a chance of choosing
//! something else.
//!
//! ## The file
//!
//! One markdown file per command, in [Claude Code slash command][cc] format, so
//! a `~/.claude/commands/` directory you already keep is reusable rather than
//! transcribed:
//!
//! ```markdown
//! ---
//! description: Generate a Conventional Commits message from a diff
//! argument-hint: "[path]"
//! ---
//!
//! Write a Conventional Commits message for $ARGUMENTS.
//! ```
//!
//! The name comes from the filename — `commit-message.md` is `commit-message` —
//! because that is what Claude Code does and what the author already typed.
//!
//! [cc]: https://docs.claude.com/en/docs/claude-code/slash-commands

use crate::error::{Error, Result};
use crate::frontmatter;

/// Extension that marks a file as a command.
pub const COMMAND_EXT: &str = "md";

/// Telegram's hard cap on a registered command name.
pub const TELEGRAM_NAME_MAX: usize = 32;

/// Separator for a command in a subdirectory: `git/status.md` is `git:status`.
/// Claude Code's namespacing, kept so a nested catalog ports unchanged.
pub const NAMESPACE_SEP: char = ':';

/// One command, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandManifest {
    /// Catalog name, normally the filename without its extension.
    pub name: String,
    /// What the command does. Required: it is the line Telegram renders in the
    /// `/` menu, and a command nobody can identify is one nobody picks.
    pub description: String,
    /// What to type after the name, shown in the menu — `"[path] [--staged]"`.
    pub argument_hint: Option<String>,
    /// `allowed-tools` from the frontmatter, carried verbatim.
    ///
    /// **A hint, not enforcement.** dotagent does not own the dispatcher, so it
    /// cannot constrain what the dispatcher's model may call. Passing it along
    /// lets a dispatcher that *does* control its own harness use it; pretending
    /// it were a guarantee would be worse than not having the field.
    pub allowed_tools: Option<String>,
    /// Everything after the frontmatter: the prompt itself.
    pub body: String,
}

impl CommandManifest {
    /// Parse a command file. `fallback_name` is the name derived from the path,
    /// used unless the frontmatter overrides it.
    pub fn parse(raw: &str, fallback_name: &str) -> Result<Self> {
        let (front, body) = frontmatter::split(raw, "a command file")?;
        let fields = frontmatter::fields(&front);
        let field = |key: &str| frontmatter::get(&fields, key);

        let command = Self {
            name: field("name").unwrap_or_else(|| fallback_name.to_string()),
            description: field("description").unwrap_or_default(),
            argument_hint: field("argument-hint").or_else(|| field("argument_hint")),
            allowed_tools: field("allowed-tools").or_else(|| field("allowed_tools")),
            body: body.trim().to_string(),
        };
        command.validate()?;
        Ok(command)
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::InvalidManifest("command has an empty name".into()));
        }
        if self.description.trim().is_empty() {
            return Err(Error::InvalidManifest(format!(
                "command '{}': description is required — it is what the menu shows",
                self.name
            )));
        }
        if self.body.trim().is_empty() {
            return Err(Error::InvalidManifest(format!(
                "command '{}': body is empty — there is no prompt to send",
                self.name
            )));
        }
        // Registering with Telegram is the point of a command, so a name it
        // will refuse is a broken command rather than a degraded one. Caught
        // here means `doctor` prints it instead of the Bot API rejecting the
        // whole `setMyCommands` batch and taking the other commands with it.
        telegram_name(&self.name)?;
        Ok(())
    }

    /// Substitute the caller's arguments into the body.
    ///
    /// `$ARGUMENTS` takes the whole tail; `$1`…`$N` take whitespace-separated
    /// positions. Purely textual — the result is a prompt, and nothing here
    /// ever reaches a shell.
    ///
    /// When the body declares no placeholder at all but arguments were given,
    /// they are appended under a header rather than dropped. Silently losing
    /// what someone typed after the command is the worse failure: they would
    /// see a plausible answer to a question they did not ask.
    pub fn render(&self, args: &str) -> String {
        let args = args.trim();
        let positional: Vec<&str> = args.split_whitespace().collect();
        let (rendered, substituted) = substitute(&self.body, args, &positional);

        if substituted || args.is_empty() {
            return rendered;
        }
        format!("{rendered}\n\n---\nArguments: {args}")
    }
}

/// Walk the body once, replacing `$ARGUMENTS` and `$N`.
///
/// A single pass rather than successive `replace` calls: replacing `$1` first
/// would corrupt `$10`, and replacing longest-first still breaks as soon as
/// someone writes `$12`.
fn substitute(body: &str, all: &str, positional: &[&str]) -> (String, bool) {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.char_indices().peekable();
    let mut substituted = false;

    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let rest = &body[i + 1..];

        if let Some(tail) = rest.strip_prefix("ARGUMENTS") {
            // Only a whole word: `$ARGUMENTSX` is not a placeholder.
            if !tail.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                out.push_str(all);
                substituted = true;
                for _ in 0.."ARGUMENTS".len() {
                    chars.next();
                }
                continue;
            }
        }

        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            // 1-indexed, matching the shell convention the syntax borrows.
            let idx: usize = digits.parse().unwrap_or(0);
            if idx >= 1 {
                out.push_str(positional.get(idx - 1).copied().unwrap_or(""));
                substituted = true;
                for _ in 0..digits.len() {
                    chars.next();
                }
                continue;
            }
        }

        out.push('$');
    }
    (out, substituted)
}

/// Map a command name to the form Telegram will register.
///
/// Telegram allows `[a-z0-9_]{1,32}` — **no hyphens**, unlike MCP tool names.
/// So `commit-message` registers as `commit_message` while the same command is
/// `command-commit-message` over MCP: two sanitizers over one name, each lossy
/// in its own way.
///
/// Lossy means collisions: `weekly-numbers` and `weekly_numbers` are distinct
/// commands that both want `/weekly_numbers`. Callers detect that the same way
/// the MCP catalog does.
///
/// Over-length is an error rather than a truncation. A truncated name produces
/// a menu entry that resolves to nothing, which is a worse outcome than a
/// `doctor` line telling you to rename the file.
pub fn telegram_name(name: &str) -> Result<String> {
    let sanitized: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        return Err(Error::InvalidManifest(format!(
            "command '{name}': name has no characters Telegram accepts"
        )));
    }
    if sanitized.chars().count() > TELEGRAM_NAME_MAX {
        return Err(Error::InvalidManifest(format!(
            "command '{name}': Telegram caps a command name at {TELEGRAM_NAME_MAX} characters \
             (this one is {}) — rename the file",
            sanitized.chars().count()
        )));
    }
    Ok(sanitized)
}

/// Split an inbound `/name args` into its parts.
///
/// Purely lexical: it recognizes Telegram's wire syntax and nothing else. It
/// does not know which commands exist, and deliberately so — resolving a name
/// to a body is the dispatcher's job, not the daemon's.
///
/// `/cmd@somebot arg` is the form Telegram uses in groups; everything from the
/// `@` to the first space is dropped without checking which bot was addressed.
/// dotagent's allowlist is per-user rather than per-chat, so the 1:1 DM is the
/// case that matters.
pub fn parse_invocation(text: &str) -> Option<(String, String)> {
    let text = text.trim_start();
    let rest = text.strip_prefix('/')?;

    let (head, args) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    // `/@bot` and a bare `/` are not invocations.
    let head = head.split('@').next().unwrap_or("");
    if head.is_empty() {
        return None;
    }
    Some((head.to_string(), args.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "---\ndescription: Does a thing.\n---\n\nThe prompt.\n";

    #[test]
    fn name_comes_from_the_path_and_description_from_the_header() {
        let c = CommandManifest::parse(MINIMAL, "commit-message").unwrap();
        assert_eq!(c.name, "commit-message");
        assert_eq!(c.description, "Does a thing.");
        assert_eq!(c.body, "The prompt.");
        assert!(c.argument_hint.is_none());
    }

    #[test]
    fn a_real_claude_code_command_parses_unchanged() {
        // Verbatim shape from ~/.claude/commands: fields dotagent has no
        // opinion about must not be a reason to reject the file.
        let raw = "---\n\
description: Code review with a house voice\n\
argument-hint: \"[path] [--post] [--dry]\"\n\
allowed-tools: Bash(git:*), Read, Grep, Task\n\
subtask: true\n\
model: claude-opus-5\n\
---\n\
Review $ARGUMENTS.\n";
        let c = CommandManifest::parse(raw, "code-review").unwrap();
        assert_eq!(c.description, "Code review with a house voice");
        assert_eq!(c.argument_hint.as_deref(), Some("[path] [--post] [--dry]"));
        assert_eq!(
            c.allowed_tools.as_deref(),
            Some("Bash(git:*), Read, Grep, Task")
        );
    }

    #[test]
    fn missing_description_is_rejected() {
        let err = CommandManifest::parse("---\nname: x\n---\nBody.\n", "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("description"), "{err}");
    }

    #[test]
    fn empty_body_is_rejected() {
        let err = CommandManifest::parse("---\ndescription: d\n---\n\n", "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("body"), "{err}");
    }

    #[test]
    fn a_name_telegram_would_refuse_fails_validation() {
        // 33 characters: one past the cap, caught at parse rather than by the
        // Bot API rejecting the whole batch later.
        let long = "a".repeat(33);
        let err = CommandManifest::parse(MINIMAL, &long)
            .unwrap_err()
            .to_string();
        assert!(err.contains("32"), "{err}");
    }

    // --- argument substitution ---

    fn cmd(body: &str) -> CommandManifest {
        CommandManifest::parse(&format!("---\ndescription: d\n---\n{body}\n"), "c").unwrap()
    }

    #[test]
    fn arguments_placeholder_takes_the_whole_tail() {
        assert_eq!(
            cmd("Review $ARGUMENTS.").render("src/ --staged"),
            "Review src/ --staged."
        );
    }

    #[test]
    fn positional_placeholders_are_one_indexed() {
        assert_eq!(cmd("$1 then $2").render("alpha beta"), "alpha then beta");
    }

    #[test]
    fn a_missing_position_renders_empty_rather_than_leaking_the_placeholder() {
        // A literal "$2" reaching a model reads as an instruction about a
        // variable, which is worse than an absent argument.
        assert_eq!(cmd("[$1][$2]").render("only"), "[only][]");
    }

    #[test]
    fn double_digit_positions_are_not_corrupted_by_single_digit_ones() {
        // The bug successive replaces would introduce: `$1` matching inside
        // `$10` and leaving a stray digit behind.
        let args = "a b c d e f g h i j";
        assert_eq!(cmd("$10 and $1").render(args), "j and a");
    }

    #[test]
    fn a_dollar_not_followed_by_a_placeholder_survives() {
        // `$x` and a trailing `$` are not placeholders, and `$ARGUMENTSX` is a
        // longer word rather than the keyword.
        assert_eq!(
            cmd("100$ or $x or $ARGUMENTSX").render(""),
            "100$ or $x or $ARGUMENTSX"
        );
    }

    #[test]
    fn a_dollar_amount_is_read_as_a_position_and_this_is_the_known_cost() {
        // `$5.00` loses its 5. The alternative — treating `$N` as literal when
        // position N was not supplied — would make the meaning of a body depend
        // on what the caller typed, so `[$1][$2]` with one argument would
        // render a literal `$2` that a model reads as an instruction.
        // Predictable beats clever; a body needing a dollar sign writes
        // "USD 5.00".
        assert_eq!(cmd("costs $5.00").render(""), "costs .00");
    }

    #[test]
    fn arguments_are_appended_when_the_body_declares_no_placeholder() {
        // The silent-loss case: someone types `/simplify src/foo.rs` and the
        // body never mentions $ARGUMENTS.
        let out = cmd("Simplify the code.").render("src/foo.rs");
        assert!(out.starts_with("Simplify the code."), "{out}");
        assert!(out.contains("Arguments: src/foo.rs"), "{out}");
    }

    #[test]
    fn no_placeholder_and_no_arguments_appends_nothing() {
        assert_eq!(cmd("Just do it.").render(""), "Just do it.");
        assert_eq!(cmd("Just do it.").render("   "), "Just do it.");
    }

    #[test]
    fn a_body_with_a_placeholder_and_no_arguments_renders_empty() {
        assert_eq!(cmd("Review $ARGUMENTS.").render(""), "Review .");
    }

    // --- telegram names ---

    #[test]
    fn hyphens_become_underscores_and_case_is_folded() {
        assert_eq!(telegram_name("commit-message").unwrap(), "commit_message");
        assert_eq!(telegram_name("PR-Description").unwrap(), "pr_description");
        assert_eq!(telegram_name("git:status").unwrap(), "git_status");
    }

    #[test]
    fn the_telegram_mapping_is_lossy_so_callers_must_dedupe() {
        // The collision the MCP mapping does not have: `-` and `_` are distinct
        // there, identical here.
        assert_eq!(
            telegram_name("weekly-numbers").unwrap(),
            telegram_name("weekly_numbers").unwrap()
        );
    }

    #[test]
    fn an_over_long_name_is_refused_rather_than_truncated() {
        assert!(telegram_name(&"a".repeat(32)).is_ok());
        assert!(telegram_name(&"a".repeat(33)).is_err());
    }

    #[test]
    fn a_name_with_nothing_telegram_accepts_is_refused() {
        // Sanitizing would produce "___", a name that says nothing and would
        // collide with every other all-punctuation name.
        assert!(telegram_name("").is_err());
        assert!(telegram_name("   ").is_err());
    }

    #[test]
    fn non_ascii_is_folded_rather_than_dropped() {
        // Deliberate: "revisão" becoming "revis_o" is ugly but reachable, and
        // it keeps the mapping total. A dropped character would silently
        // produce a different name than the file suggests.
        assert_eq!(telegram_name("revisão").unwrap(), "revis_o");
    }

    // --- invocation parsing ---

    #[test]
    fn parses_a_bare_command_and_its_arguments() {
        assert_eq!(
            parse_invocation("/weekly_numbers"),
            Some(("weekly_numbers".into(), String::new()))
        );
        assert_eq!(
            parse_invocation("/review src/ --staged"),
            Some(("review".into(), "src/ --staged".into()))
        );
    }

    #[test]
    fn the_group_form_with_a_bot_suffix_is_accepted() {
        assert_eq!(
            parse_invocation("/review@my_bot src/"),
            Some(("review".into(), "src/".into()))
        );
    }

    #[test]
    fn leading_whitespace_does_not_hide_a_command() {
        assert_eq!(
            parse_invocation("  /help"),
            Some(("help".into(), String::new()))
        );
    }

    #[test]
    fn plain_prose_is_not_an_invocation() {
        assert!(parse_invocation("how's disk on the laptop?").is_none());
        // A slash mid-sentence is a path, not a command.
        assert!(parse_invocation("check src/main.rs").is_none());
    }

    #[test]
    fn a_bare_slash_is_not_an_invocation() {
        assert!(parse_invocation("/").is_none());
        assert!(parse_invocation("/@bot").is_none());
        assert!(parse_invocation("/ spaced").is_none());
    }
}
