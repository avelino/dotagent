//! `MEMO:` capture for ordinary agents.
//!
//! The assistant harness reads capture lines out of a conversation reply.
//! This is the same capture for every other agent: a scheduled run that
//! learns something durable prints `MEMO: <fact> | topics: a, b`, and what
//! it learned outlives the run instead of scrolling past in a log.
//!
//! Two rules keep this from turning the store into a log:
//!
//! - **Opt-in per manifest.** Most agents print status, not facts, and a
//!   store that absorbs status is a store whose recall returns status.
//! - **Successful runs only.** A failing run's output is a symptom. Filing
//!   "could not reach the API" as a durable fact means recalling it as true
//!   next month, long after the outage ended.
//!
//! Failure here is never the run's failure — the run already happened and
//! its exit code already means something. A memory workspace that cannot be
//! opened costs the facts, not the agent.

use std::path::Path;

use dotagent_assistant::{flush_memos, strip_memos, CapturedMemo};
use dotagent_core::manifest::AgentManifest;
use dotagent_memory::Provenance;
use tracing::{debug, warn};

/// Facts an agent's output asked to store, or empty when it did not opt in.
///
/// An agent running under the assistant harness is skipped: the harness
/// already captures from the reply, and reading the same run's stdout again
/// would file every fact through two paths.
pub(crate) fn capture(manifest: &AgentManifest, stdout: &str, exit_code: i32) -> Vec<CapturedMemo> {
    let Some(config) = manifest.memory.as_ref().filter(|m| m.capture) else {
        return Vec::new();
    };
    if manifest
        .assistant
        .as_ref()
        .is_some_and(|a| a.enabled && a.memory)
    {
        return Vec::new();
    }
    if exit_code != 0 {
        return Vec::new();
    }
    let (_, mut memos) = strip_memos(stdout);
    for memo in &mut memos {
        for topic in &config.topics {
            if !memo.topics.contains(topic) {
                memo.topics.push(topic.clone());
            }
        }
    }
    memos
}

/// File captured facts, logging what landed. Never fails the caller.
pub(crate) fn flush(root: &Path, agent: &str, memos: &[CapturedMemo]) {
    if memos.is_empty() {
        return;
    }
    let provenance = Provenance::from(agent, "schedule");
    let written = flush_memos(root, memos, &provenance);
    if written < memos.len() {
        warn!(
            agent,
            written,
            total = memos.len(),
            "memory capture: some facts were not persisted"
        );
    } else {
        debug!(agent, written, "memory capture: facts filed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotagent_core::manifest::AgentMemoryConfig;

    fn manifest(toml: &str) -> AgentManifest {
        let base = "[agent]\nname = \"probe\"\ndescription = \"d\"\n\n[run]\ncommand = \"true\"\n";
        toml::from_str(&format!("{base}{toml}")).expect("manifest parses")
    }

    #[test]
    fn an_agent_that_did_not_opt_in_captures_nothing() {
        let m = manifest("");
        assert!(capture(&m, "MEMO: a fact", 0).is_empty());
    }

    #[test]
    fn an_opted_in_agent_captures_its_memo_lines() {
        let m = manifest("[memory]\ncapture = true\n");
        let memos = capture(
            &m,
            "status ok\nMEMO: databricks cost up 30% | topics: ops",
            0,
        );
        assert_eq!(memos.len(), 1);
        assert_eq!(memos[0].text, "databricks cost up 30%");
        assert_eq!(memos[0].topics, vec!["ops".to_string()]);
    }

    #[test]
    fn manifest_topics_are_added_to_every_fact() {
        let m = manifest("[memory]\ncapture = true\ntopics = [\"ops\"]\n");
        let memos = capture(&m, "MEMO: a fact | topics: cost", 0);
        assert_eq!(memos[0].topics, vec!["cost".to_string(), "ops".to_string()]);
    }

    #[test]
    fn a_manifest_topic_already_named_is_not_repeated() {
        let m = manifest("[memory]\ncapture = true\ntopics = [\"ops\"]\n");
        let memos = capture(&m, "MEMO: a fact | topics: ops", 0);
        assert_eq!(memos[0].topics, vec!["ops".to_string()]);
    }

    #[test]
    fn a_failing_run_files_nothing() {
        // Its output is a symptom, not a durable fact: "could not reach the
        // API" recalled next month would read as still true.
        let m = manifest("[memory]\ncapture = true\n");
        assert!(capture(&m, "MEMO: could not reach the API", 1).is_empty());
    }

    #[test]
    fn capture_can_be_switched_off_without_deleting_the_section() {
        let m = manifest("[memory]\ncapture = false\ntopics = [\"ops\"]\n");
        assert!(capture(&m, "MEMO: a fact", 0).is_empty());
    }

    #[test]
    fn an_assistant_agent_is_left_to_the_harness() {
        // Otherwise the same fact is filed twice, through two paths.
        let m = manifest("[memory]\ncapture = true\n\n[assistant]\n");
        assert!(capture(&m, "MEMO: a fact", 0).is_empty());
    }

    #[test]
    fn an_assistant_with_memory_off_still_captures_from_stdout() {
        let m = manifest("[memory]\ncapture = true\n\n[assistant]\nmemory = false\n");
        assert_eq!(capture(&m, "MEMO: a fact", 0).len(), 1);
    }

    #[test]
    fn output_without_memo_lines_captures_nothing() {
        let m = manifest("[memory]\ncapture = true\n");
        assert!(capture(&m, "just some ordinary output\ndone", 0).is_empty());
    }

    #[test]
    fn the_section_defaults_to_capturing() {
        // Writing `[memory]` at all is the opt-in; the flag exists to turn
        // it back off without deleting the topics next to it.
        let m = manifest("[memory]\n");
        assert!(m.memory.as_ref().is_some_and(AgentMemoryConfig::captures));
        assert_eq!(capture(&m, "MEMO: a fact", 0).len(), 1);
    }
}
