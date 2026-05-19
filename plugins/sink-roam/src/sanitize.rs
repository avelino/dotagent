//! Port of `roam_sanitize` + `roam_parse_hierarchy` from `lib/roam.fish`.
//!
//! Convention (mirrors the legacy Fish framework):
//! - Line 1 (no indent) is the `root`.
//! - Indent ≤ 3 spaces  → L1 (direct child of root).
//! - Indent > 3 spaces  → L2 (child of the most recent L1).
//! - Code fences ` ``` ` are stripped (Claude sometimes wraps output).
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

/// Strip code fences, cut at `---`, normalize indent, tag L1/L2.
fn sanitize(input: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut in_doc = false;
    let mut root_set = false;

    for raw in input.lines() {
        let trimmed = raw.trim_end_matches(['\r']);

        if trimmed.starts_with("```") {
            if in_doc {
                break;
            }
            continue;
        }

        // Root: first non-empty line that doesn't start with whitespace.
        if !root_set && trimmed.starts_with(|c: char| !c.is_whitespace()) {
            in_doc = true;
            root_set = true;
            out.push(Line::Root(trimmed.to_string()));
            continue;
        }

        if !in_doc {
            continue;
        }

        // Cut at the first `---` separator.
        if trimmed.trim() == "---" {
            break;
        }

        // Measure indent (in spaces — tabs treated as 1, matching the Fish
        // version's `[[:space:]]*` count).
        let indent_len = trimmed.chars().take_while(|c| c.is_whitespace()).count();
        let mut content = trimmed.trim_start().to_string();

        // Strip leading "- " literal (Roam adds bullets automatically).
        if let Some(rest) = content.strip_prefix("- ") {
            content = rest.to_string();
        } else if let Some(rest) = content.strip_prefix("-") {
            // Tolerate "-foo" without space (rare but happens).
            content = rest.to_string();
        }

        if content.is_empty() {
            continue;
        }

        if indent_len <= 3 {
            out.push(Line::L1(content));
        } else {
            out.push(Line::L2(content));
        }
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
    fn strips_code_fence() {
        // The fence is dropped; the second fence ends the document so
        // "ignored after" never enters the parser.
        let input = "```markdown\n#TAG header\n  child\n```\nignored after";
        let tree = parse_hierarchy(input).unwrap();
        assert_eq!(tree.root, "#TAG header");
        assert_eq!(tree.sections.len(), 1);
        assert_eq!(tree.sections[0].header, "child");
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
