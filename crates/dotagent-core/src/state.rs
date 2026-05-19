//! Window state — per-(agent, schedule, window) state file used by the
//! orchestrator tick to drive retry/backoff.
//!
//! Path: `~/.local/state/dotagent/windows/{name}-{slug}-{YYYY-MM-DD-HHMM}.json`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowState {
    pub agent: String,
    pub schedule_id: String,

    /// Expected time of this window (epoch seconds).
    pub expected_at: i64,

    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_attempt_at: Option<i64>,
    #[serde(default)]
    pub last_attempt_exit_code: Option<i32>,
    #[serde(default)]
    pub last_attempt_stderr: Option<String>,

    #[serde(default)]
    pub given_up: bool,
    #[serde(default)]
    pub given_up_at: Option<i64>,
}
