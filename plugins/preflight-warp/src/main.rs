//! dotagent-plugin-preflight-warp — preflight check for Cloudflare WARP.
//!
//! Runs `warp-cli status` and returns ok only when status reports "Connected".
//! Equivalent to the manual WARP check at the top of team-standup
//! today.

use std::io::Read;
use std::process::Command;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Config {
    /// Binary name. Default: `warp-cli`.
    binary: Option<String>,
    /// Optional `connect_command` reported back in the response so the
    /// orchestrator/notification can suggest a fix.
    connect_command: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InvokePayload {
    #[serde(default)]
    config: Config,
}

#[derive(Debug, Serialize)]
struct Response<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
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
        "name": "preflight-warp",
        "version": env!("CARGO_PKG_VERSION"),
        "kinds": ["preflight"],
        "platforms": ["darwin", "linux"],
        "schema": {
            "type": "object",
            "properties": {
                "binary": {"type": "string"},
                "connect_command": {"type": "string"}
            }
        }
    });
    println!("{}", serde_json::to_string(&info)?);
    Ok(())
}

fn cmd_validate() -> Result<()> {
    // No required fields.
    let _cfg: Config = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
    println!(
        "{}",
        serde_json::to_string(&Response {
            ok: true,
            error: None,
            suggest: None,
            status: None,
        })?
    );
    Ok(())
}

fn cmd_invoke() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let payload: InvokePayload = serde_json::from_str(&raw).unwrap_or(InvokePayload {
        config: Config::default(),
    });
    let bin = payload
        .config
        .binary
        .clone()
        .unwrap_or_else(|| "warp-cli".into());
    let output = Command::new(&bin).arg("status").output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let connected = text.contains("Connected");
            let resp = Response {
                ok: connected,
                error: if connected {
                    None
                } else {
                    Some("WARP not connected")
                },
                suggest: if connected {
                    None
                } else {
                    payload.config.connect_command.clone()
                },
                status: Some(text.trim().to_string()),
            };
            println!("{}", serde_json::to_string(&resp)?);
        }
        _ => {
            let resp = Response {
                ok: false,
                error: Some("could not run warp-cli"),
                suggest: payload.config.connect_command.clone(),
                status: None,
            };
            println!("{}", serde_json::to_string(&resp)?);
        }
    }
    Ok(())
}
