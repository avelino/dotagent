//! JSON-RPC 2.0 + Model Context Protocol wire types.
//!
//! This crate is types only — no IO, no agent knowledge. The server loop and
//! the mapping from manifests to tools live in the `dotagent` binary, the same
//! way `dotagent-plugin` keeps its envelope separate from the callers.
//!
//! ## Wire protocol
//!
//! ```text
//! client → server   {"jsonrpc":"2.0","id":1,"method":"tools/list"}
//! server → client   {"jsonrpc":"2.0","id":1,"result":{"tools":[...]}}
//!
//! client → server   {"jsonrpc":"2.0","method":"notifications/initialized"}
//! server → client   (nothing — a request without `id` is a notification)
//! ```
//!
//! One JSON object per line in both directions. Line-delimited framing is what
//! MCP stdio transport specifies, and it keeps the reader a `lines()` loop.
//!
//! ## Methods
//!
//! | method | meaning |
//! |---|---|
//! | `initialize` | handshake; server echoes the client's protocol version |
//! | `tools/list` | catalog of runnable agents |
//! | `tools/call` | run one agent, return its output |
//! | `ping` | liveness |

use serde::{Deserialize, Serialize};

/// Protocol version used when a client does not name one.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

// -------------------------------------------------------------------------
// JSON-RPC 2.0
// -------------------------------------------------------------------------

/// Standard JSON-RPC error codes. Application errors (an agent exiting
/// non-zero) are **not** these — those come back as a successful result with
/// `isError: true`, per the MCP spec, so the model can read the failure.
pub mod error_code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

/// An inbound call. `id` absent means notification: perform the work, send
/// nothing back.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: serde_json::Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// -------------------------------------------------------------------------
// MCP
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolsCapability {
    /// dotagent's catalog changes when manifests change on disk, but the
    /// server does not push `notifications/tools/list_changed`, so this stays
    /// false and clients re-list.
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub tools: ToolsCapability,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: Capabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

/// One callable agent.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<Tool>,
}

/// A block of tool output. Only text is produced today.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text { text: String },
}

/// Result of `tools/call`.
///
/// `is_error` reports that the *agent* failed, not that the call was malformed.
/// A protocol-level problem is a `JsonRpcError` instead. Keeping them apart is
/// what lets a model see "disk-alert exited 1" and react, rather than being
/// told the tool does not exist.
#[derive(Debug, Clone, Serialize)]
pub struct CallToolResult {
    pub content: Vec<Content>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

impl CallToolResult {
    pub fn text(body: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: vec![Content::Text { text: body.into() }],
            is_error,
        }
    }
}

/// Params of `tools/call`.
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
}

/// Arguments dotagent accepts inside `tools/call`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunArguments {
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

/// JSON Schema advertised for every agent tool.
pub fn run_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "schedule": {
                "type": "string",
                "description": "Schedule id to borrow args from. Defaults to the agent's first schedule."
            },
            "args": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Extra arguments appended after the manifest's own run.args."
            }
        },
        "additionalProperties": false
    })
}

/// JSON Schema for `skill-<name>`: no arguments. Loading a procedure is not a
/// call that needs tuning — the model either wants it or does not.
pub fn skill_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

/// JSON Schema for `skill-read`.
pub fn skill_read_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "skill": { "type": "string", "description": "Skill name, as listed in the catalog." },
            "path": {
                "type": "string",
                "description": "File to read, relative to the skill directory — one of the paths \
    the skill listed. Absolute paths and `..` are refused."
            }
        },
        "required": ["skill", "path"],
        "additionalProperties": false
    })
}

/// JSON Schema for `skill-run`.
pub fn skill_run_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "skill": { "type": "string", "description": "Skill name, as listed in the catalog." },
            "script": {
                "type": "string",
                "description": "Executable to run, relative to the skill directory. Must live \
    under scripts/ and carry the executable bit."
            },
            "args": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Arguments passed to the script. Never interpreted by a shell."
            }
        },
        "required": ["skill", "script"],
        "additionalProperties": false
    })
}

/// JSON Schema for `command-get`.
pub fn command_get_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Command name, exactly as it arrived in the trigger payload's \
    command.name — not a guess and not a rewording."
            },
            "args": {
                "type": "string",
                "description": "Everything the sender typed after the command name, verbatim. \
    Pass command.args through unchanged."
            }
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

/// JSON Schema for `command-list`: no arguments.
pub fn command_list_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

/// Turn a skill name into a valid MCP tool name.
///
/// Same sanitizer as [`tool_name_for`], different prefix — which is what keeps
/// a skill from ever colliding with an agent. Two *skills* can still collide,
/// so the caller dedupes the same way.
pub fn skill_tool_name_for(skill: &str) -> String {
    format!("skill-{}", sanitize(skill))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Turn an agent name into a valid MCP tool name.
///
/// MCP restricts tool names to `[a-zA-Z0-9_-]`, which agent names are not
/// bound by. The mapping is lossy (`a.b` and `a-b` both become `a-b`), so the
/// caller must detect collisions rather than assume this is injective.
pub fn tool_name_for(agent: &str) -> String {
    format!("run-{}", sanitize(agent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_without_id_is_a_notification() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn request_with_id_is_not_a_notification() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#).unwrap();
        assert!(!req.is_notification());
        assert_eq!(req.id.unwrap(), serde_json::json!(7));
    }

    #[test]
    fn ok_response_omits_error_field() {
        let res = JsonRpcResponse::ok(serde_json::json!(1), serde_json::json!({"a": 1}));
        let s = serde_json::to_string(&res).unwrap();
        assert!(!s.contains("error"), "{s}");
        assert!(s.contains(r#""jsonrpc":"2.0""#), "{s}");
    }

    #[test]
    fn err_response_omits_result_field() {
        let res = JsonRpcResponse::err(
            serde_json::json!(1),
            error_code::METHOD_NOT_FOUND,
            "no such method",
        );
        let s = serde_json::to_string(&res).unwrap();
        assert!(!s.contains("result"), "{s}");
        assert!(s.contains("-32601"), "{s}");
    }

    #[test]
    fn tool_name_sanitizes_illegal_chars() {
        assert_eq!(tool_name_for("disk-alert"), "run-disk-alert");
        assert_eq!(tool_name_for("hn.digest"), "run-hn-digest");
        assert_eq!(tool_name_for("my agent"), "run-my-agent");
        assert_eq!(tool_name_for("buser/finops"), "run-buser-finops");
    }

    #[test]
    fn tool_name_is_lossy_so_callers_must_dedupe() {
        assert_eq!(tool_name_for("a.b"), tool_name_for("a/b"));
    }

    #[test]
    fn skill_tool_names_use_their_own_prefix() {
        assert_eq!(skill_tool_name_for("weekly"), "skill-weekly");
        // The namespaced form Claude Code writes: `plugin:skill`.
        assert_eq!(
            skill_tool_name_for("dotagent:doc-review"),
            "skill-dotagent-doc-review"
        );
    }

    #[test]
    fn a_skill_can_never_collide_with_an_agent() {
        // Distinct prefixes are the whole guarantee — worth a test, because
        // dropping one would silently let a skill shadow a runnable agent.
        assert_ne!(skill_tool_name_for("x"), tool_name_for("x"));
    }

    #[test]
    fn call_tool_result_marks_agent_failure_without_jsonrpc_error() {
        let r = CallToolResult::text("exited 1", true);
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""isError":true"#), "{s}");
        assert!(s.contains(r#""type":"text""#), "{s}");
    }

    #[test]
    fn args_means_a_list_for_an_agent_and_a_string_for_a_command() {
        // Not an accident to be harmonized away. An agent's args are argv; a
        // command's are whatever the sender typed after the name, spaces and
        // all. The server must therefore parse `RunArguments` only after it
        // knows the tool is an agent — parsing up front rejected
        // `{"args": "src/"}` for `command-get` with the *agent* schema's error.
        let type_of = |schema: serde_json::Value| {
            schema["properties"]["args"]["type"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(type_of(run_input_schema()), "array");
        assert_eq!(type_of(command_get_input_schema()), "string");

        assert!(
            serde_json::from_value::<RunArguments>(serde_json::json!({"args": "src/"})).is_err(),
            "a string would have to be rejected, which is why order matters"
        );
    }

    #[test]
    fn run_arguments_default_when_absent() {
        let a: RunArguments = serde_json::from_str("{}").unwrap();
        assert!(a.schedule.is_none());
        assert!(a.args.is_empty());
    }

    #[test]
    fn run_arguments_parse_full_shape() {
        let a: RunArguments =
            serde_json::from_str(r#"{"schedule":"daily","args":["--verbose"]}"#).unwrap();
        assert_eq!(a.schedule.as_deref(), Some("daily"));
        assert_eq!(a.args, vec!["--verbose"]);
    }
}
