//! Roam → Outl text translation.
//!
//! The agents emit text in Roam dialect (the legacy primary backend).
//! Outl renders most of CommonMark identically but a few Roam constructs
//! become noise when written verbatim. This module rewrites those
//! constructs into the closest Outl equivalent.
//!
//! Conversions covered:
//!   `{{[[TODO]]}}`              → `TODO` (Outl's literal-prefix task syntax)
//!   `{{[[DONE]]}}`              → `DONE`
//!   `[[April 22nd, 2026]]`      → `[[2026-04-22]]`     (Roam ordinal daily)
//!   `{{[[query]]: …}}`          → `{{query: …}}`
//!   `{{[[embed]]: ((uid))}}`    → dropped (no UID mapping across backends)
//!
//! Not converted (left verbatim; future work if/when needed):
//!   `((uid))` block refs — Roam UIDs don't map to Outl block handles.
//!   `^^highlight^^` — Outl docs don't list a highlight syntax.
//!
//! Applied to every text emitted by the publish pipeline (root, section
//! headers, leaf children) **before** the `outl_batch` call.

use chrono::{Datelike, NaiveDate};
use regex::Regex;
use std::sync::OnceLock;

/// Apply every Roam→Outl rewrite. Idempotent — running it twice on text
/// that's already Outl-flavored is a no-op.
pub fn roam_to_outl(input: &str) -> String {
    let s = rewrite_todo(input);
    let s = rewrite_done(&s);
    let s = rewrite_query(&s);
    let s = drop_embed(&s);
    rewrite_ordinal_daily(&s)
}

fn rewrite_todo(s: &str) -> String {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx = RX.get_or_init(|| Regex::new(r"\{\{\[\[TODO\]\]\}\}").unwrap());
    rx.replace_all(s, "TODO").into_owned()
}

fn rewrite_done(s: &str) -> String {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx = RX.get_or_init(|| Regex::new(r"\{\{\[\[DONE\]\]\}\}").unwrap());
    rx.replace_all(s, "DONE").into_owned()
}

fn rewrite_query(s: &str) -> String {
    static RX: OnceLock<Regex> = OnceLock::new();
    // `{{[[query]]: …}}` → `{{query: …}}`. Body is opaque (parsed in
    // Outl phase 3); we only strip the `[[ ]]` page-wrap.
    let rx = RX.get_or_init(|| Regex::new(r"\{\{\[\[query\]\]:\s*").unwrap());
    rx.replace_all(s, "{{query: ").into_owned()
}

fn drop_embed(s: &str) -> String {
    static RX: OnceLock<Regex> = OnceLock::new();
    // `{{[[embed]]: ((9chars))}}` — drop entirely. We have no ID mapping
    // between Roam UIDs and Outl block handles; leaving the literal text
    // produces a confusing "broken-ref" rendering on the Outl side.
    let rx = RX.get_or_init(|| Regex::new(r"\{\{\[\[embed\]\]:\s*\(\([^)]*\)\)\}\}\s*").unwrap());
    rx.replace_all(s, "").into_owned()
}

/// Rewrite `[[<Month> <D><suffix>, YYYY]]` → `[[YYYY-MM-DD]]`. Only inside
/// `[[ ... ]]` wikilinks — other prose like "April 22nd, 2026 was…" is
/// left alone, by design.
fn rewrite_ordinal_daily(s: &str) -> String {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx = RX.get_or_init(|| {
        Regex::new(
            r"\[\[(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{1,2})(?:st|nd|rd|th),\s+(\d{4})\]\]",
        )
        .unwrap()
    });
    rx.replace_all(s, |caps: &regex::Captures| {
        let month_name = &caps[1];
        let day: u32 = caps[2].parse().unwrap_or(0);
        let year: i32 = caps[3].parse().unwrap_or(0);
        let month = month_to_num(month_name);
        if let (Some(m), Some(nd)) = (
            month,
            NaiveDate::from_ymd_opt(year, month.unwrap_or(0), day),
        ) {
            let _ = m; // silence unused warning when guard fails
            format!("[[{:04}-{:02}-{:02}]]", nd.year(), nd.month(), nd.day())
        } else {
            // Unparseable — return the original wikilink untouched.
            caps[0].to_string()
        }
    })
    .into_owned()
}

fn month_to_num(s: &str) -> Option<u32> {
    Some(match s {
        "January" => 1,
        "February" => 2,
        "March" => 3,
        "April" => 4,
        "May" => 5,
        "June" => 6,
        "July" => 7,
        "August" => 8,
        "September" => 9,
        "October" => 10,
        "November" => 11,
        "December" => 12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_becomes_literal_prefix() {
        assert_eq!(
            roam_to_outl("{{[[TODO]]}} revisar antes de postar"),
            "TODO revisar antes de postar"
        );
    }

    #[test]
    fn done_becomes_literal_prefix() {
        assert_eq!(roam_to_outl("{{[[DONE]]}} feito"), "DONE feito");
    }

    #[test]
    fn ordinal_daily_to_iso() {
        assert_eq!(
            roam_to_outl("período: [[April 22nd, 2026]] → [[May 1st, 2026]]"),
            "período: [[2026-04-22]] → [[2026-05-01]]"
        );
    }

    #[test]
    fn ordinal_prose_left_alone() {
        // Only the wikilink form is rewritten — bare prose stays.
        let s = "April 22nd, 2026 was the date";
        assert_eq!(roam_to_outl(s), s);
    }

    #[test]
    fn query_wrap_stripped() {
        assert_eq!(
            roam_to_outl("{{[[query]]: {and: [[foo]] [[bar]]}}}"),
            "{{query: {and: [[foo]] [[bar]]}}}"
        );
    }

    #[test]
    fn embed_dropped() {
        assert_eq!(
            roam_to_outl("contexto: {{[[embed]]: ((abc-def-gh))}} segue"),
            "contexto: segue"
        );
    }

    #[test]
    fn page_links_and_tags_untouched() {
        let s = "trabalhei com [[buser/tech/data]] hoje #FinOps";
        assert_eq!(roam_to_outl(s), s);
    }

    #[test]
    fn block_refs_left_verbatim() {
        // No mapping exists — better visible than silently dropped.
        let s = "ref: ((abc-def-gh))";
        assert_eq!(roam_to_outl(s), s);
    }

    #[test]
    fn idempotent_on_outl_text() {
        let s = "TODO write the report · DONE shipped · [[2026-04-22]] daily";
        assert_eq!(roam_to_outl(s), s);
    }

    #[test]
    fn invalid_date_left_alone() {
        // Feb 30 doesn't exist — keep the original wikilink.
        let s = "[[February 30th, 2026]]";
        assert_eq!(roam_to_outl(s), s);
    }
}
