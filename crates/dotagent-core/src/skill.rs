//! Skills — procedures an assistant loads on demand.
//!
//! An agent is a verb ("run this"). Memory is a fact ("this is true"). A skill
//! is the third thing: **how to do something**. It is text, not a subprocess —
//! a procedure a model reads and follows, optionally shipping scripts it can
//! ask to have run.
//!
//! ## The file
//!
//! ```text
//! <skill-dir>/
//!   SKILL.md          # YAML frontmatter + the procedure
//!   scripts/          # optional executables, reachable via `skill-run`
//!   references/       # optional supporting files, reachable via `skill-read`
//! ```
//!
//! ```markdown
//! ---
//! name: weekly-numbers
//! description: How to close out the week. Use when asked for "the numbers".
//! ---
//!
//! 1. Call run-hn-digest …
//! ```
//!
//! ## Why the frontmatter is parsed leniently
//!
//! The format is Anthropic's [Agent Skills] layout, which Claude Code already
//! reads from `~/.claude/skills/`. Reusing a skill you already wrote is the
//! point, so unknown keys (`version`, `triggers`, `allowed-tools`, `license`,
//! `metadata`) are **ignored rather than rejected** — a skill that Claude Code
//! accepts must not fail here over a field dotagent has no opinion about.
//!
//! The parsing itself lives in [`crate::frontmatter`], shared with
//! [`crate::command`].
//!
//! [Agent Skills]: https://code.claude.com/docs/en/skills

use std::path::Path;

use crate::error::{Error, Result};
use crate::frontmatter;

/// Filename that marks a directory as a skill.
pub const SKILL_FILE: &str = "SKILL.md";

/// Deadline for a `scripts/` execution when the skill declares none.
///
/// Five minutes: long enough for a report to render, short enough that a
/// script a model called and forgot about does not sit there forever.
pub const DEFAULT_SKILL_TIMEOUT_SECONDS: u64 = 300;

/// One skill, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillManifest {
    /// Catalog name. Falls back to the directory name when the frontmatter
    /// omits it.
    pub name: String,
    /// What the skill is for. This is the **trigger** — it is the only part a
    /// model sees before deciding to load the body, so an empty one makes the
    /// skill unreachable and fails validation.
    pub description: String,
    /// Deadline for `scripts/` executions, in seconds.
    pub timeout_seconds: u64,
    /// Everything after the frontmatter: the procedure itself.
    pub body: String,
}

impl SkillManifest {
    /// Parse `SKILL.md` contents. `fallback_name` is used when the frontmatter
    /// has no `name` — the directory name, which is what Claude Code shows for
    /// a skill anyway.
    pub fn parse(raw: &str, fallback_name: &str) -> Result<Self> {
        let (front, body) = frontmatter::split(raw, SKILL_FILE)?;
        let fields = frontmatter::fields(&front);
        let field = |key: &str| frontmatter::get(&fields, key);

        let name = field("name").unwrap_or_else(|| fallback_name.to_string());
        let description = field("description").unwrap_or_default();

        // An unparseable number is a typo worth surfacing, not a reason to
        // silently run with a deadline the author did not choose.
        let timeout_seconds = match field("timeout_seconds") {
            Some(v) => v.parse::<u64>().map_err(|_| {
                Error::InvalidManifest(format!("skill '{name}': timeout_seconds must be a number"))
            })?,
            None => DEFAULT_SKILL_TIMEOUT_SECONDS,
        };

        let skill = Self {
            name,
            description,
            timeout_seconds,
            body: body.trim().to_string(),
        };
        skill.validate()?;
        Ok(skill)
    }

    /// Read and parse a `SKILL.md`, taking the fallback name from its parent
    /// directory.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)?;
        let fallback = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "skill".to_string());
        Self::parse(&raw, &fallback)
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::InvalidManifest("skill has an empty name".into()));
        }
        // The description is what a model matches against. Without one the
        // skill sits in the catalog and is never chosen — inert, and silently
        // so. Better to fail where `doctor` can print it.
        if self.description.trim().is_empty() {
            return Err(Error::InvalidManifest(format!(
                "skill '{}': description is required — it is what a model matches on",
                self.name
            )));
        }
        if self.body.trim().is_empty() {
            return Err(Error::InvalidManifest(format!(
                "skill '{}': body is empty — there is no procedure to follow",
                self.name
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "---\nname: weekly\ndescription: Close the week.\n---\n\nStep one.\n";

    #[test]
    fn parses_name_description_and_body() {
        let s = SkillManifest::parse(MINIMAL, "dir-name").unwrap();
        assert_eq!(s.name, "weekly");
        assert_eq!(s.description, "Close the week.");
        assert_eq!(s.body, "Step one.");
        assert_eq!(s.timeout_seconds, DEFAULT_SKILL_TIMEOUT_SECONDS);
    }

    #[test]
    fn name_falls_back_to_the_directory() {
        let raw = "---\ndescription: Does a thing.\n---\nBody.\n";
        let s = SkillManifest::parse(raw, "from-dir").unwrap();
        assert_eq!(s.name, "from-dir");
    }

    #[test]
    fn a_claude_code_skill_parses_unchanged() {
        // The whole point: fields dotagent has no opinion about must not be a
        // reason to reject a skill that Claude Code already accepts.
        let raw = "---\n\
name: voice-humanize\n\
version: 3.0.0\n\
description: Humanize text in the user's voice.\n\
allowed-tools: Read, Grep\n\
triggers:\n\
  - humaniza\n\
  - passa no meu tom\n\
metadata:\n\
  author: someone\n\
  nested:\n\
    deeper: true\n\
---\n\
The procedure.\n";
        let s = SkillManifest::parse(raw, "dir").unwrap();
        assert_eq!(s.name, "voice-humanize");
        assert_eq!(s.description, "Humanize text in the user's voice.");
        assert_eq!(s.body, "The procedure.");
    }

    #[test]
    fn a_namespaced_name_survives_verbatim() {
        // `.claude/skills/*/SKILL.md` in this very repo uses `plugin:skill`.
        let raw = "---\nname: dotagent:doc-review\ndescription: Audit docs.\n---\nBody.\n";
        let s = SkillManifest::parse(raw, "dir").unwrap();
        assert_eq!(s.name, "dotagent:doc-review");
    }

    #[test]
    fn block_scalar_description_is_collected() {
        let raw = "---\nname: x\ndescription: |\n  First line.\n  Second line.\n---\nBody.\n";
        let s = SkillManifest::parse(raw, "dir").unwrap();
        assert_eq!(s.description, "First line.\nSecond line.");
    }

    #[test]
    fn folded_scalar_joins_with_spaces() {
        let raw = "---\nname: x\ndescription: >\n  First part\n  second part\n---\nBody.\n";
        let s = SkillManifest::parse(raw, "dir").unwrap();
        assert_eq!(s.description, "First part second part");
    }

    #[test]
    fn quoted_values_are_unquoted() {
        let raw = "---\nname: \"x\"\ndescription: 'Does: a thing.'\n---\nBody.\n";
        let s = SkillManifest::parse(raw, "dir").unwrap();
        assert_eq!(s.name, "x");
        assert_eq!(s.description, "Does: a thing.");
    }

    #[test]
    fn a_colon_in_the_description_is_not_a_key_boundary() {
        let raw = "---\nname: x\ndescription: Use when: the week closes.\n---\nBody.\n";
        let s = SkillManifest::parse(raw, "dir").unwrap();
        assert_eq!(s.description, "Use when: the week closes.");
    }

    #[test]
    fn missing_description_is_rejected() {
        let raw = "---\nname: x\n---\nBody.\n";
        let err = SkillManifest::parse(raw, "dir").unwrap_err().to_string();
        assert!(err.contains("description"), "{err}");
    }

    #[test]
    fn empty_body_is_rejected() {
        let raw = "---\nname: x\ndescription: d\n---\n\n";
        let err = SkillManifest::parse(raw, "dir").unwrap_err().to_string();
        assert!(err.contains("body"), "{err}");
    }

    #[test]
    fn missing_frontmatter_is_rejected() {
        let err = SkillManifest::parse("# Just markdown\n", "dir")
            .unwrap_err()
            .to_string();
        assert!(err.contains("frontmatter"), "{err}");
    }

    #[test]
    fn unclosed_frontmatter_is_rejected() {
        let err = SkillManifest::parse("---\nname: x\ndescription: d\nBody\n", "dir")
            .unwrap_err()
            .to_string();
        assert!(err.contains("never closed"), "{err}");
    }

    #[test]
    fn timeout_override_parses_and_a_typo_fails() {
        let ok = SkillManifest::parse(
            "---\nname: x\ndescription: d\ntimeout_seconds: 30\n---\nBody.\n",
            "dir",
        )
        .unwrap();
        assert_eq!(ok.timeout_seconds, 30);

        let err = SkillManifest::parse(
            "---\nname: x\ndescription: d\ntimeout_seconds: soon\n---\nBody.\n",
            "dir",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("timeout_seconds"), "{err}");
    }

    #[test]
    fn a_leading_bom_does_not_break_the_fence() {
        let s = SkillManifest::parse(&format!("\u{feff}{MINIMAL}"), "dir").unwrap();
        assert_eq!(s.name, "weekly");
    }

    #[test]
    fn body_keeps_its_internal_structure() {
        let raw = "---\nname: x\ndescription: d\n---\n# Title\n\n- one\n- two\n";
        let s = SkillManifest::parse(raw, "dir").unwrap();
        assert_eq!(s.body, "# Title\n\n- one\n- two");
    }
}
