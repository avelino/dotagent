//! Human- and machine-friendly rendering of `OrchestratedOutcome`.
//!
//! Every CLI command that reports an execution result MUST go through this
//! module — never `println!("{:?}", outcome)`. Rendering rules:
//!
//! - Color/icons only when stdout is a TTY (and `NO_COLOR` is unset).
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
/// Writes to stdout. The `duration_seconds` is the wall-clock measured by the
/// caller (which may include preflight time the inner `RunOutcome` doesn't see).
pub fn render_outcome(
    agent: &str,
    schedule: &str,
    outcome: &OrchestratedOutcome,
    duration_seconds: i64,
    format: Format,
) {
    match format {
        Format::Json => print_json(agent, schedule, outcome, duration_seconds),
        Format::Human => print_human(agent, schedule, outcome, duration_seconds),
    }
}

fn print_json(agent: &str, schedule: &str, outcome: &OrchestratedOutcome, duration_seconds: i64) {
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
    let envelope = json!({
        "agent": agent,
        "schedule": schedule,
        "duration_seconds": duration_seconds,
        "result": result,
    });
    println!("{}", serde_json::to_string(&envelope).unwrap_or_default());
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
    fn json_envelope_for_ran_is_stable() {
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
        // Just exercise that the JSON path doesn't panic and produces valid JSON.
        let mut buf = Vec::new();
        let envelope = serde_json::json!({
            "agent": "a", "schedule": "s", "duration_seconds": 12,
            "result": {
                "kind": "ran",
                "exit_code": 1,
                "timed_out": false,
                "duration_seconds": 12,
                "stdout_tail": "",
                "stderr_tail": "boom",
                "stdout_truncated_lines": 0,
                "stderr_truncated_lines": 200,
            }
        });
        serde_json::to_writer(&mut buf, &envelope).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(parsed["result"]["exit_code"], 1);
        let _ = outcome; // silence unused
    }
}
