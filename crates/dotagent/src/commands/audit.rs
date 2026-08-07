//! `dotagent audit verify` — say out loud what the hash chain establishes.
//!
//! The daemon's boot check answers yes/no, which is the right question for a
//! daemon and the wrong one for an operator. "No break found" hides whether
//! verification reached `GENESIS` or stopped at a seam three segments back, and
//! it never looks at a rotated segment at all — after the first rotation, no
//! embedded code path re-reads one. The four `ChainStatus` verdicts exist to
//! draw exactly those distinctions; until this command they had no way to reach
//! anybody, and `docs/security/threat-model.md` documented a forensic capability
//! that nothing printed.
//!
//! Exits non-zero when the chain is not trustworthy, so this is usable from a
//! cron entry or a `&&` chain. `--json` for anything that has to parse it.

use std::io::IsTerminal;

use anyhow::{Context, Result};
use dotagent_state::{AuditLog, ChainBreak, ChainStatus, VerifyScope};
use serde_json::{json, Value};

pub fn verify(full: bool, json_output: bool) -> Result<()> {
    let log = AuditLog::from_home().context("opening the audit log")?;
    let scope = if full {
        VerifyScope::Full
    } else {
        VerifyScope::CurrentSegment
    };
    let status = log
        .verify_chain_status(scope)
        .context("verifying the audit chain")?;
    // A directory we cannot list is not a reason to withhold the verdict we
    // already have; the segment list is context, not the answer.
    let segments: Vec<String> = log
        .segments()
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(String::from))
        .collect();
    let path = log.path().display().to_string();

    if json_output {
        let envelope = envelope(&path, full, &status, &segments);
        println!(
            "{}",
            serde_json::to_string(&envelope).expect("serializing audit verify envelope")
        );
    } else {
        print!(
            "{}",
            render(&path, full, &status, &segments, Style::detect())
        );
    }

    if !is_trustworthy(&status) {
        std::process::exit(1);
    }
    Ok(())
}

/// Whether the verdict means "nothing on disk contradicts the chain".
///
/// `IntactSinceRotation` counts: a pruned history is a legitimate state, and
/// the difference between it and a beheaded log is the whole point of the seam.
/// The text still names what is missing, so a reader can disagree.
fn is_trustworthy(status: &ChainStatus) -> bool {
    matches!(
        status,
        ChainStatus::IntactFromGenesis | ChainStatus::IntactSinceRotation { .. }
    )
}

fn scope_label(full: bool) -> &'static str {
    if full {
        "full — following seams back through every segment on disk"
    } else {
        "current segment — the live audit.log only"
    }
}

fn render(
    path: &str,
    full: bool,
    status: &ChainStatus,
    segments: &[String],
    style: Style,
) -> String {
    let mut out = String::new();
    match status {
        ChainStatus::IntactFromGenesis => {
            out.push_str(&format!("{} chain intact from GENESIS\n", style.ok("✓")));
            out.push_str("  every entry that is still on disk was checked\n");
        }
        ChainStatus::IntactSinceRotation {
            since_ts,
            segment,
            entries_before,
            segment_present,
        } => {
            out.push_str(&format!(
                "{} chain intact since {since_ts}\n",
                style.ok("✓")
            ));
            out.push_str(&format!(
                "  {entries_before} earlier entries lived in {segment}\n"
            ));
            out.push_str(if *segment_present {
                "  that segment is on disk and was not walked — re-run with --full\n"
            } else {
                "  that segment is gone: retention, or evidence removed. The chain\n  cannot tell those apart — only that the seam explaining it survived\n"
            });
        }
        ChainStatus::UnexplainedTruncation { orphan_prev_hash } => {
            out.push_str(&format!(
                "{} unexplained truncation — the head of the log was removed\n",
                style.bad("✗")
            ));
            out.push_str(&format!(
                "  the oldest entry links to {orphan_prev_hash}, and nothing accounts for it\n"
            ));
            out.push_str("  no seam survived, so this was not a rotation\n");
        }
        ChainStatus::Broken(brk) => {
            out.push_str(&format!(
                "{} chain broken at position {}\n",
                style.bad("✗"),
                brk.position
            ));
            out.push_str(&format!(
                "  in:       {}\n",
                brk.segment.as_deref().unwrap_or("the live audit.log")
            ));
            out.push_str(&format!("  expected: {}\n", brk.expected));
            out.push_str(&format!("  actual:   {}\n", brk.actual));
        }
    }

    out.push_str(&format!("  file:     {path}\n"));
    out.push_str(&format!("  scope:    {}\n", scope_label(full)));
    out.push_str(&format!(
        "  segments: {}\n",
        if segments.is_empty() {
            "none — the log has never rotated".to_string()
        } else {
            format!("{} on disk ({})", segments.len(), segments.join(", "))
        }
    ));
    out
}

fn envelope(path: &str, full: bool, status: &ChainStatus, segments: &[String]) -> Value {
    let mut verdict = match status {
        ChainStatus::IntactFromGenesis => json!({ "status": "intact_from_genesis" }),
        ChainStatus::IntactSinceRotation {
            since_ts,
            segment,
            entries_before,
            segment_present,
        } => json!({
            "status": "intact_since_rotation",
            "since_ts": since_ts,
            "segment": segment,
            "entries_before": entries_before,
            "segment_present": segment_present,
        }),
        ChainStatus::UnexplainedTruncation { orphan_prev_hash } => json!({
            "status": "unexplained_truncation",
            "orphan_prev_hash": orphan_prev_hash,
        }),
        ChainStatus::Broken(ChainBreak {
            position,
            expected,
            actual,
            segment,
        }) => json!({
            "status": "broken",
            "position": position,
            "expected": expected,
            "actual": actual,
            "segment": segment,
        }),
    };

    let obj = verdict.as_object_mut().expect("verdict is a JSON object");
    obj.insert("ok".into(), json!(is_trustworthy(status)));
    obj.insert(
        "scope".into(),
        json!(if full { "full" } else { "current_segment" }),
    );
    obj.insert("file".into(), json!(path));
    obj.insert("segments".into(), json!(segments));
    verdict
}

// ───────────────────────── color helpers ─────────────────────────

/// Same rule as `output.rs`: escapes only when stdout is a TTY and `NO_COLOR`
/// is unset. Kept local because the verdict line is all that needs painting.
#[derive(Clone, Copy)]
struct Style {
    enabled: bool,
}

impl Style {
    fn detect() -> Self {
        Self {
            enabled: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    #[cfg(test)]
    fn plain() -> Self {
        Self { enabled: false }
    }

    fn ok(&self, text: &str) -> String {
        self.paint(text, "32")
    }

    fn bad(&self, text: &str) -> String {
        self.paint(text, "31")
    }

    fn paint(&self, text: &str, code: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }
        format!("\x1b[1;{code}m{text}\x1b[0m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broken() -> ChainStatus {
        ChainStatus::Broken(ChainBreak {
            position: 42,
            expected: "aaaa".into(),
            actual: "bbbb".into(),
            segment: Some("audit.log.20260806T101500".into()),
        })
    }

    fn rotated(segment_present: bool) -> ChainStatus {
        ChainStatus::IntactSinceRotation {
            since_ts: "2026-05-19T08:31:07-0300".into(),
            segment: "audit.log.20260806T101500".into(),
            entries_before: 38513,
            segment_present,
        }
    }

    #[test]
    fn only_the_two_intact_verdicts_exit_zero() {
        assert!(is_trustworthy(&ChainStatus::IntactFromGenesis));
        assert!(is_trustworthy(&rotated(false)));
        assert!(!is_trustworthy(&broken()));
        assert!(!is_trustworthy(&ChainStatus::UnexplainedTruncation {
            orphan_prev_hash: "cccc".into(),
        }));
    }

    #[test]
    fn a_break_names_the_position_the_segment_and_both_hashes() {
        let out = render("/x/audit.log", true, &broken(), &[], Style::plain());
        assert!(out.contains("broken at position 42"), "{out}");
        assert!(out.contains("audit.log.20260806T101500"), "{out}");
        assert!(out.contains("aaaa") && out.contains("bbbb"), "{out}");
    }

    #[test]
    fn a_break_in_the_live_file_says_so_instead_of_printing_none() {
        // The `{:?}` trap this module exists to avoid: `segment: None` must
        // never reach the terminal as the word "None".
        let status = ChainStatus::Broken(ChainBreak {
            position: 1,
            expected: "aaaa".into(),
            actual: "bbbb".into(),
            segment: None,
        });
        let out = render("/x/audit.log", false, &status, &[], Style::plain());
        assert!(out.contains("the live audit.log"), "{out}");
        assert!(!out.contains("None"), "{out}");
    }

    #[test]
    fn a_missing_segment_is_described_as_indistinguishable_not_as_fine() {
        let out = render("/x/audit.log", true, &rotated(false), &[], Style::plain());
        assert!(out.contains("38513 earlier entries"), "{out}");
        assert!(out.contains("evidence removed"), "{out}");

        // Present but unwalked is a different sentence: it points at --full.
        let out = render("/x/audit.log", false, &rotated(true), &[], Style::plain());
        assert!(out.contains("--full"), "{out}");
    }

    #[test]
    fn truncation_names_the_orphan_hash() {
        let status = ChainStatus::UnexplainedTruncation {
            orphan_prev_hash: "c8d2beef".into(),
        };
        let out = render("/x/audit.log", true, &status, &[], Style::plain());
        assert!(out.contains("c8d2beef"), "{out}");
        assert!(out.contains("not a rotation"), "{out}");
    }

    #[test]
    fn the_footer_always_names_the_file_the_scope_and_the_segments() {
        let segs = vec!["audit.log.20260806T101500".to_string()];
        let out = render(
            "/x/audit.log",
            false,
            &ChainStatus::IntactFromGenesis,
            &segs,
            Style::plain(),
        );
        assert!(out.contains("file:     /x/audit.log"), "{out}");
        assert!(out.contains("current segment"), "{out}");
        assert!(out.contains("1 on disk"), "{out}");

        let out = render(
            "/x/audit.log",
            true,
            &ChainStatus::IntactFromGenesis,
            &[],
            Style::plain(),
        );
        assert!(out.contains("never rotated"), "{out}");
        assert!(out.contains("full —"), "{out}");
    }

    #[test]
    fn plain_style_emits_no_escape_sequences() {
        for status in [
            ChainStatus::IntactFromGenesis,
            rotated(true),
            broken(),
            ChainStatus::UnexplainedTruncation {
                orphan_prev_hash: "c".into(),
            },
        ] {
            let out = render("/x/audit.log", false, &status, &[], Style::plain());
            assert!(!out.contains('\x1b'), "{out}");
        }
    }

    #[test]
    fn the_json_envelope_locks_the_schema() {
        let segs = vec!["audit.log.20260806T101500".to_string()];
        let e = envelope("/x/audit.log", true, &rotated(false), &segs);
        assert_eq!(e["status"], "intact_since_rotation");
        assert_eq!(e["ok"], true);
        assert_eq!(e["scope"], "full");
        assert_eq!(e["file"], "/x/audit.log");
        assert_eq!(e["segments"][0], "audit.log.20260806T101500");
        assert_eq!(e["entries_before"], 38513);
        assert_eq!(e["segment_present"], false);

        let e = envelope("/x/audit.log", false, &broken(), &[]);
        assert_eq!(e["status"], "broken");
        assert_eq!(e["ok"], false);
        assert_eq!(e["scope"], "current_segment");
        assert_eq!(e["position"], 42);
        assert_eq!(e["expected"], "aaaa");
        assert_eq!(e["actual"], "bbbb");

        // Round-trips through the same path `verify` prints.
        let raw = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), e);
    }

    #[test]
    fn every_verdict_serializes_with_a_status_and_an_ok_flag() {
        for status in [
            ChainStatus::IntactFromGenesis,
            rotated(true),
            broken(),
            ChainStatus::UnexplainedTruncation {
                orphan_prev_hash: "c".into(),
            },
        ] {
            let e = envelope("/x/audit.log", false, &status, &[]);
            assert!(e["status"].is_string());
            assert!(e["ok"].is_boolean());
        }
    }
}
