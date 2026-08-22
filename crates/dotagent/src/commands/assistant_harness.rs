//! Assistant harness glue for the gateway adapter.
//!
//! Everything here answers two questions for one trigger:
//!
//! 1. **Before the run** — which environment should the agent see?
//!    ([`harness_env`]: session pointer, toolkit config path + hash, memory
//!    block)
//! 2. **After the run** — what should the daemon remember and deliver?
//!    ([`finalize`]: apply the session frame, retire oversized transcripts,
//!    strip `MEMO:` capture lines from the reply)
//!
//! Both are no-ops unless the manifest opts in via `[assistant]`. A failure
//! anywhere in the harness degrades to "no context / unchanged reply" — it
//! sits in the hot path of every conversation turn and must never break one.

use std::path::PathBuf;

use dotagent_assistant::registry::{RegistryChange, DEFAULT_TRANSCRIPT_BYTES_MAX};
use dotagent_assistant::store::{ensure_toolkit_file, RegistryStore};
use dotagent_assistant::toolkit::build_mcp_config;
use dotagent_assistant::{recall_context_block, strip_memos, CapturedMemo, RegistryRecord};
use dotagent_core::manifest::AgentManifest;
use dotagent_core::TriggerRequest;
use dotagent_memory::Provenance;
use tracing::{info, warn};

use crate::gateway::AssistantSessionFrame;

/// Upper bound of the injected memory block. Recall is a recall, not a
/// dump: the block shares the prompt with the user's message and the system
/// prompt, and a runaway memory must not crowd either out.
const MAX_MEMORY_BLOCK_BYTES: usize = 2_048;

/// Where the harness reads and writes. Derived from the daemon's canonical
/// paths so CLI and daemon agree on the same files.
#[derive(Debug, Clone)]
pub(crate) struct HarnessDirs {
    /// `state/assistant/` — registry records + content-addressed toolkits.
    pub assistant_state: PathBuf,
    /// The outl memory workspace.
    pub memory_root: PathBuf,
    /// Absolute path of this daemon binary, for the `dotagent` MCP server.
    pub dotagent_bin: PathBuf,
}

impl HarnessDirs {
    pub fn from_defaults() -> Self {
        Self {
            assistant_state: dotagent_state::paths::state_dir().join("assistant"),
            memory_root: dotagent_state::paths::memory_workspace_dir(),
            dotagent_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dotagent")),
        }
    }
}

/// Whether the harness should act for this manifest at all.
pub(crate) fn enabled(manifest: &AgentManifest) -> bool {
    manifest.assistant.as_ref().is_some_and(|a| a.enabled)
}

/// Environment injected before the run. Empty when the harness is off.
///
/// Registry persistence happens here, not after the run: a toolkit change
/// must invalidate the recorded pointer even if this very run then fails,
/// otherwise the next trigger resumes a session that cannot see the new
/// tools.
pub(crate) fn harness_env(
    manifest: &AgentManifest,
    req: &TriggerRequest,
    dirs: &HarnessDirs,
) -> Vec<(String, String)> {
    let Some(config) = manifest.assistant.as_ref().filter(|a| a.enabled) else {
        return Vec::new();
    };
    let mut env = Vec::new();
    let store = RegistryStore::new(&dirs.assistant_state);
    let mut record = load_record(req, &store);
    if !config.toolkit.servers.is_empty() {
        let bin = dirs.dotagent_bin.to_string_lossy();
        match build_mcp_config(&config.toolkit.servers, &bin) {
            Ok(mcp) => match ensure_toolkit_file(&dirs.assistant_state, &mcp) {
                Ok((path, hash)) => {
                    let toolkit_changed = record.as_mut().is_some_and(|r| {
                        matches!(r.note_toolkit_hash(&hash), RegistryChange::ToolkitChanged)
                    });
                    if toolkit_changed {
                        info!(
                            agent = %req.agent,
                            "assistant harness: toolkit changed; recorded session pointer cleared"
                        );
                    }
                    env.push((
                        "AGENT_ASSISTANT_MCP_CONFIG".to_string(),
                        path.to_string_lossy().into_owned(),
                    ));
                    env.push(("AGENT_ASSISTANT_TOOLKIT_HASH".to_string(), hash));
                }
                Err(e) => {
                    warn!(error = %e, "assistant harness: toolkit provisioning failed; running without it")
                }
            },
            Err(e) => {
                warn!(error = %e, "assistant harness: toolkit assembly failed; running without it")
            }
        }
    }
    if let Some(pointer) = record.as_ref().and_then(|r| r.session_pointer()) {
        env.push(("AGENT_ASSISTANT_SESSION".to_string(), pointer.to_string()));
    }
    if config.memory {
        let query = req
            .payload
            .as_ref()
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let block = recall_context_block(&dirs.memory_root, query, MAX_MEMORY_BLOCK_BYTES);
        if !block.is_empty() {
            env.push(("AGENT_ASSISTANT_MEMORY".to_string(), block));
        }
    }

    if let Some(record) = record {
        if let Err(e) = store.save(&record, chrono::Utc::now()) {
            warn!(error = %e, "assistant harness: could not persist registry record");
        }
    }
    env
}

/// What [`finalize`] tells the caller to flush after delivering the reply.
pub(crate) struct FinalizeOutcome {
    /// The reply the sink should see (`MEMO:` lines stripped).
    pub reply: String,
    /// Captured facts bound for the memory workspace. The caller flushes
    /// them off the reply path — delivery must not wait on the outl write.
    pub memos: Vec<CapturedMemo>,
    /// Who is writing these facts, recorded alongside them. A memory store
    /// you cannot audit is one you stop trusting: when the assistant states
    /// something wrong, the first question is which run put it there.
    pub provenance: Provenance,
}

/// Apply the run's session frame and strip capture lines from the reply.
///
/// Pass-through when the harness is off. A reported session frame retires
/// the record's pointer when the transcript passed the ceiling — the next
/// trigger then starts fresh while durable facts survive in memory.
pub(crate) fn finalize(
    manifest: &AgentManifest,
    req: &TriggerRequest,
    reply: String,
    session_frame: Option<AssistantSessionFrame>,
    dirs: &HarnessDirs,
) -> FinalizeOutcome {
    let Some(config) = manifest.assistant.as_ref().filter(|a| a.enabled) else {
        return FinalizeOutcome {
            reply,
            memos: Vec::new(),
            provenance: Provenance::default(),
        };
    };

    let store = RegistryStore::new(&dirs.assistant_state);
    let mut record = load_record(req, &store);
    if let (Some(frame), Some(record)) = (&session_frame, record.as_mut()) {
        let ceiling = config
            .transcript_bytes_max
            .unwrap_or(DEFAULT_TRANSCRIPT_BYTES_MAX);
        if matches!(
            record.apply_session_frame(&frame.claude_session, frame.transcript_bytes, ceiling),
            RegistryChange::Retired
        ) {
            info!(
                agent = %req.agent,
                bytes = frame.transcript_bytes,
                "assistant harness: transcript retired; next run starts a fresh session"
            );
        }
    }
    if let Some(record) = record {
        if let Err(e) = store.save(&record, chrono::Utc::now()) {
            warn!(error = %e, "assistant harness: could not persist registry record after run");
        }
    }

    let (reply, memos) = strip_memos(&reply);
    FinalizeOutcome {
        reply,
        memos: if config.memory { memos } else { Vec::new() },
        provenance: provenance(req),
    }
}

/// Provenance for facts captured from this trigger.
fn provenance(req: &TriggerRequest) -> Provenance {
    let mut p = Provenance::from(req.agent.clone(), req.source.to_string());
    if let Some(session) = req.session_id.as_deref() {
        p = p.with_session(session);
    }
    p
}

/// The record for this conversation, fresh when none was ever saved.
/// `None` only when the trigger carries no session id — there is nothing
/// to persist for a conversation that has no key.
fn load_record(req: &TriggerRequest, store: &RegistryStore) -> Option<RegistryRecord> {
    let session_id = req.session_id.as_deref()?;
    match store.load(&req.source.to_string(), session_id) {
        Ok(Some(record)) => Some(record),
        Ok(None) => Some(RegistryRecord::new(req.source.to_string(), session_id)),
        Err(e) => {
            warn!(error = %e, "assistant harness: registry unreadable; starting a fresh record");
            Some(RegistryRecord::new(req.source.to_string(), session_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotagent_core::manifest::{AssistantConfig, AssistantToolkit, ToolkitServer};
    use dotagent_core::{TriggerRequest, TriggerSource};
    use tempfile::tempdir;

    fn dirs(temp: &tempfile::TempDir) -> HarnessDirs {
        HarnessDirs {
            assistant_state: temp.path().join("assistant"),
            memory_root: temp.path().join("memory"),
            dotagent_bin: PathBuf::from("/bin/dotagent"),
        }
    }

    fn manifest_with(assistant: Option<AssistantConfig>) -> AgentManifest {
        let raw = r#"
            [agent]
            name = "dispatcher"
            [run]
            command = "true"
        "#;
        let mut manifest: AgentManifest = toml::from_str(raw).unwrap();
        manifest.assistant = assistant;
        manifest
    }

    fn request(session_id: Option<&str>, text: &str) -> TriggerRequest {
        TriggerRequest {
            agent: "dispatcher".into(),
            source: TriggerSource::Telegram,
            actor: Some("42".into()),
            session_id: session_id.map(str::to_string),
            reply_to: None,
            schedule: None,
            args: Vec::new(),
            payload: Some(serde_json::json!({ "text": text })),
        }
    }

    fn assistant_config(servers: Vec<ToolkitServer>) -> AssistantConfig {
        AssistantConfig {
            enabled: true,
            memory: true,
            transcript_bytes_max: None,
            toolkit: AssistantToolkit { servers },
        }
    }

    #[test]
    fn harness_off_injects_nothing() {
        let temp = tempdir().unwrap();
        let manifest = manifest_with(None);
        let env = harness_env(&manifest, &request(Some("c1"), "hi"), &dirs(&temp));
        assert!(env.is_empty());
    }

    #[test]
    fn first_message_gets_toolkit_but_no_session() {
        let temp = tempdir().unwrap();
        let manifest = manifest_with(Some(assistant_config(vec![
            ToolkitServer::Dotagent,
            ToolkitServer::Http {
                url: "http://127.0.0.1:7333/mcp".into(),
            },
        ])));
        let env = harness_env(&manifest, &request(Some("c1"), "hi"), &dirs(&temp));
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"AGENT_ASSISTANT_MCP_CONFIG"));
        assert!(keys.contains(&"AGENT_ASSISTANT_TOOLKIT_HASH"));
        assert!(
            !keys.contains(&"AGENT_ASSISTANT_SESSION"),
            "nothing recorded yet"
        );
    }

    #[test]
    fn recorded_session_is_reinjected_on_the_next_trigger() {
        let temp = tempdir().unwrap();
        let manifest = manifest_with(Some(assistant_config(vec![])));
        let dirs = dirs(&temp);
        let store = RegistryStore::new(&dirs.assistant_state);

        let mut record = RegistryRecord::new("telegram", "c1");
        record.apply_session_frame("s-9", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        store.save(&record, chrono::Utc::now()).unwrap();

        let env = harness_env(&manifest, &request(Some("c1"), "again"), &dirs);
        let session = env
            .iter()
            .find(|(k, _)| k == "AGENT_ASSISTANT_SESSION")
            .map(|(_, v)| v.clone());
        assert_eq!(session.as_deref(), Some("s-9"));
    }

    #[test]
    fn memory_recall_lands_in_env() {
        let temp = tempdir().unwrap();
        let dirs = dirs(&temp);
        let memory = dotagent_memory::MemoryStore::open_or_init(&dirs.memory_root).unwrap();
        memory.remember("likes rust", &[]).unwrap();

        let manifest = manifest_with(Some(assistant_config(vec![])));
        let env = harness_env(&manifest, &request(Some("c1"), "anything"), &dirs);
        let block = env
            .iter()
            .find(|(k, _)| k == "AGENT_ASSISTANT_MEMORY")
            .map(|(_, v)| v.clone());
        assert!(block.unwrap().contains("likes rust"));
    }

    #[test]
    fn finalize_strips_memos_and_reports_them() {
        let temp = tempdir().unwrap();
        let manifest = manifest_with(Some(assistant_config(vec![])));
        let outcome = finalize(
            &manifest,
            &request(Some("c1"), "x"),
            "the answer\nMEMO: fact | topics: a".into(),
            None,
            &dirs(&temp),
        );
        assert_eq!(outcome.reply, "the answer");
        assert_eq!(outcome.memos.len(), 1);
        assert_eq!(outcome.memos[0].text, "fact");
    }

    #[test]
    fn finalize_passes_through_when_harness_off() {
        let temp = tempdir().unwrap();
        let manifest = manifest_with(None);
        let outcome = finalize(
            &manifest,
            &request(Some("c1"), "x"),
            "MEMO: should stay visible".into(),
            None,
            &dirs(&temp),
        );
        assert_eq!(outcome.reply, "MEMO: should stay visible");
        assert!(outcome.memos.is_empty());
    }

    #[test]
    fn finalize_applies_session_frame_and_persists_pointer() {
        let temp = tempdir().unwrap();
        let dirs = dirs(&temp);
        let manifest = manifest_with(Some(assistant_config(vec![])));

        let outcome = finalize(
            &manifest,
            &request(Some("c1"), "x"),
            "ok".into(),
            Some(AssistantSessionFrame {
                claude_session: "s-1".into(),
                transcript_bytes: 500,
            }),
            &dirs,
        );
        assert_eq!(outcome.reply, "ok");

        let store = RegistryStore::new(&dirs.assistant_state);
        let record = store.load("telegram", "c1").unwrap().unwrap();
        assert_eq!(record.session_pointer(), Some("s-1"));

        // And the next trigger sees the pointer.
        let env = harness_env(&manifest, &request(Some("c1"), "again"), &dirs);
        assert!(env
            .iter()
            .any(|(k, v)| k == "AGENT_ASSISTANT_SESSION" && v == "s-1"));
    }

    #[test]
    fn finalize_retires_transcripts_past_the_ceiling() {
        let temp = tempdir().unwrap();
        let dirs = dirs(&temp);
        let manifest = manifest_with(Some(assistant_config(vec![])));

        finalize(
            &manifest,
            &request(Some("c1"), "x"),
            "ok".into(),
            Some(AssistantSessionFrame {
                claude_session: "s-big".into(),
                transcript_bytes: DEFAULT_TRANSCRIPT_BYTES_MAX + 1,
            }),
            &dirs,
        );

        let store = RegistryStore::new(&dirs.assistant_state);
        let record = store.load("telegram", "c1").unwrap().unwrap();
        assert_eq!(record.session_pointer(), None, "retired, not recorded");
        assert_eq!(record.generation, 1);
    }

    #[test]
    fn trigger_without_session_id_skips_registry_but_keeps_memory() {
        let temp = tempdir().unwrap();
        let dirs = dirs(&temp);
        let memory = dotagent_memory::MemoryStore::open_or_init(&dirs.memory_root).unwrap();
        memory.remember("fact", &[]).unwrap();

        let manifest = manifest_with(Some(assistant_config(vec![])));
        let env = harness_env(&manifest, &request(None, "hi"), &dirs);
        assert!(env.iter().any(|(k, _)| k == "AGENT_ASSISTANT_MEMORY"));
        assert!(
            !temp.path().join("assistant").exists() || {
                // No registry file may exist for a session-less trigger.
                std::fs::read_dir(temp.path().join("assistant"))
                    .map(|entries| entries.count() == 0)
                    .unwrap_or(true)
            }
        );
    }
}
