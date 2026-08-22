//! Toolkit provisioning: manifest declaration → MCP config → stable hash.
//!
//! The manifest's `[assistant]` block declares which MCP servers a
//! conversation runs with. The daemon assembles the `mcp.json` the model
//! client consumes, writes it to a content-addressed path and hashes it.
//! The hash is the session-invalidation key: a resumed model session freezes
//! its MCP config at creation time, so any toolkit change must start a new
//! session — deterministically, not by luck of a sorted string.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The manifest's server declaration is the canonical type; it lives in
/// `dotagent-core` (the `agent.toml` surface) so the schema and the
/// assembly can never drift apart.
pub use dotagent_core::manifest::ToolkitServer;

fn server_config_value(server: &ToolkitServer, dotagent_bin: &str) -> Value {
    match server {
        ToolkitServer::Dotagent => json!({
            "type": "stdio",
            "command": dotagent_bin,
            "args": ["mcp"],
        }),
        ToolkitServer::Http { url } => json!({
            "type": "http",
            "url": url,
        }),
        ToolkitServer::Stdio { command, args } => json!({
            "type": "stdio",
            "command": command,
            "args": args,
        }),
    }
}

/// Failures while assembling an `mcp.json` from a manifest declaration.
#[derive(Debug, Error)]
pub enum ToolkitError {
    /// Two servers map to the same `mcp.json` key: a JSON object cannot
    /// hold both, and the last-write-wins insert would silently hide the
    /// misconfiguration instead of surfacing it.
    #[error("duplicate MCP server name '{name}' in toolkit declaration")]
    DuplicateServerName { name: String },
}

/// Assemble the `mcp.json` servers object from the declaration.
///
/// The emitted shape is what model clients consume from a config file —
/// Claude's `--mcp-config` expects `{"mcpServers": {...}}` — so the servers
/// live under that key, not at the top level.
///
/// Servers are emitted sorted by name so two manifests listing the same
/// servers in different orders produce identical bytes — and therefore an
/// identical hash, keeping warm sessions alive across harmless reorders.
/// Duplicate server names are rejected (see [`ToolkitError`]). The
/// `dotagent_bin` path is excluded from hashing (see [`toolkit_hash`]).
pub fn build_mcp_config(
    servers: &[ToolkitServer],
    dotagent_bin: &str,
) -> Result<Value, ToolkitError> {
    let mut sorted: Vec<&ToolkitServer> = servers.iter().collect();
    sorted.sort_by_key(|s| s.name());

    let mut map = Map::new();
    for server in sorted {
        let name = server.name().to_string();
        if map.contains_key(&name) {
            return Err(ToolkitError::DuplicateServerName { name });
        }
        map.insert(name, server_config_value(server, dotagent_bin));
    }
    Ok(json!({ "mcpServers": Value::Object(map) }))
}

/// The stable identity of a toolkit: hex sha256 of the canonical config.
///
/// The local daemon binary path is normalized out: rebuilding or moving the
/// binary must not retire every warm session, since the server it launches
/// is the same catalog.
pub fn toolkit_hash(config: &Value) -> String {
    let mut canonical = config.clone();
    let servers_root = canonical
        .as_object_mut()
        .and_then(|root| root.get_mut("mcpServers"))
        .and_then(Value::as_object_mut);
    if let Some(servers) = servers_root {
        if let Some(dotagent) = servers.get_mut("dotagent") {
            if let Some(cmd) = dotagent.get_mut("command") {
                *cmd = Value::String("<dotagent>".to_string());
            }
        }
    }
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy() -> ToolkitServer {
        ToolkitServer::Http {
            url: "http://127.0.0.1:7333/mcp".into(),
        }
    }

    #[test]
    fn assembles_dotagent_and_proxy() {
        let config = build_mcp_config(
            &[ToolkitServer::Dotagent, proxy()],
            "/usr/local/bin/dotagent",
        )
        .unwrap();
        let servers = &config["mcpServers"];
        assert_eq!(servers.as_object().unwrap().len(), 2);
        assert_eq!(servers["dotagent"]["command"], "/usr/local/bin/dotagent");
        assert_eq!(servers["dotagent"]["args"], json!(["mcp"]));
        assert_eq!(servers["mcp"]["url"], "http://127.0.0.1:7333/mcp");
    }

    #[test]
    fn declaration_order_does_not_change_the_hash() {
        let a = build_mcp_config(&[ToolkitServer::Dotagent, proxy()], "/bin/dotagent").unwrap();
        let b = build_mcp_config(&[proxy(), ToolkitServer::Dotagent], "/other/dotagent").unwrap();
        assert_eq!(toolkit_hash(&a), toolkit_hash(&b));
    }

    #[test]
    fn daemon_binary_path_does_not_change_the_hash() {
        let a = build_mcp_config(&[ToolkitServer::Dotagent], "/bin/dotagent").unwrap();
        let b = build_mcp_config(&[ToolkitServer::Dotagent], "/nix/store/xxx-dotagent").unwrap();
        assert_eq!(toolkit_hash(&a), toolkit_hash(&b));
    }

    #[test]
    fn different_proxy_url_changes_the_hash() {
        let a = build_mcp_config(&[proxy()], "/bin/dotagent").unwrap();
        let other = ToolkitServer::Http {
            url: "http://127.0.0.1:7332/mcp".into(),
        };
        let b = build_mcp_config(&[other], "/bin/dotagent").unwrap();
        assert_ne!(toolkit_hash(&a), toolkit_hash(&b));
    }

    #[test]
    fn adding_a_server_changes_the_hash() {
        let one = build_mcp_config(&[ToolkitServer::Dotagent], "/bin/dotagent").unwrap();
        let two = build_mcp_config(&[ToolkitServer::Dotagent, proxy()], "/bin/dotagent").unwrap();
        assert_ne!(toolkit_hash(&one), toolkit_hash(&two));
    }

    #[test]
    fn stdio_server_carries_command_and_args() {
        let server = ToolkitServer::Stdio {
            command: "mcp".into(),
            args: vec!["serve".into()],
        };
        let name = server.name().to_string();
        let config = build_mcp_config(&[server], "/bin/dotagent").unwrap();
        assert_eq!(
            config["mcpServers"]["stdio"],
            json!({"type": "stdio", "command": "mcp", "args": ["serve"]})
        );
        assert_eq!(name, "stdio");
    }

    #[test]
    fn same_config_hashed_twice_is_identical() {
        let config =
            build_mcp_config(&[ToolkitServer::Dotagent, proxy()], "/bin/dotagent").unwrap();
        assert_eq!(toolkit_hash(&config), toolkit_hash(&config));
    }

    #[test]
    fn duplicate_server_names_are_rejected_not_overwritten() {
        // Two Http servers would both land under the key "mcp"; the map
        // insert would silently keep only the last one.
        let result = build_mcp_config(
            &[
                proxy(),
                ToolkitServer::Http {
                    url: "http://127.0.0.1:7334/mcp".into(),
                },
            ],
            "/bin/dotagent",
        );
        match result {
            Err(ToolkitError::DuplicateServerName { name }) => assert_eq!(name, "mcp"),
            other => panic!("expected duplicate-name error, got {other:?}"),
        }
    }
}
