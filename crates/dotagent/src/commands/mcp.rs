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
use dotagent_runner::{OrchestratedOutcome, RunSpec};
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
    let mut tools: Vec<Tool> = catalog(&found.agents)
        .into_iter()
        .map(|(name, agent)| Tool {
            name,
            description: describe(agent),
            input_schema: dotagent_mcp::run_input_schema(),
        })
        .collect();
    tools.extend(introspection_tools());
    tools.extend(remediation_tools(&found.agents));
    tools.extend(memory_tools());
    tools.extend(skill_tools());
    tools.extend(command_tools());

    serde_json::to_value(ToolsListResult { tools }).context("serializing tool list")
}

/// Two tools for the whole command catalog — **not** one tool per command.
///
/// This is the one place commands deliberately break the pattern skills follow.
/// A `skill-<name>` per skill is right because the model is meant to *choose*;
/// the catalog is the menu it picks from. A command was already chosen, by a
/// human, before the model saw anything. Publishing N command tools would
/// re-open that decision — the model could pick `command-weekly-numbers` when
/// the sender typed `/simplify`, which is precisely the failure commands exist
/// to make impossible.
///
/// So the dispatcher resolves a name it was handed, and the model never browses.
fn command_tools() -> Vec<Tool> {
    if !commands_enabled() {
        return Vec::new();
    }
    let found = crate::slash::discover();
    for bad in &found.invalid {
        warn!(path = %bad.path.display(), error = %bad.error, "skipping unloadable command");
    }
    if found.commands.is_empty() {
        return Vec::new();
    }
    vec![
        Tool {
            name: "command-get".into(),
            description: "Resolve a command the sender invoked into the prompt to follow. Pass \
                command.name and command.args from the trigger payload exactly as they arrived. \
                Returns the prompt — it does not perform it."
                .into(),
            input_schema: dotagent_mcp::command_get_input_schema(),
        },
        Tool {
            name: "command-list".into(),
            description: "List every installed command with what it does and what it takes. For \
                answering \"what can you do?\" — not for choosing a command on the sender's \
                behalf."
                .into(),
            input_schema: dotagent_mcp::command_list_input_schema(),
        },
    ]
}

fn commands_enabled() -> bool {
    dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .commands
        .enabled
}

/// One tool per installed skill, plus the two verbs that reach inside one.
///
/// A broken `SKILL.md` does **not** fail the catalog the way a broken manifest
/// does. The blast radius is different: a manifest that fails to parse means an
/// agent silently never runs, while a skill that fails to parse means a
/// procedure is missing from an answer. `doctor` reports them; a model that
/// still has its agents is more useful than an error.
fn skill_tools() -> Vec<Tool> {
    if !skills_enabled() {
        return Vec::new();
    }
    let found = crate::skills::discover();
    for bad in &found.invalid {
        warn!(path = %bad.path.display(), error = %bad.error, "skipping unloadable skill");
    }
    if found.skills.is_empty() {
        return Vec::new();
    }

    let mut tools: Vec<Tool> = skill_catalog(&found.skills)
        .into_iter()
        .map(|(name, skill)| Tool {
            name,
            // The description leads, because it is what the model matches on.
            // The sentence after it exists because half these descriptions are
            // written in the imperative ("Cut a new release") and a model could
            // reasonably read the call itself as doing the thing.
            description: format!(
                "{}\n\nCalling this returns the written procedure — it does not perform it.",
                skill.manifest.description.trim()
            ),
            input_schema: dotagent_mcp::skill_input_schema(),
        })
        .collect();

    tools.push(Tool {
        name: "skill-read".into(),
        description: "Read a supporting file belonging to a skill — the reference documents a \
            procedure points at. Use the paths the skill listed; nothing outside its directory \
            is reachable."
            .into(),
        input_schema: dotagent_mcp::skill_read_input_schema(),
    });
    tools.push(Tool {
        name: "skill-run".into(),
        description: "Run an executable packaged inside a skill (its scripts/ directory) and \
            return the output. Only for scripts a skill listed — this is not a shell."
            .into(),
        input_schema: dotagent_mcp::skill_run_input_schema(),
    });
    tools
}

/// Pair each skill with its tool name, dropping collisions. Same rule as
/// [`catalog`]: sanitization is lossy, first discovered wins.
fn skill_catalog(
    skills: &[crate::skills::DiscoveredSkill],
) -> Vec<(String, &crate::skills::DiscoveredSkill)> {
    let mut out: Vec<(String, &crate::skills::DiscoveredSkill)> = Vec::with_capacity(skills.len());
    for skill in skills {
        let name = dotagent_mcp::skill_tool_name_for(&skill.manifest.name);
        if out.iter().any(|(taken, _)| taken == &name) {
            warn!(
                skill = %skill.manifest.name,
                tool = %name,
                "tool name collides with an earlier skill — skipping"
            );
            continue;
        }
        out.push((name, skill));
    }
    out
}

fn skills_enabled() -> bool {
    dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .skills
        .enabled
}

/// Introspection tools. Always available — reading a log has nothing to do
/// with whether memory is on.
fn introspection_tools() -> Vec<Tool> {
    let no_args =
        || serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false});
    vec![
        Tool {
            name: "dotagent-status".into(),
            description:
                "Health of every scheduled agent: ok / degraded / failing / stale, with the \
            last run and why. Answers \"is everything running?\" without guessing."
                    .into(),
            input_schema: no_args(),
        },
        Tool {
            name: "dotagent-inspect".into(),
            description:
                "Heartbeat, window state and manifest hash for one agent — when it last ran, \
            whether it succeeded, how many attempts the current window has used."
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "agent": { "type": "string" } },
                "required": ["agent"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "dotagent-doctor".into(),
            description: "Validate every manifest, resolve plugin references, and report secrets, \
            inbound Telegram and memory status. Use for \"is anything misconfigured?\"."
                .into(),
            input_schema: no_args(),
        },
        Tool {
            name: "dotagent-next-runs".into(),
            description:
                "What the scheduler would dispatch right now, and when the next event is. \
            A preview — nothing is executed."
                    .into(),
            input_schema: no_args(),
        },
        Tool {
            name: "dotagent-logs".into(),
            description: "Read the captured output of an agent's recent runs. Use this to answer \
                \"did X run?\", \"why did X fail?\", or to quote what a run actually printed \
                instead of guessing."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Agent name." },
                    "lines": { "type": "integer", "description": "Trailing lines. Default 40, max 400." }
                },
                "required": ["agent"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Memory tools, when `[memory]` is enabled.
///
/// Separate from the agent catalog on purpose: agents are things the operator
/// installed, memory is a capability of the server itself.
fn memory_tools() -> Vec<Tool> {
    if !memory_config().enabled {
        return Vec::new();
    }
    vec![
        Tool {
            name: "memory-remember".into(),
            description: "Store a fact worth keeping across conversations — a preference, a \
                decision, something about the user. Give it topics: each becomes a page that \
                gathers every fact about that subject. Not for chit-chat or for what is \
                already in this conversation."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The fact, in one sentence." },
                    "topics": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Subjects this fact belongs to (people, projects, themes). \
            Each becomes a linked page that gathers every fact mentioning it. Use existing \
            topics when they fit — a new name for the same subject splits the graph."
                    }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "memory-topics".into(),
            description: "List the subjects memory knows about. Check this before inventing a \
                topic name, so related facts land on the same page instead of splitting across \
                near-duplicates."
                .into(),
            input_schema: serde_json::json!({
                "type": "object", "properties": {}, "additionalProperties": false
            }),
        },
        Tool {
            name: "memory-recall".into(),
            description: "Search stored facts. Ranks by shared words first and recency second, \
                best match first. An empty query returns the most recent facts. Each result \
                starts with an id you can pass to memory-forget or memory-supersede."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for." },
                    "topic": {
                        "type": "string",
                        "description": "Instead of a text search, return every fact linked to \
            this subject, gathered from all days. The better question when you know the \
            subject: it asks the graph instead of guessing which words the fact used."
                    },
                    "limit": { "type": "integer", "description": "Default 10." }
                },
                "additionalProperties": false
            }),
        },
        Tool {
            name: "memory-supersede".into(),
            description: "Replace a stored fact with a corrected one. Use this when something \
                you remembered stopped being true — a preference that changed, a decision that \
                was reversed. The old fact stays readable in its journal but stops coming back \
                from recall, so you are never choosing between two answers. Get the id from \
                memory-recall."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Id of the fact being replaced." },
                    "text": { "type": "string", "description": "The corrected fact, in one sentence." },
                    "topics": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Subjects the new fact belongs to."
                    }
                },
                "required": ["id", "text"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "memory-forget".into(),
            description: "Delete a stored fact for good. For things that should never have been \
                stored — noise, something private, something you got wrong. A fact that merely \
                stopped being true wants memory-supersede instead, which keeps the history. \
                Get the id from memory-recall."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Id of the fact to delete." }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Render recall results so each fact is addressable.
///
/// The id leads because the follow-up move — correcting a fact, deleting one
/// — needs it, and a model that has to ask "which one?" spends a round trip
/// on something the first answer could have carried.
fn render_memories(hits: &[dotagent_memory::Memory]) -> String {
    hits.iter()
        .map(|m| {
            let mut line = format!("[{}] ", m.id);
            if !m.date.is_empty() {
                line.push_str(&m.date);
                line.push_str(": ");
            }
            line.push_str(&m.text);
            if !m.topics.is_empty() {
                line.push_str(&format!(" (topics: {})", m.topics.join(", ")));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A string array argument, or empty when absent or the wrong shape.
fn string_array(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn memory_config() -> dotagent_core::config::MemoryConfig {
    dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .memory
}

/// Run one of dotagent's own read-only subcommands and return its output.
///
/// Re-executes this binary rather than reimplementing each command. The
/// rendering lives in the CLI, and duplicating it here would mean two versions
/// of the truth drifting apart — an assistant quoting a status that no longer
/// matches what `dotagent status` prints is worse than a subprocess.
///
/// Only ever called with a fixed set of read-only subcommands; the one variable
/// argument is an agent name already resolved through discovery.
fn run_readonly(argv: &[&str]) -> CallToolResult {
    let Ok(exe) = std::env::current_exe() else {
        return CallToolResult::text("Could not locate the dotagent binary.", true);
    };
    match std::process::Command::new(exe).args(argv).output() {
        Ok(out) => {
            let body = String::from_utf8_lossy(&out.stdout);
            let body = body.trim();
            if body.is_empty() {
                let err = String::from_utf8_lossy(&out.stderr);
                return CallToolResult::text(
                    format!("No output. {}", err.trim()),
                    !out.status.success(),
                );
            }
            // A non-zero exit still carries useful output (doctor reports
            // errors and exits non-zero), so the body goes back either way.
            CallToolResult::text(body.to_string(), false)
        }
        Err(e) => CallToolResult::text(format!("Could not run dotagent {}: {e}", argv[0]), true),
    }
}

/// Tail an agent's captured output by delegating to `dotagent logs`.
///
/// The name is resolved through discovery first, deliberately: the log path is
/// derived from it, and `../` in a raw name would reach outside the log tree.
fn read_agent_logs(args: &serde_json::Value) -> CallToolResult {
    let agent = args
        .get("agent")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    if agent.is_empty() {
        return CallToolResult::text("Which agent?", true);
    }
    if discovery::find_by_name(agent).is_err() {
        return CallToolResult::text(format!("No agent named {agent}."), true);
    }
    let lines = args
        .get("lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(40)
        .clamp(1, 400)
        .to_string();
    run_readonly(&["logs", agent, "-n", &lines])
}

/// Resolve the workspace and scaffold it if needed. Shared with
/// `dotagent memory` so the CLI and the MCP server can never disagree about
/// which workspace they are talking to.
fn memory_store() -> Result<dotagent_memory::MemoryStore> {
    crate::commands::memory::store()
}

/// Run a memory tool. `None` means "not a memory tool".
fn call_memory(tool: &str, args: &serde_json::Value) -> Option<CallToolResult> {
    let store = match tool {
        // Introspection needs no memory workspace.
        "dotagent-logs" => return Some(read_agent_logs(args)),
        "dotagent-status" => return Some(run_readonly(&["status"])),
        "dotagent-doctor" => return Some(run_readonly(&["doctor"])),
        "dotagent-next-runs" => return Some(run_readonly(&["tick", "--dry-run"])),
        "dotagent-inspect" => {
            let agent = args
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if agent.is_empty() {
                return Some(CallToolResult::text("Which agent?", true));
            }
            if discovery::find_by_name(&agent).is_err() {
                return Some(CallToolResult::text(
                    format!("No agent named {agent}."),
                    true,
                ));
            }
            return Some(run_readonly(&["inspect", &agent]));
        }
        "memory-remember" | "memory-recall" | "memory-topics" | "memory-supersede"
        | "memory-forget" => match memory_store() {
            Ok(s) => s,
            Err(e) => {
                return Some(CallToolResult::text(
                    format!("Memory unavailable: {e}"),
                    true,
                ))
            }
        },
        _ => return None,
    };

    Some(match tool {
        "memory-topics" => match store.topics() {
            Ok(t) if t.is_empty() => CallToolResult::text("No topics yet.", false),
            Ok(t) => CallToolResult::text(t.join("\n"), false),
            Err(e) => CallToolResult::text(format!("Could not list topics: {e}"), true),
        },
        "memory-remember" => {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let topics = string_array(args, "topics");
            match store.remember(text, &topics) {
                Ok(m) => {
                    CallToolResult::text(format!("Remembered ({}): {}", m.date, m.text), false)
                }
                Err(e) => CallToolResult::text(format!("Could not remember: {e}"), true),
            }
        }
        "memory-recall" => {
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .clamp(1, 50) as usize;
            // `topic` asks the graph; `query` guesses at words. When both
            // are given the graph wins — it is the more precise question.
            let topic = args
                .get("topic")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty());
            let hits = match topic {
                Some(topic) => store.recall_topic(topic).map(|mut hits| {
                    hits.truncate(limit);
                    hits
                }),
                None => store.recall(
                    args.get("query")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default(),
                    limit,
                ),
            };
            match hits {
                Ok(hits) if hits.is_empty() => {
                    CallToolResult::text("Nothing remembered about that.", false)
                }
                Ok(hits) => CallToolResult::text(render_memories(&hits), false),
                Err(e) => CallToolResult::text(format!("Could not recall: {e}"), true),
            }
        }
        "memory-supersede" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let topics = string_array(args, "topics");
            match store.supersede(id, text, &topics, &dotagent_memory::Provenance::default()) {
                Ok(m) => CallToolResult::text(format!("Replaced. Now: {}", m.text), false),
                Err(e) => CallToolResult::text(format!("Could not supersede: {e}"), true),
            }
        }
        "memory-forget" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            match store.forget(id) {
                Ok(true) => CallToolResult::text("Forgotten.", false),
                Ok(false) => CallToolResult::text(format!("No memory with id {id}."), true),
                Err(e) => CallToolResult::text(format!("Could not forget: {e}"), true),
            }
        }
        _ => unreachable!("guarded above"),
    })
}

// -------------------------------------------------------------------------
// Remediation
// -------------------------------------------------------------------------

/// One tool per `[[preflight]] remediation` an operator declared.
///
/// A preflight abort names what is wrong and often what would fix it, and
/// until now the fix was something you got up and typed. The gap is not
/// knowing the command — the plugin already returns it in `suggest`. The gap
/// is that running a string a plugin wrote, triggered from a chat, is
/// arbitrary execution over an inbound path.
///
/// Declaring it in the manifest closes that: the catalog holds commands a
/// human committed to a file, and a model picks from the catalog. Same
/// property as `run-*`, for the same reason — a name that is not there is not
/// callable.
fn remediation_tools(agents: &[DiscoveredAgent]) -> Vec<Tool> {
    let mut tools = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for agent in agents {
        let agent_name = &agent.manifest.agent.name;
        for pr in &agent.manifest.preflight {
            let Some(command) = pr
                .remediation
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            else {
                continue;
            };
            let name = dotagent_mcp::remediation_tool_name_for(agent_name, &pr.plugin);
            if !seen.insert(name.clone()) {
                warn!(
                    agent = %agent_name,
                    plugin = %pr.plugin,
                    tool = %name,
                    "remediation tool name collides with an earlier one — skipping"
                );
                continue;
            }
            tools.push(Tool {
                // The command is in the description on purpose: an assistant
                // about to run something on the operator's machine should be
                // able to say what it is before doing it.
                description: format!(
                    "Fix what makes {plugin} block {agent_name}, by running: {command}\n\n\
                     Declared in {agent_name}'s manifest. Ask before calling this — it \
                     changes the machine, unlike reading a log.",
                    plugin = pr.plugin,
                ),
                name,
                input_schema: dotagent_mcp::remediation_input_schema(),
            });
        }
    }
    tools
}

/// Resolve a tool name back to the declared command, through the same catalog
/// the model saw. Returns `(agent, plugin, command)`.
fn resolve_remediation(tool: &str) -> Option<(String, String, String)> {
    for agent in discovery::discover().agents {
        let agent_name = agent.manifest.agent.name.clone();
        for pr in &agent.manifest.preflight {
            let Some(command) = pr
                .remediation
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            else {
                continue;
            };
            if dotagent_mcp::remediation_tool_name_for(&agent_name, &pr.plugin) == tool {
                return Some((agent_name, pr.plugin.clone(), command.to_string()));
            }
        }
    }
    None
}

/// Deadline for a remediation. Long enough for a VPN handshake, short enough
/// that a command waiting on a prompt nobody will answer does not sit forever.
const REMEDIATION_TIMEOUT_SECONDS: u64 = 120;

/// Run a declared remediation under the supervisor. `None` means "not one".
async fn call_remediation(tool: &str) -> Option<CallToolResult> {
    if !tool.starts_with("remediate-") {
        return None;
    }
    let Some((agent, plugin, command)) = resolve_remediation(tool) else {
        return Some(CallToolResult::text(
            format!("No declared remediation named {tool}."),
            true,
        ));
    };

    // argv, never a shell: `remediation = "x && curl ... | sh"` runs a program
    // called `x` with those literal arguments, and fails, rather than becoming
    // three commands.
    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else {
        return Some(CallToolResult::text(
            format!("{agent}: remediation for {plugin} is empty."),
            true,
        ));
    };
    let args: Vec<&str> = parts.collect();

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let plugins = PluginClient::from_environment();
    let spec = dotagent_supervisor::SpawnSpec {
        kind: dotagent_supervisor::ProcessKind::Skill,
        owner: dotagent_supervisor::ProcessOwner {
            agent: agent.clone(),
            plugin: Some(plugin.clone()),
            ..Default::default()
        },
        deadline: std::time::Duration::from_secs(REMEDIATION_TIMEOUT_SECONDS),
        label: format!("remediate:{plugin}"),
    };

    let handle = match plugins.supervisor().spawn_supervised(cmd, spec).await {
        Ok(h) => h,
        Err(e) => {
            return Some(CallToolResult::text(
                format!("Could not start `{command}`: {e}"),
                true,
            ))
        }
    };

    let (body, is_error, exit_code, timed_out) = match handle.wait_with_output().await {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if code == 0 {
                let tail = if stdout.is_empty() {
                    String::new()
                } else {
                    format!("\n{stdout}")
                };
                (
                    format!("Ran `{command}`. It exited 0.{tail}\n\nThis does not re-run {agent} — ask for that separately."),
                    false,
                    code,
                    false,
                )
            } else {
                (
                    format!(
                        "`{command}` exited {code}.\n{}",
                        cap(&stderr, SKILL_OUTPUT_MAX_BYTES)
                    ),
                    true,
                    code,
                    false,
                )
            }
        }
        Err(dotagent_supervisor::SupervisorError::TimedOut { elapsed, .. }) => (
            format!(
                "`{command}` was still running after {}s and was killed.",
                elapsed.as_secs()
            ),
            true,
            -1,
            true,
        ),
        Err(e) => (format!("`{command}` could not run: {e}"), true, -1, false),
    };

    // Audited because this changes the machine on behalf of whoever was in a
    // chat window. `dotagent status` shows it live; this is the record after.
    if let Ok(audit) = AuditLog::from_home() {
        let _ = audit.append(dotagent_core::AuditEvent::RemediationInvoked {
            agent,
            plugin,
            command,
            exit_code,
            timed_out,
        });
    }
    Some(CallToolResult::text(body, is_error))
}

// -------------------------------------------------------------------------
// Skills
// -------------------------------------------------------------------------

/// Cap on a procedure returned by `skill-<name>`.
///
/// A tool result is context. A skill long enough to evict what the assistant
/// needs in order to *act* on it has defeated its own purpose, so the tail is
/// dropped with a marker rather than silently — following half a procedure
/// while believing it is whole is the failure worth preventing.
const SKILL_BODY_MAX_BYTES: usize = 32 * 1024;

/// Cap on a supporting file returned by `skill-read`. Larger than a procedure:
/// reference material is asked for deliberately, one file at a time.
const SKILL_FILE_MAX_BYTES: usize = 64 * 1024;

/// Cap on what a script prints back.
const SKILL_OUTPUT_MAX_BYTES: usize = 16 * 1024;

/// Truncate on a char boundary, saying so.
fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[truncated: {} of {} bytes shown]",
        &text[..end],
        end,
        text.len()
    )
}

/// Serve a skill tool. `None` means "not a skill tool".
async fn call_skill(tool: &str, args: &serde_json::Value) -> Option<CallToolResult> {
    if !tool.starts_with("skill-") || !skills_enabled() {
        return None;
    }
    match tool {
        "skill-read" => Some(read_skill_file(args)),
        "skill-run" => Some(run_skill_script(args).await),
        _ => resolve_skill(tool).map(|skill| load_skill(&skill)),
    }
}

/// Map a tool name back to a skill through the same catalog the model saw.
/// Never reconstructed from the string — that direction is lossy.
fn resolve_skill(tool: &str) -> Option<crate::skills::DiscoveredSkill> {
    let skills = crate::skills::discover().skills;
    skill_catalog(&skills)
        .into_iter()
        .find(|(name, _)| name == tool)
        .map(|(_, skill)| skill.clone())
}

/// Return the procedure, followed by an index of what sits next to it.
///
/// The index is not decoration. Skills written for Claude Code routinely say
/// "see references/x.md", relying on a filesystem tool the caller may not have.
/// Listing the files — and naming the tool that fetches them — is what keeps
/// that instruction followable instead of a dead end.
fn load_skill(skill: &crate::skills::DiscoveredSkill) -> CallToolResult {
    let mut body = cap(&skill.manifest.body, SKILL_BODY_MAX_BYTES);
    let files = skill.files();
    if !files.is_empty() {
        body.push_str(
            "\n\n---\nFiles in this skill (fetch with skill-read, \
            passing skill=\"",
        );
        body.push_str(&skill.manifest.name);
        body.push_str("\"):\n");
        for f in files {
            body.push_str("  ");
            body.push_str(&f);
            // Labeled by what would actually run, not by the directory name.
            if skill.resolve_script(&f).is_ok() {
                body.push_str("  (executable — run with skill-run)");
            }
            body.push('\n');
        }
    }
    CallToolResult::text(body, false)
}

/// A trimmed string argument. Shared by the skill and command tools —
/// every one of them takes strings and nothing else.
fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> &'a str {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
}

fn read_skill_file(args: &serde_json::Value) -> CallToolResult {
    let name = str_arg(args, "skill");
    let rel = str_arg(args, "path");
    if name.is_empty() || rel.is_empty() {
        return CallToolResult::text("Both skill and path are required.", true);
    }
    let skill = match crate::skills::find_by_name(name) {
        Ok(s) => s,
        Err(e) => return CallToolResult::text(e.to_string(), true),
    };
    let path = match skill.resolve_readable(rel) {
        Ok(p) => p,
        Err(e) => return CallToolResult::text(e.to_string(), true),
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => CallToolResult::text(cap(&text, SKILL_FILE_MAX_BYTES), false),
        // Binary assets are a legitimate thing to package and a useless thing
        // to return as text; say which it was rather than emit replacement
        // characters the model would try to reason about.
        Err(e) => CallToolResult::text(format!("Could not read {rel}: {e}"), true),
    }
}

/// Run a `scripts/` executable under the supervisor.
///
/// Through `dotagent-supervisor` for the same reason every other orchestrated
/// subprocess is: a deadline that is actually enforced, and kill-tree so a
/// script that spawns children cannot leave orphans behind when it is killed.
async fn run_skill_script(args: &serde_json::Value) -> CallToolResult {
    let name = str_arg(args, "skill");
    let rel = str_arg(args, "script");
    if name.is_empty() || rel.is_empty() {
        return CallToolResult::text("Both skill and script are required.", true);
    }
    let extra: Vec<String> = args
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let skill = match crate::skills::find_by_name(name) {
        Ok(s) => s,
        Err(e) => return CallToolResult::text(e.to_string(), true),
    };
    let script = match skill.resolve_script(rel) {
        Ok(p) => p,
        Err(e) => return CallToolResult::text(e.to_string(), true),
    };

    // Arguments go through argv, never a shell — the same posture as
    // `[run] args` in a manifest.
    let mut cmd = tokio::process::Command::new(&script);
    cmd.args(&extra)
        .current_dir(&skill.dir)
        .env("SKILL_NAME", &skill.manifest.name)
        .env("SKILL_DIR", &skill.dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let plugins = PluginClient::from_environment();
    let spec = dotagent_supervisor::SpawnSpec {
        kind: dotagent_supervisor::ProcessKind::Skill,
        owner: dotagent_supervisor::ProcessOwner {
            agent: skill.manifest.name.clone(),
            ..Default::default()
        },
        deadline: std::time::Duration::from_secs(skill.manifest.timeout_seconds),
        label: format!("skill-run:{rel}"),
    };

    let handle = match plugins.supervisor().spawn_supervised(cmd, spec).await {
        Ok(h) => h,
        Err(e) => return CallToolResult::text(format!("Could not start {rel}: {e}"), true),
    };

    let (body, is_error, exit_code, timed_out) = match handle.wait_with_output().await {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if code == 0 {
                let text = if stdout.is_empty() {
                    format!("{rel} finished with no output.")
                } else {
                    cap(&stdout, SKILL_OUTPUT_MAX_BYTES)
                };
                (text, false, code, false)
            } else {
                // stderr first: on failure it is the part that explains.
                let text = format!(
                    "{rel} exited {code}.\n{}",
                    cap(&stderr, SKILL_OUTPUT_MAX_BYTES)
                );
                (text, true, code, false)
            }
        }
        Err(dotagent_supervisor::SupervisorError::TimedOut { elapsed, .. }) => (
            format!(
                "{rel} timed out after {}s and was killed.",
                elapsed.as_secs()
            ),
            true,
            -1,
            true,
        ),
        Err(e) => (format!("{rel} could not run: {e}"), true, -1, false),
    };

    // Audited because this is code executing outside any manifest — without
    // the entry, "what ran on this machine" has a hole in it.
    if let Ok(audit) = AuditLog::from_home() {
        let _ = audit.append(dotagent_core::AuditEvent::SkillInvoked {
            skill: skill.manifest.name.clone(),
            script: rel.to_string(),
            exit_code,
            timed_out,
        });
    }
    CallToolResult::text(body, is_error)
}

// -------------------------------------------------------------------------
// Commands
// -------------------------------------------------------------------------

/// Cap on a rendered command body. Same reasoning as `SKILL_BODY_MAX_BYTES`.
const COMMAND_BODY_MAX_BYTES: usize = 32 * 1024;

/// Serve a command tool. `None` means "not a command tool".
fn call_command(tool: &str, args: &serde_json::Value) -> Option<CallToolResult> {
    if !commands_enabled() {
        return None;
    }
    match tool {
        "command-get" => Some(get_command(args)),
        "command-list" => Some(list_commands()),
        _ => None,
    }
}

/// Resolve a name to its rendered prompt.
fn get_command(args: &serde_json::Value) -> CallToolResult {
    let name = str_arg(args, "name");
    if name.is_empty() {
        return CallToolResult::text("Which command? Pass command.name from the payload.", true);
    }
    let found = crate::slash::discover();
    let Some(cmd) = found.resolve(name) else {
        // Naming the alternatives beats a bare "not found": the sender
        // mistyped, and the dispatcher can say which one they meant.
        let tail = match found.installed().as_str() {
            "" => String::new(),
            installed => format!(" Installed: {installed}."),
        };
        return CallToolResult::text(format!("No command named '{name}'.{tail}"), true);
    };

    let rendered = cap(
        &cmd.manifest.render(str_arg(args, "args")),
        COMMAND_BODY_MAX_BYTES,
    );
    // The hint is carried but flagged. dotagent does not control the
    // dispatcher's harness, so presenting it as a constraint would describe a
    // guarantee that does not exist.
    let body = match &cmd.manifest.allowed_tools {
        Some(tools) => format!(
            "{rendered}\n\n---\nThe command suggests these tools: {tools}\n\
             (A hint from its author, not a restriction dotagent enforces.)"
        ),
        None => rendered,
    };
    CallToolResult::text(body, false)
}

fn list_commands() -> CallToolResult {
    let found = crate::slash::discover();
    let menu = found.telegram_menu();
    if menu.is_empty() {
        return CallToolResult::text("No commands installed.", false);
    }
    let lines: Vec<String> = menu
        .into_iter()
        .map(|(tg, cmd)| cmd.menu_line(&tg))
        .collect();
    CallToolResult::text(lines.join("\n"), false)
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
    // Raw JSON, because every non-agent tool has its own argument shape.
    // `RunArguments` is parsed further down, once the tool is known to be an
    // agent: doing it here would reject `{"args": "src/"}` for `command-get`,
    // whose `args` is a string, before the handler that understands it ever
    // ran — a tool failing on the schema of a *different* tool.
    let raw_args = call.arguments.clone().unwrap_or(serde_json::Value::Null);

    // A declared remediation runs a command, not an agent.
    if let Some(result) = call_remediation(&call.name).await {
        return match serde_json::to_value(result) {
            Ok(v) => JsonRpcResponse::ok(id, v),
            Err(e) => JsonRpcResponse::err(id, error_code::INTERNAL_ERROR, e.to_string()),
        };
    }

    // Skills are text and (occasionally) a script — never an agent run.
    if let Some(result) = call_skill(&call.name, &raw_args).await {
        return match serde_json::to_value(result) {
            Ok(v) => JsonRpcResponse::ok(id, v),
            Err(e) => JsonRpcResponse::err(id, error_code::INTERNAL_ERROR, e.to_string()),
        };
    }

    // Commands are text a human already picked — resolved, never run.
    if let Some(result) = call_command(&call.name, &raw_args) {
        return match serde_json::to_value(result) {
            Ok(v) => JsonRpcResponse::ok(id, v),
            Err(e) => JsonRpcResponse::err(id, error_code::INTERNAL_ERROR, e.to_string()),
        };
    }

    // Memory tools are served by this process, not by spawning an agent.
    if let Some(result) = call_memory(&call.name, &raw_args) {
        return match serde_json::to_value(result) {
            Ok(v) => JsonRpcResponse::ok(id, v),
            Err(e) => JsonRpcResponse::err(id, error_code::INTERNAL_ERROR, e.to_string()),
        };
    }

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

    // Now that the tool is known to be an agent, its schema applies.
    let arguments: RunArguments = match call.arguments {
        Some(a) => match serde_json::from_value(a) {
            Ok(a) => a,
            Err(e) => {
                return JsonRpcResponse::err(id, error_code::INVALID_PARAMS, e.to_string());
            }
        },
        None => RunArguments::default(),
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
    let outcome = crate::commands::run_scoped(
        spec,
        &state,
        plugins.supervisor(),
        Some(&plugins),
        Some(&audit),
    )
    .await?;

    Ok(match outcome {
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

    // --- remediation ---

    fn agent_with(toml_src: &str) -> DiscoveredAgent {
        DiscoveredAgent {
            manifest: toml::from_str(toml_src).unwrap(),
            dir: std::path::PathBuf::from("/tmp"),
        }
    }

    const GATED: &str = r#"
[agent]
name = "needs-vpn"
[run]
command = "true"
[[preflight]]
plugin = "preflight-warp"
remediation = "warp-cli connect"
"#;

    #[test]
    fn a_declared_remediation_becomes_a_tool_naming_the_command() {
        let tools = remediation_tools(&[agent_with(GATED)]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "remediate-needs-vpn-preflight-warp");
        // The command is visible before it runs: an assistant should be able
        // to say what it is about to do to the machine.
        assert!(
            tools[0].description.contains("warp-cli connect"),
            "{}",
            tools[0].description
        );
    }

    #[test]
    fn a_preflight_without_remediation_publishes_nothing() {
        // The default. Declaring the command is opt-in, and a plugin's own
        // `suggest` string never becomes callable on its own.
        let plain = r#"
[agent]
name = "x"
[run]
command = "true"
[[preflight]]
plugin = "preflight-warp"
"#;
        assert!(remediation_tools(&[agent_with(plain)]).is_empty());
    }

    #[test]
    fn an_empty_remediation_is_ignored() {
        let blank = r#"
[agent]
name = "x"
[run]
command = "true"
[[preflight]]
plugin = "preflight-warp"
remediation = "   "
"#;
        assert!(remediation_tools(&[agent_with(blank)]).is_empty());
    }

    #[test]
    fn two_agents_gated_by_the_same_plugin_get_separate_tools() {
        let other = GATED.replace("needs-vpn", "also-needs-vpn");
        let tools = remediation_tools(&[agent_with(GATED), agent_with(&other)]);
        assert_eq!(tools.len(), 2);
    }

    #[tokio::test]
    async fn an_unknown_remediation_is_refused_rather_than_guessed() {
        let result = call_remediation("remediate-nope-nope").await.unwrap();
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(rendered.contains(r#""isError":true"#), "{rendered}");
    }

    #[tokio::test]
    async fn a_non_remediation_tool_is_not_claimed() {
        assert!(call_remediation("run-something").await.is_none());
        assert!(call_remediation("skill-x").await.is_none());
    }

    // --- skills ---

    fn skill_fixture(
        dir: &std::path::Path,
        name: &str,
        body: &str,
    ) -> crate::skills::DiscoveredSkill {
        crate::skills::DiscoveredSkill {
            manifest: dotagent_core::SkillManifest {
                name: name.to_string(),
                description: "Does a thing.".into(),
                timeout_seconds: 300,
                body: body.to_string(),
            },
            dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn cap_leaves_short_text_untouched() {
        assert_eq!(cap("hello", 100), "hello");
    }

    #[test]
    fn cap_marks_the_truncation_instead_of_hiding_it() {
        let long = "x".repeat(100);
        let out = cap(&long, 10);
        assert!(out.starts_with("xxxxxxxxxx"));
        assert!(out.contains("truncated"), "{out}");
        assert!(out.contains("100"), "must say how much was dropped: {out}");
    }

    #[test]
    fn cap_never_splits_a_multibyte_char() {
        // "é" is two bytes; cutting at 1 would panic on a naive slice.
        let text = "é".repeat(20);
        let out = cap(&text, 5);
        assert!(out.contains("truncated"));
        assert!(out.starts_with("éé"), "{out}");
    }

    #[test]
    fn loading_a_skill_lists_the_files_beside_it() {
        // Without this index, a procedure that says "see references/x.md" is a
        // dead end for a caller with no filesystem tool.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("references")).unwrap();
        std::fs::write(dir.path().join("references/glossary.md"), "g").unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts/report.sh"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dir.path().join("scripts/report.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let skill = skill_fixture(dir.path(), "weekly", "Step one.");
        let result = load_skill(&skill);
        let rendered = serde_json::to_string(&result).unwrap();

        assert!(rendered.contains("Step one."));
        assert!(rendered.contains("references/glossary.md"), "{rendered}");
        assert!(rendered.contains("skill-read"), "{rendered}");
        assert!(rendered.contains("skill-run"), "{rendered}");
        assert!(rendered.contains(r#""isError":false"#));
    }

    #[test]
    fn a_skill_with_no_extra_files_returns_only_the_procedure() {
        let dir = tempfile::tempdir().unwrap();
        let skill = skill_fixture(dir.path(), "bare", "Just this.");
        let result = load_skill(&skill);
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(rendered.contains("Just this."));
        assert!(!rendered.contains("Files in this skill"), "{rendered}");
    }

    #[test]
    fn skill_catalog_drops_a_colliding_name() {
        let dir = tempfile::tempdir().unwrap();
        // `a.b` and `a/b` both sanitize to `skill-a-b`.
        let skills = vec![
            skill_fixture(dir.path(), "a.b", "one"),
            skill_fixture(dir.path(), "a/b", "two"),
        ];
        let catalog = skill_catalog(&skills);
        assert_eq!(
            catalog.len(),
            1,
            "shadowing must not be silent in the catalog"
        );
        assert_eq!(catalog[0].0, "skill-a-b");
        assert_eq!(catalog[0].1.manifest.body, "one", "first discovered wins");
    }

    #[tokio::test]
    async fn skill_read_requires_both_arguments() {
        let result = read_skill_file(&serde_json::json!({ "skill": "x" }));
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(rendered.contains(r#""isError":true"#), "{rendered}");
    }

    #[test]
    fn recall_results_lead_with_an_addressable_id() {
        // The follow-up move (correct it, delete it) needs the id; making
        // the model ask "which one?" costs a round trip.
        let hits = vec![dotagent_memory::Memory {
            id: "01ABC".into(),
            date: "2026-08-21".into(),
            text: "prefere reunião depois das 14h".into(),
            topics: vec!["agenda".into()],
            provenance: Default::default(),
            seen: 1,
            superseded_by: None,
        }];
        assert_eq!(
            render_memories(&hits),
            "[01ABC] 2026-08-21: prefere reunião depois das 14h (topics: agenda)"
        );
    }

    #[test]
    fn a_fact_with_no_date_renders_without_an_empty_prefix() {
        let hits = vec![dotagent_memory::Memory {
            id: "01ABC".into(),
            date: String::new(),
            text: "fato numa página".into(),
            topics: vec![],
            provenance: Default::default(),
            seen: 1,
            superseded_by: None,
        }];
        assert_eq!(render_memories(&hits), "[01ABC] fato numa página");
    }

    #[test]
    fn string_array_tolerates_absent_and_malformed_arguments() {
        assert!(string_array(&serde_json::json!({}), "topics").is_empty());
        assert!(string_array(&serde_json::json!({"topics": "x"}), "topics").is_empty());
        assert_eq!(
            string_array(&serde_json::json!({"topics": ["a", 2, "b"]}), "topics"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn the_memory_catalog_offers_correction_as_well_as_capture() {
        // A store you can only append to is one that accumulates
        // contradictions the model then has to choose between.
        let names: Vec<String> = memory_tools().into_iter().map(|t| t.name).collect();
        for expected in [
            "memory-remember",
            "memory-recall",
            "memory-topics",
            "memory-supersede",
            "memory-forget",
        ] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
    }

    #[tokio::test]
    async fn a_non_skill_tool_is_not_claimed_by_the_skill_handler() {
        assert!(call_skill("run-something", &serde_json::Value::Null)
            .await
            .is_none());
        assert!(call_skill("memory-recall", &serde_json::Value::Null)
            .await
            .is_none());
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
