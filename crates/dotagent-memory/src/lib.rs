//! Long-term memory for agents, stored in an embedded [outl] workspace.
//!
//! A conversation session already remembers what was just said — that is what
//! makes "yes" resolve to the thing that was proposed. This is the other kind:
//! facts that outlive the conversation, that you can read and edit yourself.
//!
//! Memories land in the workspace's journals, one block per fact, dated. The
//! result is a normal outl workspace — open it in the desktop app, edit a bad
//! memory by hand, delete a page. Nothing here is a private format.
//!
//! [outl]: https://github.com/avelino/outl
//!
//! ## The embedder contract
//!
//! Every mutation goes through `outl-actions` and is then **projected** back to
//! `.md`. Skipping the projection leaves the file on disk stale; editing the
//! `.md` directly writes state the CRDT knows nothing about. Both are the
//! documented ways to corrupt a workspace, so `remember` always does the pair.
//!
//! Each call opens and drops the workspace. That is deliberate: dotagent's MCP
//! server is a short-lived process, and holding the lock across a whole session
//! would block the desktop app. `outl-ws` hands us an ephemeral actor when
//! something else owns the config actor, which is the normal case, not an error.

use std::path::PathBuf;

use outl_actions::{append_block, apply_page_md_with_sidecar, page, read_page_outline};
use thiserror::Error;

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
}

pub type Result<T> = std::result::Result<T, MemoryError>;

/// One remembered fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// Journal slug it was written to (`2026-08-04`), which is also its date.
    pub date: String,
    pub text: String,
}

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
    /// Topics become `[[slug]]` references, and outl resolves the backlinks:
    /// opening the `roam` page shows every fact that mentioned roam, gathered
    /// from whichever day it was learned. That is the difference between a list
    /// of notes and something you can navigate — the same fact reachable
    /// chronologically and by subject, without storing it twice.
    ///
    /// Each topic page is created if absent, so a link never dangles.
    pub fn remember(&self, text: &str, topics: &[String]) -> Result<Memory> {
        let text = text.trim();
        if text.is_empty() {
            return Err(MemoryError::Empty);
        }

        let topics: Vec<String> = topics
            .iter()
            .filter_map(|t| {
                let slug = slugify(t);
                (!slug.is_empty()).then_some(slug)
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let mut ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;

        // Create the topic pages first. A `[[ref]]` to a page that does not
        // exist still renders, but nothing gathers under it, so the graph
        // silently has holes.
        //
        // Each one is projected to disk right away. Creating it only in the op
        // log leaves a page the CRDT knows about and the desktop app does not —
        // the graph would look empty to the one reader it exists for.
        for slug in &topics {
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

        let body = if topics.is_empty() {
            text.to_string()
        } else {
            let refs: Vec<String> = topics.iter().map(|s| format!("[[{s}]]")).collect();
            format!("{text} {}", refs.join(" "))
        };

        let page_id = page::open_today(&mut ctx.workspace, &ctx.hlc)
            .map_err(|e| MemoryError::Write(e.to_string()))?;
        append_block(&mut ctx.workspace, &ctx.hlc, Some(page_id), Some(&body))
            .map_err(|e| MemoryError::Write(e.to_string()))?;

        // Project, always. Without this the op log has the memory but the .md
        // on disk does not, and the next reader sees stale content.
        apply_page_md_with_sidecar(&ctx.workspace, &ctx.root, page_id)
            .map_err(|e| MemoryError::Write(e.to_string()))?;

        Ok(Memory {
            date: today_slug(),
            text: body,
        })
    }

    /// Every fact that references a topic, gathered from all journals.
    ///
    /// This is the backlink view: the fact lives once, in the day it was
    /// learned, and the topic page collects it. Returns an empty vec for a
    /// topic nobody ever linked.
    pub fn recall_topic(&self, topic: &str) -> Result<Vec<Memory>> {
        let slug = slugify(topic);
        let ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;

        let Some(meta) = page::list_all(&ctx.workspace)
            .into_iter()
            .find(|m| m.slug == slug)
        else {
            return Ok(Vec::new());
        };

        Ok(
            outl_actions::backlinks_for_page(&ctx.workspace, &ctx.root, &meta)
                .into_iter()
                .map(|b| Memory {
                    date: b
                        .source_page
                        .as_ref()
                        .map(|p| p.slug.clone())
                        .unwrap_or_default(),
                    text: b.block_text.trim().to_string(),
                })
                .collect(),
        )
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

    /// Find memories whose text contains `query`, case-insensitive.
    ///
    /// Substring rather than semantic: outl stores text, not vectors, and a
    /// fuzzy match that silently returns the wrong fact is worse than no match
    /// for something an assistant will state as truth. Newest first.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let ctx = outl_ws::open(&self.root).map_err(|e| MemoryError::Open(e.to_string()))?;
        let needle = query.trim().to_lowercase();

        let mut pages = page::list_all(&ctx.workspace);
        // Journal slugs are ISO dates, so a reverse lexicographic sort is
        // reverse chronological.
        pages.sort_by(|a, b| b.slug.cmp(&a.slug));

        let mut out = Vec::new();
        for meta in &pages {
            let Ok(outline) = read_page_outline(&ctx.root, meta) else {
                // A page that fails to project should not hide the rest.
                continue;
            };
            for node in flatten(&outline.nodes) {
                if needle.is_empty() || node.to_lowercase().contains(&needle) {
                    out.push(Memory {
                        date: meta.slug.clone(),
                        text: node,
                    });
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Depth-first text of every block, parents before children.
fn flatten(nodes: &[outl_actions::OutlineNode]) -> Vec<String> {
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

fn today_slug() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Journal slugs are ISO dates. Used to tell a day apart from a topic.
fn is_journal_slug(slug: &str) -> bool {
    let b = slug.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Normalize a topic into an outl slug.
///
/// Lowercase, `/` preserved for hierarchy (`ops/cost-report` is a real page
/// path in outl), everything else collapsed to a single dash. Without this,
/// "Roam Research" and "roam research" would be two disconnected pages and
/// the graph would fragment on capitalization.
fn slugify(topic: &str) -> String {
    let mut out = String::with_capacity(topic.len());
    let mut last_dash = true; // leading dashes are dropped
    for c in topic.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_dash = false;
        } else if c == '/' {
            out.push('/');
            last_dash = true;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Human-readable title from a slug, for the page header.
fn title_for(slug: &str) -> String {
    slug.rsplit('/')
        .next()
        .unwrap_or(slug)
        .split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    fn remembering_twice_keeps_both() {
        let (_dir, store) = store();
        store.remember("primeiro fato", &[]).unwrap();
        store.remember("segundo fato", &[]).unwrap();
        let all = store.recall("fato", 10).unwrap();
        assert!(all.iter().any(|m| m.text.contains("primeiro")), "{all:?}");
        assert!(all.iter().any(|m| m.text.contains("segundo")), "{all:?}");
    }

    // --- the graph: topics, links, backlinks ---

    #[test]
    fn slugify_normalizes_so_the_graph_does_not_fragment() {
        // "Roam Research" and "roam research" must be the same node.
        assert_eq!(slugify("Roam Research"), "roam-research");
        assert_eq!(slugify("roam research"), "roam-research");
        assert_eq!(slugify("  Roam   Research  "), "roam-research");
        // Hierarchy is a real thing in outl, so `/` survives.
        assert_eq!(slugify("ops/cost-report"), "ops/cost-report");
        assert_eq!(slugify("Reunião!!"), "reunião");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn title_is_readable_and_keeps_only_the_leaf() {
        assert_eq!(title_for("roam-research"), "Roam Research");
        assert_eq!(title_for("ops/cost-report"), "Cost Report");
    }

    #[test]
    fn is_journal_slug_tells_days_from_topics() {
        assert!(is_journal_slug("2026-08-04"));
        assert!(!is_journal_slug("roam-research"));
        assert!(!is_journal_slug("2026-08-0x"));
        assert!(!is_journal_slug("2026-08"));
    }

    #[test]
    fn a_fact_links_to_its_topics() {
        let (_dir, store) = store();
        let m = store
            .remember(
                "prefere reunião depois das 14h",
                &["reuniao".into(), "agenda".into()],
            )
            .unwrap();
        assert!(m.text.contains("[[reuniao]]"), "{}", m.text);
        assert!(m.text.contains("[[agenda]]"), "{}", m.text);
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
    fn recall_topic_gathers_facts_across_days_via_backlinks() {
        // The point of the graph: the fact lives in the journal, the topic
        // page collects it without a second copy.
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
        assert!(
            hits.iter().all(|m| m.text.contains("[[dotagent]]")),
            "{hits:?}"
        );
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
        assert_eq!(m.text.matches("[[roam]]").count(), 1, "{}", m.text);
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
}
