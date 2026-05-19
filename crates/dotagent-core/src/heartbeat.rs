//! Heartbeat — per-run state file written before and after agent execution.
//!
//! Path: `~/.local/state/dotagent/agents/{name}/{slug}.heartbeat.json`.
//!
//! Shape is intentionally compatible with the legacy `lib/agent.fish`
//! heartbeat — `last_success_at` is preserved across runs and never overwritten
//! when a run fails, so the orchestrator can answer "did this schedule
//! complete successfully on or after its expected window?".

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub name: String,
    pub slug: String,
    pub args: Vec<String>,

    pub started_at: i64,
    pub started_at_iso: String,

    #[serde(default)]
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub finished_at_iso: Option<String>,

    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_seconds: Option<i64>,

    #[serde(default)]
    pub last_success_at: Option<i64>,
    #[serde(default)]
    pub last_success_at_iso: Option<String>,
}

impl Heartbeat {
    /// Whether the most recent run finished successfully (exit code 0).
    pub fn is_last_run_success(&self) -> bool {
        matches!(self.exit_code, Some(0))
    }

    /// Whether this heartbeat indicates the agent is currently running.
    pub fn is_running(&self) -> bool {
        self.finished_at.is_none()
    }
}
