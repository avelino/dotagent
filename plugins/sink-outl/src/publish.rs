//! High-level pipeline: sanitize → parse → resolve target → idempotent
//! batched write.
//!
//! Outl exposes `outl_batch` (atomic-ish sequence) and `outl_block_append_tree`
//! (whole subtree in one call), so the entire publish is a single
//! `outl_batch` call: N `block_delete` ops (matched marker) + one
//! `block_append_tree` op (the new content).

use anyhow::{anyhow, Result};
use chrono::{Datelike, Duration, Local, NaiveDate};
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::mcp::Mcp;
use crate::sanitize::{parse_hierarchy, Tree};

/// Where to write: the daily journal of a date, or a named page.
#[derive(Debug, Clone)]
enum Target {
    /// `outl_daily_*` semantics. `slug` is always the ISO date.
    Daily { slug: String },
    /// Named/namespaced page. `slug` is the lowercased ref, `/` preserved.
    Page { slug: String },
}

impl Target {
    fn slug(&self) -> &str {
        match self {
            Target::Daily { slug } | Target::Page { slug } => slug.as_str(),
        }
    }
}

/// Decide which Outl target the `page_ref` maps to.
///
/// Rules:
/// - `"today"` / `"yesterday"` / `"tomorrow"` → `Daily` (resolved locally)
/// - ISO `YYYY-MM-DD` → `Daily`
/// - `"<Month> <D><suffix>, YYYY"` (Roam-native ordinal) → `Daily`
/// - everything else → `Page` (lowercased; `/` kept for hierarchy)
fn resolve_target(page_ref: &str) -> Result<Target> {
    let trimmed = page_ref.trim();

    // Reserved daily keywords resolved against the local clock.
    let local_today = Local::now().date_naive();
    match trimmed {
        "today" => {
            return Ok(Target::Daily {
                slug: local_today.to_string(),
            })
        }
        "yesterday" => {
            return Ok(Target::Daily {
                slug: (local_today - Duration::days(1)).to_string(),
            })
        }
        "tomorrow" => {
            return Ok(Target::Daily {
                slug: (local_today + Duration::days(1)).to_string(),
            })
        }
        _ => {}
    }

    if is_iso_date(trimmed) {
        return Ok(Target::Daily {
            slug: trimmed.to_string(),
        });
    }

    if let Some(iso) = parse_roam_ordinal_daily(trimmed) {
        return Ok(Target::Daily { slug: iso });
    }

    Ok(Target::Page {
        slug: trimmed.to_lowercase(),
    })
}

fn is_iso_date(s: &str) -> bool {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

/// `April 22nd, 2026` → `2026-04-22`. Returns `None` on shape mismatch.
fn parse_roam_ordinal_daily(s: &str) -> Option<String> {
    let (head, year_part) = s.rsplit_once(", ")?;
    let year: i32 = year_part.parse().ok()?;
    let (month_name, day_part) = head.split_once(' ')?;
    // Strip ordinal suffix: "22nd" → 22.
    let day_digits: String = day_part
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let day: u32 = day_digits.parse().ok()?;
    let month = match month_name {
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
    };
    let nd = NaiveDate::from_ymd_opt(year, month, day)?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        nd.year(),
        nd.month(),
        nd.day()
    ))
}

/// Convert the 2-level `Tree` (root + sections of L1 headers with L2 children)
/// into Outl's tree shape: `{text, children: [{text, children: [{text}]}]}`.
/// Every text node is passed through [`crate::translate::roam_to_outl`] so
/// Roam-flavor constructs (`{{[[TODO]]}}`, ordinal dailies, …) render
/// natively on the Outl side.
fn tree_to_outl(tree: &Tree) -> Value {
    let sections: Vec<Value> = tree
        .sections
        .iter()
        .map(|s| {
            let children: Vec<Value> = s
                .children
                .iter()
                .map(|c| json!({ "text": crate::translate::roam_to_outl(c) }))
                .collect();
            let header = crate::translate::roam_to_outl(&s.header);
            if children.is_empty() {
                json!({ "text": header })
            } else {
                json!({ "text": header, "children": children })
            }
        })
        .collect();

    let root = crate::translate::roam_to_outl(&tree.root);
    if sections.is_empty() {
        json!({ "text": root })
    } else {
        json!({ "text": root, "children": sections })
    }
}

/// Search blocks containing the marker's literal prefix, then filter
/// client-side with the full regex. Returns block IDs to delete.
fn find_marker_blocks(mcp: &Mcp, target: &Target, marker: &Regex) -> Result<Vec<String>> {
    let target_slug = target.slug();
    let probe = fts_probe(marker.as_str());
    let resp = mcp.outl(
        "outl_search",
        &json!({ "query": probe, "in": "blocks", "limit": 50 }),
    )?;

    let Some(blocks) = resp.get("blocks").and_then(|b| b.as_array()) else {
        return Ok(vec![]);
    };

    let mut ids = vec![];
    for blk in blocks {
        let Some(text) = blk.get("text").and_then(|s| s.as_str()) else {
            continue;
        };
        let Some(page) = blk.get("page").and_then(|s| s.as_str()) else {
            continue;
        };
        if page != target_slug {
            continue;
        }
        if marker.is_match(text) {
            if let Some(id) = blk.get("id").and_then(|s| s.as_str()) {
                ids.push(id.to_string());
            }
        }
    }
    Ok(ids)
}

/// Derive a cheap FTS probe from the regex pattern: first contiguous run
/// of `#`, alphanumeric, `_` or `-`. Outl FTS matches any anchor-substring.
fn fts_probe(pattern: &str) -> String {
    let mut probe = String::new();
    let mut started = false;
    for c in pattern.chars() {
        let keep = c == '#' || c.is_alphanumeric() || c == '_' || c == '-';
        if keep {
            probe.push(c);
            started = true;
        } else if started {
            break;
        }
        if probe.len() >= 32 {
            break;
        }
    }
    if probe.is_empty() {
        return pattern.chars().take(32).collect();
    }
    probe
}

/// Build the batch op list and dispatch as a single `outl_batch` call.
pub fn publish(mcp: &Mcp, content: &str, page_ref: &str, marker: &Regex) -> Result<String> {
    let tree = parse_hierarchy(content)
        .ok_or_else(|| anyhow!("sink-outl: empty / unparseable content"))?;

    let target = resolve_target(page_ref)?;

    // For named pages, ensure the slug exists (idempotent in Outl).
    if let Target::Page { slug } = &target {
        let _ = mcp.outl("outl_page_create", &json!({ "slug": slug }))?;
    }

    let delete_ids = find_marker_blocks(mcp, &target, marker)?;

    let outl_tree = tree_to_outl(&tree);
    let mut append_args = Map::new();
    append_args.insert("page".into(), Value::String(target.slug().to_string()));
    append_args.insert("tree".into(), outl_tree);

    // Compose ops: deletes first (idempotency), then a single append_tree.
    let mut ops: Vec<Value> = delete_ids
        .iter()
        .map(|id| {
            json!({
                "op": "block_delete",
                "args": { "id": id, "confirm": true }
            })
        })
        .collect();
    ops.push(json!({
        "op": "block_append_tree",
        "args": Value::Object(append_args),
    }));

    let resp = mcp.outl("outl_batch", &json!({ "ops": ops }))?;

    // Detect partial failure (batch stops at first error).
    if let Some(err) = resp.get("error") {
        return Err(anyhow!(
            "outl_batch failed at op {}: {}",
            resp.get("failed_at")
                .and_then(|v| v.as_u64())
                .unwrap_or_default(),
            err
        ));
    }

    let root_id = resp
        .get("results")
        .and_then(|r| r.as_array())
        .and_then(|arr| arr.last())
        .and_then(|last| last.pointer("/data/tree/id"))
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("outl_batch response missing tree id: {resp}"))?
        .to_string();

    Ok(root_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_detection() {
        assert!(is_iso_date("2026-06-01"));
        assert!(!is_iso_date("2026-13-01"));
        assert!(!is_iso_date("today"));
    }

    #[test]
    fn roam_ordinal_to_iso() {
        assert_eq!(
            parse_roam_ordinal_daily("April 22nd, 2026").as_deref(),
            Some("2026-04-22")
        );
        assert_eq!(
            parse_roam_ordinal_daily("January 1st, 2026").as_deref(),
            Some("2026-01-01")
        );
        assert_eq!(
            parse_roam_ordinal_daily("December 31st, 2025").as_deref(),
            Some("2025-12-31")
        );
        assert_eq!(parse_roam_ordinal_daily("not a date"), None);
    }

    #[test]
    fn fts_probe_picks_first_run() {
        assert_eq!(fts_probe("#DORA.*2026-05-19"), "#DORA");
        assert_eq!(fts_probe("#FinOps weekly"), "#FinOps");
        assert_eq!(fts_probe("^foo$"), "foo");
    }

    #[test]
    fn page_slug_lowercased() {
        match resolve_target("Acme/Tech/AWS").unwrap() {
            Target::Page { slug } => assert_eq!(slug, "acme/tech/aws"),
            t => panic!("expected Page, got {t:?}"),
        }
    }
}
