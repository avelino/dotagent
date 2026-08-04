//! Manifest discovery.
//!
//! dotagent searches for `agent.toml` files in:
//!   1. `$DOTAGENT_ROOT` (colon-separated list of directories) — only for
//!      one-off overrides (testing, CI). Daily use should put manifests
//!      under the standard root below.
//!   2. `$DOTAGENT_HOME/agents/` (default `~/.config/dotagent/agents/`)
//!   3. `$CWD/agents/`
//!   4. `$CWD` (each direct subdirectory)
//!
//! Each manifest is loaded once and cached by `agent.name`.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use dotagent_core::AgentManifest;

/// A loaded manifest paired with the directory it came from.
pub struct DiscoveredAgent {
    pub manifest: AgentManifest,
    pub dir: PathBuf,
}

/// A manifest that exists on disk but could not be loaded.
#[derive(Debug, Clone)]
pub struct InvalidManifest {
    pub path: PathBuf,
    /// Human-readable parse / validation failure. Never a `Debug` dump.
    pub error: String,
}

/// Everything a scan found: the agents that loaded, and the ones that didn't.
#[derive(Default)]
pub struct Discovery {
    pub agents: Vec<DiscoveredAgent>,
    pub invalid: Vec<InvalidManifest>,
}

/// Scan the search roots, keeping going past a broken manifest.
///
/// **One bad `agent.toml` must not take the others down with it.** Aborting
/// the scan is how a single typo silently stops *all* scheduling: the daemon
/// gets an empty agent list, dispatches nothing, and the only trace is one
/// `warn!` line. Callers are expected to surface `invalid` — `doctor` prints
/// it, the daemon audits it as `manifest_invalid` (Critical), and the MCP
/// server refuses to serve a catalog that silently lost entries.
pub fn discover() -> Discovery {
    let mut out = Discovery::default();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for root in search_roots() {
        scan_root(&root, &mut out, &mut seen);
    }
    out
}

/// Scan one root, appending to `out`. Split out so tests exercise the real
/// loop against a temp directory instead of re-implementing it — a test that
/// duplicates the logic passes even when the logic regresses.
fn scan_root(
    root: &std::path::Path,
    out: &mut Discovery,
    seen: &mut std::collections::HashSet<String>,
) {
    if !root.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let manifest_path = path.join("agent.toml");
        if !manifest_path.is_file() {
            continue;
        }
        match AgentManifest::load(&manifest_path) {
            Ok(manifest) => {
                if seen.insert(manifest.agent.name.clone()) {
                    out.agents.push(DiscoveredAgent {
                        manifest,
                        dir: path,
                    });
                }
            }
            Err(e) => out.invalid.push(InvalidManifest {
                path: manifest_path,
                error: e.to_string(),
            }),
        }
    }
}

/// Find every agent manifest that loads cleanly.
///
/// Broken manifests are dropped silently here — use [`discover`] when the
/// caller can report them, which is nearly always.
pub fn discover_all() -> Result<Vec<DiscoveredAgent>> {
    Ok(discover().agents)
}

/// Find a single manifest by `agent.name`.
///
/// A broken manifest elsewhere in the search path does not hide a healthy
/// agent, but if the *requested* name is nowhere to be found and something
/// failed to parse, the error says so — otherwise "agent not found" sends you
/// looking in the wrong place.
pub fn find_by_name(name: &str) -> Result<DiscoveredAgent> {
    let found = discover();
    if let Some(agent) = found
        .agents
        .into_iter()
        .find(|d| d.manifest.agent.name == name)
    {
        return Ok(agent);
    }
    if found.invalid.is_empty() {
        return Err(anyhow!("agent not found: {name}"));
    }
    let paths: Vec<String> = found
        .invalid
        .iter()
        .map(|i| i.path.display().to_string())
        .collect();
    Err(anyhow!(
        "agent not found: {name} ({} manifest(s) failed to load and may be the one you meant: {})",
        paths.len(),
        paths.join(", ")
    ))
}

fn search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(env_root) = std::env::var("DOTAGENT_ROOT") {
        for p in env_root.split(':') {
            if !p.is_empty() {
                roots.push(PathBuf::from(p));
            }
        }
    }
    roots.push(dotagent_state::paths::agents_dir());
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("agents"));
        roots.push(cwd);
    }
    roots
}

/// Resolve `name` to a schedule within a manifest. Errors if the schedule id
/// is not declared.
pub fn schedule_by_id<'a>(
    manifest: &'a AgentManifest,
    schedule_id: &str,
) -> Result<&'a dotagent_core::Schedule> {
    manifest
        .schedules
        .iter()
        .find(|s| s.id() == schedule_id)
        .ok_or_else(|| anyhow!("schedule id not found: {schedule_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a search root with the given `<dir>/agent.toml` contents.
    fn root_with(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in entries {
            let sub = dir.path().join(name);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("agent.toml"), body).unwrap();
        }
        dir
    }

    const GOOD: &str = r#"
[agent]
name = "good"
[run]
command = "true"
"#;

    const ALSO_GOOD: &str = r#"
[agent]
name = "also-good"
[run]
command = "true"
"#;

    /// Not valid TOML.
    const BROKEN: &str = "[agent\nname = ";

    /// Parses, but fails `AgentManifest::validate` (empty name).
    const INVALID: &str = r#"
[agent]
name = ""
[run]
command = "true"
"#;

    /// `discover` reads `DOTAGENT_ROOT`, which tests must not mutate (parallel
    /// tests + the modern `set_var` contract). Drive the real `scan_root`
    /// against a temp directory instead.
    fn scan(root: &std::path::Path) -> Discovery {
        let mut out = Discovery::default();
        let mut seen = std::collections::HashSet::new();
        scan_root(root, &mut out, &mut seen);
        out
    }

    #[test]
    fn one_broken_manifest_does_not_hide_the_healthy_ones() {
        // The regression this whole change exists for: before, a single typo
        // aborted the scan and the daemon saw zero agents.
        let dir = root_with(&[("a", GOOD), ("b", BROKEN), ("c", ALSO_GOOD)]);
        let found = scan(dir.path());
        assert_eq!(found.agents.len(), 2, "healthy agents must survive");
        assert_eq!(found.invalid.len(), 1);
    }

    #[test]
    fn a_manifest_that_fails_validation_counts_as_invalid() {
        // Parses as TOML but `validate()` rejects it — must not be silently
        // dropped as if it did not exist.
        let dir = root_with(&[("a", GOOD), ("b", INVALID)]);
        let found = scan(dir.path());
        assert_eq!(found.agents.len(), 1);
        assert_eq!(found.invalid.len(), 1);
    }

    #[test]
    fn invalid_entry_carries_path_and_a_readable_error() {
        let dir = root_with(&[("b", BROKEN)]);
        let found = scan(dir.path());
        let bad = &found.invalid[0];
        assert!(bad.path.ends_with("b/agent.toml"), "{:?}", bad.path);
        assert!(!bad.error.is_empty());
        // Never a Debug dump — this reaches the audit log and `doctor`.
        assert!(!bad.error.contains("Error {"), "{}", bad.error);
    }

    #[test]
    fn all_broken_yields_no_agents_but_a_populated_error_list() {
        // The dangerous shape: zero agents. It must be distinguishable from
        // "no agents installed", which is why `invalid` is non-empty here.
        let dir = root_with(&[("a", BROKEN), ("b", INVALID)]);
        let found = scan(dir.path());
        assert!(found.agents.is_empty());
        assert_eq!(found.invalid.len(), 2);
    }

    #[test]
    fn empty_root_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let found = scan(dir.path());
        assert!(found.agents.is_empty());
        assert!(found.invalid.is_empty());
    }

    #[test]
    fn directories_without_a_manifest_are_skipped_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("not-an-agent")).unwrap();
        let found = scan(dir.path());
        assert!(found.agents.is_empty());
        assert!(
            found.invalid.is_empty(),
            "a plain directory is not a failure"
        );
    }

    #[test]
    fn duplicate_names_keep_the_first_and_do_not_error() {
        let dir = root_with(&[("a", GOOD), ("b", GOOD)]);
        let found = scan(dir.path());
        assert_eq!(found.agents.len(), 1);
        assert!(found.invalid.is_empty());
    }
}
