//! Port of `roam_sanitize` + `roam_parse_hierarchy` from `lib/roam.fish`.
//!
//! Convention (mirrors the legacy Fish framework):
//! - Line 1 (no indent) is the `root`.
//! - Indent ≤ 3 spaces  → L1 (direct child of root).
//! - Indent > 3 spaces  → L2 (child of the most recent L1).
//! - Code fences ` ``` ` BEFORE the root line are stripped (Claude
//!   sometimes wraps the whole output in a fence).
//! - Code fences AFTER the root are preserved as a **single multi-line
//!   block** at the indent level of the opening fence. Mirrors what
//!   `agent.fish`'s fence-merge does for Roam so dashboards, sql, and
//!   diagrams stay as one block instead of one block per line.
//! - Cut off at the first `---` line (Claude sometimes adds notes after).
//! - Leading `-` literal is removed from each line (Roam renders bullets
//!   automatically — typing `- foo` would produce `- - foo`).

use serde::Serialize;

/// Sanitized line tagged with its hierarchy level.
#[derive(Debug, Clone, PartialEq)]
enum Line {
    Root(String),
    L1(String),
    L2(String),
}

/// Final tree structure consumed by `roam_write_tree`.
#[derive(Debug, Clone, Serialize)]
pub struct Tree {
    pub root: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Section {
    pub header: String,
    pub children: Vec<String>,
}

/// L1 vs L2 by indent — the single place the 3-space cutoff lives.
fn tag(indent: usize, text: String) -> Line {
    if indent <= 3 {
        Line::L1(text)
    } else {
        Line::L2(text)
    }
}

/// Strip code fences, cut at `---`, normalize indent, tag L1/L2.
///
/// Fence handling has two modes:
///
/// - **Pre-root fence** (we haven't seen the root line yet): treated
///   as a wrapping fence Claude added around the whole output, and
///   skipped. Both opening and closing fences are dropped.
/// - **In-doc fence** (we already have the root): start of a
///   multi-line block. Every subsequent line is appended verbatim to
///   the accumulator until the closing fence, then we emit one
///   `Line::L1`/`Line::L2` whose text is the original fenced content
///   (fences included) joined with `\n`. The indent of the opening
///   fence picks L1 vs L2.
fn sanitize(input: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut root_set = false;
    // Pre-root fence opener seen — its closer (which may show up
    // after the root, if Claude wraps the reply) must also be
    // dropped instead of starting a new in-doc fence.
    let mut wrapper_fence_pending = false;
    // Fence accumulator state, only used after the root is set.
    let mut in_fence = false;
    let mut fence_indent = 0usize;
    let mut fence_lines: Vec<String> = Vec::new();

    for raw in input.lines() {
        let trimmed = raw.trim_end_matches(['\r']);
        let stripped = trimmed.trim_start();
        let indent_len = trimmed.chars().take_while(|c| c.is_whitespace()).count();

        // ---- inside an in-doc fence: accumulate until the closer ----
        if in_fence {
            fence_lines.push(stripped.to_string());
            if stripped.starts_with("```") {
                // Closing fence — emit a single block at the opener's
                // indent. Joined with `\n` so the consumer (Outl)
                // stores the whole code block as one rich text body.
                out.push(tag(fence_indent, fence_lines.join("\n")));
                in_fence = false;
                fence_lines.clear();
            }
            continue;
        }

        // ---- fence opener ----
        if stripped.starts_with("```") {
            if !root_set {
                // Pre-root: drop the wrapping fence opener and arm
                // the wrapper-pending flag so its matching closer
                // (which lands after the root) gets dropped too.
                wrapper_fence_pending = true;
            } else if wrapper_fence_pending {
                // Closer of the pre-root wrapping fence. Drop it,
                // disarm the flag, do NOT start an in-doc fence.
                wrapper_fence_pending = false;
            } else {
                in_fence = true;
                fence_indent = indent_len;
                fence_lines.push(stripped.to_string());
            }
            continue;
        }

        // Root: first non-empty line that doesn't start with whitespace.
        // Anything before it is preamble and gets dropped.
        if !root_set {
            if indent_len == 0 && !stripped.is_empty() {
                root_set = true;
                out.push(Line::Root(trimmed.to_string()));
            }
            continue;
        }

        // Cut at the first `---` separator.
        if stripped.trim_end() == "---" {
            break;
        }

        // Strip leading "- " literal (Roam adds bullets automatically),
        // tolerating "-foo" without the space (rare but happens).
        let content = match stripped.strip_prefix('-') {
            Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
            None => stripped,
        };

        if content.is_empty() {
            continue;
        }

        out.push(tag(indent_len, content.to_string()));
    }

    // Unclosed fence (LLM truncated mid-block): flush whatever was
    // accumulated as a best-effort block instead of dropping it.
    // `fence_lines` is only non-empty while a fence is open.
    if !fence_lines.is_empty() {
        out.push(tag(fence_indent, fence_lines.join("\n")));
    }

    out
}

/// Run `sanitize` + group L1/L2 into a [`Tree`].
pub fn parse_hierarchy(input: &str) -> Option<Tree> {
    let lines = sanitize(input);
    let mut iter = lines.into_iter();

    let root = match iter.next()? {
        Line::Root(s) => s,
        Line::L1(s) | Line::L2(s) => s, // tolerate documents that start directly with L1
    };

    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;

    for line in iter {
        match line {
            Line::Root(_) => continue, // already consumed
            Line::L1(content) => {
                if let Some(s) = current.take() {
                    sections.push(s);
                }
                current = Some(Section {
                    header: content,
                    children: Vec::new(),
                });
            }
            Line::L2(content) => {
                if let Some(s) = current.as_mut() {
                    s.children.push(content);
                }
                // Orphan L2 (no preceding L1) is silently dropped — matches
                // the Fish parser's behavior.
            }
        }
    }
    if let Some(s) = current {
        sections.push(s);
    }

    Some(Tree { root, sections })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_pre_root_wrapping_fence() {
        // Claude sometimes wraps the whole reply in a fence. Both
        // opener and closer must vanish so the real root + children
        // survive.
        let input = "```markdown\n#TAG header\n  child\n```";
        let tree = parse_hierarchy(input).unwrap();
        assert_eq!(tree.root, "#TAG header");
        assert_eq!(tree.sections.len(), 1);
        assert_eq!(tree.sections[0].header, "child");
    }

    #[test]
    fn preserves_in_doc_fence_as_single_block() {
        let input = "#root\n  L1 alpha\n    ```sql\n    select 1\n    select 2\n    ```\n  L1 beta";
        let tree = parse_hierarchy(input).unwrap();
        assert_eq!(tree.root, "#root");
        assert_eq!(tree.sections.len(), 2);
        assert_eq!(tree.sections[0].header, "L1 alpha");
        // Fence opens at indent 4 → L2 child of "L1 alpha". Text is
        // the whole fenced block joined with newlines, fences kept.
        assert_eq!(tree.sections[0].children.len(), 1);
        assert_eq!(
            tree.sections[0].children[0],
            "```sql\nselect 1\nselect 2\n```"
        );
        assert_eq!(tree.sections[1].header, "L1 beta");
    }

    #[test]
    fn truncated_fence_flushed_as_best_effort_block() {
        // LLM cut mid-block: we'd rather emit what we have than drop
        // it on the floor.
        let input = "#root\n  L1 alpha\n    ```sql\n    select 1";
        let tree = parse_hierarchy(input).unwrap();
        assert_eq!(tree.sections[0].children.len(), 1);
        assert_eq!(tree.sections[0].children[0], "```sql\nselect 1");
    }

    #[test]
    fn multiple_in_doc_fences_kept_separate() {
        let input = "#root\n  L1 alpha\n    ```sql\n    select 1\n    ```\n    text in between\n    ```sql\n    select 2\n    ```";
        let tree = parse_hierarchy(input).unwrap();
        let kids = &tree.sections[0].children;
        assert_eq!(kids.len(), 3);
        assert_eq!(kids[0], "```sql\nselect 1\n```");
        assert_eq!(kids[1], "text in between");
        assert_eq!(kids[2], "```sql\nselect 2\n```");
    }

    #[test]
    fn cuts_at_triple_dash() {
        let input = "#root\n  L1 alpha\n---\nignored notes";
        let tree = parse_hierarchy(input).unwrap();
        assert_eq!(tree.root, "#root");
        assert_eq!(tree.sections.len(), 1);
        assert_eq!(tree.sections[0].header, "L1 alpha");
    }

    #[test]
    fn groups_l1_and_l2() {
        let input = r#"#root
  L1 alpha
    L2 alpha-1
    L2 alpha-2
  L1 beta
    L2 beta-1"#;
        let tree = parse_hierarchy(input).unwrap();
        assert_eq!(tree.root, "#root");
        assert_eq!(tree.sections.len(), 2);
        assert_eq!(tree.sections[0].header, "L1 alpha");
        assert_eq!(tree.sections[0].children, vec!["L2 alpha-1", "L2 alpha-2"]);
        assert_eq!(tree.sections[1].header, "L1 beta");
        assert_eq!(tree.sections[1].children, vec!["L2 beta-1"]);
    }

    #[test]
    fn strips_dash_prefix() {
        let input = "#root\n  - L1 with dash\n    - L2 with dash";
        let tree = parse_hierarchy(input).unwrap();
        assert_eq!(tree.sections[0].header, "L1 with dash");
        assert_eq!(tree.sections[0].children[0], "L2 with dash");
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(parse_hierarchy("").is_none());
    }
}
