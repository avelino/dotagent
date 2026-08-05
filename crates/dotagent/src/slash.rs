//! Command discovery.
//!
//! Named `slash` rather than `commands` because `crate::commands` is already
//! the CLI's subcommand module. The domain word is "command" everywhere a user
//! can see it; the file name gets out of the way.
//!
//! A command is a single `.md` file. dotagent searches, in order:
//!
//!   1. `[commands] paths` from `config.toml` — extra roots, first so they win
//!   2. `$DOTAGENT_ROOT` (and `$DOTAGENT_ROOT/commands`) — overrides for testing
//!   3. `$DOTAGENT_HOME/commands/` (default `~/.config/dotagent/commands/`)
//!   4. `~/.claude/commands/` and `$CWD/.claude/commands/` — only when
//!      `[commands] claude_commands = true`
//!   5. `$CWD/commands/`
//!
//! Step 4 is opt-in, which is the one place this module deliberately differs
//! from [`crate::skills`]. A skill costs a line in a list until a model judges
//! it relevant. A command is *published as a menu*, and a Claude Code catalog is
//! usually full of things that assume a shell (`/apply` switching a Nix
//! profile). Menu entries that cannot work are worse than absent ones.
//!
//! ## Two name mappings
//!
//! One command carries two derived names, and they are lossy in different ways:
//!
//! | | `commit-message` | collides with |
//! |---|---|---|
//! | MCP | `command-commit-message` | `commit.message` |
//! | Telegram | `commit_message` | `commit_message` |
//!
//! So the catalog is deduped **twice**. A pair that survives the MCP check can
//! still collide on Telegram, and registering both would let the menu resolve
//! to a command nobody picked.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use dotagent_core::{CommandManifest, COMMAND_EXT, NAMESPACE_SEP};

/// How deep the scan walks. `git/status.md` is depth 1; a catalog nested deeper
/// than this is organizing something other than commands.
const MAX_DEPTH: usize = 3;

/// Directories never worth scanning.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "__pycache__", ".venv"];

/// A loaded command paired with the file it came from.
#[derive(Debug, Clone)]
pub struct DiscoveredCommand {
    pub manifest: CommandManifest,
    pub path: PathBuf,
}

impl DiscoveredCommand {
    /// `[hint] — description`, the half Telegram renders beside the name it
    /// already knows. Also the tail of [`Self::menu_line`].
    pub fn summary(&self) -> String {
        let hint = self
            .manifest
            .argument_hint
            .as_deref()
            .map(|h| format!("{h} — "))
            .unwrap_or_default();
        format!("{hint}{}", self.manifest.description.trim())
    }

    /// `/name [hint] — description`, for a list rendered as plain text.
    ///
    /// Not `format!("/{telegram} {}", self.summary())`: without a hint that
    /// would read `/standup What ran overnight`, losing the dash that separates
    /// the name from what it does.
    ///
    /// Takes the Telegram spelling rather than deriving it — the caller got it
    /// from the deduped menu, and re-deriving here could print a name belonging
    /// to a command that lost the collision.
    pub fn menu_line(&self, telegram: &str) -> String {
        let hint = self
            .manifest
            .argument_hint
            .as_deref()
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        format!("/{telegram}{hint} — {}", self.manifest.description.trim())
    }
}

/// A `.md` that sits in a command root but could not be loaded.
#[derive(Debug, Clone)]
pub struct InvalidCommand {
    pub path: PathBuf,
    /// Human-readable failure. Never a `Debug` dump — this reaches `doctor`.
    pub error: String,
}

#[derive(Debug, Default)]
pub struct CommandDiscovery {
    pub commands: Vec<DiscoveredCommand>,
    pub invalid: Vec<InvalidCommand>,
}

impl CommandDiscovery {
    /// Commands that can actually be registered with Telegram, paired with the
    /// name they register under.
    ///
    /// Deduped on the Telegram name specifically: `weekly-numbers` and
    /// `weekly_numbers` both want `/weekly_numbers`, and the menu must resolve
    /// to exactly one of them. First discovered wins, matching every other
    /// collision rule in the project.
    pub fn telegram_menu(&self) -> Vec<(String, &DiscoveredCommand)> {
        let mut out: Vec<(String, &DiscoveredCommand)> = Vec::new();
        let mut taken: HashSet<String> = HashSet::new();
        for cmd in &self.commands {
            // Validation already refused the names Telegram would reject, so a
            // failure here means the catalog was built by another path.
            let Ok(tg) = dotagent_core::command::telegram_name(&cmd.manifest.name) else {
                continue;
            };
            if !taken.insert(tg.clone()) {
                tracing::warn!(
                    command = %cmd.manifest.name,
                    telegram = %tg,
                    "telegram command name collides with an earlier command — skipping"
                );
                continue;
            }
            out.push((tg, cmd));
        }
        out
    }

    /// Telegram names that more than one command wants, each paired with the
    /// commands that want it, in discovery order — so the first is the winner.
    ///
    /// Returns the commands rather than their names because the only useful
    /// question a collision raises is *which file to rename*, and a caller
    /// handed bare names would have to look each one up again.
    pub fn telegram_collisions(&self) -> HashMap<String, Vec<&DiscoveredCommand>> {
        let mut by_name: HashMap<String, Vec<&DiscoveredCommand>> = HashMap::new();
        for cmd in &self.commands {
            if let Ok(tg) = dotagent_core::command::telegram_name(&cmd.manifest.name) {
                by_name.entry(tg).or_default().push(cmd);
            }
        }
        by_name.retain(|_, cmds| cmds.len() > 1);
        by_name
    }

    pub fn find(&self, name: &str) -> Option<&DiscoveredCommand> {
        self.commands.iter().find(|c| c.manifest.name == name)
    }

    /// `/a, /b` — what to offer someone who named something that is not here.
    /// Empty when nothing is installed, which callers phrase differently.
    pub fn installed(&self) -> String {
        self.telegram_menu()
            .into_iter()
            .map(|(tg, _)| format!("/{tg}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Resolve what a sender typed. Accepts the catalog name (`commit-message`)
    /// and the Telegram form (`commit_message`), because those are two spellings
    /// of one thing and a user typing the one the menu showed must not miss.
    pub fn resolve(&self, typed: &str) -> Option<&DiscoveredCommand> {
        if let Some(exact) = self.find(typed) {
            return Some(exact);
        }
        let typed = dotagent_core::command::telegram_name(typed).ok()?;
        self.telegram_menu()
            .into_iter()
            .find(|(tg, _)| tg == &typed)
            .map(|(_, cmd)| cmd)
    }
}

/// Scan every root, keeping going past a broken file.
///
/// One unparseable command must not empty the catalog: the failure mode would
/// be a menu that silently loses entries, which reads as "that command was
/// removed" rather than "that command is broken". `doctor` surfaces `invalid`.
pub fn discover() -> CommandDiscovery {
    let cfg = dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .commands;
    if !cfg.enabled {
        return CommandDiscovery::default();
    }
    discover_in(&search_roots(&cfg))
}

/// Scan the given roots. Split out so tests drive the real loop against temp
/// directories instead of re-implementing it.
pub fn discover_in(roots: &[PathBuf]) -> CommandDiscovery {
    let mut out = CommandDiscovery::default();
    let mut seen: HashSet<String> = HashSet::new();
    for root in roots {
        scan(root, root, 0, &mut out, &mut seen);
    }
    out
}

fn scan(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut CommandDiscovery,
    seen: &mut HashSet<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Directory order is filesystem-defined; sort so "first one wins" on a
    // name collision is reproducible rather than a coin flip.
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if file_name.starts_with('.') || SKIP_DIRS.contains(&file_name.as_str()) {
            continue;
        }
        if path.is_dir() {
            scan(root, &path, depth + 1, out, seen);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some(COMMAND_EXT) {
            continue;
        }
        // A skill directory reached through a shared root would otherwise
        // register SKILL.md as a command named "SKILL".
        if file_name == dotagent_core::SKILL_FILE {
            continue;
        }
        let Some(name) = name_from_path(root, &path) else {
            continue;
        };

        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                out.invalid.push(InvalidCommand {
                    path,
                    error: e.to_string(),
                });
                continue;
            }
        };
        match CommandManifest::parse(&raw, &name) {
            Ok(manifest) => {
                if seen.insert(manifest.name.clone()) {
                    out.commands.push(DiscoveredCommand { manifest, path });
                }
            }
            Err(e) => out.invalid.push(InvalidCommand {
                path,
                error: e.to_string(),
            }),
        }
    }
}

/// `git/status.md` under root → `git:status`. Claude Code's namespacing.
fn name_from_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let stem = rel.file_stem()?.to_string_lossy().to_string();
    let parents: Vec<String> = rel
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    if parents.is_empty() {
        return Some(stem);
    }
    let mut name = parents.join(&NAMESPACE_SEP.to_string());
    name.push(NAMESPACE_SEP);
    name.push_str(&stem);
    Some(name)
}

fn search_roots(cfg: &dotagent_core::CommandsConfig) -> Vec<PathBuf> {
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
            // Only the subdirectory, unlike skills — which also scan the root
            // itself. A skill must be a directory holding a SKILL.md, so
            // scanning a repo root finds nothing by accident. A command is any
            // `.md`, so doing the same here would turn a repo's README into a
            // broken command and fill `doctor` with noise.
            push(PathBuf::from(p).join("commands"), &mut roots);
        }
    }
    push(dotagent_state::paths::commands_dir(), &mut roots);
    if cfg.claude_commands {
        if let Some(home) = dirs::home_dir() {
            push(home.join(".claude/commands"), &mut roots);
        }
        if let Ok(cwd) = std::env::current_dir() {
            push(cwd.join(".claude/commands"), &mut roots);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        push(cwd.join("commands"), &mut roots);
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\ndescription: Does alpha.\n---\nThe prompt.\n";
    const ALSO_GOOD: &str = "---\ndescription: Does beta.\n---\nThe prompt.\n";
    /// Parses as frontmatter but fails validation (no description).
    const INVALID: &str = "---\nname: gamma\n---\nThe prompt.\n";
    /// No frontmatter at all.
    const BROKEN: &str = "# just markdown\n";

    fn root_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, body) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir
    }

    #[test]
    fn loads_commands_and_names_them_from_the_filename() {
        let dir = root_with(&[("commit-message.md", GOOD), ("weekly.md", ALSO_GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        let names: Vec<&str> = found
            .commands
            .iter()
            .map(|c| c.manifest.name.as_str())
            .collect();
        assert_eq!(names, vec!["commit-message", "weekly"]);
        assert!(found.invalid.is_empty());
    }

    #[test]
    fn a_subdirectory_is_namespaced_with_a_colon() {
        let dir = root_with(&[("git/status.md", GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.commands[0].manifest.name, "git:status");
    }

    #[test]
    fn one_broken_command_does_not_hide_the_healthy_ones() {
        let dir = root_with(&[("a.md", GOOD), ("b.md", BROKEN), ("c.md", ALSO_GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.commands.len(), 2, "healthy commands must survive");
        assert_eq!(found.invalid.len(), 1);
    }

    #[test]
    fn a_command_that_fails_validation_counts_as_invalid() {
        let dir = root_with(&[("a.md", GOOD), ("b.md", INVALID)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.commands.len(), 1);
        assert_eq!(found.invalid.len(), 1);
        assert!(found.invalid[0].error.contains("description"));
        assert!(
            !found.invalid[0].error.contains("Error {"),
            "no Debug dumps"
        );
    }

    #[test]
    fn non_markdown_files_are_ignored() {
        let dir = root_with(&[("a.md", GOOD), ("README.txt", GOOD), ("script.sh", GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.commands.len(), 1);
        assert!(found.invalid.is_empty(), "not-a-command is not a failure");
    }

    #[test]
    fn a_skill_file_sharing_a_root_is_not_a_command() {
        // Otherwise a root holding both would publish a command named "SKILL".
        let dir = root_with(&[("triage/SKILL.md", GOOD), ("a.md", GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.commands.len(), 1);
        assert_eq!(found.commands[0].manifest.name, "a");
    }

    #[test]
    fn the_first_root_wins_a_name_collision() {
        let first = root_with(&[("a.md", GOOD)]);
        let second = root_with(&[("a.md", ALSO_GOOD)]);
        let found = discover_in(&[first.path().to_path_buf(), second.path().to_path_buf()]);
        assert_eq!(found.commands.len(), 1);
        assert_eq!(found.commands[0].manifest.description, "Does alpha.");
    }

    // --- the second dedupe, which the MCP mapping does not need ---

    #[test]
    fn two_commands_wanting_one_telegram_name_register_once() {
        let dir = root_with(&[
            ("weekly-numbers.md", GOOD),
            ("weekly_numbers.md", ALSO_GOOD),
        ]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.commands.len(), 2, "distinct as catalog entries");

        let menu = found.telegram_menu();
        assert_eq!(menu.len(), 1, "but one menu entry: {menu:?}");
        assert_eq!(menu[0].0, "weekly_numbers");

        let collisions = found.telegram_collisions();
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions["weekly_numbers"].len(), 2);
    }

    #[test]
    fn menu_line_keeps_its_dash_with_or_without_a_hint() {
        // The regression `format!("/{tg} {}", summary())` would introduce:
        // without a hint it reads "/a Does alpha." and the name runs into the
        // description.
        let dir = root_with(&[
            ("a.md", GOOD),
            (
                "b.md",
                "---\ndescription: Does beta.\nargument-hint: \"[path]\"\n---\nBody.\n",
            ),
        ]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        let line = |n: &str, tg: &str| found.find(n).unwrap().menu_line(tg);

        assert_eq!(line("a", "a"), "/a — Does alpha.");
        assert_eq!(line("b", "b"), "/b [path] — Does beta.");

        // Telegram renders the name itself, so the description drops it.
        assert_eq!(found.find("a").unwrap().summary(), "Does alpha.");
        assert_eq!(found.find("b").unwrap().summary(), "[path] — Does beta.");
    }

    #[test]
    fn installed_lists_the_menu_and_is_empty_when_there_is_none() {
        let dir = root_with(&[("a.md", GOOD), ("b.md", ALSO_GOOD)]);
        assert_eq!(
            discover_in(&[dir.path().to_path_buf()]).installed(),
            "/a, /b"
        );
        assert_eq!(CommandDiscovery::default().installed(), "");
    }

    #[test]
    fn a_clean_catalog_reports_no_collisions() {
        let dir = root_with(&[("a.md", GOOD), ("b.md", ALSO_GOOD)]);
        assert!(discover_in(&[dir.path().to_path_buf()])
            .telegram_collisions()
            .is_empty());
    }

    #[test]
    fn resolve_accepts_both_the_catalog_name_and_the_telegram_form() {
        // What the menu shows is `commit_message`; what the file is called is
        // `commit-message`. Typing either must land on the same command.
        let dir = root_with(&[("commit-message.md", GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert!(found.resolve("commit-message").is_some());
        assert!(found.resolve("commit_message").is_some());
        assert!(found.resolve("nope").is_none());
    }

    #[test]
    fn a_command_telegram_would_refuse_never_reaches_the_catalog() {
        let long = format!("{}.md", "a".repeat(33));
        let dir = root_with(&[(long.as_str(), GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        assert!(found.commands.is_empty());
        assert_eq!(found.invalid.len(), 1, "and it is reported, not dropped");
        assert!(found.invalid[0].error.contains("32"));
    }

    #[test]
    fn nesting_deeper_than_the_cap_is_not_scanned() {
        let dir = root_with(&[("a/b/c/d/deep.md", GOOD), ("top.md", GOOD)]);
        let found = discover_in(&[dir.path().to_path_buf()]);
        let names: Vec<&str> = found
            .commands
            .iter()
            .map(|c| c.manifest.name.as_str())
            .collect();
        assert_eq!(names, vec!["top"]);
    }
}
