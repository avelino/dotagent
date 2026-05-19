//! dotagent-plugin-preflight-cmd — generic preflight: run a command, check exit.

use std::io::Read;
use std::process::Command;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Config {
    /// Command to run. Required.
    command: String,
    /// Args.
    args: Vec<String>,
    /// Expected exit code (default 0).
    expect_exit: Option<i32>,
    /// Optional substring to match against stdout/stderr for additional gating.
    expect_contains: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InvokePayload {
    config: Config,
}

#[derive(Debug, Serialize)]
struct Response<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
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
        "name": "preflight-cmd",
        "version": env!("CARGO_PKG_VERSION"),
        "kinds": ["preflight"],
        "platforms": ["darwin", "linux"],
        "schema": {
            "type": "object",
            "required": ["command"],
            "properties": {
                "command":         {"type": "string"},
                "args":            {"type": "array", "items": {"type": "string"}},
                "expect_exit":     {"type": "integer"},
                "expect_contains": {"type": "string"}
            }
        }
    });
    println!("{}", serde_json::to_string(&info)?);
    Ok(())
}

fn cmd_validate() -> Result<()> {
    let cfg: Config = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
    let ok = !cfg.command.is_empty();
    println!(
        "{}",
        serde_json::to_string(&Response {
            ok,
            error: if ok {
                None
            } else {
                Some("command is required")
            },
            exit_code: None,
        })?
    );
    Ok(())
}

fn cmd_invoke() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let payload: InvokePayload = serde_json::from_str(&raw)?;
    let expected = payload.config.expect_exit.unwrap_or(0);

    let output = Command::new(&payload.config.command)
        .args(&payload.config.args)
        .output()?;
    let code = output.status.code().unwrap_or(-1);

    let exit_ok = code == expected;
    let contains_ok = match payload.config.expect_contains.as_deref() {
        None => true,
        Some(needle) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            stdout.contains(needle) || stderr.contains(needle)
        }
    };
    let ok = exit_ok && contains_ok;

    println!(
        "{}",
        serde_json::to_string(&Response {
            ok,
            error: if ok {
                None
            } else {
                Some("preflight command failed")
            },
            exit_code: Some(code),
        })?
    );
    Ok(())
}
