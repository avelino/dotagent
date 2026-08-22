//! The shape of one remembered fact, and the text conventions around it.
//!
//! A fact is a block whose text carries the statement and its `[[topic]]`
//! links, with everything else hanging off it as outl block properties:
//!
//! ```text
//! - prefere reunião depois das 14h [[reuniao]] [[agenda]]
//!   agent:: telegram-assistant
//!   at:: 2026-08-21T14:03:00-03:00
//!   seen:: 2
//!   source:: telegram
//! ```
//!
//! Properties rather than a serialized blob in the text, because outl parses
//! `key:: value` natively: the desktop app shows them as properties, they
//! round-trip through the `.md` projection, and the fact stays one readable
//! sentence for the human who opens the workspace. Properties rather than
//! child blocks for the same reason — a child block is another fact, and
//! recall would surface `agent:: telegram-assistant` as something the
//! assistant knows.

use std::collections::BTreeSet;

/// Which agent wrote the fact, through which door.
///
/// Recorded because a memory store you cannot audit is one you stop
/// trusting: when the assistant states something wrong, the first question
/// is where it came from, and the second is what else that run wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// Agent name, as discovery knows it.
    pub agent: Option<String>,
    /// Trigger source (`telegram`, `local`, `schedule`, ...).
    pub source: Option<String>,
    /// Conversation session id, when the write came from one.
    pub session: Option<String>,
}

impl Provenance {
    /// Provenance for a fact written by `agent`.
    pub fn agent(name: impl Into<String>) -> Self {
        Self {
            agent: Some(name.into()),
            ..Self::default()
        }
    }

    /// Same, naming the door it came through.
    pub fn from(agent: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            agent: Some(agent.into()),
            source: Some(source.into()),
            session: None,
        }
    }

    /// Attach the conversation this came from.
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        let session = session.into();
        self.session = (!session.trim().is_empty()).then_some(session);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.agent.is_none() && self.source.is_none() && self.session.is_none()
    }
}

/// One remembered fact, as read back from the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// Block id, stringified. The handle for `forget` and `supersede`.
    pub id: String,
    /// Journal slug it lives in (`2026-08-04`), which is also its date.
    pub date: String,
    /// The statement, with `[[topic]]` links stripped out.
    pub text: String,
    /// Topics it was linked to.
    pub topics: Vec<String>,
    pub provenance: Provenance,
    /// How many times the fact was written. 1 for a fact stated once.
    pub seen: u32,
    /// Id of the fact that replaced this one, when it was superseded.
    pub superseded_by: Option<String>,
}

impl Memory {
    /// Whether a later fact replaced this one. Superseded facts stay in the
    /// workspace — the history of a changed preference is worth keeping —
    /// but they never come back from recall.
    pub fn is_superseded(&self) -> bool {
        self.superseded_by.is_some()
    }

    /// The fact as it reads in the journal, links included.
    pub fn to_block_text(&self) -> String {
        block_text(&self.text, &self.topics)
    }
}

/// Property keys. Namespaced by convention only — these read as ordinary
/// outl properties in the desktop app, which is the point.
pub mod keys {
    pub const AGENT: &str = "agent";
    pub const SOURCE: &str = "source";
    pub const SESSION: &str = "session";
    pub const SEEN: &str = "seen";
    pub const LAST_SEEN: &str = "last-seen";
    pub const SUPERSEDED_BY: &str = "superseded-by";
}

/// Render the block text for a fact: statement first, links after.
pub fn block_text(text: &str, topics: &[String]) -> String {
    let text = text.trim();
    if topics.is_empty() {
        return text.to_string();
    }
    let refs: Vec<String> = topics.iter().map(|s| format!("[[{s}]]")).collect();
    format!("{text} {}", refs.join(" "))
}

/// Split a block's text into (statement, topics), undoing [`block_text`].
///
/// Links anywhere in the text are collected, not just trailing ones: a fact
/// the human edited by hand in the desktop app may well read
/// "o [[dotagent]] roda no launchd", and dropping that topic on read would
/// make the graph disagree with the file.
pub fn split_links(text: &str) -> (String, Vec<String>) {
    let mut statement = String::with_capacity(text.len());
    let mut topics: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let Some(end_rel) = rest[start + 2..].find("]]") else {
            break;
        };
        let end = start + 2 + end_rel;
        let link = rest[start + 2..end].trim();
        statement.push_str(&rest[..start]);
        if !link.is_empty() && !topics.iter().any(|t| t == link) {
            topics.push(link.to_string());
        }
        rest = &rest[end + 2..];
    }
    statement.push_str(rest);
    (collapse_whitespace(&statement), topics)
}

/// Normalize topics into slugs, deduplicated and ordered.
///
/// Without this, "Roam Research" and "roam research" would be two
/// disconnected pages and each half of the subject would look incomplete.
pub fn normalize_topics(topics: &[String]) -> Vec<String> {
    topics
        .iter()
        .filter_map(|t| {
            let slug = slugify(t);
            (!slug.is_empty()).then_some(slug)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The key two facts must share to count as the same fact.
///
/// Lowercased, links dropped, punctuation dropped, whitespace collapsed. It
/// deliberately ignores how a fact was phrased on the surface: "Prefere
/// reunião depois das 14h." and "prefere reunião depois das 14h" are one
/// fact stated twice, and storing both is how a store fills with noise.
pub fn dedup_key(text: &str) -> String {
    let (statement, _) = split_links(text);
    let mut out = String::with_capacity(statement.len());
    let mut pending_space = false;
    for c in statement.chars() {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(c.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// Normalize a topic into an outl slug. `/` survives, because hierarchy is a
/// real thing in outl — `ops/cost-report` is a page path.
pub fn slugify(topic: &str) -> String {
    let mut out = String::with_capacity(topic.len());
    let mut last_dash = true; // leading dashes are dropped
    for c in topic.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_dash = false;
        } else if c == '/' {
            out.push('/');
            last_dash = true;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Human-readable title from a slug, for the topic page header.
pub fn title_for(slug: &str) -> String {
    slug.rsplit('/')
        .next()
        .unwrap_or(slug)
        .split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Journal slugs are ISO dates. Used to tell a day apart from a topic.
pub fn is_journal_slug(slug: &str) -> bool {
    let b = slug.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_text_appends_links_after_the_statement() {
        assert_eq!(
            block_text("prefere 14h", &["reuniao".into(), "agenda".into()]),
            "prefere 14h [[reuniao]] [[agenda]]"
        );
    }

    #[test]
    fn block_text_without_topics_is_the_statement() {
        assert_eq!(block_text("  fato  ", &[]), "fato");
    }

    #[test]
    fn split_links_round_trips_block_text() {
        let topics = vec!["agenda".to_string(), "reuniao".to_string()];
        let rendered = block_text("prefere 14h", &topics);
        let (statement, back) = split_links(&rendered);
        assert_eq!(statement, "prefere 14h");
        assert_eq!(back, topics);
    }

    #[test]
    fn split_links_collects_links_written_mid_sentence() {
        // A human editing the workspace by hand writes it this way.
        let (statement, topics) = split_links("o [[dotagent]] roda no [[launchd]] desde março");
        assert_eq!(statement, "o roda no desde março");
        assert_eq!(topics, vec!["dotagent", "launchd"]);
    }

    #[test]
    fn split_links_on_plain_text_returns_it_unchanged() {
        let (statement, topics) = split_links("nenhum link aqui");
        assert_eq!(statement, "nenhum link aqui");
        assert!(topics.is_empty());
    }

    #[test]
    fn split_links_tolerates_an_unclosed_link() {
        // Malformed markup must not swallow the rest of the fact.
        let (statement, topics) = split_links("fato [[aberto");
        assert_eq!(statement, "fato [[aberto");
        assert!(topics.is_empty());
    }

    #[test]
    fn split_links_deduplicates() {
        let (_, topics) = split_links("a [[x]] b [[x]]");
        assert_eq!(topics, vec!["x"]);
    }

    #[test]
    fn dedup_key_ignores_punctuation_case_and_links() {
        assert_eq!(
            dedup_key("Prefere reunião depois das 14h."),
            dedup_key("prefere reunião  depois das 14h [[agenda]]")
        );
    }

    #[test]
    fn dedup_key_keeps_genuinely_different_facts_apart() {
        assert_ne!(
            dedup_key("prefere reunião de manhã"),
            dedup_key("prefere reunião depois das 14h")
        );
    }

    #[test]
    fn dedup_key_of_only_punctuation_is_empty() {
        assert_eq!(dedup_key("... !!! ---"), "");
    }

    #[test]
    fn slugify_normalizes_so_the_graph_does_not_fragment() {
        assert_eq!(slugify("Roam Research"), "roam-research");
        assert_eq!(slugify("  Roam   Research  "), "roam-research");
        assert_eq!(slugify("ops/cost-report"), "ops/cost-report");
        assert_eq!(slugify("Reunião!!"), "reunião");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn normalize_topics_dedupes_and_orders() {
        assert_eq!(
            normalize_topics(&["Roam".into(), "agenda".into(), "roam".into(), "!!".into()]),
            vec!["agenda", "roam"]
        );
    }

    #[test]
    fn title_is_readable_and_keeps_only_the_leaf() {
        assert_eq!(title_for("roam-research"), "Roam Research");
        assert_eq!(title_for("ops/cost-report"), "Cost Report");
    }

    #[test]
    fn is_journal_slug_tells_days_from_topics() {
        assert!(is_journal_slug("2026-08-04"));
        assert!(!is_journal_slug("roam-research"));
        assert!(!is_journal_slug("2026-08-0x"));
        assert!(!is_journal_slug("2026-08"));
    }

    #[test]
    fn provenance_builders_skip_a_blank_session() {
        let p = Provenance::from("telegram-assistant", "telegram").with_session("   ");
        assert_eq!(p.agent.as_deref(), Some("telegram-assistant"));
        assert_eq!(p.source.as_deref(), Some("telegram"));
        assert_eq!(p.session, None);
        assert!(!p.is_empty());
    }

    #[test]
    fn default_provenance_is_empty() {
        assert!(Provenance::default().is_empty());
    }
}
