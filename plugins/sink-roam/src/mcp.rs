//! Thin wrapper around the user's `mcp` CLI (already in Rust, lives at
//! `~/.cargo/bin/mcp`). Each helper invokes `mcp roam <tool> '<json>'`,
//! filters tracing output, and parses the JSON response.
//!
//! The `mcp` CLI's response envelope is `{"content":[{"type":"text",
//! "text":"<inner json>"}]}` — we unwrap to `<inner json>` (parsed as
//! `serde_json::Value`).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Mcp {
    binary: PathBuf,
}

impl Mcp {
    /// Locate the `mcp` binary. Order: `MCP_BIN` env var → `~/.cargo/bin/mcp`
    /// → `$PATH`.
    pub fn from_env() -> Result<Self> {
        if let Ok(p) = std::env::var("MCP_BIN") {
            return Ok(Self {
                binary: PathBuf::from(p),
            });
        }
        if let Some(home) = dirs::home_dir() {
            let cargo = home.join(".cargo/bin/mcp");
            if cargo.is_file() {
                return Ok(Self { binary: cargo });
            }
        }
        Ok(Self {
            binary: PathBuf::from("mcp"),
        })
    }

    pub fn with_binary(binary: PathBuf) -> Self {
        Self { binary }
    }

    /// Invoke `mcp roam <tool> <json>` and return the parsed inner payload.
    pub fn roam(&self, tool: &str, args: &Value) -> Result<Value> {
        let payload = args.to_string();
        let output = Command::new(&self.binary)
            .arg("roam")
            .arg(tool)
            .arg(&payload)
            .output()
            .with_context(|| format!("spawning mcp roam {tool}"))?;
        if !output.status.success() {
            return Err(anyhow!(
                "mcp roam {tool} exited {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        // mcp CLI may prepend `tracing` info lines (INFO ... etc.). Find the
        // first '{' or '[' and parse from there — matches the awk filter in
        // lib/agent.fish.
        let raw = String::from_utf8_lossy(&output.stdout);
        let json_start = raw
            .find(['{', '['])
            .ok_or_else(|| anyhow!("no JSON in mcp output: {raw}"))?;
        let envelope: Value = serde_json::from_str(raw[json_start..].trim())?;
        // Unwrap `.content[0].text` (string) → parse as JSON.
        let text = envelope
            .pointer("/content/0/text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("mcp envelope missing .content[0].text: {envelope}"))?;
        let inner: Value = serde_json::from_str(text)
            .with_context(|| format!("parsing inner mcp payload: {text}"))?;
        Ok(inner)
    }
}
