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
///
/// Dates in the statement are linked on the way in, so `TODO até 2026-08-24`
/// files itself under that day and shows up in its journal's backlinks. Doing
/// it here rather than asking a model to write `[[…]]` is the difference
/// between a rule and a hope: the link either always happens or it depends on
/// whichever model wrote the fact remembering the syntax.
pub fn block_text(text: &str, topics: &[String]) -> String {
    let text = link_dates(text.trim());
    if topics.is_empty() {
        return text;
    }
    let refs: Vec<String> = topics.iter().map(|s| format!("[[{s}]]")).collect();
    format!("{text} {}", refs.join(" "))
}

/// Wrap every bare `YYYY-MM-DD` in `[[…]]`.
///
/// Only ISO, because that is the slug outl gives a daily page — a link in any
/// other spelling would point at a page that does not exist. A date already
/// inside brackets is left alone, so rendering a fact twice does not nest
/// them.
fn link_dates(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        // An existing link is copied whole. Stepping over it is also what
        // makes a date inside one safe: the scan never stands in the middle
        // of `[[…]]` when it decides.
        if b[i..].starts_with(b"[[") {
            if let Some(rel) = text[i + 2..].find("]]") {
                let end = i + 2 + rel + 2;
                out.push_str(&text[i..end]);
                i = end;
                continue;
            }
        }
        if is_iso_date_at(b, i) {
            out.push_str("[[");
            out.push_str(&text[i..i + 10]);
            out.push_str("]]");
            i += 10;
            continue;
        }
        // Walk by char so a multi-byte character is copied whole.
        let ch = text[i..].chars().next().expect("index is a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Is there a `YYYY-MM-DD` starting exactly at `i`, standing on its own?
///
/// Bounded on both sides so a version string (`1.2026-08-24`) or a longer run
/// of digits is not mistaken for a date.
fn is_iso_date_at(b: &[u8], i: usize) -> bool {
    if i + 10 > b.len() || !is_iso_shape(&b[i..i + 10]) {
        return false;
    }
    // A dot *before* means a version (`v1.2026-08-24`); a dot *after* is
    // usually the end of a sentence, so it only disqualifies when a digit
    // follows it.
    let before_ok = i == 0 || !matches!(b[i - 1], b'0'..=b'9' | b'-' | b'.' | b'[');
    let after = i + 10;
    let after_ok = match b.get(after) {
        None => true,
        Some(b'0'..=b'9') | Some(b'-') | Some(b']') => false,
        Some(b'.') => !matches!(b.get(after + 1), Some(b'0'..=b'9')),
        Some(_) => true,
    };
    before_ok && after_ok
}

/// Split a block's text into (statement, topics), undoing [`block_text`].
///
/// Links are collected wherever they appear, but only the **trailing** run is
/// removed from the statement — those are the ones [`block_text`] appended. A
/// link inside the sentence is part of the sentence: a fact edited by hand in
/// the desktop app reads "o [[dotagent]] roda no launchd", and a date written
/// as "TODO até [[2026-08-24]]" carries the date in the prose. Stripping
/// those left `"o roda no launchd"` and `"TODO até : ..."` — the link target
/// survived as a topic while the sentence lost the word it was built around.
pub fn split_links(text: &str) -> (String, Vec<String>) {
    let mut topics: Vec<String> = Vec::new();

    // Peel the trailing run first: `… [[a]] [[b]]` with nothing but spaces
    // after it. Those were appended, so they leave the statement entirely.
    let mut head = text.trim_end();
    let mut trailing: Vec<String> = Vec::new();
    while head.ends_with("]]") {
        let Some(start) = head.rfind("[[") else { break };
        let link = head[start + 2..head.len() - 2].trim();
        if link.contains("[[") || link.contains("]]") {
            break;
        }
        // A day is never one of the appended topics — `block_text` appends
        // topics, and a date got its brackets from `link_dates` while it sat
        // in the prose. Peeling it would drop the date from a fact that ends
        // in one ("prazo final 2026-08-24"), and the statement left behind no
        // longer matches its own `dedup_key`, so restating that fact would
        // file a copy instead of reinforcing it.
        if is_journal_slug(link) {
            break;
        }
        trailing.push(link.to_string());
        head = head[..start].trim_end();
    }
    trailing.reverse();

    // Whatever links remain are inline: keep their text, keep them as topics.
    let mut statement = String::with_capacity(head.len());
    let mut rest = head;
    while let Some(start) = rest.find("[[") {
        let Some(end_rel) = rest[start + 2..].find("]]") else {
            break;
        };
        let end = start + 2 + end_rel;
        let link = rest[start + 2..end].trim();
        statement.push_str(&rest[..start]);
        statement.push_str(link);
        push_unique(&mut topics, link);
        rest = &rest[end + 2..];
    }
    statement.push_str(rest);

    for link in trailing {
        push_unique(&mut topics, &link);
    }
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
    is_iso_shape(slug.as_bytes())
}

/// Exactly ten bytes reading `YYYY-MM-DD`.
///
/// One definition, because the two callers must never disagree about what a
/// date is: [`is_journal_slug`] decides whether a link points at a day rather
/// than a topic, and [`is_iso_date_at`] decides whether to write that link in
/// the first place.
fn is_iso_shape(b: &[u8]) -> bool {
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Append a link, ignoring blanks and repeats.
fn push_unique(topics: &mut Vec<String>, link: &str) {
    if !link.is_empty() && !topics.iter().any(|t| t == link) {
        topics.push(link.to_string());
    }
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
        // A human editing the workspace by hand writes it this way. The link
        // is part of the sentence, so its text stays: dropping it used to
        // leave "o roda no desde março", which is not a fact anyone can read.
        let (statement, topics) = split_links("o [[dotagent]] roda no [[launchd]] desde março");
        assert_eq!(statement, "o dotagent roda no launchd desde março");
        assert_eq!(topics, vec!["dotagent", "launchd"]);
    }

    #[test]
    fn split_links_removes_only_the_trailing_run() {
        let (statement, topics) =
            split_links("TODO até [[2026-08-24]]: abrir issue [[pendencias]] [[dotagent]]");
        assert_eq!(statement, "TODO até 2026-08-24: abrir issue");
        assert_eq!(topics, vec!["2026-08-24", "pendencias", "dotagent"]);
    }

    #[test]
    fn a_fact_ending_in_a_date_keeps_it() {
        // Found in review: the trailing peel ate a date that closed the
        // sentence, so "prazo final 2026-08-24" came back as "prazo final"
        // and stopped matching its own dedup_key — every restatement filed a
        // copy instead of reinforcing.
        let rendered = block_text("prazo final 2026-08-24", &[]);
        assert_eq!(rendered, "prazo final [[2026-08-24]]");
        let (statement, topics) = split_links(&rendered);
        assert_eq!(statement, "prazo final 2026-08-24");
        assert_eq!(topics, vec!["2026-08-24"]);
        assert_eq!(dedup_key("prazo final 2026-08-24"), dedup_key(&statement));
    }

    #[test]
    fn a_dated_fact_with_topics_still_peels_the_topics() {
        let rendered = block_text("vence em 2026-08-24", &["pendencias".into()]);
        assert_eq!(rendered, "vence em [[2026-08-24]] [[pendencias]]");
        let (statement, topics) = split_links(&rendered);
        assert_eq!(statement, "vence em 2026-08-24");
        assert!(topics.contains(&"pendencias".to_string()));
    }

    #[test]
    fn a_fact_that_is_only_links_keeps_its_statement_empty() {
        let (statement, topics) = split_links("[[dotagent]] [[outl]]");
        assert_eq!(statement, "");
        assert_eq!(topics, vec!["dotagent", "outl"]);
    }

    #[test]
    fn a_date_in_the_statement_becomes_a_link() {
        assert_eq!(
            block_text("TODO até 2026-08-24: abrir issue", &[]),
            "TODO até [[2026-08-24]]: abrir issue"
        );
    }

    #[test]
    fn every_date_in_a_statement_is_linked() {
        assert_eq!(
            block_text("de 2026-01-01 a 2026-12-31", &[]),
            "de [[2026-01-01]] a [[2026-12-31]]"
        );
    }

    #[test]
    fn a_date_already_linked_is_not_nested() {
        assert_eq!(
            block_text("TODO até [[2026-08-24]]: x", &[]),
            "TODO até [[2026-08-24]]: x"
        );
    }

    #[test]
    fn linking_dates_is_idempotent_across_renders() {
        let once = block_text("prazo 2026-08-24", &[]);
        assert_eq!(block_text(&once, &[]), once);
    }

    #[test]
    fn a_number_that_merely_looks_like_a_date_is_left_alone() {
        assert_eq!(block_text("build 12026-08-244", &[]), "build 12026-08-244");
        assert_eq!(block_text("v1.2026-08-24", &[]), "v1.2026-08-24");
        assert_eq!(block_text("2026-08-241", &[]), "2026-08-241");
        assert_eq!(block_text("rev 2026-08-24.3", &[]), "rev 2026-08-24.3");
    }

    #[test]
    fn a_date_survives_next_to_punctuation() {
        assert_eq!(
            block_text("prazo: 2026-08-24.", &[]),
            "prazo: [[2026-08-24]]."
        );
        assert_eq!(block_text("(2026-08-24)", &[]), "([[2026-08-24]])");
    }

    #[test]
    fn linking_a_date_does_not_corrupt_multibyte_text() {
        assert_eq!(
            block_text("reunião até 2026-08-24 com José", &[]),
            "reunião até [[2026-08-24]] com José"
        );
    }

    #[test]
    fn a_dated_fact_keeps_its_topics_after_the_statement() {
        assert_eq!(
            block_text("TODO até 2026-08-24: x", &["pendencias".into()]),
            "TODO até [[2026-08-24]]: x [[pendencias]]"
        );
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
