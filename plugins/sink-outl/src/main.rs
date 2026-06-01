//! dotagent-plugin-sink-outl — publish agent output to Outl.
//!
//! Twin of `sink-roam`. Same config schema (`page`, `marker_regex`,
//! `mcp_binary`) so a single agent can ship the same payload to both
//! backends by declaring two `[[on_success]]` blocks. Uses `outl_batch`
//! to make the delete+write idempotent in one round-trip.
//!
//! Config schema:
//! ```jsonc
//! {
//!   "page":         "today" | "April 22nd, 2026" | "2026-06-01" | "acme/tech/aws",
//!   "marker_regex": "#TAG.*2026-05-19",   // matches the block to replace
//!   "mcp_binary":   "/path/to/mcp"        // optional; defaults to env discovery
//! }
//! ```

mod mcp;
mod publish;
mod sanitize;
mod translate;

use std::io::Read;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::mcp::Mcp;

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Config {
    /// Page reference: `today`, ISO date, `April 22nd, 2026`, or namespaced slug.
    page: String,
    /// Regex used to find and delete existing blocks (idempotency).
    marker_regex: Option<String>,
    /// Override path to the `mcp` CLI binary.
    mcp_binary: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct InvokePayload {
    #[serde(default)]
    message: Option<String>,
    config: Config,
}

#[derive(Debug, Serialize)]
struct Response<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_id: Option<String>,
}

fn main() -> Result<()> {
    let verb = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("missing verb"))?;
    match verb.as_str() {
        "info" => cmd_info(),
        "validate" => cmd_validate(),
        "invoke" => cmd_invoke(),
        other => bail!("unknown verb: {other}"),
    }
}

fn cmd_info() -> Result<()> {
    let info = json!({
        "name": "sink-outl",
        "version": env!("CARGO_PKG_VERSION"),
        "kinds": ["sink"],
        "platforms": ["darwin", "linux"],
        "schema": {
            "type": "object",
            "required": ["page"],
            "properties": {
                "page":         {"type": "string"},
                "marker_regex": {"type": "string"},
                "mcp_binary":   {"type": "string"}
            }
        }
    });
    println!("{}", serde_json::to_string(&info)?);
    Ok(())
}

fn cmd_validate() -> Result<()> {
    let cfg: Config = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
    let mut errors = vec![];
    if cfg.page.is_empty() {
        errors.push("page is required");
    }
    if let Some(rx) = cfg.marker_regex.as_ref() {
        if Regex::new(rx).is_err() {
            errors.push("marker_regex is not a valid regex");
        }
    }
    let ok = errors.is_empty();
    println!(
        "{}",
        serde_json::to_string(&Response {
            ok,
            error: (!ok).then(|| errors.join("; ")),
            note: None,
            root_id: None,
        })?
    );
    Ok(())
}

fn cmd_invoke() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let payload: InvokePayload = serde_json::from_str(&raw)?;
    let cfg = payload.config;
    let content = payload.message.unwrap_or_default();

    if content.trim().is_empty() {
        println!(
            "{}",
            serde_json::to_string(&Response {
                ok: false,
                error: Some("empty message".into()),
                note: None,
                root_id: None,
            })?
        );
        return Ok(());
    }

    let mcp = match cfg.mcp_binary {
        Some(p) => Mcp::with_binary(p),
        None => Mcp::from_env()?,
    };

    let marker = match cfg.marker_regex {
        Some(rx) => Regex::new(&rx)?,
        None => Regex::new("$^").unwrap(),
    };

    let response = match publish::publish(&mcp, &content, &cfg.page, &marker) {
        Ok(root_id) => Response {
            ok: true,
            error: None,
            note: None,
            root_id: Some(root_id),
        },
        Err(e) => Response {
            ok: false,
            error: Some(e.to_string()),
            note: None,
            root_id: None,
        },
    };
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}
