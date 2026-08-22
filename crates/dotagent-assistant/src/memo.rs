//! Memory hooks: `MEMO:` capture (write path) and recall-block assembly
//! (read path).
//!
//! The model ends a reply with durable facts as `MEMO: <fact> | topics: a, b`
//! lines. The daemon strips them before the reply reaches any sink (the chat
//! never sees the bookkeeping) and flushes them to the memory store. Parsing
//! is lenient on purpose: a malformed memo line is left in the reply as
//! ordinary text rather than silently dropped, so the author notices it.
//! Before the next run, [`assemble_context_block`] renders the bounded
//! recall block from the stored facts — recall is capped by bytes, not by
//! truncating a fact mid-sentence.

/// One captured fact, as the model wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedMemo {
    pub text: String,
    pub topics: Vec<String>,
}

/// Split a reply into (clean reply, captured memos).
///
/// A memo line is `MEMO: <fact>` with an optional ` | topics: a, b` suffix.
/// Lines that start with `MEMO:` but fail to produce a non-empty fact are
/// kept in the reply. Trailing whitespace-only lines left behind by the
/// strip are trimmed from the reply's end.
pub fn strip_memos(reply: &str) -> (String, Vec<CapturedMemo>) {
    let mut memos = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for line in reply.lines() {
        match parse_memo_line(line) {
            Some(memo) => memos.push(memo),
            None => kept.push(line),
        }
    }
    let mut clean = kept.join("\n");
    clean = clean.trim_end().to_string();
    (clean, memos)
}

fn parse_memo_line(line: &str) -> Option<CapturedMemo> {
    let rest = line.trim().strip_prefix("MEMO:")?;
    let (fact, topics) = match rest.split_once('|') {
        Some((fact, tail)) => {
            let tail = tail.trim();
            let list = tail.strip_prefix("topics:")?;
            let topics = list
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            (fact, topics)
        }
        None => (rest, Vec::new()),
    };
    let text = fact.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(CapturedMemo { text, topics })
}

/// What recall found: the facts, and the topic vocabulary they live under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryContext {
    /// Facts, already ranked by the caller.
    pub entries: Vec<CapturedMemo>,
    /// Topics that already exist in the store.
    pub topics: Vec<String>,
}

/// Assemble the bounded recall block injected before a run.
///
/// Facts are rendered in the order given (the caller ranks them) and whole:
/// a fact that would push the block past `max_bytes` stops the assembly — a
/// truncated fact is worse than a missing one. An empty result means "inject
/// nothing"; the header never travels alone.
///
/// The block closes with the topics already in use, so the model tags a new
/// fact with a name the store already knows. Without it the same subject
/// accumulates under `reuniao`, `reunioes` and `agenda`, and each fragment
/// looks like the whole picture to whoever reads one. The vocabulary is the
/// first thing dropped when the budget runs out — a fact that does not fit
/// is lost context, a vocabulary that does not fit is only a missed hint.
pub fn assemble_context_block(context: &MemoryContext, max_bytes: usize) -> String {
    const HEADER: &str = "## Memory";
    const TOPICS_PREFIX: &str = "Topics in use: ";

    let mut bullets: Vec<String> = Vec::new();
    let mut total = HEADER.len();
    for entry in &context.entries {
        let text = collapse(&entry.text);
        if text.is_empty() {
            continue;
        }
        let bullet = match render_topics(&entry.topics) {
            Some(topics) => format!("- {text} [{topics}]"),
            None => format!("- {text}"),
        };
        // +1 accounts for the newline separating the bullet from what
        // precedes it.
        if total + 1 + bullet.len() > max_bytes {
            break;
        }
        total += 1 + bullet.len();
        bullets.push(bullet);
    }
    if bullets.is_empty() {
        return String::new();
    }

    let mut block = format!("{HEADER}\n{}", bullets.join("\n"));
    if let Some(topics) = render_topics(&context.topics) {
        let line = format!("{TOPICS_PREFIX}{topics}");
        // +2 for the blank line that separates it from the facts.
        if total + 2 + line.len() <= max_bytes {
            block.push_str("\n\n");
            block.push_str(&line);
        }
    }
    block
}

/// Topics as a comma-separated list, or `None` when there are none worth
/// rendering.
fn render_topics(topics: &[String]) -> Option<String> {
    let list: Vec<&str> = topics
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    (!list.is_empty()).then(|| list.join(", "))
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_single_memo_with_topics() {
        let (clean, memos) = strip_memos(
            "here is your answer\nMEMO: prefers meetings after 14h | topics: avelino, calendar",
        );
        assert_eq!(clean, "here is your answer");
        assert_eq!(
            memos,
            vec![CapturedMemo {
                text: "prefers meetings after 14h".into(),
                topics: vec!["avelino".into(), "calendar".into()],
            }]
        );
    }

    #[test]
    fn strips_multiple_memos_in_order() {
        let (clean, memos) =
            strip_memos("answer\nMEMO: fact one | topics: a\nMEMO: fact two | topics: b, c");
        assert_eq!(clean, "answer");
        assert_eq!(memos.len(), 2);
        assert_eq!(memos[1].topics, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn memo_without_topics_is_captured_with_empty_topics() {
        let (_, memos) = strip_memos("answer\nMEMO: standalone fact");
        assert_eq!(
            memos,
            vec![CapturedMemo {
                text: "standalone fact".into(),
                topics: vec![],
            }]
        );
    }

    #[test]
    fn reply_with_no_memos_is_trimmed_only() {
        let (clean, memos) = strip_memos("line one\nline two\n");
        assert_eq!(clean, "line one\nline two");
        assert!(memos.is_empty());
    }

    #[test]
    fn malformed_memo_stays_in_reply() {
        // No fact after the colon: keep the line visible so the model's
        // mistake surfaces instead of vanishing.
        let (clean, memos) = strip_memos("answer\nMEMO:   \nMEMO:");
        assert!(memos.is_empty());
        assert!(clean.contains("MEMO:"));
    }

    #[test]
    fn topics_suffix_without_keyword_keeps_line() {
        let (clean, memos) = strip_memos("answer\nMEMO: fact | notes: x");
        assert!(memos.is_empty());
        assert!(clean.contains("MEMO: fact | notes: x"));
    }

    #[test]
    fn memo_marker_mid_line_is_not_a_memo() {
        let (clean, memos) = strip_memos("he said MEMO: something");
        assert!(memos.is_empty());
        assert_eq!(clean, "he said MEMO: something");
    }

    #[test]
    fn interior_blank_lines_are_preserved() {
        let (clean, _) = strip_memos("para one\n\npara two\nMEMO: fact | topics: t");
        assert_eq!(clean, "para one\n\npara two");
    }

    #[test]
    fn empty_topics_entries_are_dropped() {
        let (_, memos) = strip_memos("a\nMEMO: fact | topics: , x ,,");
        assert_eq!(memos[0].topics, vec!["x".to_string()]);
    }

    fn ctx(entries: &[(&str, &[&str])], topics: &[&str]) -> MemoryContext {
        MemoryContext {
            entries: entries
                .iter()
                .map(|(text, topics)| CapturedMemo {
                    text: (*text).into(),
                    topics: topics.iter().map(|t| (*t).to_string()).collect(),
                })
                .collect(),
            topics: topics.iter().map(|t| (*t).to_string()).collect(),
        }
    }

    #[test]
    fn context_block_renders_facts_under_header() {
        let block =
            assemble_context_block(&ctx(&[("fact one", &[]), ("fact two", &[])], &[]), 10_000);
        assert_eq!(block, "## Memory\n- fact one\n- fact two");
    }

    #[test]
    fn context_block_tags_facts_with_their_topics() {
        let block = assemble_context_block(
            &ctx(&[("prefers 14h", &["agenda", "reuniao"])], &[]),
            10_000,
        );
        assert_eq!(block, "## Memory\n- prefers 14h [agenda, reuniao]");
    }

    #[test]
    fn context_block_closes_with_the_topic_vocabulary() {
        // So the model reuses `reuniao` instead of coining `reunioes`.
        let block =
            assemble_context_block(&ctx(&[("a fact", &[])], &["agenda", "reuniao"]), 10_000);
        assert_eq!(
            block,
            "## Memory\n- a fact\n\nTopics in use: agenda, reuniao"
        );
    }

    #[test]
    fn context_block_without_entries_is_empty() {
        // Not even the vocabulary travels alone: a header with no facts is
        // prompt weight that teaches the model nothing.
        assert_eq!(assemble_context_block(&ctx(&[], &["agenda"]), 1_000), "");
    }

    #[test]
    fn context_block_drops_whole_facts_at_the_budget() {
        // "## Memory" (9) + "\n- fact one" (11) = 20 bytes fit; the second
        // fact would need 11 more (31 > 25) and is dropped whole.
        let block = assemble_context_block(&ctx(&[("fact one", &[]), ("fact two", &[])], &[]), 25);
        assert_eq!(block, "## Memory\n- fact one");
    }

    #[test]
    fn context_block_drops_the_vocabulary_before_dropping_a_fact() {
        // A fact that does not fit is lost context; a vocabulary that does
        // not fit is a missed hint.
        let block = assemble_context_block(&ctx(&[("fact one", &[])], &["agenda", "reuniao"]), 25);
        assert_eq!(block, "## Memory\n- fact one");
    }

    #[test]
    fn context_block_budget_below_header_is_empty() {
        assert_eq!(assemble_context_block(&ctx(&[("fact", &[])], &[]), 5), "");
    }

    #[test]
    fn context_block_flattens_multiline_facts_to_one_bullet() {
        let block = assemble_context_block(&ctx(&[("line one\nline two", &[])], &[]), 1_000);
        assert_eq!(block, "## Memory\n- line one line two");
    }

    #[test]
    fn context_block_skips_blank_facts() {
        let block = assemble_context_block(&ctx(&[("   ", &[]), ("real fact", &[])], &[]), 1_000);
        assert_eq!(block, "## Memory\n- real fact");
    }

    #[test]
    fn context_block_ignores_blank_topics() {
        let block = assemble_context_block(&ctx(&[("a fact", &["  ", "agenda"])], &["  "]), 1_000);
        assert_eq!(block, "## Memory\n- a fact [agenda]");
    }
}
