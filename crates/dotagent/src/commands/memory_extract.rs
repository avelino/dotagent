//! Capturing facts the model did not volunteer.
//!
//! The `MEMO:` convention works only when the dispatcher's prompt asks for it
//! and the model complies. Both are outside the daemon: swap the dispatcher
//! for one whose prompt never mentions `MEMO`, or let the model finish a long
//! conversation without one, and the store stops growing with no error
//! anywhere. Silence is the failure mode, which is the worst kind.
//!
//! `[assistant.extractor]` closes that. After a turn is delivered, the daemon
//! hands the turn to a declared command and files whatever comes back. The
//! daemon still does not read the conversation itself — it decides *when* to
//! extract and what to do with the result, and inference stays in a process
//! the operator named. That is the line that keeps "not an AI runtime" true.
//!
//! Off the reply path on purpose: the chat has already been answered by the
//! time this runs, so a slow extractor costs nothing a person waits on.

use dotagent_assistant::{strip_memos, CapturedMemo};
use dotagent_core::manifest::AssistantExtractor;
use dotagent_memory::record::dedup_key;
use tracing::{debug, warn};

/// The turn handed to the extractor, on stdin as one JSON object.
///
/// Deliberately just the turn. Giving it the whole transcript would make the
/// prompt grow without bound and invite it to re-file facts from days ago
/// that the store already has.
#[derive(serde::Serialize)]
struct Turn<'a> {
    message: &'a str,
    reply: &'a str,
    source: &'a str,
    session: Option<&'a str>,
}

/// Run the extractor over one turn and return what it decided to keep.
///
/// Every failure is best-effort and logged: an extractor that is missing,
/// slow or broken must never cost a reply that was already delivered. It
/// returns no memos and the turn is simply not remembered, which is exactly
/// where the system was before the extractor existed.
pub async fn extract(
    cfg: &AssistantExtractor,
    manifest_dir: &std::path::Path,
    message: &str,
    reply: &str,
    source: &str,
    session: Option<&str>,
) -> Vec<CapturedMemo> {
    let turn = Turn {
        message,
        reply,
        source,
        session,
    };
    let Ok(payload) = serde_json::to_string(&turn) else {
        warn!("memory extractor: could not encode the turn");
        return Vec::new();
    };

    let mut cmd = tokio::process::Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .current_dir(manifest_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let plugins = dotagent_plugin::PluginClient::from_environment();
    let spec = dotagent_supervisor::SpawnSpec {
        kind: dotagent_supervisor::ProcessKind::Skill,
        owner: dotagent_supervisor::ProcessOwner {
            agent: "memory-extractor".to_string(),
            ..Default::default()
        },
        deadline: std::time::Duration::from_secs(cfg.timeout_seconds),
        label: "memory:extract".to_string(),
    };

    let mut handle = match plugins.supervisor().spawn_supervised(cmd, spec).await {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, command = %cfg.command, "memory extractor: could not start");
            return Vec::new();
        }
    };

    let stdin = handle.take_stdin();
    let write = async move {
        let Some(mut stdin) = stdin else {
            return Ok::<(), std::io::Error>(());
        };
        use tokio::io::AsyncWriteExt;
        stdin.write_all(payload.as_bytes()).await?;
        drop(stdin);
        Ok(())
    };

    let (write_result, output_result) = tokio::join!(write, handle.wait_with_output());
    if let Err(e) = write_result {
        warn!(error = %e, "memory extractor: could not write the turn");
        return Vec::new();
    }

    let out = match output_result {
        Ok(out) => out,
        Err(e) => {
            warn!(error = %e, "memory extractor: did not finish");
            return Vec::new();
        }
    };

    if !out.status.success() {
        warn!(
            code = out.status.code().unwrap_or(-1),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "memory extractor: exited non-zero"
        );
        return Vec::new();
    }

    // Same parser as the reply path. One format, two producers — an extractor
    // is written against the syntax already documented for agents.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (_, memos) = strip_memos(&stdout);
    debug!(count = memos.len(), "memory extractor: facts captured");
    memos
}

/// Merge what the model volunteered with what the extractor found.
///
/// The model's own `MEMO:` lines stay a shortcut worth taking: it has the
/// turn in context and costs nothing extra. The extractor is the net under
/// it, not a replacement, so a turn that produced memos both ways keeps both.
///
/// Two sources describing the same thing collapse to one here, using the
/// store's own [`dedup_key`] rather than a second idea of what "the same
/// fact" means — it folds case, punctuation and links, so "Prefere reuniões
/// após 14h." and "prefere reunioes apos 14h" are one fact, which a textual
/// comparison would have filed as two.
///
/// The store would also catch it downstream, by reinforcing instead of
/// duplicating. Catching it here keeps `seen::` honest: that counter means
/// "restated over time", and two voices in a single turn is not that.
pub fn merge(volunteered: Vec<CapturedMemo>, extracted: Vec<CapturedMemo>) -> Vec<CapturedMemo> {
    let mut out = volunteered;
    for memo in extracted {
        let key = dedup_key(&memo.text);
        if !out.iter().any(|m| dedup_key(&m.text) == key) {
            out.push(memo);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memo(text: &str) -> CapturedMemo {
        CapturedMemo {
            text: text.to_string(),
            topics: Vec::new(),
        }
    }

    #[test]
    fn merge_keeps_both_sources() {
        let out = merge(
            vec![memo("from the model")],
            vec![memo("from the extractor")],
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_drops_a_duplicate_of_the_same_turn() {
        let out = merge(vec![memo("same fact")], vec![memo("same fact")]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn merge_ignores_surrounding_whitespace_when_comparing() {
        let out = merge(vec![memo("same fact")], vec![memo("  same fact  ")]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn merge_collapses_two_spellings_of_one_fact() {
        // The case a textual comparison missed: same sentence, different case
        // and punctuation. Filing both would inflate `seen::` for one turn.
        let out = merge(
            vec![memo("Prefere reuniões após 14h.")],
            vec![memo("prefere reuniões após 14h")],
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn merge_does_not_fold_accents() {
        // `dedup_key` folds case, punctuation and links — not accents. Recall
        // folds those, the write path does not, and pinning the boundary here
        // keeps the next reader from assuming otherwise.
        let out = merge(vec![memo("reuniões")], vec![memo("reunioes")]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_with_nothing_extracted_is_the_old_behavior() {
        let out = merge(vec![memo("a"), memo("b")], Vec::new());
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_captures_a_turn_the_model_stayed_silent_on() {
        // The case that started this: the model volunteered nothing.
        let out = merge(Vec::new(), vec![memo("prefers meetings after 14h")]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn the_turn_encodes_as_the_documented_shape() {
        let json = serde_json::to_string(&Turn {
            message: "oi",
            reply: "olá",
            source: "telegram",
            session: Some("chat-1"),
        })
        .unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back["message"], "oi");
        assert_eq!(back["reply"], "olá");
        assert_eq!(back["source"], "telegram");
        assert_eq!(back["session"], "chat-1");
    }
}
