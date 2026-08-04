//! `dotagent mcp` — expose every discovered agent as an MCP tool.
//!
//! Speaks JSON-RPC 2.0 over stdio, one object per line, so any MCP client can
//! spawn it:
//!
//! ```json
//! { "mcpServers": { "dotagent": { "command": "dotagent", "args": ["mcp"] } } }
//! ```
//!
//! The point is that a model **selects** from the catalog instead of composing
//! a command. A tool name that does not exist is not callable, so "only run
//! what the operator declared" is a property of the protocol rather than a
//! check someone has to remember to write.
//!
//! ## Process model
//!
//! Runs the agent **in this process**, like `run-now` — not through the
//! daemon. Consequences, all shared with `run-now`:
//!
//! - The subprocess tree is invisible to `dotagent status`, which only reflects
//!   the daemon's supervisor.
//! - Nothing reaps it when the daemon stops.
//!
//! State is keyed off `TriggerSource::Mcp.slug()` rather than the schedule's
//! args, so an on-demand run can never overwrite `last_success_at` for a cron
//! window that never fired. That isolates MCP runs from the daemon, not from
//! each other: two clients running the same agent at once share a slug, and
//! the later finisher wins. Writes are still serialized by the state store's
//! lock, so the file never tears.
//!
//! Logging goes to stderr (wired in `main.rs`); stdout carries protocol only.

use anyhow::{Context, Result};
use dotagent_mcp::{
    error_code, tool_name_for, CallToolParams, CallToolResult, Capabilities, InitializeResult,
    JsonRpcRequest, JsonRpcResponse, RunArguments, ServerInfo, Tool, ToolsCapability,
    ToolsListResult, DEFAULT_PROTOCOL_VERSION,
};
use dotagent_plugin::PluginClient;
use dotagent_runner::{run_with_hooks, OrchestratedOutcome, RunContext, RunSpec};
use dotagent_state::{AuditLog, StateStore};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};

use crate::discovery::{self, DiscoveredAgent};

/// Serve MCP on stdio until the client closes the stream.
pub async fn run() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await.context("reading stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle_line(&line).await else {
            continue;
        };
        let encoded = serde_json::to_string(&response).context("serializing response")?;
        stdout.write_all(encoded.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }
    Ok(())
}

/// Parse and dispatch one line. `None` means "say nothing" — the correct
/// answer to a notification.
async fn handle_line(line: &str) -> Option<JsonRpcResponse> {
    let req: JsonRpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            // No id is recoverable from unparseable input, so the spec says
            // answer with a null id rather than stay silent.
            return Some(JsonRpcResponse::err(
                serde_json::Value::Null,
                error_code::PARSE_ERROR,
                format!("invalid JSON: {e}"),
            ));
        }
    };

    debug!(method = %req.method, "mcp request");
    if req.is_notification() {
        return None;
    }
    let id = req.id.clone().unwrap_or(serde_json::Value::Null);

    Some(match req.method.as_str() {
        "initialize" => JsonRpcResponse::ok(id, initialize(req.params.as_ref())),
        "ping" => JsonRpcResponse::ok(id, serde_json::json!({})),
        "tools/list" => match tools_list() {
            Ok(result) => JsonRpcResponse::ok(id, result),
            Err(e) => JsonRpcResponse::err(id, error_code::INTERNAL_ERROR, e.to_string()),
        },
        "tools/call" => tools_call(id, req.params).await,
        other => JsonRpcResponse::err(
            id,
            error_code::METHOD_NOT_FOUND,
            format!("unknown method '{other}'"),
        ),
    })
}

/// Handshake. Echoes the client's protocol version when it sent one — a client
/// that speaks an older revision gets an answer it understands instead of a
/// version mismatch.
fn initialize(params: Option<&serde_json::Value>) -> serde_json::Value {
    let protocol_version = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_PROTOCOL_VERSION)
        .to_string();

    serde_json::to_value(InitializeResult {
        protocol_version,
        capabilities: Capabilities {
            tools: ToolsCapability {
                list_changed: false,
            },
        },
        server_info: ServerInfo {
            name: "dotagent",
            version: env!("CARGO_PKG_VERSION"),
        },
    })
    .unwrap_or_else(|_| serde_json::json!({}))
}

/// Build the catalog from the manifests on disk.
///
/// A malformed manifest no longer hides the healthy agents, but it is still
/// reported: a model handed a catalog that quietly lost entries would answer
/// confidently and wrongly about what it can do. The broken ones come back as
/// an error so the operator sees them.
fn tools_list() -> Result<serde_json::Value> {
    let found = discovery::discover();
    if !found.invalid.is_empty() {
        let detail: Vec<String> = found
            .invalid
            .iter()
            .map(|i| format!("{}: {}", i.path.display(), i.error))
            .collect();
        return Err(anyhow::anyhow!(
            "{} manifest(s) failed to load — {}",
            detail.len(),
            detail.join("; ")
        ));
    }
    let tools = catalog(&found.agents)
        .into_iter()
        .map(|(name, agent)| Tool {
            name,
            description: describe(agent),
            input_schema: dotagent_mcp::run_input_schema(),
        })
        .collect();

    serde_json::to_value(ToolsListResult { tools }).context("serializing tool list")
}

/// Pair each agent with its tool name, dropping collisions.
///
/// Sanitization is lossy, so two agent names can map to one tool name. First
/// one wins; shadowing silently would make `tools/call` run an agent nobody
/// chose. Both `tools/list` and `tools/call` go through here so the catalog a
/// model sees is exactly the one that resolves.
fn catalog(agents: &[DiscoveredAgent]) -> Vec<(String, &DiscoveredAgent)> {
    let mut out: Vec<(String, &DiscoveredAgent)> = Vec::with_capacity(agents.len());
    for agent in agents {
        let name = tool_name_for(&agent.manifest.agent.name);
        if out.iter().any(|(taken, _)| taken == &name) {
            warn!(
                agent = %agent.manifest.agent.name,
                tool = %name,
                "tool name collides with an earlier agent — skipping"
            );
            continue;
        }
        out.push((name, agent));
    }
    out
}

/// Tool description shown to the model. Falls back to the schedule list when
/// the manifest has no `description`, because "runs on weekdays at 08:00" still
/// helps a model pick better than a bare name.
fn describe(agent: &DiscoveredAgent) -> String {
    if let Some(d) = &agent.manifest.agent.description {
        if !d.trim().is_empty() {
            return d.clone();
        }
    }
    let ids: Vec<&str> = agent.manifest.schedules.iter().map(|s| s.id()).collect();
    if ids.is_empty() {
        format!("Run the {} agent.", agent.manifest.agent.name)
    } else {
        format!(
            "Run the {} agent (schedules: {}).",
            agent.manifest.agent.name,
            ids.join(", ")
        )
    }
}

async fn tools_call(id: serde_json::Value, params: Option<serde_json::Value>) -> JsonRpcResponse {
    let params = match params {
        Some(p) => p,
        None => {
            return JsonRpcResponse::err(id, error_code::INVALID_PARAMS, "missing params");
        }
    };
    let call: CallToolParams = match serde_json::from_value(params) {
        Ok(c) => c,
        Err(e) => {
            return JsonRpcResponse::err(id, error_code::INVALID_PARAMS, e.to_string());
        }
    };
    let arguments: RunArguments = match call.arguments {
        Some(a) => match serde_json::from_value(a) {
            Ok(a) => a,
            Err(e) => {
                return JsonRpcResponse::err(id, error_code::INVALID_PARAMS, e.to_string());
            }
        },
        None => RunArguments::default(),
    };

    // Resolve the tool name back to an agent through the same catalog the
    // model saw. Never reconstruct the agent name from the tool name — the
    // mapping is lossy in that direction.
    let agent_name = match resolve_agent(&call.name) {
        Ok(Some(name)) => name,
        Ok(None) => {
            return JsonRpcResponse::err(
                id,
                error_code::INVALID_PARAMS,
                format!("unknown tool '{}'", call.name),
            );
        }
        Err(e) => {
            return JsonRpcResponse::err(id, error_code::INTERNAL_ERROR, e.to_string());
        }
    };

    let result = match execute(&agent_name, &arguments).await {
        Ok(r) => r,
        // An agent that could not even start is still tool output, not a
        // protocol fault: the model should read it and react.
        Err(e) => CallToolResult::text(format!("Could not run {agent_name}: {e}"), true),
    };
    match serde_json::to_value(result) {
        Ok(v) => JsonRpcResponse::ok(id, v),
        Err(e) => JsonRpcResponse::err(id, error_code::INTERNAL_ERROR, e.to_string()),
    }
}

/// Map a tool name back to an agent name through the same catalog the model
/// saw. Never reconstructed from the string — that direction is lossy.
fn resolve_agent(tool: &str) -> Result<Option<String>> {
    let agents = discovery::discover_all().context("discovering agents")?;
    Ok(catalog(&agents)
        .into_iter()
        .find(|(name, _)| name == tool)
        .map(|(_, agent)| agent.manifest.agent.name.clone()))
}

/// Run the agent and render the outcome as text for the model.
async fn execute(agent_name: &str, arguments: &RunArguments) -> Result<CallToolResult> {
    let agent = discovery::find_by_name(agent_name)?;

    let (schedule_id, mut args) = match &arguments.schedule {
        Some(id) => {
            let sched = discovery::schedule_by_id(&agent.manifest, id)?;
            (sched.id().to_string(), sched.args().to_vec())
        }
        None => match agent.manifest.schedules.first() {
            Some(sched) => (sched.id().to_string(), sched.args().to_vec()),
            // An agent that exists only to be asked for needs no schedule.
            None => (dotagent_core::TRIGGER_SCHEDULE_ID.to_string(), Vec::new()),
        },
    };
    args.extend(arguments.args.iter().cloned());

    let state = StateStore::from_home()?;
    let audit = AuditLog::from_home()?;
    let plugins = PluginClient::from_environment();
    let manifest_sha256 = dotagent_state::hash_manifest_file(&agent.dir.join("agent.toml")).ok();
    let source = dotagent_core::TriggerSource::Mcp;
    let mcp_slug = source.slug();
    let extra_env = vec![("AGENT_TRIGGER_SOURCE".to_string(), source.to_string())];

    let spec = RunSpec {
        manifest: &agent.manifest,
        manifest_dir: &agent.dir,
        schedule_id: &schedule_id,
        args: &args,
        dry_run: false,
        manifest_sha256,
        slug_override: Some(&mcp_slug),
        extra_env: &extra_env,
    };
    let ctx = RunContext {
        state: &state,
        plugins: Some(&plugins),
        audit: Some(&audit),
        supervisor: Some(plugins.supervisor()),
    };

    Ok(match run_with_hooks(spec, &ctx).await? {
        OrchestratedOutcome::Ran(outcome) if outcome.exit_code == 0 => {
            let body = if outcome.stdout_tail.trim().is_empty() {
                format!("{agent_name} finished with no output.")
            } else {
                outcome.stdout_tail
            };
            CallToolResult::text(body, false)
        }
        OrchestratedOutcome::Ran(outcome) => {
            let what = if outcome.timed_out {
                format!("timed out after {}s", outcome.duration_seconds)
            } else {
                format!("exited {}", outcome.exit_code)
            };
            CallToolResult::text(
                format!("{agent_name} {what}.\n{}", outcome.stderr_tail),
                true,
            )
        }
        OrchestratedOutcome::PreflightFailed { plugin, suggest } => {
            let hint = suggest.map(|s| format!(": {s}")).unwrap_or_default();
            CallToolResult::text(
                format!("{agent_name} was blocked by preflight check {plugin}{hint}"),
                true,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_method_yields_method_not_found() {
        let res = handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#)
            .await
            .expect("request must be answered");
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains("-32601"), "{s}");
    }

    #[tokio::test]
    async fn malformed_json_yields_parse_error_with_null_id() {
        let res = handle_line("{not json").await.expect("must answer");
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains("-32700"), "{s}");
        assert!(s.contains(r#""id":null"#), "{s}");
    }

    #[tokio::test]
    async fn notification_is_not_answered() {
        let res = handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn initialize_echoes_client_protocol_version() {
        let res = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        )
        .await
        .expect("must answer");
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains("2025-06-18"), "{s}");
        assert!(s.contains(r#""name":"dotagent""#), "{s}");
    }

    #[tokio::test]
    async fn initialize_falls_back_to_default_version() {
        let res = handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
            .await
            .expect("must answer");
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains(DEFAULT_PROTOCOL_VERSION), "{s}");
    }

    #[tokio::test]
    async fn tools_call_without_params_is_invalid_params() {
        let res = handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call"}"#)
            .await
            .expect("must answer");
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains("-32602"), "{s}");
    }

    #[tokio::test]
    async fn tools_call_with_unknown_tool_is_invalid_params() {
        let res = handle_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"run-nope-nope"}}"#,
        )
        .await
        .expect("must answer");
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains("-32602"), "{s}");
        assert!(s.contains("unknown tool"), "{s}");
    }

    #[test]
    fn description_falls_back_to_schedule_ids() {
        let toml = r#"
[agent]
name = "x"
[run]
command = "true"
[[schedules]]
id = "daily"
type = "cron"
weekdays = [1]
hours = [8]
"#;
        let manifest: dotagent_core::AgentManifest = toml::from_str(toml).unwrap();
        let agent = DiscoveredAgent {
            manifest,
            dir: std::path::PathBuf::from("/tmp"),
        };
        assert_eq!(describe(&agent), "Run the x agent (schedules: daily).");
    }

    #[test]
    fn description_prefers_manifest_description() {
        let toml = r#"
[agent]
name = "x"
description = "Does the thing."
[run]
command = "true"
"#;
        let manifest: dotagent_core::AgentManifest = toml::from_str(toml).unwrap();
        let agent = DiscoveredAgent {
            manifest,
            dir: std::path::PathBuf::from("/tmp"),
        };
        assert_eq!(describe(&agent), "Does the thing.");
    }
}
