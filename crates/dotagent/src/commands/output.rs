//! Human- and machine-friendly rendering of `OrchestratedOutcome`.
//!
//! Every CLI command that reports an execution result MUST go through this
//! module — never `println!("{:?}", outcome)`. Rendering rules:
//!
//! - Color is applied only when stdout is a TTY and `NO_COLOR` is unset.
//! - Status icons (`✓`/`✗`/`⊘`) are always emitted; modern terminals and pagers
//!   handle UTF-8 fine, and operators redirecting to a file usually want the
//!   icon preserved. Scripts that need pure ASCII / structured fields should
//!   use `--json`.
//! - stdout/stderr blocks are emitted with real newlines, indented.
//! - Truncated tails are explicitly labeled with the number of dropped lines
//!   and a pointer to the full log under `$DOTAGENT_HOME/logs/agents/<name>/`.
//! - `--json` format is stable and machine-parseable.

use std::io::IsTerminal;

use dotagent_runner::{OrchestratedOutcome, RunOutcome};
use serde_json::json;

/// Output format selected by the caller (`--json` toggles it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Human,
    Json,
}

/// Render an `OrchestratedOutcome` for an agent/schedule pair.
///
/// Writes to stdout. The `wall_duration_seconds` is the wall-clock measured by
/// the caller (which may include preflight time the inner `RunOutcome` doesn't
/// see). Inside the JSON envelope, the runner's own `duration_seconds` (just
/// the agent process) is kept under `result.duration_seconds` so consumers can
/// tell them apart.
pub fn render_outcome(
    agent: &str,
    schedule: &str,
    outcome: &OrchestratedOutcome,
    wall_duration_seconds: i64,
    format: Format,
) {
    match format {
        Format::Json => print_json(agent, schedule, outcome, wall_duration_seconds),
        Format::Human => print_human(agent, schedule, outcome, wall_duration_seconds),
    }
}

/// Build the JSON envelope without serializing — useful for tests that need to
/// assert on the schema without capturing stdout.
fn build_envelope(
    agent: &str,
    schedule: &str,
    outcome: &OrchestratedOutcome,
    wall_duration_seconds: i64,
) -> serde_json::Value {
    let result = match outcome {
        OrchestratedOutcome::Ran(run) => json!({
            "kind": "ran",
            "exit_code": run.exit_code,
            "timed_out": run.timed_out,
            "duration_seconds": run.duration_seconds,
            "stdout_tail": run.stdout_tail,
            "stderr_tail": run.stderr_tail,
            "stdout_truncated_lines": run.stdout_truncated_lines,
            "stderr_truncated_lines": run.stderr_truncated_lines,
        }),
        OrchestratedOutcome::PreflightFailed { plugin, suggest } => json!({
            "kind": "preflight_failed",
            "plugin": plugin,
            "suggest": suggest,
        }),
    };
    json!({
        "agent": agent,
        "schedule": schedule,
        "wall_duration_seconds": wall_duration_seconds,
        "result": result,
    })
}

fn print_json(
    agent: &str,
    schedule: &str,
    outcome: &OrchestratedOutcome,
    wall_duration_seconds: i64,
) {
    let envelope = build_envelope(agent, schedule, outcome, wall_duration_seconds);
    // serde_json::to_string of a Value built via the json! macro from Strings
    // and primitives never fails in practice; if it ever does, a panic is
    // better than a silent empty line in scripted --json consumers.
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("serializing run-now JSON envelope")
    );
}

fn print_human(agent: &str, schedule: &str, outcome: &OrchestratedOutcome, duration_seconds: i64) {
    let style = Style::detect();
    match outcome {
        OrchestratedOutcome::Ran(run) => print_run(agent, schedule, run, duration_seconds, &style),
        OrchestratedOutcome::PreflightFailed { plugin, suggest } => print_preflight_failed(
            agent,
            schedule,
            plugin,
            suggest.as_deref(),
            duration_seconds,
            &style,
        ),
    }
}

fn print_run(agent: &str, schedule: &str, run: &RunOutcome, duration_seconds: i64, style: &Style) {
    let (icon, label, color) = if run.timed_out {
        (
            "✗",
            format!("timed out (exit {})", run.exit_code),
            Color::Red,
        )
    } else if run.exit_code == 0 {
        ("✓", "ok".to_string(), Color::Green)
    } else {
        ("✗", format!("failed (exit {})", run.exit_code), Color::Red)
    };

    println!(
        "{} {}/{}  {}  {}",
        style.paint(icon, color, true),
        agent,
        schedule,
        style.paint(&label, color, true),
        style.dim(&format!("{duration_seconds}s")),
    );

    if !run.stdout_tail.is_empty() {
        println!();
        println!("{}", style.dim("stdout:"));
        print_block(&run.stdout_tail);
        if run.stdout_truncated_lines > 0 {
            print_truncation_notice(agent, run.stdout_truncated_lines, style);
        }
    }

    if !run.stderr_tail.is_empty() {
        println!();
        println!("{}", style.dim("stderr:"));
        print_block(&run.stderr_tail);
        if run.stderr_truncated_lines > 0 {
            print_truncation_notice(agent, run.stderr_truncated_lines, style);
        }
    }
}

fn print_preflight_failed(
    agent: &str,
    schedule: &str,
    plugin: &str,
    suggest: Option<&str>,
    duration_seconds: i64,
    style: &Style,
) {
    println!(
        "{} {}/{}  {}  {}",
        style.paint("⊘", Color::Yellow, true),
        agent,
        schedule,
        style.paint("aborted by preflight", Color::Yellow, true),
        style.dim(&format!("{duration_seconds}s")),
    );
    println!("  {} {plugin}", style.dim("plugin:"));
    if let Some(s) = suggest {
        println!("  {} {s}", style.dim("suggest:"));
    }
}

fn print_block(text: &str) {
    for line in text.lines() {
        println!("  {line}");
    }
}

fn print_truncation_notice(agent: &str, dropped: usize, style: &Style) {
    let path = dotagent_state::paths::agent_logs_dir(agent).join(format!("{agent}.log"));
    println!(
        "  {}",
        style.dim(&format!(
            "… {dropped} earlier line(s) truncated — full log: {}",
            path.display()
        )),
    );
}

// ───────────────────────── color helpers ─────────────────────────

#[derive(Clone, Copy)]
enum Color {
    Red,
    Green,
    Yellow,
}

impl Color {
    fn code(self) -> &'static str {
        match self {
            Color::Red => "31",
            Color::Green => "32",
            Color::Yellow => "33",
        }
    }
}

struct Style {
    enabled: bool,
}

impl Style {
    fn detect() -> Self {
        // Respect the de facto NO_COLOR convention (https://no-color.org/).
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let tty = std::io::stdout().is_terminal();
        Self {
            enabled: tty && !no_color,
        }
    }

    fn paint(&self, text: &str, color: Color, bold: bool) -> String {
        if !self.enabled {
            return text.to_string();
        }
        let weight = if bold { "1;" } else { "" };
        format!("\x1b[{weight}{}m{text}\x1b[0m", color.code())
    }

    fn dim(&self, text: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }
        format!("\x1b[2m{text}\x1b[0m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotagent_runner::RunOutcome;

    fn no_color_style() -> Style {
        Style { enabled: false }
    }

    #[test]
    fn paint_disabled_returns_plain_text() {
        let s = no_color_style();
        assert_eq!(s.paint("hello", Color::Red, true), "hello");
        assert_eq!(s.dim("hello"), "hello");
    }

    #[test]
    fn paint_enabled_wraps_with_ansi() {
        let s = Style { enabled: true };
        assert!(s.paint("hello", Color::Red, true).contains("\x1b[1;31m"));
        assert!(s.dim("hello").starts_with("\x1b[2m"));
    }

    #[test]
    fn json_envelope_for_ran_locks_the_schema() {
        let run = RunOutcome {
            exit_code: 1,
            timed_out: false,
            duration_seconds: 12,
            stdout_tail: String::new(),
            stderr_tail: "boom".into(),
            stdout_truncated_lines: 0,
            stderr_truncated_lines: 200,
        };
        let outcome = OrchestratedOutcome::Ran(run);

        let envelope = build_envelope("smoke", "morning", &outcome, 13);

        // Top-level envelope
        assert_eq!(envelope["agent"], "smoke");
        assert_eq!(envelope["schedule"], "morning");
        assert_eq!(envelope["wall_duration_seconds"], 13);
        // Top-level must NOT carry a bare `duration_seconds` — that lives
        // inside `result` and would be ambiguous if duplicated up top.
        assert!(envelope.get("duration_seconds").is_none());

        // Result block
        let result = &envelope["result"];
        assert_eq!(result["kind"], "ran");
        assert_eq!(result["exit_code"], 1);
        assert_eq!(result["timed_out"], false);
        assert_eq!(result["duration_seconds"], 12);
        assert_eq!(result["stderr_tail"], "boom");
        assert_eq!(result["stderr_truncated_lines"], 200);
        assert_eq!(result["stdout_truncated_lines"], 0);

        // Round-trip through serde_json — same path `print_json` uses.
        let serialized = serde_json::to_string(&envelope).expect("must serialize");
        let reparsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reparsed, envelope);
    }

    #[test]
    fn json_envelope_for_preflight_failed_includes_plugin_and_suggest() {
        let outcome = OrchestratedOutcome::PreflightFailed {
            plugin: "preflight-warp".into(),
            suggest: Some("install warp first".into()),
        };

        let envelope = build_envelope("smoke", "morning", &outcome, 0);

        assert_eq!(envelope["result"]["kind"], "preflight_failed");
        assert_eq!(envelope["result"]["plugin"], "preflight-warp");
        assert_eq!(envelope["result"]["suggest"], "install warp first");
    }
}
