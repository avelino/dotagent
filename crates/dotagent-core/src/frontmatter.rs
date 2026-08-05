//! YAML frontmatter, parsed leniently.
//!
//! Shared by [`crate::skill`] and [`crate::command`], which read two different
//! markdown formats that agree on how the header works: `---`, some top-level
//! `key: value` pairs, `---`, then a body.
//!
//! ## Why lenient
//!
//! Both formats are Anthropic's — Agent Skills and Claude Code slash commands —
//! and the whole point is that a file you already wrote for Claude Code works
//! here unchanged. So unknown keys (`version`, `model`, `triggers`, `license`,
//! `metadata`) are **ignored rather than rejected**: a file Claude Code accepts
//! must not fail over a field dotagent has no opinion about.
//!
//! The parser handles the subset that carries meaning: top-level scalars and
//! block scalars. Nested maps and lists are skipped wholesale. It is not a
//! general YAML implementation and does not pretend to be — pulling in a YAML
//! crate to read three strings would be the tail wagging the dog.

use crate::error::{Error, Result};

/// Split `---\n…\n---\n` off the front. Returns `(frontmatter, body)`.
///
/// `what` names the file in the error, so a failure reads
/// "SKILL.md must start with…" rather than something generic.
pub fn split(raw: &str, what: &str) -> Result<(String, String)> {
    // A BOM before the opening fence is common enough from editors that
    // failing on it would be a bad first experience.
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw.lines();

    match lines.next() {
        Some(first) if first.trim() == "---" => {}
        _ => {
            return Err(Error::InvalidManifest(format!(
                "{what} must start with a `---` frontmatter block"
            )))
        }
    }

    let mut front = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        let t = line.trim_end();
        if t.trim() == "---" || t.trim() == "..." {
            closed = true;
            break;
        }
        front.push_str(line);
        front.push('\n');
    }
    if !closed {
        return Err(Error::InvalidManifest(format!(
            "{what} frontmatter is never closed by a `---` line"
        )));
    }

    let body: String = lines.collect::<Vec<_>>().join("\n");
    Ok((front, body))
}

/// Pull top-level scalar keys out of a frontmatter block.
///
/// Anything that opens a nested structure (a list, a map) is skipped along with
/// its indented continuation. Block scalars (`|`, `>`) are collected because a
/// long `description` is often written that way.
pub fn fields(front: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = front.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        i += 1;

        // Indented lines belong to a structure we already decided about.
        if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
            continue;
        }
        if line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        let rest = rest.trim();

        // Block scalar: take every following indented line.
        // Covers every indicator variant: `|`, `|-`, `|+`, `>`, `>-`, `>2`.
        if rest.starts_with('|') || rest.starts_with('>') {
            let folded = rest.starts_with('>');
            let mut block: Vec<String> = Vec::new();
            while i < lines.len() {
                let cont = lines[i];
                if cont.trim().is_empty() {
                    block.push(String::new());
                    i += 1;
                    continue;
                }
                if !(cont.starts_with(' ') || cont.starts_with('\t')) {
                    break;
                }
                block.push(cont.trim().to_string());
                i += 1;
            }
            let joined = if folded {
                block.join(" ")
            } else {
                block.join("\n")
            };
            out.push((key.to_string(), joined.trim().to_string()));
            continue;
        }

        // `key:` with nothing after it opens a list or a map — skip the whole
        // structure rather than record an empty value that would shadow
        // nothing useful.
        if rest.is_empty() {
            while i < lines.len()
                && (lines[i].starts_with(' ')
                    || lines[i].starts_with('\t')
                    || lines[i].trim_start().starts_with('-')
                    || lines[i].trim().is_empty())
            {
                i += 1;
            }
            continue;
        }

        out.push((key.to_string(), unquote(rest)));
    }
    out
}

/// Look one key up, trimmed. `None` when absent or empty.
pub fn get(fields: &[(String, String)], key: &str) -> Option<String> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_header_from_body() {
        let (front, body) = split("---\nname: x\n---\nBody.\n", "F").unwrap();
        assert_eq!(front, "name: x\n");
        assert_eq!(body, "Body.");
    }

    #[test]
    fn a_leading_bom_does_not_break_the_fence() {
        assert!(split("\u{feff}---\nname: x\n---\nBody.\n", "F").is_ok());
    }

    #[test]
    fn missing_and_unclosed_frontmatter_name_the_file() {
        let err = split("# just markdown\n", "SKILL.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("SKILL.md"), "{err}");

        let err = split("---\nname: x\nBody\n", "cmd.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("never closed"), "{err}");
    }

    #[test]
    fn skips_nested_structures_but_keeps_scalars() {
        let front = "name: x\nlist:\n  - a\n  - b\nmap:\n  k: v\ndescription: kept\n";
        let f = fields(front);
        assert_eq!(get(&f, "name").as_deref(), Some("x"));
        assert_eq!(get(&f, "description").as_deref(), Some("kept"));
        assert!(get(&f, "list").is_none());
    }

    #[test]
    fn block_and_folded_scalars() {
        let f = fields("description: |\n  One.\n  Two.\n");
        assert_eq!(get(&f, "description").as_deref(), Some("One.\nTwo."));

        let f = fields("description: >\n  One\n  two\n");
        assert_eq!(get(&f, "description").as_deref(), Some("One two"));
    }

    #[test]
    fn a_colon_in_a_value_is_not_a_key_boundary() {
        let f = fields("description: Use when: the week closes.\n");
        assert_eq!(
            get(&f, "description").as_deref(),
            Some("Use when: the week closes.")
        );
    }

    #[test]
    fn quoted_values_are_unquoted() {
        let f = fields("name: \"x\"\nhint: 'a b'\n");
        assert_eq!(get(&f, "name").as_deref(), Some("x"));
        assert_eq!(get(&f, "hint").as_deref(), Some("a b"));
    }

    #[test]
    fn get_treats_an_empty_value_as_absent() {
        let f = fields("description: \"\"\n");
        assert!(get(&f, "description").is_none());
    }
}
