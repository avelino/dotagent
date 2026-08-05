//! Skill discovery.
//!
//! A skill is a directory holding a `SKILL.md`. dotagent searches, in order:
//!
//!   1. `[skills] paths` from `config.toml` — extra roots, first so they win
//!   2. `$DOTAGENT_ROOT` (and `$DOTAGENT_ROOT/skills`) — overrides for testing
//!   3. `$DOTAGENT_HOME/skills/` (default `~/.config/dotagent/skills/`)
//!   4. `~/.claude/skills/` and `$CWD/.claude/skills/` — unless
//!      `[skills] claude_skills = false`
//!   5. `$CWD/skills/`
//!
//! Step 4 is the reason this module exists in the shape it does. Skills are
//! Anthropic's Agent Skills format, and the ones worth exposing are usually
//! already written for Claude Code. Requiring a copy would mean two versions
//! drifting apart; requiring a symlink would mean a feature that looks broken
//! until you read a doc.
//!
//! **What ports and what does not.** A skill that is a procedure plus its own
//! `scripts/` works anywhere. A skill whose steps are "use Read, then Grep,
//! then call another skill" assumes the Claude Code harness, and an assistant
//! reached over MCP has none of that. dotagent serves the text; it cannot
//! conjure the tools the text asks for.
//!
//! ## Path containment
//!
//! `skill-read` and `skill-run` take a caller-supplied relative path, which
//! makes this module a security boundary. Every resolution goes through
//! [`resolve_within`]: absolute paths and `..` are refused before touching the
//! disk, and the canonicalized result must still sit under the canonicalized
//! skill directory — which is what stops a symlink from escaping.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use dotagent_core::{SkillManifest, SKILL_FILE};

/// Files listed for one skill. A cap, not a promise: a skill with hundreds of
/// reference files would otherwise turn its own index into the context problem
/// skills exist to avoid.
const MAX_LISTED_FILES: usize = 100;

/// How deep the file index walks. `references/databricks/companies/x.md` is 3;
/// anything deeper is unusual enough to be worth not paying for on every call.
const MAX_LIST_DEPTH: usize = 4;

/// Directories never worth indexing — noise at best, huge at worst.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "__pycache__", ".venv"];

/// A loaded skill paired with the directory it came from.
#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    pub manifest: SkillManifest,
    pub dir: PathBuf,
}

/// A `SKILL.md` that exists but could not be loaded.
#[derive(Debug, Clone)]
pub struct InvalidSkill {
    pub path: PathBuf,
    /// Human-readable failure. Never a `Debug` dump — this reaches `doctor`.
    pub error: String,
}

#[derive(Debug, Default)]
pub struct SkillDiscovery {
    pub skills: Vec<DiscoveredSkill>,
    pub invalid: Vec<InvalidSkill>,
}

/// Scan every root, keeping going past a broken `SKILL.md`.
///
/// One unparseable skill must not empty the catalog: the failure mode would be
/// an assistant that quietly forgets every procedure it knows and answers from
/// nothing instead. Callers surface `invalid` — `doctor` prints it.
pub fn discover() -> SkillDiscovery {
    let cfg = dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .skills;
    discover_in(&search_roots(&cfg))
}

/// Scan the given roots. Split out so tests drive the real loop against temp
/// directories instead of re-implementing it.
pub fn discover_in(roots: &[PathBuf]) -> SkillDiscovery {
    let mut out = SkillDiscovery::default();
    let mut seen: HashSet<String> = HashSet::new();
    for root in roots {
        scan_root(root, &mut out, &mut seen);
    }
    out
}

fn scan_root(root: &Path, out: &mut SkillDiscovery, seen: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    // Directory order is filesystem-defined; sort so "first one wins" on a
    // name collision is reproducible rather than a coin flip.
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();

    for dir in dirs {
        let file = dir.join(SKILL_FILE);
        if !file.is_file() {
            continue;
        }
        match SkillManifest::load(&file) {
            Ok(manifest) => {
                if seen.insert(manifest.name.clone()) {
                    out.skills.push(DiscoveredSkill { manifest, dir });
                }
            }
            Err(e) => out.invalid.push(InvalidSkill {
                path: file,
                error: e.to_string(),
            }),
        }
    }
}

pub fn find_by_name(name: &str) -> Result<DiscoveredSkill> {
    discover()
        .skills
        .into_iter()
        .find(|s| s.manifest.name == name)
        .ok_or_else(|| anyhow!("skill not found: {name}"))
}

fn search_roots(cfg: &dotagent_core::SkillsConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let push = |p: PathBuf, roots: &mut Vec<PathBuf>| {
        if p.is_dir() && !roots.contains(&p) {
            roots.push(p);
        }
    };

    for p in &cfg.paths {
        if !p.trim().is_empty() {
            push(PathBuf::from(p.trim()), &mut roots);
        }
    }
    if let Ok(env_root) = std::env::var("DOTAGENT_ROOT") {
        for p in env_root.split(':').filter(|p| !p.is_empty()) {
            // Both, because a root is sometimes a directory of skills and
            // sometimes a repo that keeps them one level down.
            push(PathBuf::from(p).join("skills"), &mut roots);
            push(PathBuf::from(p), &mut roots);
        }
    }
    push(dotagent_state::paths::skills_dir(), &mut roots);
    if cfg.claude_skills {
        if let Some(home) = dirs::home_dir() {
            push(home.join(".claude/skills"), &mut roots);
        }
        if let Ok(cwd) = std::env::current_dir() {
            push(cwd.join(".claude/skills"), &mut roots);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        push(cwd.join("skills"), &mut roots);
    }
    roots
}

impl DiscoveredSkill {
    /// Supporting files next to `SKILL.md`, as paths relative to the skill
    /// directory.
    ///
    /// Returned with the body so a procedure that says "see
    /// `references/glossary.md`" is followed by something the model can
    /// actually ask for. Without this, progressive disclosure across files —
    /// how every non-trivial Claude Code skill is written — fails silently:
    /// the model reads half a procedure and proceeds as if it had all of it.
    pub fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(&self.dir, &self.dir, 0, &mut out);
        out.sort();
        out.truncate(MAX_LISTED_FILES);
        out
    }

    /// Resolve a readable file inside the skill.
    pub fn resolve_readable(&self, rel: &str) -> Result<PathBuf> {
        let path = resolve_within(&self.dir, rel)?;
        if !path.is_file() {
            bail!("not a file: {rel}");
        }
        Ok(path)
    }

    /// Resolve an executable inside the skill's `scripts/` directory.
    ///
    /// Two restrictions beyond containment, both narrowing what a model can
    /// reach for. `scripts/` only: a skill's `references/` are documents, and
    /// "run this document" is never the intent. Executable bit required: it is
    /// the author saying which files are entry points, and honoring it means
    /// dropping a helper into the directory does not silently make it callable.
    pub fn resolve_script(&self, rel: &str) -> Result<PathBuf> {
        let path = resolve_within(&self.dir, rel)?;
        let scripts = self.dir.join("scripts");
        let scripts = scripts.canonicalize().unwrap_or(scripts);
        if !path.starts_with(&scripts) {
            bail!("scripts must live under scripts/ — refusing {rel}");
        }
        if !path.is_file() {
            bail!("not a file: {rel}");
        }
        if !is_executable(&path) {
            bail!("{rel} is not executable — chmod +x it to make it callable");
        }
        Ok(path)
    }
}

/// Join `rel` onto `dir` without letting it escape.
///
/// Rejects absolute paths and any `..` component before touching the disk,
/// then canonicalizes and re-checks the prefix — the second step is what
/// catches a symlink pointing outside the skill.
pub fn resolve_within(dir: &Path, rel: &str) -> Result<PathBuf> {
    let rel = rel.trim();
    if rel.is_empty() {
        bail!("no path given");
    }
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        bail!("path must be relative to the skill directory: {rel}");
    }
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("path escapes the skill directory: {rel}"),
        }
    }

    let base = dir
        .canonicalize()
        .map_err(|e| anyhow!("skill directory unreadable: {e}"))?;
    let joined = base.join(candidate);
    let resolved = joined
        .canonicalize()
        .map_err(|_| anyhow!("no such file in this skill: {rel}"))?;
    if !resolved.starts_with(&base) {
        bail!("path escapes the skill directory: {rel}");
    }
    Ok(resolved)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn walk(base: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_LIST_DEPTH || out.len() >= MAX_LISTED_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        if path.is_dir() {
            walk(base, &path, depth + 1, out);
        } else if name != SKILL_FILE {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\nname: alpha\ndescription: Does alpha.\n---\nStep one.\n";
    const ALSO_GOOD: &str = "---\nname: beta\ndescription: Does beta.\n---\nStep one.\n";
    /// Parses as frontmatter but fails validation (no description).
    const INVALID: &str = "---\nname: gamma\n---\nStep one.\n";
    /// No frontmatter at all — a plain markdown file that happens to be named
    /// SKILL.md.
    const BROKEN: &str = "# just markdown\n";

    fn root_with(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in entries {
            let sub = dir.path().join(name);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(SKILL_FILE), body).unwrap();
        }
        dir
    }

    #[test]
    fn loads_skills_from_a_root() {
        let dir = root_with(&[("a", GOOD), ("b", ALSO_GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.skills.len(), 2);
        assert!(found.invalid.is_empty());
    }

    #[test]
    fn one_broken_skill_does_not_hide_the_healthy_ones() {
        let dir = root_with(&[("a", GOOD), ("b", BROKEN), ("c", ALSO_GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.skills.len(), 2, "healthy skills must survive");
        assert_eq!(found.invalid.len(), 1);
    }

    #[test]
    fn a_skill_that_fails_validation_counts_as_invalid() {
        let dir = root_with(&[("a", GOOD), ("b", INVALID)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.invalid.len(), 1);
        assert!(found.invalid[0].error.contains("description"));
        assert!(
            !found.invalid[0].error.contains("Error {"),
            "no Debug dumps"
        );
    }

    #[test]
    fn a_directory_without_skill_md_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("not-a-skill")).unwrap();
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert!(found.skills.is_empty());
        assert!(found.invalid.is_empty());
    }

    #[test]
    fn the_first_root_wins_a_name_collision() {
        // The precedence rule config depends on: `[skills] paths` is searched
        // before ~/.claude/skills precisely so a local override sticks.
        let first = root_with(&[("a", GOOD)]);
        let second = tempfile::tempdir().unwrap();
        let sub = second.path().join("elsewhere");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join(SKILL_FILE),
            "---\nname: alpha\ndescription: Shadowed.\n---\nOther.\n",
        )
        .unwrap();

        let found = discover_in(&[first.path().to_path_buf(), second.path().to_path_buf()]);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].manifest.description, "Does alpha.");
    }

    #[test]
    fn files_lists_supporting_documents_but_not_skill_md() {
        let dir = root_with(&[("a", GOOD)]);
        let skill_dir = dir.path().join("a");
        std::fs::create_dir_all(skill_dir.join("references/deep")).unwrap();
        std::fs::write(skill_dir.join("references/glossary.md"), "g").unwrap();
        std::fs::write(skill_dir.join("references/deep/more.md"), "m").unwrap();
        std::fs::create_dir_all(skill_dir.join(".git")).unwrap();
        std::fs::write(skill_dir.join(".git/config"), "x").unwrap();

        let found = discover_in(&[dir.path().to_path_buf()]);
        let files = found.skills[0].files();
        assert!(files.contains(&"references/glossary.md".to_string()));
        assert!(files.contains(&"references/deep/more.md".to_string()));
        assert!(!files.iter().any(|f| f.contains(SKILL_FILE)));
        assert!(!files.iter().any(|f| f.contains(".git")), "{files:?}");
    }

    // --- containment. Deny cases dominate on purpose: this is the boundary
    // between "a model picked a path" and "a file left the directory". ---

    #[test]
    fn resolve_within_accepts_a_plain_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.md"), "x").unwrap();
        assert!(resolve_within(dir.path(), "ok.md").is_ok());
        assert!(resolve_within(dir.path(), "./ok.md").is_ok());
    }

    #[test]
    fn resolve_within_refuses_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.md"), "x").unwrap();
        for bad in [
            "../secret",
            "a/../../secret",
            "..",
            "./../x",
            "references/../../x",
        ] {
            let err = resolve_within(dir.path(), bad).unwrap_err().to_string();
            assert!(err.contains("escapes"), "{bad} → {err}");
        }
    }

    #[test]
    fn resolve_within_refuses_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_within(dir.path(), "/etc/passwd")
            .unwrap_err()
            .to_string();
        assert!(err.contains("relative"), "{err}");
    }

    #[test]
    fn resolve_within_refuses_an_empty_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_within(dir.path(), "   ").is_err());
    }

    #[test]
    fn resolve_within_reports_a_missing_file_without_leaking_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_within(dir.path(), "nope.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such file"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_within_refuses_a_symlink_that_escapes() {
        // The case component checks cannot catch: a well-formed relative path
        // whose target is outside.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "s").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();

        let err = resolve_within(dir.path(), "link").unwrap_err().to_string();
        assert!(err.contains("escapes"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_within_allows_a_symlink_that_stays_inside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.md"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.md"), dir.path().join("alias.md"))
            .unwrap();
        assert!(resolve_within(dir.path(), "alias.md").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_script_requires_scripts_dir_and_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = root_with(&[("a", GOOD)]);
        let skill_dir = dir.path().join("a");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::write(skill_dir.join("scripts/run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(skill_dir.join("scripts/helper.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            skill_dir.join("scripts/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        // A document, in the right place but not an entry point.
        std::fs::write(skill_dir.join("notes.md"), "x").unwrap();

        let found = discover_in(&[dir.path().to_path_buf()]);
        let skill = &found.skills[0];

        assert!(skill.resolve_script("scripts/run.sh").is_ok());

        let err = skill
            .resolve_script("scripts/helper.sh")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not executable"), "{err}");

        let err = skill.resolve_script("notes.md").unwrap_err().to_string();
        assert!(err.contains("scripts/"), "{err}");

        // Readable, though — the two verbs answer differently on purpose.
        assert!(skill.resolve_readable("notes.md").is_ok());
    }
}
