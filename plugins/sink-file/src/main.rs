//! dotagent-plugin-sink-file — persist agent output to a file path.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Config {
    /// Destination file path. Required.
    path: PathBuf,
    /// `overwrite` (default) | `append`.
    mode: Option<String>,
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
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    written_to: Option<String>,
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
        "name": "sink-file",
        "version": env!("CARGO_PKG_VERSION"),
        "kinds": ["sink"],
        "platforms": ["darwin", "linux"],
        "schema": {
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string"},
                "mode": {"type": "string", "enum": ["overwrite", "append"]}
            }
        }
    });
    println!("{}", serde_json::to_string(&info)?);
    Ok(())
}

fn cmd_validate() -> Result<()> {
    let cfg: Config = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
    let ok = !cfg.path.as_os_str().is_empty();
    println!(
        "{}",
        serde_json::to_string(&Response {
            ok,
            error: if ok { None } else { Some("path is required") },
            written_to: None,
        })?
    );
    Ok(())
}

fn cmd_invoke() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let payload: InvokePayload = serde_json::from_str(&raw)?;
    let message = payload.message.unwrap_or_default();
    let mode = payload.config.mode.as_deref().unwrap_or("overwrite");

    if let Some(parent) = payload.config.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match mode {
        "append" => {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&payload.config.path)?;
            f.write_all(message.as_bytes())?;
        }
        _ => std::fs::write(&payload.config.path, message)?,
    }
    println!(
        "{}",
        serde_json::to_string(&Response {
            ok: true,
            error: None,
            written_to: Some(payload.config.path.display().to_string()),
        })?
    );
    Ok(())
}
