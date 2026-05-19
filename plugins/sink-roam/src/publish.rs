//! High-level pipeline: sanitize → parse → resolve page → idempotent write.
//!
//! Ports `roam_page_uid`, `roam_replace_block`, `roam_write_tree`,
//! `roam_publish` from `lib/roam.fish`.

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde_json::{json, Value};

use crate::mcp::Mcp;
use crate::sanitize::{parse_hierarchy, Tree};

/// Resolve a `page_ref` to a Roam block UID, creating the page when needed.
///
/// Accepted forms:
/// - `"today"`                     — today's daily note
/// - `"April 22nd, 2026"`          — named daily (Roam native ordinal format)
/// - `"acme/tech/infra/aws/X"`    — namespaced page (created if missing)
pub fn page_uid(mcp: &Mcp, page_ref: &str) -> Result<String> {
    // JSON Pointer (RFC 6901) treats `/` as token separator. To access a key
    // that literally contains `/` (Roam's Clojure-style `:block/uid`), encode
    // it as `~1`. Equivalent to `.get(":block/uid")` but stays consistent with
    // the rest of the file.
    if page_ref == "today" {
        let v = mcp.roam("get_daily_note", &json!({}))?;
        return v
            .pointer("/:block~1uid")
            .and_then(|s| s.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("daily note response missing :block/uid"));
    }

    // Try existing page by title (works for named dailies + namespaced).
    if let Ok(v) = mcp.roam("get_page", &json!({ "title": page_ref })) {
        if let Some(uid) = v.pointer("/:block~1uid").and_then(|s| s.as_str()) {
            return Ok(uid.to_string());
        }
    }

    // Create.
    let created = mcp.roam("create_page", &json!({ "title": page_ref }))?;
    created
        .get("uid")
        .and_then(|s| s.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("create_page response missing uid: {created}"))
}

/// Find a child block of `parent_uid` whose `:block/string` matches the
/// regex and delete it. Idempotent (no-op when no match).
pub fn replace_block(mcp: &Mcp, parent_uid: &str, marker: &Regex) -> Result<()> {
    // Some daily notes return more reliably via `get_daily_note` than
    // `get_page` with the UID — try both.
    let page = mcp
        .roam("get_page", &json!({ "uid": parent_uid }))
        .or_else(|_| mcp.roam("get_daily_note", &json!({})))
        .context("listing children of parent block")?;

    let Some(children) = page.pointer("/:block~1children").and_then(|c| c.as_array()) else {
        return Ok(());
    };

    for child in children {
        let Some(s) = child.get(":block/string").and_then(|v| v.as_str()) else {
            continue;
        };
        if marker.is_match(s) {
            if let Some(uid) = child.get(":block/uid").and_then(|v| v.as_str()) {
                let _ = mcp.roam("delete_block", &json!({ "uid": uid }));
                break;
            }
        }
    }
    Ok(())
}

/// Create `root` as child of `parent_uid`, then each section as child of
/// the new root with its string-array children attached. Returns the new
/// root's UID.
pub fn write_tree(mcp: &Mcp, parent_uid: &str, tree: &Tree) -> Result<String> {
    // 1) root block (no children)
    let root_payload = json!({
        "parent_uid": parent_uid,
        "content": tree.root,
        "order": "last",
    });
    let root_resp = mcp.roam("create_block_with_children", &root_payload)?;
    let root_uid = root_resp
        .get("uid")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("create_block_with_children missing uid: {root_resp}"))?
        .to_string();

    // 2) sections as children of the root
    for section in &tree.sections {
        let children_json: Value = json!(section.children);
        let payload = json!({
            "parent_uid": root_uid,
            "content": section.header,
            "order": "last",
            "children": children_json.to_string(),
        });
        let _ = mcp.roam("create_block_with_children", &payload);
    }
    Ok(root_uid)
}

/// Full pipeline used by `roam_publish` in the Fish framework.
pub fn publish(mcp: &Mcp, content: &str, page_ref: &str, marker: &Regex) -> Result<String> {
    let tree = parse_hierarchy(content)
        .ok_or_else(|| anyhow!("sink-roam: empty / unparseable content"))?;
    let parent_uid = page_uid(mcp, page_ref)?;
    replace_block(mcp, &parent_uid, marker)?;
    write_tree(mcp, &parent_uid, &tree)
}
