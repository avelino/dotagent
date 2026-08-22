//! Memory hooks: the read and write edges against `dotagent-memory`.
//!
//! Both directions are non-fatal by contract — memory sits in the hot path
//! of every reply, so a closed or corrupt workspace must degrade to "no
//! context" / "facts dropped", never to a failed conversation turn.
//!
//! Recall is two passes. The **relevance** pass ranks facts against the
//! message (shared terms first, recency second); the **recent** pass fills
//! whatever budget is left, because a conversation needs standing context
//! even when the message is "oi". Relevance goes first, so when both find
//! something the answer to what was asked leads.

use std::path::Path;

use dotagent_memory::{MemoryStore, Provenance};

use crate::memo::{assemble_context_block, CapturedMemo, MemoryContext};

/// Facts kept per recall pass before merging.
const PASS_LIMIT: usize = 8;

/// Topics offered back to the model as vocabulary.
const TOPIC_LIMIT: usize = 24;

/// Build the bounded context block for a message, or `""` when memory is
/// unavailable/empty. The block is injected as `AGENT_ASSISTANT_MEMORY`.
pub fn recall_context_block(root: &Path, message: &str, max_bytes: usize) -> String {
    let Ok(store) = MemoryStore::open(root) else {
        return String::new();
    };
    assemble_context_block(&recall_context(&store, message), max_bytes)
}

/// Persist captured memos; returns how many landed. A store that cannot be
/// opened or a fact that fails to write is skipped, not propagated.
pub fn flush_memos(root: &Path, memos: &[CapturedMemo], provenance: &Provenance) -> usize {
    let Ok(store) = MemoryStore::open(root) else {
        return 0;
    };
    memos
        .iter()
        .filter(|m| store.remember_with(&m.text, &m.topics, provenance).is_ok())
        .count()
}

fn recall_context(store: &MemoryStore, message: &str) -> MemoryContext {
    let relevant = store.recall(message, PASS_LIMIT).unwrap_or_default();
    let recent = store.recent(PASS_LIMIT).unwrap_or_default();

    let mut entries: Vec<CapturedMemo> = Vec::new();
    for memory in relevant.into_iter().chain(recent) {
        if entries.iter().any(|e| e.text == memory.text) {
            continue;
        }
        entries.push(CapturedMemo {
            text: memory.text,
            topics: memory.topics,
        });
    }

    // The vocabulary is what keeps the graph from fragmenting: without it
    // the model coins `reuniao` one day and `reunioes` the next, and each
    // half of the subject looks incomplete to whoever opens the workspace.
    let mut topics = store.topics().unwrap_or_default();
    topics.truncate(TOPIC_LIMIT);

    MemoryContext { entries, topics }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store_with_facts() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open_or_init(dir.path()).unwrap();
        store
            .remember("prefers meetings after 14h", &["avelino".into()])
            .unwrap();
        store
            .remember("databricks-cost-daily broken since 07-13", &[])
            .unwrap();
        (dir, store)
    }

    #[test]
    fn recall_returns_recent_facts_under_header() {
        let (dir, _store) = store_with_facts();
        let block = recall_context_block(dir.path(), "oi tudo bem?", 4_000);
        assert!(block.starts_with("## Memory\n"), "{block}");
        assert!(block.contains("prefers meetings after 14h"), "{block}");
    }

    #[test]
    fn a_relevant_fact_ranks_above_the_merely_recent_one() {
        let (dir, _store) = store_with_facts();
        let block = recall_context_block(dir.path(), "como está o databricks?", 4_000);
        let databricks = block.find("databricks-cost-daily").expect("relevant fact");
        let meetings = block.find("prefers meetings").expect("recent fact");
        assert!(databricks < meetings, "{block}");
    }

    #[test]
    fn recall_deduplicates_across_passes() {
        let (dir, _store) = store_with_facts();
        let block = recall_context_block(dir.path(), "", 4_000);
        assert_eq!(block.matches("\n- ").count(), 2, "each fact once: {block}");
    }

    #[test]
    fn recall_offers_the_existing_topics_as_vocabulary() {
        let (dir, _store) = store_with_facts();
        let block = recall_context_block(dir.path(), "", 4_000);
        assert!(block.contains("Topics in use: avelino"), "{block}");
    }

    #[test]
    fn recall_is_bounded_by_bytes() {
        let (dir, _store) = store_with_facts();
        let block = recall_context_block(dir.path(), "", 30);
        // Enough budget for at most the header + one short fact — and the
        // two stored facts are both longer than the remainder, so the block
        // may legitimately be empty or tiny; it must never exceed budget.
        assert!(block.len() <= 30, "{block}");
    }

    #[test]
    fn recall_on_missing_workspace_is_empty_not_an_error() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(recall_context_block(&missing, "anything", 1_000), "");
    }

    #[test]
    fn flush_writes_captured_memos() {
        let dir = tempdir().unwrap();
        let _ = MemoryStore::open_or_init(dir.path()).unwrap();
        let written = flush_memos(
            dir.path(),
            &[
                CapturedMemo {
                    text: "fact one".into(),
                    topics: vec!["alpha".into()],
                },
                CapturedMemo {
                    text: "fact two".into(),
                    topics: vec![],
                },
            ],
            &Provenance::default(),
        );
        assert_eq!(written, 2);
        let block = recall_context_block(dir.path(), "", 4_000);
        assert!(block.contains("fact one"), "{block}");
        assert!(block.contains("fact two"), "{block}");
    }

    #[test]
    fn flush_records_provenance() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open_or_init(dir.path()).unwrap();
        flush_memos(
            dir.path(),
            &[CapturedMemo {
                text: "fato com origem".into(),
                topics: vec![],
            }],
            &Provenance::from("telegram-assistant", "telegram").with_session("s1"),
        );
        let hits = store.recall("origem", 5).unwrap();
        assert_eq!(
            hits[0].provenance.agent.as_deref(),
            Some("telegram-assistant")
        );
        assert_eq!(hits[0].provenance.session.as_deref(), Some("s1"));
    }

    #[test]
    fn flushing_the_same_fact_twice_does_not_duplicate_it() {
        // Assistants restate what they learned; the store must absorb that
        // rather than fill up with copies that crowd out everything else.
        let dir = tempdir().unwrap();
        let store = MemoryStore::open_or_init(dir.path()).unwrap();
        let memo = [CapturedMemo {
            text: "prefere reunião depois das 14h".into(),
            topics: vec![],
        }];
        flush_memos(dir.path(), &memo, &Provenance::default());
        flush_memos(dir.path(), &memo, &Provenance::default());
        let hits = store.recall("reunião", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].seen, 2);
    }

    #[test]
    fn flush_on_missing_workspace_writes_nothing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        let written = flush_memos(
            &missing,
            &[CapturedMemo {
                text: "x".into(),
                topics: vec![],
            }],
            &Provenance::default(),
        );
        assert_eq!(written, 0);
    }

    #[test]
    fn flush_skips_empty_facts_without_aborting() {
        let dir = tempdir().unwrap();
        let _ = MemoryStore::open_or_init(dir.path()).unwrap();
        // `remember` rejects empty text; the hook must carry on to the next.
        let written = flush_memos(
            dir.path(),
            &[
                CapturedMemo {
                    text: "   ".into(),
                    topics: vec![],
                },
                CapturedMemo {
                    text: "real".into(),
                    topics: vec![],
                },
            ],
            &Provenance::default(),
        );
        assert_eq!(written, 1);
    }
}
