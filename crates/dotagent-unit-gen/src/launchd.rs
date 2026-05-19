//! launchd plist generation (macOS).
//!
//! Template: `templates/daemon.plist`. Variables: `LABEL`, `BINARY`,
//! `STDOUT_LOG`, `STDERR_LOG`.

use crate::template::{find_unrendered_placeholder, render};
use crate::{GenContext, Result, UnitGenError, UnitPath, DAEMON_LABEL};

const TEMPLATE: &str = include_str!("../templates/daemon.plist");

pub fn generate_daemon(ctx: &GenContext) -> Result<UnitPath> {
    let home = dirs::home_dir().ok_or(UnitGenError::NoHome)?;
    let agents_dir = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents_dir)?;

    let plist = render_plist(ctx);
    if let Some(left) = find_unrendered_placeholder(&plist) {
        return Err(UnitGenError::Unsupported(format!(
            "unrendered placeholder in daemon.plist: {left}"
        )));
    }

    let path = agents_dir.join(format!("{DAEMON_LABEL}.plist"));
    std::fs::write(&path, plist)?;
    Ok(UnitPath { path })
}

fn render_plist(ctx: &GenContext) -> String {
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
    fn plist_template_renders_without_placeholders() {
        let ctx = GenContext {
            dotagent_binary: PathBuf::from("/usr/local/bin/dotagent"),
            log_dir: PathBuf::from("/tmp/logs"),
        };
        let out = render_plist(&ctx);
        assert!(
            find_unrendered_placeholder(&out).is_none(),
            "leftover: {out}"
        );
        assert!(out.contains(DAEMON_LABEL));
        assert!(out.contains("/usr/local/bin/dotagent"));
    }
}
