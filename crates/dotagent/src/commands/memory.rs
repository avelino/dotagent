//! `dotagent memory` — read and curate the long-term memory workspace.
//!
//! The same store `dotagent mcp` exposes to agents, reachable from a shell
//! and without a running daemon. That matters for curation: consolidating a
//! day's raw facts into durable ones is a job an agent should be able to do
//! on a schedule, and an agent can only do it if the verbs exist outside the
//! daemon process.
//!
//! Every verb prints one line per fact, id first, because the follow-up move
//! — correct this, delete that — takes the id.

use anyhow::Result;
use dotagent_memory::{Memory, MemoryStore, Provenance};

/// Resolve the workspace, scaffolding the default location if needed.
///
/// A configured path came from a human, so a typo must fail loudly rather
/// than scaffold an empty workspace nobody will look at. The default path
/// has no typo to protect against.
pub fn store() -> Result<MemoryStore> {
    let cfg = dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .memory;
    match cfg.workspace_override() {
        Some(path) => MemoryStore::open(path).map_err(Into::into),
        None => MemoryStore::open_or_init(dotagent_state::paths::memory_workspace_dir())
            .map_err(Into::into),
    }
}

pub fn recall(query: &str, topic: Option<&str>, limit: usize, json: bool) -> Result<()> {
    let store = store()?;
    let hits = match topic {
        Some(topic) => {
            let mut hits = store.recall_topic(topic)?;
            hits.truncate(limit);
            hits
        }
        None => store.recall(query, limit)?,
    };
    if hits.is_empty() && !json {
        println!("Nothing remembered about that.");
        return Ok(());
    }
    for memory in &hits {
        println!("{}", render(memory, json));
    }
    Ok(())
}

pub fn remember(text: &str, topics: &[String]) -> Result<()> {
    let memory = store()?.remember_with(text, topics, &Provenance::agent("cli"))?;
    // `seen > 1` means dedup absorbed it rather than storing a second copy;
    // saying "remembered" there would be a small lie.
    let verb = if memory.seen > 1 {
        "Reinforced"
    } else {
        "Remembered"
    };
    println!("{verb} [{}]: {}", memory.id, memory.text);
    Ok(())
}

pub fn supersede(id: &str, text: &str, topics: &[String]) -> Result<()> {
    let memory = store()?.supersede(id, text, topics, &Provenance::agent("cli"))?;
    println!("Replaced [{}]: {}", memory.id, memory.text);
    Ok(())
}

pub fn forget(id: &str) -> Result<()> {
    if store()?.forget(id)? {
        println!("Forgotten.");
        Ok(())
    } else {
        anyhow::bail!("no memory with id {id}")
    }
}

pub fn topics() -> Result<()> {
    let topics = store()?.topics()?;
    if topics.is_empty() {
        println!("No topics yet.");
        return Ok(());
    }
    for topic in topics {
        println!("{topic}");
    }
    Ok(())
}

pub fn stats() -> Result<()> {
    let s = store()?.stats()?;
    println!("facts:      {}", s.live);
    println!("reinforced: {}", s.reinforced);
    println!("superseded: {}", s.superseded);
    println!("topics:     {}", s.topics);
    Ok(())
}

/// One fact as a line. Never `{:?}` — the JSON form is a stable contract for
/// scripting, and the human form is for reading.
fn render(memory: &Memory, json: bool) -> String {
    if json {
        return serde_json::json!({
            "id": memory.id,
            "date": memory.date,
            "text": memory.text,
            "topics": memory.topics,
            "seen": memory.seen,
            "agent": memory.provenance.agent,
            "source": memory.provenance.source,
        })
        .to_string();
    }
    let mut line = format!("[{}] ", memory.id);
    if !memory.date.is_empty() {
        line.push_str(&memory.date);
        line.push_str(": ");
    }
    line.push_str(&memory.text);
    if !memory.topics.is_empty() {
        line.push_str(&format!(" ({})", memory.topics.join(", ")));
    }
    if memory.seen > 1 {
        line.push_str(&format!(" ×{}", memory.seen));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Memory {
        Memory {
            id: "01ABC".into(),
            date: "2026-08-21".into(),
            text: "prefere reunião depois das 14h".into(),
            topics: vec!["agenda".into(), "reuniao".into()],
            provenance: Provenance::from("telegram-assistant", "telegram"),
            seen: 3,
            superseded_by: None,
        }
    }

    #[test]
    fn a_fact_renders_id_first_so_the_next_command_can_address_it() {
        assert_eq!(
            render(&memory(), false),
            "[01ABC] 2026-08-21: prefere reunião depois das 14h (agenda, reuniao) ×3"
        );
    }

    #[test]
    fn a_fact_stated_once_renders_without_a_count() {
        let mut m = memory();
        m.seen = 1;
        assert!(!render(&m, false).contains('×'), "{}", render(&m, false));
    }

    #[test]
    fn a_fact_with_no_date_renders_without_an_empty_prefix() {
        let mut m = memory();
        m.date = String::new();
        assert!(render(&m, false).starts_with("[01ABC] prefere"));
    }

    #[test]
    fn the_json_form_carries_provenance_for_scripting() {
        let line = render(&memory(), true);
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(parsed["id"], "01ABC");
        assert_eq!(parsed["agent"], "telegram-assistant");
        assert_eq!(parsed["source"], "telegram");
        assert_eq!(parsed["seen"], 3);
        assert_eq!(parsed["topics"][0], "agenda");
    }

    #[test]
    fn the_json_form_never_renders_a_rust_debug_shape() {
        // A `{:?}` leak would put `Some("telegram")` on a user's terminal.
        let line = render(&memory(), true);
        assert!(!line.contains("Some("), "{line}");
        assert!(!line.contains("Provenance"), "{line}");
    }
}
