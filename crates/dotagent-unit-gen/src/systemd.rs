//! systemd user-unit generation (Linux).
//!
//! Template: `templates/daemon.service`. Variables: `LABEL`, `BINARY`,
//! `STDOUT_LOG`, `STDERR_LOG`.
//!
//! `StandardError=` stays an append file on purpose — **do not point it at
//! `null`.** The daemon no longer mirrors `tracing` events there (see
//! `dotagent-telemetry`), so it is a low-volume crash channel: panics,
//! aborts, loader failures, and anything that dies before the subscriber
//! exists. Those never reach the structured JSON log. Rewiring this to
//! `journal` is fine, but then set `DOTAGENT_LOG_STDERR=1` so the mirror
//! comes back.

use crate::template::{find_unrendered_placeholder, render};
use crate::{GenContext, Result, UnitGenError, UnitPath, DAEMON_LABEL};

const TEMPLATE: &str = include_str!("../templates/daemon.service");

pub fn generate_daemon(ctx: &GenContext) -> Result<UnitPath> {
    let home = dirs::home_dir().ok_or(UnitGenError::NoHome)?;
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;

    let unit = render_service(ctx);
    if let Some(left) = find_unrendered_placeholder(&unit) {
        return Err(UnitGenError::Unsupported(format!(
            "unrendered placeholder in daemon.service: {left}"
        )));
    }

    let path = unit_dir.join(format!("{DAEMON_LABEL}.service"));
    std::fs::write(&path, unit)?;
    Ok(UnitPath { path })
}

fn render_service(ctx: &GenContext) -> String {
    let bin = ctx.dotagent_binary.display().to_string();
    let stdout_log = ctx
        .log_dir
        .join(format!("{DAEMON_LABEL}.log"))
        .display()
        .to_string();
    let stderr_log = ctx
        .log_dir
        .join(format!("{DAEMON_LABEL}-error.log"))
        .display()
        .to_string();

    render(
        TEMPLATE,
        &[
            ("LABEL", DAEMON_LABEL),
            ("BINARY", &bin),
            ("STDOUT_LOG", &stdout_log),
            ("STDERR_LOG", &stderr_log),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn service_template_renders_without_placeholders() {
        let ctx = GenContext {
            dotagent_binary: PathBuf::from("/usr/bin/dotagent"),
            log_dir: PathBuf::from("/var/log"),
        };
        let out = render_service(&ctx);
        assert!(
            find_unrendered_placeholder(&out).is_none(),
            "leftover: {out}"
        );
        assert!(out.contains("ExecStart=/usr/bin/dotagent daemon"));
    }

    #[test]
    fn service_captures_stderr_to_a_real_file() {
        let ctx = GenContext {
            dotagent_binary: PathBuf::from("/usr/bin/dotagent"),
            log_dir: PathBuf::from("/var/log"),
        };
        let out = render_service(&ctx);

        assert!(out.contains("StandardError=append:/var/log/run.avelino.dotagent-error.log"));
        assert!(out.contains("StandardOutput=append:/var/log/run.avelino.dotagent.log"));
        assert!(
            !out.contains("StandardError=null"),
            "discarding stderr erases panic diagnostics"
        );
    }
}
