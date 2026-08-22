//! Long-term memory for agents, stored in an embedded [outl] workspace.
//!
//! A conversation session already remembers what was just said — that is what
//! makes "yes" resolve to the thing that was proposed. This is the other kind:
//! facts that outlive the conversation, that you can read and edit yourself.
//!
//! Memories land in the workspace's journals, one block per fact, dated, with
//! provenance hanging off the block as outl properties. The result is a normal
//! outl workspace — open it in the desktop app, edit a bad memory by hand,
//! delete a page. Nothing here is a private format.
//!
//! [outl]: https://github.com/avelino/outl
//!
//! ## Three things keep it from rotting
//!
//! A store that only ever appends degrades into noise, and recall from noise
//! is worse than no recall — an assistant states what it recalls as fact.
//!
//! - **Dedup on write.** The same fact stated twice reinforces one block
//!   (`seen::`) instead of creating a second one. Repetition becomes a
//!   ranking signal rather than a duplicate.
//! - **Supersede, don't contradict.** A fact that replaces another marks it
//!   `superseded-by::`. The old one stays readable in its journal — the
//!   history of a changed preference is worth keeping — but recall stops
//!   returning it, so the assistant is never picking between two answers.
//! - **Rank, don't just list.** [`MemoryStore::recall`] scores by shared
//!   terms first and recency second. See [`score`].
//!
//! ## The embedder contract
//!
//! Every mutation goes through `outl-actions` and is then **projected** back
//! to `.md`. Skipping the projection leaves the file on disk stale; editing
//! the `.md` directly writes state the CRDT knows nothing about. Both are the
//! documented ways to corrupt a workspace, so every write here does the pair.
//!
//! Each call opens and drops the workspace. That is deliberate: dotagent's MCP
//! server is a short-lived process, and holding the lock across a whole session
//! would block the desktop app. `outl-ws` hands us an ephemeral actor when
//! something else owns the config actor, which is the normal case, not an error.

pub mod record;
pub mod score;

use std::collections::HashMap;
use std::path::PathBuf;

use outl_actions::{apply_page_md_with_sidecar, block, page, OutlineNode};
use outl_core::id::NodeId;
use outl_core::property::PropValue;
use outl_core::workspace::Workspace;
use thiserror::Error;

use record::{
    block_text, dedup_key, is_journal_slug, keys, normalize_topics, slugify, split_links, title_for,
};
pub use record::{Memory, Provenance};

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("no outl workspace at {0} — run `outl init` there first")]
    NoWorkspace(PathBuf),
    #[error("opening workspace: {0}")]
    Open(String),
    #[error("writing memory: {0}")]
    Write(String),
    #[error("refusing to store an empty memory")]
    Empty,
    #[error("no memory with id {0}")]
    NotFound(String),
}

pub type Result<T> = std::result::Result<T, MemoryError>;

/// A memory store backed by an outl workspace directory.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    /// Point at an existing outl workspace, failing if there is none.
    ///
    /// Use this when the path came from a human: silently scaffolding at a
    /// mistyped path looks like it worked while writing memories nobody will
    /// ever find.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.join(".outl").exists() {
            return Err(MemoryError::NoWorkspace(root));
        }
        Ok(Self { root })
    }

    /// Open the workspace, scaffolding it if absent.
    ///
    /// This is the path for dotagent's own default location, where there is no
    /// typo to protect against and the alternative is memory that does not work
    /// until someone reads a doc and runs `outl init`.
    pub fn open_or_init(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.join(".outl").exists() {
            let paths = outl_ws::layout::Paths::at(root.clone());
            outl_ws::layout::init(&paths).map_err(|e| MemoryError::Open(e.to_string()))?;
        }
        Ok(Self { root })
    }

    /// Append a fact to today's journal, linked to its topics.
    ///
    /// Equivalent to [`remember_with`](Self::remember_with) with no
    /// provenance — for callers that have none to record, like a human at a
    /// prompt.
    pub fn remember(&self, text: &str, topics: &[String]) -> Result<Memory> {
        self.remember_with(text, topics, &Provenance::default())
    }

    /// Append a fact to today's journal, or reinforce the one already there.
    ///
    /// Topics become `[[slug]]` references, and outl resolves the backlinks:
    /// opening the `roam` page shows every fact that mentioned roam, gathered
    /// from whichever day it was learned. That is the difference between a
    /// list of notes and something you can navigate — the same fact reachable
    /// chronologically and by subject, without storing it twice. Each topic
    /// page is created if absent, so a link never dangles.
    ///
    /// If the same fact is already stored (same [`dedup_key`], not
    /// superseded), nothing new is appended: the existing block's `seen::`
    /// goes up, `last-seen::` moves to today, and any topic it did not have
    /// yet is added. A user restating a preference every other week should
    /// end up with one well-established fact, not fifteen copies that crowd
    /// out everything else in recall.
    pub fn remember_with(
        &self,
        text: &str,
        topics: &[String],
        provenance: &Provenance,
    ) -> Result<Memory> {
        let text = text.trim();
        if text.is_empty() {
            return Err(MemoryError::Empty);
        }
        let topics = normalize_topics(topics);
        let mut ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;

        if let Some(existing) = find_by_key(&ctx.workspace, &dedup_key(text)) {
            let memory = reinforce(&mut ctx, &existing, &topics)?;
            return Ok(memory);
        }

        self.ensure_topic_pages(&mut ctx, &topics)?;
        let page_id = page::open_today(&mut ctx.workspace, &ctx.hlc)
            .map_err(|e| MemoryError::Write(e.to_string()))?;
        let body = block_text(text, &topics);
        let node = block::append_block(&mut ctx.workspace, &ctx.hlc, Some(page_id), Some(&body))
            .map_err(|e| MemoryError::Write(e.to_string()))?;
        set_prop(&mut ctx, node, keys::SEEN, "1")?;
        write_provenance(&mut ctx, node, provenance)?;

        // Project, always. Without this the op log has the memory but the .md
        // on disk does not, and the next reader sees stale content.
        project(&ctx, page_id)?;

        Ok(Memory {
            id: node.to_string(),
            date: today_slug(),
            text: text.to_string(),
            topics,
            provenance: provenance.clone(),
            seen: 1,
            superseded_by: None,
        })
    }

    /// Replace a fact with a newer one, keeping the old one readable.
    ///
    /// The old block is marked `superseded-by::` rather than deleted:
    /// "prefere reunião de manhã" becoming "prefere depois das 14h" is a
    /// change worth being able to see, and deleting it would leave the
    /// journal for that day quietly rewritten. Recall skips superseded facts,
    /// so the assistant only ever sees the current one.
    pub fn supersede(
        &self,
        old_id: &str,
        text: &str,
        topics: &[String],
        provenance: &Provenance,
    ) -> Result<Memory> {
        let text = text.trim();
        if text.is_empty() {
            return Err(MemoryError::Empty);
        }
        // Written first so a failure leaves the old fact intact and current,
        // rather than retiring it with no replacement.
        let fresh = self.remember_with(text, topics, provenance)?;
        if fresh.id == old_id {
            // The "replacement" deduped into the very fact being replaced.
            // Marking it superseded by itself would retire it for good.
            return Ok(fresh);
        }

        let mut ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        let target = find_by_id(&ctx.workspace, old_id)
            .ok_or_else(|| MemoryError::NotFound(old_id.into()))?;
        set_prop(&mut ctx, target.node, keys::SUPERSEDED_BY, &fresh.id)?;
        project(&ctx, target.page)?;
        Ok(fresh)
    }

    /// Delete a fact outright. Returns `false` when no such fact exists.
    ///
    /// For facts that should never have been stored — noise, something
    /// private, a hallucination. A fact that merely stopped being true wants
    /// [`supersede`](Self::supersede) instead.
    pub fn forget(&self, id: &str) -> Result<bool> {
        let mut ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        let Some(target) = find_by_id(&ctx.workspace, id) else {
            return Ok(false);
        };
        block::delete(&mut ctx.workspace, &ctx.hlc, target.node)
            .map_err(|e| MemoryError::Write(e.to_string()))?;
        project(&ctx, target.page)?;
        Ok(true)
    }

    /// One fact by id, or `None` when it does not exist.
    pub fn get(&self, id: &str) -> Result<Option<Memory>> {
        let ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        Ok(find_by_id(&ctx.workspace, id).map(|f| f.memory))
    }

    /// Facts relevant to `query`, best first.
    ///
    /// Ranking is lexical: a fact competes only if it shares a real term with
    /// the query, and among those, recency and reinforcement break the tie
    /// (see [`score`]). Substring-and-hope is what this replaces — a query
    /// almost never appeared verbatim inside a stored fact, so recall used to
    /// return whatever was written most recently regardless of subject.
    ///
    /// An empty query means "the most recent facts", the same thing it always
    /// meant. Superseded facts never come back.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let terms = score::terms(query);
        if terms.is_empty() {
            // A query of nothing but stopwords is not a query. Treating it as
            // a match-everything would hand back an unranked recent dump
            // under the guise of relevance.
            return if query.trim().is_empty() {
                self.recent(limit)
            } else {
                Ok(Vec::new())
            };
        }
        let ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        let today = chrono::Local::now().date_naive();

        let mut scored: Vec<(f32, Memory)> = scan(&ctx.workspace)
            .into_iter()
            .filter(|f| !f.memory.is_superseded())
            .filter_map(|f| {
                let candidate = score::Candidate {
                    text: &f.memory.text,
                    topics: &f.memory.topics,
                    age_days: age_days(&f.memory.date, today),
                    seen: f.memory.seen,
                };
                let s = score::score(&terms, &candidate);
                (s > 0.0).then_some((s, f.memory))
            })
            .collect();

        sort_by_score(&mut scored);
        Ok(scored.into_iter().take(limit).map(|(_, m)| m).collect())
    }

    /// The most recent facts, newest and best-established first.
    ///
    /// The ordering is by journal date, not by page slug. Sorting every page
    /// by slug used to put any topic page starting with a letter ahead of
    /// every journal, so "recent" silently meant "alphabetically last page".
    pub fn recent(&self, limit: usize) -> Result<Vec<Memory>> {
        let ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        let today = chrono::Local::now().date_naive();
        let mut scored: Vec<(f32, Memory)> = scan(&ctx.workspace)
            .into_iter()
            .filter(|f| !f.memory.is_superseded())
            .map(|f| {
                let candidate = score::Candidate {
                    text: &f.memory.text,
                    topics: &f.memory.topics,
                    age_days: age_days(&f.memory.date, today),
                    seen: f.memory.seen,
                };
                (score::baseline(&candidate), f.memory)
            })
            .collect();
        sort_by_score(&mut scored);
        Ok(scored.into_iter().take(limit).map(|(_, m)| m).collect())
    }

    /// Every fact that references a topic, gathered from all journals.
    ///
    /// This is the backlink view: the fact lives once, in the day it was
    /// learned, and the topic page collects it. Asking the graph beats
    /// guessing which words the fact happened to use. Returns an empty vec
    /// for a topic nobody ever linked.
    pub fn recall_topic(&self, topic: &str) -> Result<Vec<Memory>> {
        let slug = slugify(topic);
        let ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        let mut hits: Vec<Memory> = scan(&ctx.workspace)
            .into_iter()
            .map(|f| f.memory)
            .filter(|m| !m.is_superseded() && m.topics.contains(&slug))
            .collect();
        hits.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(hits)
    }

    /// What the store holds, for a human deciding whether it needs pruning.
    pub fn stats(&self) -> Result<MemoryStats> {
        let ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        let facts = scan(&ctx.workspace);
        let superseded = facts.iter().filter(|f| f.memory.is_superseded()).count();
        let reinforced = facts
            .iter()
            .filter(|f| !f.memory.is_superseded() && f.memory.seen > 1)
            .count();
        let topics = page::list_all(&ctx.workspace)
            .into_iter()
            .filter(|m| !is_journal_slug(&m.slug))
            .count();
        Ok(MemoryStats {
            live: facts.len() - superseded,
            superseded,
            reinforced,
            topics,
        })
    }

    /// Topics that exist, i.e. every page that is not a journal.
    pub fn topics(&self) -> Result<Vec<String>> {
        let ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        Ok(page::list_all(&ctx.workspace)
            .into_iter()
            .filter(|m| !is_journal_slug(&m.slug))
            .map(|m| m.slug)
            .collect())
    }

    /// Create every topic page up front so a `[[ref]]` never dangles.
    ///
    /// A link to a page that does not exist still renders, but nothing
    /// gathers under it — the graph silently has holes. Each page is
    /// projected right away: creating it only in the op log leaves a page the
    /// CRDT knows about and the desktop app does not.
    fn ensure_topic_pages(&self, ctx: &mut outl_ws::WsCtx, topics: &[String]) -> Result<()> {
        for slug in topics {
            let topic_id = page::open_or_create(
                &mut ctx.workspace,
                &ctx.hlc,
                slug,
                &title_for(slug),
                page::PageKind::Page,
            )
            .map_err(|e| MemoryError::Write(e.to_string()))?;
            apply_page_md_with_sidecar(&ctx.workspace, &ctx.root, topic_id)
                .map_err(|e| MemoryError::Write(e.to_string()))?;
        }
        Ok(())
    }
}

/// A count of what the store holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryStats {
    /// Facts recall can return.
    pub live: usize,
    /// Facts a later one replaced. Still readable, never recalled.
    pub superseded: usize,
    /// Live facts stated more than once.
    pub reinforced: usize,
    /// Topic pages.
    pub topics: usize,
}

/// One fact, with the handles needed to mutate it.
struct Fact {
    node: NodeId,
    page: NodeId,
    memory: Memory,
}

/// Every fact in the workspace.
///
/// Blocks at any depth count. An outliner's whole point is that detail nests
/// under a claim, and a nested block is a more specific fact, not a footnote
/// — the consolidation pass groups facts under a heading and both levels
/// stay recallable. Properties never surface as facts: outl parses
/// `key:: value` into the parent block's properties, so provenance can sit
/// with the fact without recall ever handing back `agent:: telegram-assistant`
/// as something the assistant knows.
fn scan(ws: &Workspace) -> Vec<Fact> {
    let children = children_index(ws);
    let mut out = Vec::new();
    for page_id in children.get(&NodeId::root()).into_iter().flatten() {
        let Some(meta) = page::page_meta(ws, *page_id) else {
            continue;
        };
        // Facts live in journals; a page contributes the ones a human filed
        // there by hand. Only a journal slug carries a date.
        let date = if is_journal_slug(&meta.slug) {
            meta.slug.clone()
        } else {
            String::new()
        };
        collect(ws, &children, *page_id, *page_id, &date, &mut out);
    }
    out
}

fn collect(
    ws: &Workspace,
    children: &ChildIndex,
    page: NodeId,
    parent: NodeId,
    date: &str,
    out: &mut Vec<Fact>,
) {
    for node in children.get(&parent).into_iter().flatten() {
        if let Some(memory) = read_fact(ws, *node, date) {
            out.push(Fact {
                node: *node,
                page,
                memory,
            });
        }
        collect(ws, children, page, *node, date, out);
    }
}

type ChildIndex = HashMap<NodeId, Vec<NodeId>>;

/// `parent -> children in order`, built in one pass.
///
/// `outl_actions::tree::children_of` scans every node per call, so walking a
/// whole workspace with it is quadratic. Memory scans the whole workspace on
/// every recall, which is exactly the case that must not be.
fn children_index(ws: &Workspace) -> ChildIndex {
    let mut rows: Vec<(NodeId, NodeId, outl_core::fractional::Fractional)> = ws
        .tree()
        .iter_nodes()
        .map(|(id, parent, pos)| (id, parent, pos.clone()))
        .collect();
    rows.sort_by(|a, b| a.2.cmp(&b.2));
    let mut index: ChildIndex = HashMap::new();
    for (id, parent, _) in rows {
        index.entry(parent).or_default().push(id);
    }
    index
}

/// Read one block as a fact, or `None` when it holds no text.
fn read_fact(ws: &Workspace, node: NodeId, date: &str) -> Option<Memory> {
    let raw = ws.block_text(node).unwrap_or_default();
    if raw.trim().is_empty() {
        return None;
    }
    let (text, topics) = split_links(&raw);
    if text.trim().is_empty() && topics.is_empty() {
        return None;
    }
    let mut memory = Memory {
        id: node.to_string(),
        date: date.to_string(),
        text,
        topics,
        provenance: Provenance::default(),
        seen: 1,
        superseded_by: None,
    };
    for (key, value) in ws.tree().properties_of(node) {
        let Some(value) = prop_text(value) else {
            continue;
        };
        match key {
            keys::AGENT => memory.provenance.agent = Some(value),
            keys::SOURCE => memory.provenance.source = Some(value),
            keys::SESSION => memory.provenance.session = Some(value),
            // A fact written before `seen::` existed, or one a human typed
            // by hand, reads as stated once rather than as unranked.
            keys::SEEN => memory.seen = value.trim().parse().unwrap_or(1).max(1),
            keys::SUPERSEDED_BY => memory.superseded_by = Some(value),
            _ => {}
        }
    }
    Some(memory)
}

fn prop_text(value: &PropValue) -> Option<String> {
    match value {
        PropValue::Text(s) | PropValue::PageRef(s) | PropValue::Tag(s) => Some(s.clone()),
        PropValue::List(_) => None,
    }
}

/// The live fact matching `key`, if any. Superseded facts do not count —
/// restating one is stating it fresh, not reviving the retired copy.
fn find_by_key(ws: &Workspace, key: &str) -> Option<Fact> {
    if key.is_empty() {
        return None;
    }
    scan(ws)
        .into_iter()
        .find(|f| !f.memory.is_superseded() && dedup_key(&f.memory.text) == key)
}

fn find_by_id(ws: &Workspace, id: &str) -> Option<Fact> {
    scan(ws).into_iter().find(|f| f.memory.id == id)
}

/// Bump an existing fact instead of storing it twice, adding any topic it
/// was not linked to yet.
fn reinforce(ctx: &mut outl_ws::WsCtx, existing: &Fact, topics: &[String]) -> Result<Memory> {
    let mut memory = existing.memory.clone();
    memory.seen = memory.seen.saturating_add(1);

    let missing: Vec<String> = topics
        .iter()
        .filter(|t| !memory.topics.contains(t))
        .cloned()
        .collect();
    if !missing.is_empty() {
        memory.topics.extend(missing);
        memory.topics.sort();
        memory.topics.dedup();
        let store = MemoryStore {
            root: ctx.root.clone(),
        };
        store.ensure_topic_pages(ctx, &memory.topics)?;
        block::edit_text(
            &mut ctx.workspace,
            &ctx.hlc,
            existing.node,
            &memory.to_block_text(),
        )
        .map_err(|e| MemoryError::Write(e.to_string()))?;
    }

    set_prop(ctx, existing.node, keys::SEEN, &memory.seen.to_string())?;
    set_prop(ctx, existing.node, keys::LAST_SEEN, &today_slug())?;
    project(ctx, existing.page)?;
    Ok(memory)
}

fn write_provenance(ctx: &mut outl_ws::WsCtx, node: NodeId, provenance: &Provenance) -> Result<()> {
    for (key, value) in [
        (keys::AGENT, provenance.agent.as_deref()),
        (keys::SOURCE, provenance.source.as_deref()),
        (keys::SESSION, provenance.session.as_deref()),
    ] {
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
            set_prop(ctx, node, key, value)?;
        }
    }
    Ok(())
}

fn set_prop(ctx: &mut outl_ws::WsCtx, node: NodeId, key: &str, value: &str) -> Result<()> {
    page::set_property(
        &mut ctx.workspace,
        &ctx.hlc,
        node,
        key,
        Some(PropValue::Text(value.to_string())),
    )
    .map_err(|e| MemoryError::Write(e.to_string()))
}

fn project(ctx: &outl_ws::WsCtx, page_id: NodeId) -> Result<()> {
    apply_page_md_with_sidecar(&ctx.workspace, &ctx.root, page_id)
        .map(|_| ())
        .map_err(|e| MemoryError::Write(e.to_string()))
}

/// Best score first; ties broken by date so the order is stable across runs
/// rather than dependent on tree iteration.
fn sort_by_score(scored: &mut [(f32, Memory)]) {
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.date.cmp(&a.1.date))
            .then_with(|| a.1.text.cmp(&b.1.text))
    });
}

/// Days between a fact's journal and today. A fact with no date (filed on a
/// page by hand) reads as current: a human curating a topic page is a
/// stronger signal than an agent's automatic write, not a weaker one.
fn age_days(date: &str, today: chrono::NaiveDate) -> i64 {
    let Ok(day) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
        return 0;
    };
    (today - day).num_days()
}

fn today_slug() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Depth-first text of every block, parents before children.
#[doc(hidden)]
pub fn flatten(nodes: &[OutlineNode]) -> Vec<String> {
    let mut out = Vec::new();
    for n in nodes {
        let text = n.text.trim();
        if !text.is_empty() {
            out.push(text.to_string());
        }
        out.extend(flatten(&n.children));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_refuses_a_directory_that_is_not_a_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err = MemoryStore::open(dir.path()).unwrap_err();
        assert!(matches!(err, MemoryError::NoWorkspace(_)));
    }

    #[test]
    fn open_refuses_a_missing_path() {
        let err = MemoryStore::open("/nonexistent/outl/workspace").unwrap_err();
        assert!(matches!(err, MemoryError::NoWorkspace(_)));
    }

    #[test]
    fn remember_rejects_empty_and_whitespace() {
        // Constructed directly: the guard runs before any workspace access, so
        // this needs no outl fixture.
        let store = MemoryStore {
            root: PathBuf::from("/tmp/unused"),
        };
        assert!(matches!(store.remember("", &[]), Err(MemoryError::Empty)));
        assert!(matches!(
            store.remember("   \n ", &[]),
            Err(MemoryError::Empty)
        ));
    }

    #[test]
    fn today_slug_is_iso() {
        let s = today_slug();
        assert_eq!(s.len(), 10, "{s}");
        assert_eq!(s.matches('-').count(), 2, "{s}");
    }

    #[test]
    fn age_days_counts_back_from_today() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
        assert_eq!(age_days("2026-08-21", today), 0);
        assert_eq!(age_days("2026-08-11", today), 10);
        // No date: a hand-filed fact reads as current.
        assert_eq!(age_days("", today), 0);
        assert_eq!(age_days("not-a-date", today), 0);
    }

    // --- round trip against a real workspace ---
    //
    // These drive outl end to end rather than mocking it. The contract that
    // matters (mutate through an action, then project to .md) only holds if
    // a real read sees what a real write left behind.

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_or_init(dir.path()).expect("scaffold workspace");
        (dir, store)
    }

    #[test]
    fn open_or_init_scaffolds_a_usable_workspace() {
        let (dir, _store) = store();
        assert!(dir.path().join(".outl").is_dir());
        // Re-opening must not fail or clobber.
        assert!(MemoryStore::open(dir.path()).is_ok());
    }

    #[test]
    fn a_remembered_fact_is_recalled() {
        let (_dir, store) = store();
        store.remember("prefiro reunião de manhã", &[]).unwrap();
        let hits = store.recall("manhã", 10).unwrap();
        assert!(
            hits.iter().any(|m| m.text.contains("reunião de manhã")),
            "{hits:?}"
        );
    }

    #[test]
    fn recall_is_case_insensitive() {
        let (_dir, store) = store();
        store.remember("Projeto favorito: dotagent", &[]).unwrap();
        assert!(!store.recall("DOTAGENT", 10).unwrap().is_empty());
        assert!(!store.recall("dotagent", 10).unwrap().is_empty());
    }

    #[test]
    fn recall_misses_return_empty_rather_than_guessing() {
        // An assistant states recall results as fact, so a fuzzy near-match
        // would become a confident lie.
        let (_dir, store) = store();
        store.remember("prefiro reunião de manhã", &[]).unwrap();
        assert!(store.recall("kubernetes", 10).unwrap().is_empty());
    }

    #[test]
    fn recall_answers_a_question_that_is_not_a_substring() {
        // The regression that motivated scoring: the message is never a
        // substring of the fact, so the old `contains` recall returned
        // nothing and the assistant answered from recency alone.
        let (_dir, store) = store();
        store
            .remember("databricks-cost-daily quebrou em 07-13", &[])
            .unwrap();
        store
            .remember("almoçou no japonês da esquina", &[])
            .unwrap();
        let hits = store
            .recall("e aí, o que ficou pendente do databricks?", 10)
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].text.contains("databricks-cost-daily"), "{hits:?}");
    }

    #[test]
    fn recall_ranks_the_relevant_fact_above_the_merely_recent_one() {
        let (_dir, store) = store();
        store
            .remember("prefere café sem açúcar", &["cafe".into()])
            .unwrap();
        store
            .remember("o dotagent roda no launchd", &["dotagent".into()])
            .unwrap();
        let hits = store.recall("dotagent", 10).unwrap();
        assert!(hits[0].text.contains("launchd"), "{hits:?}");
    }

    #[test]
    fn recall_honors_the_limit() {
        let (_dir, store) = store();
        for i in 0..5 {
            store.remember(&format!("fato numero {i}"), &[]).unwrap();
        }
        assert_eq!(store.recall("fato", 2).unwrap().len(), 2);
    }

    #[test]
    fn an_empty_query_returns_recent_memories() {
        let (_dir, store) = store();
        store.remember("alguma coisa", &[]).unwrap();
        assert!(!store.recall("", 10).unwrap().is_empty());
    }

    #[test]
    fn a_query_of_only_stopwords_returns_nothing() {
        // Not the same as an empty query: the user asked something, and
        // answering with an unranked recent dump would look like relevance.
        let (_dir, store) = store();
        store.remember("alguma coisa", &[]).unwrap();
        assert!(store.recall("o que é isso", 10).unwrap().is_empty());
    }

    #[test]
    fn remembering_twice_keeps_both() {
        let (_dir, store) = store();
        store.remember("primeiro fato", &[]).unwrap();
        store.remember("segundo fato", &[]).unwrap();
        let all = store.recall("fato", 10).unwrap();
        assert!(all.iter().any(|m| m.text.contains("primeiro")), "{all:?}");
        assert!(all.iter().any(|m| m.text.contains("segundo")), "{all:?}");
    }

    #[test]
    fn recall_returns_the_statement_without_the_links() {
        // The block on disk carries `[[topic]]`; what the assistant reads
        // should be the sentence, not the markup.
        let (_dir, store) = store();
        store
            .remember("usa rust no dotagent", &["dotagent".into()])
            .unwrap();
        let hits = store.recall("rust", 10).unwrap();
        assert_eq!(hits[0].text, "usa rust no dotagent");
        assert_eq!(hits[0].topics, vec!["dotagent".to_string()]);
    }

    // --- the graph: topics, links, backlinks ---

    #[test]
    fn a_fact_links_to_its_topics() {
        let (_dir, store) = store();
        let m = store
            .remember(
                "prefere reunião depois das 14h",
                &["reuniao".into(), "agenda".into()],
            )
            .unwrap();
        assert_eq!(
            m.to_block_text(),
            "prefere reunião depois das 14h [[agenda]] [[reuniao]]"
        );
    }

    #[test]
    fn topic_pages_are_created_so_links_never_dangle() {
        let (_dir, store) = store();
        store
            .remember("mantém dotagent e outl", &["Dotagent".into()])
            .unwrap();
        assert!(store.topics().unwrap().contains(&"dotagent".to_string()));
    }

    #[test]
    fn a_topic_page_exists_on_disk_not_only_in_the_op_log() {
        // The whole reason for outl as the backend is that you can open it.
        // A page created without projecting is invisible to every reader.
        let (dir, store) = store();
        store.remember("fato", &["algum-assunto".into()]).unwrap();
        let md = dir.path().join("pages").join("algum-assunto.md");
        assert!(md.exists(), "topic page was not projected to disk");
    }

    #[test]
    fn recall_topic_gathers_facts_across_days() {
        // The point of the graph: the fact lives in the journal, the topic
        // gathers it without a second copy.
        let (_dir, store) = store();
        store
            .remember("usa rust no dotagent", &["dotagent".into()])
            .unwrap();
        store
            .remember("dotagent roda no launchd", &["dotagent".into()])
            .unwrap();
        store
            .remember("nada a ver", &["outra-coisa".into()])
            .unwrap();

        let hits = store.recall_topic("dotagent").unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
    }

    #[test]
    fn recall_topic_is_slug_insensitive() {
        let (_dir, store) = store();
        store.remember("fato", &["Roam Research".into()]).unwrap();
        assert_eq!(store.recall_topic("roam research").unwrap().len(), 1);
        assert_eq!(store.recall_topic("Roam Research").unwrap().len(), 1);
    }

    #[test]
    fn recall_topic_on_an_unknown_topic_is_empty_not_an_error() {
        let (_dir, store) = store();
        assert!(store.recall_topic("nunca-mencionado").unwrap().is_empty());
    }

    #[test]
    fn duplicate_topics_produce_one_link_each() {
        let (_dir, store) = store();
        let m = store
            .remember("fato", &["roam".into(), "Roam".into(), "roam".into()])
            .unwrap();
        assert_eq!(m.topics, vec!["roam".to_string()]);
    }

    #[test]
    fn topics_excludes_journals() {
        let (_dir, store) = store();
        store.remember("fato", &["assunto".into()]).unwrap();
        let topics = store.topics().unwrap();
        assert!(topics.contains(&"assunto".to_string()));
        assert!(
            !topics.iter().any(|t| is_journal_slug(t)),
            "journals must not show up as topics: {topics:?}"
        );
    }

    #[test]
    fn the_memory_lands_in_a_readable_markdown_file() {
        // The whole point of outl as the backend: you can open it yourself.
        // If the projection step were skipped the op log would have the fact
        // and the file on disk would not.
        let (dir, store) = store();
        store.remember("fato projetado em disco", &[]).unwrap();
        let journals = dir.path().join("journals");
        let found = std::fs::read_dir(&journals)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                std::fs::read_to_string(e.path())
                    .map(|c| c.contains("fato projetado em disco"))
                    .unwrap_or(false)
            });
        assert!(found, "no journal .md contains the memory");
    }

    // --- provenance ---

    #[test]
    fn provenance_round_trips_as_block_properties() {
        let (_dir, store) = store();
        let prov = Provenance::from("telegram-assistant", "telegram").with_session("abc123");
        store
            .remember_with("fato com origem", &["origem".into()], &prov)
            .unwrap();
        let hits = store.recall("origem", 10).unwrap();
        assert_eq!(hits[0].provenance, prov, "{hits:?}");
    }

    #[test]
    fn provenance_is_written_as_properties_not_as_child_facts() {
        // A child block would be a second fact, and recall would hand back
        // `agent:: telegram-assistant` as something the assistant knows.
        let (dir, store) = store();
        store
            .remember_with(
                "fato com origem",
                &[],
                &Provenance::agent("telegram-assistant"),
            )
            .unwrap();
        let md = std::fs::read_to_string(
            dir.path()
                .join("journals")
                .join(format!("{}.md", today_slug())),
        )
        .unwrap();
        assert!(md.contains("agent:: telegram-assistant"), "{md}");
        assert!(
            !md.contains("- agent::"),
            "provenance leaked as a block: {md}"
        );
        // And it never comes back as a fact of its own.
        assert!(store.recall("telegram-assistant", 10).unwrap().is_empty());
    }

    #[test]
    fn a_fact_without_provenance_writes_no_empty_properties() {
        let (dir, store) = store();
        store.remember("fato anônimo", &[]).unwrap();
        let md = std::fs::read_to_string(
            dir.path()
                .join("journals")
                .join(format!("{}.md", today_slug())),
        )
        .unwrap();
        assert!(!md.contains("agent::"), "{md}");
        assert!(!md.contains("source::"), "{md}");
    }

    // --- dedup, supersede, forget ---

    #[test]
    fn restating_a_fact_reinforces_it_instead_of_duplicating() {
        let (_dir, store) = store();
        store
            .remember("prefere reunião depois das 14h", &[])
            .unwrap();
        let again = store
            .remember("Prefere reunião depois das 14h.", &[])
            .unwrap();
        assert_eq!(again.seen, 2);
        let hits = store.recall("reunião", 10).unwrap();
        assert_eq!(hits.len(), 1, "the fact must be stored once: {hits:?}");
        assert_eq!(hits[0].seen, 2);
    }

    #[test]
    fn reinforcing_keeps_the_original_id() {
        let (_dir, store) = store();
        let first = store.remember("fato estável", &[]).unwrap();
        let again = store.remember("fato estável", &[]).unwrap();
        assert_eq!(first.id, again.id);
    }

    #[test]
    fn restating_a_fact_with_a_new_topic_adds_the_link() {
        let (_dir, store) = store();
        store.remember("usa rust", &["rust".into()]).unwrap();
        let again = store.remember("usa rust", &["dotagent".into()]).unwrap();
        assert_eq!(
            again.topics,
            vec!["dotagent".to_string(), "rust".to_string()]
        );
        // And the new topic page exists, so the link does not dangle.
        assert!(store.topics().unwrap().contains(&"dotagent".to_string()));
        assert_eq!(store.recall_topic("dotagent").unwrap().len(), 1);
    }

    #[test]
    fn different_facts_are_not_deduplicated() {
        let (_dir, store) = store();
        store.remember("prefere reunião de manhã", &[]).unwrap();
        store
            .remember("prefere reunião depois das 14h", &[])
            .unwrap();
        assert_eq!(store.recall("reunião", 10).unwrap().len(), 2);
    }

    #[test]
    fn a_superseded_fact_stops_being_recalled_but_stays_on_disk() {
        let (dir, store) = store();
        let old = store.remember("prefere reunião de manhã", &[]).unwrap();
        store
            .supersede(
                &old.id,
                "prefere reunião depois das 14h",
                &[],
                &Provenance::default(),
            )
            .unwrap();

        let hits = store.recall("reunião", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].text.contains("14h"), "{hits:?}");

        // The history is still there for a human to read.
        let md = std::fs::read_to_string(
            dir.path()
                .join("journals")
                .join(format!("{}.md", today_slug())),
        )
        .unwrap();
        assert!(md.contains("de manhã"), "{md}");
        assert!(md.contains("superseded-by::"), "{md}");
    }

    #[test]
    fn superseding_an_unknown_id_is_an_error() {
        let (_dir, store) = store();
        let err = store
            .supersede("nao-existe", "novo fato", &[], &Provenance::default())
            .unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn superseding_a_fact_with_itself_does_not_retire_it() {
        // The replacement text dedups into the very fact being replaced;
        // marking it superseded by itself would silently erase it.
        let (_dir, store) = store();
        let old = store.remember("prefere café sem açúcar", &[]).unwrap();
        let new = store
            .supersede(
                &old.id,
                "Prefere café sem açúcar.",
                &[],
                &Provenance::default(),
            )
            .unwrap();
        assert_eq!(new.id, old.id);
        assert_eq!(store.recall("café", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_superseded_fact_does_not_block_restating_the_same_thing() {
        // Restating a retired fact stores it fresh rather than reviving the
        // retired copy, which recall would still be skipping.
        let (_dir, store) = store();
        let old = store.remember("prefere reunião de manhã", &[]).unwrap();
        store
            .supersede(
                &old.id,
                "prefere depois das 14h",
                &[],
                &Provenance::default(),
            )
            .unwrap();
        store.remember("prefere reunião de manhã", &[]).unwrap();
        let hits = store.recall("manhã", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(!hits[0].is_superseded());
    }

    #[test]
    fn forget_removes_a_fact_for_good() {
        let (_dir, store) = store();
        let m = store
            .remember("algo que não devia estar aqui", &[])
            .unwrap();
        assert!(store.forget(&m.id).unwrap());
        assert!(store.recall("devia", 10).unwrap().is_empty());
        assert!(store.get(&m.id).unwrap().is_none());
    }

    #[test]
    fn forgetting_an_unknown_id_reports_false_rather_than_failing() {
        let (_dir, store) = store();
        assert!(!store.forget("nao-existe").unwrap());
    }

    #[test]
    fn forget_reprojects_the_page_so_disk_agrees() {
        let (dir, store) = store();
        let m = store.remember("apagar isso", &[]).unwrap();
        store.forget(&m.id).unwrap();
        let md = std::fs::read_to_string(
            dir.path()
                .join("journals")
                .join(format!("{}.md", today_slug())),
        )
        .unwrap();
        assert!(!md.contains("apagar isso"), "{md}");
    }

    #[test]
    fn get_returns_a_fact_by_id() {
        let (_dir, store) = store();
        let m = store.remember("fato endereçável", &["x".into()]).unwrap();
        let back = store.get(&m.id).unwrap().expect("fact exists");
        assert_eq!(back.text, "fato endereçável");
        assert_eq!(back.topics, vec!["x".to_string()]);
    }

    #[test]
    fn stats_count_what_the_store_holds() {
        let (_dir, store) = store();
        store.remember("fato um", &["a".into()]).unwrap();
        store.remember("fato um", &[]).unwrap(); // reinforces
        let old = store.remember("fato dois", &[]).unwrap();
        store
            .supersede(&old.id, "fato dois corrigido", &[], &Provenance::default())
            .unwrap();

        let s = store.stats().unwrap();
        assert_eq!(s.live, 2, "{s:?}");
        assert_eq!(s.superseded, 1, "{s:?}");
        assert_eq!(s.reinforced, 1, "{s:?}");
        assert_eq!(s.topics, 1, "{s:?}");
    }

    #[test]
    fn stats_on_an_empty_store_are_zero() {
        let (_dir, store) = store();
        assert_eq!(store.stats().unwrap(), MemoryStats::default());
    }

    #[test]
    fn recent_is_ordered_by_journal_date_not_by_page_slug() {
        // The old sort put any topic page starting with a letter ahead of
        // every journal, so "recent" meant "alphabetically last page".
        let (_dir, store) = store();
        store
            .remember("fato do dia", &["zzz-topico".into()])
            .unwrap();
        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 1, "{recent:?}");
        assert_eq!(recent[0].text, "fato do dia");
    }
}
