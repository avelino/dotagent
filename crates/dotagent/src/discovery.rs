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

use anyhow::{anyhow, Context, Result};
use dotagent_core::AgentManifest;

/// A loaded manifest paired with the directory it came from.
pub struct DiscoveredAgent {
    pub manifest: AgentManifest,
    pub dir: PathBuf,
}

/// Find every agent manifest reachable from the default search roots.
pub fn discover_all() -> Result<Vec<DiscoveredAgent>> {
    let roots = search_roots();
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&root) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let manifest_path = path.join("agent.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest = AgentManifest::load(&manifest_path)
                .with_context(|| format!("loading {}", manifest_path.display()))?;
            if seen.insert(manifest.agent.name.clone()) {
                out.push(DiscoveredAgent {
                    manifest,
                    dir: path,
                });
            }
        }
    }
    Ok(out)
}

/// Find a single manifest by `agent.name`.
pub fn find_by_name(name: &str) -> Result<DiscoveredAgent> {
    let all = discover_all()?;
    all.into_iter()
        .find(|d| d.manifest.agent.name == name)
        .ok_or_else(|| anyhow!("agent not found: {name}"))
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
