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
//!   `{{[[query]]: …}}` / `{{query: …}}`
//!                               → a fenced `query` block, in outl's line-oriented
//!                                 DSL, when every clause has an equivalent.
//!                                 Otherwise the original component is kept
//!                                 in a plain code block and a warning is
//!                                 logged. `{{query: …}}` is not executed.
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

/// Text plus the warnings a rewrite emitted. Warnings are logged by
/// [`roam_to_outl`]; tests assert on this so they don't have to capture
/// stderr.
struct Rewritten {
    text: String,
    warnings: Vec<String>,
}

/// Apply every Roam→Outl rewrite. Idempotent — running it twice on text
/// that's already Outl-flavored is a no-op.
pub fn roam_to_outl(input: &str) -> String {
    let rewritten = roam_to_outl_parts(input);
    for warning in &rewritten.warnings {
        // Plugin protocol: human-readable logs go to stderr. stdout is
        // the JSON response.
        eprintln!("sink-outl: warning: {warning}");
    }
    rewritten.text
}

fn roam_to_outl_parts(input: &str) -> Rewritten {
    let mut text = String::with_capacity(input.len());
    let mut warnings = Vec::new();
    let mut i = 0;
    while i < input.len() {
        // Fences are opaque. A preserved query lives inside one, and a
        // second pass must not wrap it again or rewrite the original text.
        if let Some(end) = fence_end(input, i) {
            text.push_str(&input[i..end]);
            i = end;
            continue;
        }
        if let Some(end) = query_end(input, i) {
            let raw = &input[i..end];
            match translate_query(raw) {
                Ok(body) => {
                    text.push_str("```query\n");
                    text.push_str(&body);
                    text.push_str("\n```");
                }
                Err(reason) => {
                    warnings.push(format!(
                        "Roam query has no outl equivalent ({reason}); kept the original in a code block"
                    ));
                    text.push_str("```\n");
                    text.push_str(raw);
                    text.push_str("\n```");
                }
            }
            i = end;
            continue;
        }
        let start = i;
        i += input[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        while i < input.len() && fence_end(input, i).is_none() && query_end(input, i).is_none() {
            i += input[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
        text.push_str(&rewrite_plain(&input[start..i]));
    }
    Rewritten { text, warnings }
}

fn rewrite_plain(s: &str) -> String {
    let s = rewrite_todo(s);
    let s = rewrite_done(&s);
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

/// A code fence starting at `i`, through its closing line (newline included
/// when there is one). An unclosed fence runs to the end of the string:
/// the body is code, not prose, so a query inside it is left alone.
fn fence_end(s: &str, i: usize) -> Option<usize> {
    if !s[i..].starts_with("```") {
        return None;
    }
    let after_open = i + 3;
    let Some(nl) = s[after_open..].find('\n') else {
        return Some(s.len());
    };
    let mut j = after_open + nl + 1;
    while j < s.len() {
        let line_end = s[j..].find('\n').map(|p| j + p).unwrap_or(s.len());
        if s[j..line_end].starts_with("```") {
            return Some(if line_end < s.len() {
                line_end + 1
            } else {
                line_end
            });
        }
        j = if line_end < s.len() {
            line_end + 1
        } else {
            s.len()
        };
    }
    Some(s.len())
}

/// End (exclusive) of a `{{[[query]]: …}}` or `{{query: …}}` component
/// starting at `i`. `None` when `i` is not the start of one, or the
/// braces never balance — a broken component stays prose.
fn query_end(s: &str, i: usize) -> Option<usize> {
    if !s[i..].starts_with('{') || !is_query_header(&s[i..]) {
        return None;
    }
    let mut depth = 0i32;
    for (idx, ch) in s[i..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + idx + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// `{{[[query]]:` / `{{query:` with the whitespace Roam's editor leaves in.
fn is_query_header(rest: &str) -> bool {
    let mut c = Cur { s: rest, i: 0 };
    if !c.eat("{{") {
        return false;
    }
    c.skip_ws();
    if c.eat("[[") {
        c.skip_ws();
        if !c.eat("query") {
            return false;
        }
        c.skip_ws();
        if !c.eat("]]") {
            return false;
        }
    } else if !c.eat("query") {
        return false;
    }
    c.skip_ws();
    c.eat(":")
}

/// Roam native query → body of an outl `query` fence.
///
/// outl runs one `key: value` directive per line, implicitly ANDed, with
/// `not-<key>` as the negation of that same directive (upstream
/// `docs/query.md` and `crates/outl-exec/src/runtimes/query/dsl.rs`).
/// There is no OR, no
/// boolean grouping, and no `page:` or calendar-range filter — `since:`
/// is a relative duration (`7d`, `2w`, `3m`) and `page:` is documented
/// as not implemented. A clause we cannot say is an error, and the
/// caller keeps the original component in a plain code block.
fn translate_query(raw: &str) -> Result<String, String> {
    let body = query_body(raw)?;
    let mut c = Cur { s: body, i: 0 };
    skip_title(&mut c)?;
    let mut filts = Vec::new();
    loop {
        c.skip_ws();
        if c.rest().is_empty() {
            break;
        }
        filts.extend(parse_clause(&mut c)?);
    }
    if filts.is_empty() {
        return Err("query has no conditions".into());
    }
    Ok(filts
        .iter()
        .map(Filt::render)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Inside of the outer `{{ … }}`, after the `query` header's colon.
fn query_body(raw: &str) -> Result<&str, String> {
    let Some(rest) = raw.strip_prefix("{{") else {
        return Err("not a query component".into());
    };
    let Some(inner) = rest.strip_suffix("}}") else {
        return Err("not a query component".into());
    };
    let mut c = Cur { s: inner, i: 0 };
    c.skip_ws();
    if c.eat("[[") {
        c.skip_ws();
        if !c.eat("query") {
            return Err("not a query component".into());
        }
        c.skip_ws();
        if !c.eat("]]") {
            return Err("not a query component".into());
        }
    } else if !c.eat("query") {
        return Err("not a query component".into());
    }
    c.skip_ws();
    if !c.eat(":") {
        return Err("not a query component".into());
    }
    Ok(&inner[c.i..])
}

#[derive(Clone, Copy)]
struct Cur<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Cur<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self, n: usize) {
        self.i += n;
    }

    fn bump_char(&mut self) {
        if let Some(ch) = self.peek() {
            self.i += ch.len_utf8();
        }
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump_char();
        }
    }

    fn starts_with(&self, pat: &str) -> bool {
        self.rest().starts_with(pat)
    }

    fn eat(&mut self, pat: &str) -> bool {
        if self.starts_with(pat) {
            self.bump(pat.len());
            true
        } else {
            false
        }
    }
}

/// One outl directive. `neg` selects the `not-<key>` form, which the
/// parser accepts for every filter and rejects for `sort` / `limit`.
struct Filt {
    neg: bool,
    kind: Atom,
}

enum Atom {
    Todo,
    Done,
    Tag(String),
    Text(String),
}

impl Filt {
    fn render(&self) -> String {
        let (key, value) = match &self.kind {
            Atom::Todo => (if self.neg { "not-status" } else { "status" }, "todo"),
            Atom::Done => (if self.neg { "not-status" } else { "status" }, "done"),
            Atom::Tag(name) => (if self.neg { "not-tag" } else { "tag" }, name.as_str()),
            Atom::Text(value) => (if self.neg { "not-text" } else { "text" }, value.as_str()),
        };
        format!("{key}: {value}")
    }
}

/// `"title"` before the clause is display-only in Roam. Drop it.
fn skip_title(c: &mut Cur<'_>) -> Result<(), String> {
    c.skip_ws();
    if !c.eat("\"") {
        return Ok(());
    }
    while let Some(ch) = c.peek() {
        c.bump_char();
        if ch == '\\' {
            if c.peek().is_some() {
                c.bump_char();
            }
            continue;
        }
        if ch == '"' {
            return Ok(());
        }
    }
    Err("unterminated query title".into())
}

fn parse_clause(c: &mut Cur<'_>) -> Result<Vec<Filt>, String> {
    c.skip_ws();
    if let Some(kind) = try_task_marker(c) {
        return Ok(vec![Filt { neg: false, kind }]);
    }
    if c.starts_with("[[") {
        let name = parse_page(c)?;
        return Ok(vec![Filt {
            neg: false,
            kind: page_atom(&name)?,
        }]);
    }
    if c.starts_with("#") {
        return Ok(vec![Filt {
            neg: false,
            kind: parse_tag(c)?,
        }]);
    }
    if c.starts_with("((") {
        return Err(
            "((block ref)) has no outl equivalent; Roam UIDs do not map to block handles".into(),
        );
    }
    if c.starts_with("{") {
        return parse_brace(c);
    }
    let snippet: String = c.rest().chars().take(24).collect();
    Err(format!("unsupported query syntax at '{snippet}'"))
}

fn parse_brace(c: &mut Cur<'_>) -> Result<Vec<Filt>, String> {
    let mut look = *c;
    if !look.eat("{") {
        return Err("expected a clause".into());
    }
    look.skip_ws();
    let name_start = look.i;
    while look
        .peek()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        look.bump_char();
    }
    if look.i == name_start {
        return Err("unsupported query syntax at '{'".into());
    }
    let name = &look.s[name_start..look.i];
    look.skip_ws();
    if !look.eat(":") {
        return Err(format!("{{{name}}} is missing its colon"));
    }
    match name {
        "and" => {
            *c = look;
            let mut out = Vec::new();
            loop {
                c.skip_ws();
                if c.eat("}") {
                    break;
                }
                if c.rest().is_empty() {
                    return Err("unclosed {and:}".into());
                }
                out.extend(parse_clause(c)?);
            }
            if out.is_empty() {
                // An empty outl query matches every block in the workspace.
                return Err("empty {and:} would match every block".into());
            }
            Ok(out)
        }
        // docs/query.md: "There is no general boolean grouping, and no OR
        // across positive filters". De Morgan on `{not: {or:}}` would be
        // expressible, but the `{or:}` itself has no equivalent, so the
        // whole component stays verbatim rather than half-translated.
        "or" => {
            Err("{or:} is not expressible; the DSL ANDs every line and has no OR".into())
        }
        "between" => Err(
            "{between:} matches a calendar range; since: only takes a relative duration (Nd, Nw, Nm)"
                .into(),
        ),
        "not" => {
            *c = look;
            c.skip_ws();
            if c.starts_with("}") {
                return Err("{not:} takes exactly one condition".into());
            }
            let inner = parse_clause(c)?;
            c.skip_ws();
            if !c.eat("}") {
                return Err("{not:} takes exactly one condition".into());
            }
            if inner.len() != 1 {
                return Err(
                    "{not:} of a compound condition has no outl equivalent (no boolean grouping)"
                        .into(),
                );
            }
            let mut filt = inner.into_iter().next().expect("len checked");
            filt.neg = !filt.neg;
            Ok(vec![filt])
        }
        // `{search:}` is the Roam clause whose meaning is outl's `text:`.
        "search" => {
            *c = look;
            let Some(rel) = c.rest().find('}') else {
                return Err("unclosed {search:}".into());
            };
            let text = c.rest()[..rel].trim();
            if text.is_empty() {
                return Err("{search:} is empty".into());
            }
            if text.contains('\n') {
                return Err("{search:} spans more than one line".into());
            }
            c.bump(rel + 1);
            Ok(vec![Filt {
                neg: false,
                kind: Atom::Text(text.to_string()),
            }])
        }
        // `kind: journal` is the hosting page only. `{daily-notes:}` also
        // matches blocks that merely mention a date, so it is not that filter.
        "daily-notes" => Err(
            "{daily-notes:} has no outl equivalent (kind: journal is only the hosting page)"
                .into(),
        ),
        other => Err(format!("{{{other}:}} has no outl equivalent")),
    }
}

/// `{{[[TODO]]}}` / `{{TODO}}` / the DONE twins. Checked before `{…}`
/// clauses because the checkbox form is itself a `{{…}}` component.
fn try_task_marker(c: &mut Cur<'_>) -> Option<Atom> {
    for (marker, kind) in [
        ("{{[[TODO]]}}", Atom::Todo),
        ("{{[[DONE]]}}", Atom::Done),
        ("{{TODO}}", Atom::Todo),
        ("{{DONE}}", Atom::Done),
    ] {
        if c.eat(marker) {
            return Some(kind);
        }
    }
    None
}

fn parse_page(c: &mut Cur<'_>) -> Result<String, String> {
    if !c.eat("[[") {
        return Err("expected [[page]]".into());
    }
    let mut depth = 1i32;
    let start = c.i;
    while c.peek().is_some() {
        if c.starts_with("[[") {
            depth += 1;
            c.bump(2);
            continue;
        }
        if c.starts_with("]]") {
            depth -= 1;
            if depth == 0 {
                let name = c.s[start..c.i].to_string();
                c.bump(2);
                return Ok(name);
            }
            c.bump(2);
            continue;
        }
        c.bump_char();
    }
    Err("unclosed [[page]]".into())
}

/// `[[page]]` in a Roam query means "references this page" (and, via
/// inheritance, blocks that live on it). outl can say the reference
/// half — `text:` is a case-insensitive substring, which is what the
/// outl Roam importer emits for a non-date page — and cannot say the
/// "lives on this page" half (`page:` is not a directive). A daily-note
/// page is the case where those two halves diverge the most, and
/// `since:` cannot name a calendar day, so a date page is not translated.
fn page_atom(name: &str) -> Result<Atom, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty [[page]]".into());
    }
    match name {
        "TODO" => Ok(Atom::Todo),
        "DONE" => Ok(Atom::Done),
        _ if is_daily_note_page(name) => Err(format!(
            "[[{name}]] is a daily-note page and outl has no calendar filter (since: is relative, page: is not a directive)"
        )),
        _ => Ok(Atom::Text(format!("[[{name}]]"))),
    }
}

fn parse_tag(c: &mut Cur<'_>) -> Result<Atom, String> {
    if !c.eat("#") {
        return Err("expected #tag".into());
    }
    // `#[[My Project]]` is a page reference, same as `[[My Project]]`.
    if c.starts_with("[[") {
        let name = parse_page(c)?;
        return page_atom(&name);
    }
    let start = c.i;
    while c
        .peek()
        .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '/'))
    {
        c.bump_char();
    }
    let name = &c.s[start..c.i];
    if name.is_empty() {
        return Err("#tag has no name".into());
    }
    // The same alphabet `TagFilter::new` accepts. Anything outside it
    // parses and then matches nothing, which is a silent wrong query.
    Ok(Atom::Tag(name.to_string()))
}

fn is_daily_note_page(name: &str) -> bool {
    // The relative daily pages Roam's `{between:}` docs list. Matched
    // exactly: a page titled "Today" is a page, not the keyword.
    const RELATIVE: &[&str] = &[
        "today",
        "yesterday",
        "tomorrow",
        "last week",
        "next week",
        "last month",
        "next month",
    ];
    if RELATIVE.contains(&name) {
        return true;
    }
    if NaiveDate::parse_from_str(name, "%Y-%m-%d").is_ok() {
        return true;
    }
    ordinal_daily(name).is_some()
}

fn ordinal_daily(name: &str) -> Option<NaiveDate> {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx = RX.get_or_init(|| {
        Regex::new(
            r"^(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{1,2})(?:st|nd|rd|th),\s+(\d{4})$",
        )
        .unwrap()
    });
    let caps = rx.captures(name)?;
    let month = month_to_num(&caps[1])?;
    let day: u32 = caps[2].parse().ok()?;
    let year: i32 = caps[3].parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
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

    /// Real Roam native queries, from the Roam query docs and graphs that
    /// actually ship these blocks. outl only runs a fenced `query` block
    /// (docs/query.md + `outl-exec` `dsl.rs`): one `key: value` per line,
    /// implicit AND, `not-<key>` for negation. `{or:}` and `{between:}`
    /// have no equivalent and must come back as a plain code block.
    #[test]
    fn roam_queries_become_outl_query_fences() {
        struct Case {
            name: &'static str,
            input: &'static str,
            output: &'static str,
            warning: Option<&'static str>,
        }

        let cases = [
            Case {
                // roamdocs.fyi/help/query — "a query is written inside the block"
                name: "and of a page and a todo",
                input: "{{[[query]]: {and: [[Project]] [[TODO]]}}}",
                output: "```query\ntext: [[Project]]\nstatus: todo\n```",
                warning: None,
            },
            Case {
                // the live example on that same page
                name: "and with a not-page",
                input: "{{[[query]]: {and: [[Templates]] {not: [[Change Log]]}}}}",
                output: "```query\ntext: [[Templates]]\nnot-text: [[Change Log]]\n```",
                warning: None,
            },
            Case {
                // short form, no [[query]] page wrap
                name: "short query form with a tag and done",
                input: "{{query: {and: [[DONE]] #review}}}",
                output: "```query\nstatus: done\ntag: review\n```",
                warning: None,
            },
            Case {
                name: "todo checkbox marker and a namespaced tag",
                input: "{{[[query]]: {and: {{[[TODO]]}} #ops/deploy}}}",
                output: "```query\nstatus: todo\ntag: ops/deploy\n```",
                warning: None,
            },
            Case {
                name: "not of a tag",
                input: "{{[[query]]: {and: [[TODO]] {not: #waiting}}}}",
                output: "```query\nstatus: todo\nnot-tag: waiting\n```",
                warning: None,
            },
            Case {
                name: "nested and flattens",
                input: "{{[[query]]: {and: [[Client]] {and: #FinOps [[DONE]]}}}}",
                output: "```query\ntext: [[Client]]\ntag: FinOps\nstatus: done\n```",
                warning: None,
            },
            Case {
                // roamdocs: optional title before the clause
                name: "titled query drops the title",
                input: r#"{{[[query]]: "Open tasks" {and: [[TODO]]}}}"#,
                output: "```query\nstatus: todo\n```",
                warning: None,
            },
            Case {
                // {search:} is the Roam clause whose meaning is outl's `text:`
                name: "search clause",
                input: "{{[[query]]: {and: [[TODO]] {search: deploy}} }}",
                output: "```query\nstatus: todo\ntext: deploy\n```",
                warning: None,
            },
            Case {
                // multi-word tag is a page reference in Roam
                name: "multiword tag is a page reference",
                input: "{{[[query]]: {and: #[[My Project]] [[TODO]]}}}",
                output: "```query\ntext: [[My Project]]\nstatus: todo\n```",
                warning: None,
            },
            Case {
                // whitespace the query builder and hand-edited blocks both emit
                name: "spaces inside the component header",
                input: "{{ [[query]] : {and: [[TODO]] #urgent} }}",
                output: "```query\nstatus: todo\ntag: urgent\n```",
                warning: None,
            },
            Case {
                name: "query surrounded by prose still rewrites the prose",
                input: "see {{[[query]]: {and: [[TODO]]}}} on [[April 22nd, 2026]]",
                output: "see ```query\nstatus: todo\n``` on [[2026-04-22]]",
                warning: None,
            },
            Case {
                // {or:} cannot be said in one fence (outl issue #323, query.md)
                name: "or has no equivalent",
                input: "{{[[query]]: {or: [[Meeting]] [[1on1]]}}}",
                output: "```\n{{[[query]]: {or: [[Meeting]] [[1on1]]}}}\n```",
                warning: Some("{or:}"),
            },
            Case {
                // roamdocs: {between:} is a daily-note calendar range.
                // outl's since: is Nd/Nw/Nm only.
                name: "between has no equivalent",
                input: "{{[[query]]: {and: [[TODO]] {between: [[last week]] [[today]]}}}}",
                output: "```\n{{[[query]]: {and: [[TODO]] {between: [[last week]] [[today]]}}}}\n```",
                warning: Some("{between:}"),
            },
            Case {
                // kylestratis/roam-daily-notes — the daily TODO query
                name: "real daily-todo query with between",
                input: "{{query: {and: [[TODO]] {not: [[Overdue Tasks]]}{between: [[today]] [[today]]}}}}",
                output: "```\n{{query: {and: [[TODO]] {not: [[Overdue Tasks]]}{between: [[today]] [[today]]}}}}\n```",
                warning: Some("{between:}"),
            },
            Case {
                // a date page is not `text:` — blocks on that journal do not
                // contain the link — and `page:` is not a directive yet.
                name: "daily-note page has no equivalent",
                input: "{{[[query]]: {and: [[April 22nd, 2026]] [[TODO]]}}}",
                output: "```\n{{[[query]]: {and: [[April 22nd, 2026]] [[TODO]]}}}\n```",
                warning: Some("[[April 22nd, 2026]]"),
            },
            Case {
                name: "not of an or has no equivalent",
                input: "{{[[query]]: {and: [[TODO]] {not: {or: [[Inbox]] [[Archive]]}}}}}",
                output: "```\n{{[[query]]: {and: [[TODO]] {not: {or: [[Inbox]] [[Archive]]}}}}}\n```",
                warning: Some("{or:}"),
            },
            Case {
                // The fallback fence has to round-trip. A query that was
                // already inside a code sample is not a query we wrote.
                name: "query inside an existing fence stays literal",
                input: "```\n{{[[query]]: {and: [[TODO]]}}}\n```",
                output: "```\n{{[[query]]: {and: [[TODO]]}}}\n```",
                warning: None,
            },
            Case {
                name: "ordinal date inside a fence stays literal",
                input: "```\n[[April 22nd, 2026]]\n```",
                output: "```\n[[April 22nd, 2026]]\n```",
                warning: None,
            },
            Case {
                name: "block ref has no equivalent",
                input: "{{[[query]]: {and: [[TODO]] ((CLGp3LXxM))}}}",
                output: "```\n{{[[query]]: {and: [[TODO]] ((CLGp3LXxM))}}}\n```",
                warning: Some("((block ref))"),
            },
            Case {
                name: "author clause has no equivalent",
                input: "{{[[query]]: {and: [[TODO]] {by: [[Avelino]]}}}}",
                output: "```\n{{[[query]]: {and: [[TODO]] {by: [[Avelino]]}}}}\n```",
                warning: Some("{by:}"),
            },
        ];

        for case in cases {
            let got = roam_to_outl_parts(case.input);
            assert_eq!(got.text, case.output, "output: {}", case.name);
            match case.warning {
                None => assert!(
                    got.warnings.is_empty(),
                    "unexpected warning on {}: {:?}",
                    case.name,
                    got.warnings
                ),
                Some(needle) => {
                    assert!(
                        got.warnings.iter().any(|w| w.contains(needle)),
                        "warning on {} should mention {needle}, got {:?}",
                        case.name,
                        got.warnings
                    );
                    assert!(
                        got.warnings.iter().all(|w| w.contains("code block")),
                        "warning should say the original was kept: {:?}",
                        got.warnings
                    );
                }
            }
            // A second pass must not wrap the fence again, and must not
            // rewrite the preserved Roam text.
            let again = roam_to_outl_parts(&got.text);
            assert_eq!(again.text, got.text, "idempotent: {}", case.name);
            assert!(
                again.warnings.is_empty(),
                "second pass warned on {}: {:?}",
                case.name,
                again.warnings
            );
        }
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
