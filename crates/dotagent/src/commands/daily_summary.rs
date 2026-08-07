//! End-of-day health summary, delivered through the built-in notifiers.
//!
//! The daemon delivers this once a day at `[daily_summary].time` in
//! `~/.config/dotagent/config.toml` (default 22:45 local), and schedules a
//! wake-up for it; `grace_minutes` (default 30) covers the case where the
//! wake-up could not happen at all — machine asleep, machine off.
//!
//! Destinations come from `[[daily_summary.notifiers]]`, which takes the same
//! entries a manifest's `[[notifiers]]` takes. With none configured the
//! summary goes to the `desktop` driver: the only one that needs no address
//! and reaches nothing off the machine.
//!
//! Standalone invocation prints the body and its destinations without sending:
//! `dotagent daily-summary --dry-run`.

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use dotagent_core::DailySummaryConfig;
use dotagent_notify::{NotifierConfig, NotifierEntry};
use dotagent_plugin::PluginClient;
use dotagent_runner::notifiers::fire_notifier_entries;
use dotagent_scheduler::{health_state, window_key, HealthState, ResolvedPolicy};
use dotagent_state::{audit::AuditLog, slug_from_args, StateStore};
use tracing::info;

use crate::discovery::{self, DiscoveredAgent};

/// Identity the summary carries into notifications and the audit log.
///
/// Not an agent: no manifest declares it, nothing schedules it as a run. But
/// both the audit schema and the reply-correlation store key on a name, and
/// "dotagent itself" is the honest one to give them.
const SUMMARY_AGENT: &str = "dotagent";
const SUMMARY_SCHEDULE: &str = "daily-summary";
const SUMMARY_EVENT: &str = "daily_summary";

/// One line per unhealthy `(agent, schedule)`, plus the healthy count.
#[derive(Debug, Default, PartialEq, Eq)]
struct Summary {
    ok: usize,
    degraded: Vec<String>,
    failing: Vec<String>,
    stale: Vec<String>,
}

/// Classify every monitored `(agent, schedule)` from state on disk.
///
/// The window is looked up by `window_key` — the same key the daemon writes
/// against. Deriving it here from `last_success_at` instead produced a
/// filename that only matched when a run finished inside its own window
/// minute, so `degraded` was effectively unreachable in the daily summary.
fn collect(agents: &[DiscoveredAgent], state: &StateStore, now: DateTime<Local>) -> Summary {
    let mut summary = Summary::default();

    for agent in agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        let name = &agent.manifest.agent.name;
        for sched in &agent.manifest.schedules {
            let policy = ResolvedPolicy::resolve(&agent.manifest, sched);
            let slug = slug_from_args(sched.args());
            let hb = state.read_heartbeat(name, &slug).ok().flatten();
            let window = window_key(sched, hb.as_ref(), now)
                .and_then(|key| state.read_window(name, &slug, key).ok().flatten());
            let (health, reason) = health_state(sched, &policy, hb.as_ref(), window.as_ref(), now);
            let label = format!("{}/{} — {}", name, sched.id(), reason);
            match health {
                HealthState::Ok => summary.ok += 1,
                HealthState::Degraded => summary.degraded.push(label),
                HealthState::Failing => summary.failing.push(label),
                HealthState::Stale => summary.stale.push(label),
            }
        }
    }

    summary
}

fn render(summary: &Summary, now: DateTime<Local>) -> String {
    let ok = summary.ok;
    let total = ok + summary.degraded.len() + summary.failing.len() + summary.stale.len();
    let mut body = format!("📊 Agents · {}\n", now.format("%Y-%m-%d"));
    body.push_str(&format!("{ok}/{total} ok\n"));
    for (header, lines) in [
        ("\n❌ Failing:\n", &summary.failing),
        ("\n⚠️ Degraded:\n", &summary.degraded),
        ("\n🕑 Stale:\n", &summary.stale),
    ] {
        if lines.is_empty() {
            continue;
        }
        body.push_str(header);
        for line in lines {
            body.push_str(&format!("  · {line}\n"));
        }
    }
    body
}

/// Where this summary goes.
///
/// An empty `[[daily_summary.notifiers]]` resolves to `desktop` rather than to
/// nothing. The alternative — deliver only when configured — is how this
/// feature spent its whole life so far: it ran nightly, addressed a constant
/// nobody owned, and left no trace when the delivery went nowhere.
fn resolve_notifiers(cfg: &DailySummaryConfig) -> Vec<NotifierEntry> {
    if cfg.notifiers.is_empty() {
        return vec![NotifierEntry {
            config: NotifierConfig::Desktop(Default::default()),
            events: Vec::new(),
        }];
    }
    cfg.notifiers.iter().cloned().map(unfiltered).collect()
}

/// Drop any `events` filter from a daily-summary notifier.
///
/// The list is already scoped to a single event, so a filter there can only
/// subtract. An entry copied from a manifest — `events = ["given_up"]` is the
/// common one — would match nothing and drop the summary without a word.
/// Honoring a field whose only reachable effect is silence is worse than
/// ignoring it.
fn unfiltered(mut entry: NotifierEntry) -> NotifierEntry {
    entry.events.clear();
    if let NotifierConfig::Plugin(p) = &mut entry.config {
        p.events.clear();
    }
    entry
}

fn driver_list(entries: &[NotifierEntry]) -> String {
    entries
        .iter()
        .map(|e| e.driver_name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Load the config from disk and deliver. Used by the CLI subcommand.
pub async fn run(dry_run: bool) -> Result<()> {
    let config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    run_with(&config.daily_summary, dry_run).await
}

/// Deliver against an already-loaded config. The daemon uses this so the
/// summary honors the same config generation the rest of the tick did.
///
/// `enabled = false` is deliberately **not** checked here: it governs the
/// daemon's scheduled delivery, and someone who typed `dotagent daily-summary`
/// asked for this one.
pub async fn run_with(cfg: &DailySummaryConfig, dry_run: bool) -> Result<()> {
    let state = StateStore::from_home().context("opening state store")?;
    let agents = discovery::discover_all()?;
    let now = Local::now();

    let body = render(&collect(&agents, &state, now), now);
    let entries = resolve_notifiers(cfg);

    if dry_run {
        println!("{body}");
        println!("→ would deliver to: {}", driver_list(&entries));
        return Ok(());
    }

    info!(
        drivers = %driver_list(&entries),
        "delivering daily summary"
    );
    // Per-notifier outcome (including failure) is logged and audited inside
    // `fire_notifier_entries`, as `notifier:<driver>` — the same trail every
    // agent notification leaves. A delivery that goes nowhere is now
    // answerable from `dotagent audit` instead of from silence.
    let plugins = PluginClient::from_environment();
    let audit = AuditLog::from_home().ok();
    fire_notifier_entries(
        SUMMARY_AGENT,
        &entries,
        SUMMARY_SCHEDULE,
        SUMMARY_EVENT,
        &body,
        Some(&plugins),
        audit.as_ref(),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use dotagent_core::{AgentManifest, Heartbeat, WindowState};
    use std::path::Path;

    const CRON_MANIFEST: &str = r#"
[agent]
name = "disk-alert"
timeout_seconds = 600

[run]
command = "/bin/true"

[[schedules]]
id = "morning"
type = "cron"
weekdays = [0, 1, 2, 3, 4, 5, 6]
hours = [10]
minute = 0
"#;

    const INTERVAL_MANIFEST: &str = r#"
[agent]
name = "hn-digest"
timeout_seconds = 600

[run]
command = "/bin/true"

[[schedules]]
id = "digest-90min"
type = "interval"
interval_minutes = 90
"#;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 6, h, m, s).unwrap()
    }

    fn agent_from(dir: &Path, manifest: &str) -> DiscoveredAgent {
        let path = dir.join("agent.toml");
        std::fs::write(&path, manifest).unwrap();
        DiscoveredAgent {
            manifest: AgentManifest::load(&path).unwrap(),
            dir: dir.to_path_buf(),
        }
    }

    fn heartbeat(agent: &str, last_success: DateTime<Local>) -> Heartbeat {
        Heartbeat {
            name: agent.into(),
            slug: "default".into(),
            args: vec![],
            started_at: last_success.timestamp(),
            started_at_iso: String::new(),
            finished_at: Some(last_success.timestamp()),
            finished_at_iso: Some(last_success.to_rfc3339()),
            exit_code: Some(0),
            duration_seconds: Some(1),
            last_success_at: Some(last_success.timestamp()),
            last_success_at_iso: None,
        }
    }

    fn window(
        agent: &str,
        schedule: &str,
        expected: DateTime<Local>,
        attempts: u32,
        exit_code: i32,
    ) -> WindowState {
        WindowState {
            agent: agent.into(),
            schedule_id: schedule.into(),
            expected_at: expected.timestamp(),
            attempts,
            last_attempt_at: Some(expected.timestamp()),
            last_attempt_exit_code: Some(exit_code),
            last_attempt_stderr: None,
            given_up: false,
            given_up_at: None,
        }
    }

    /// The bug: the window was keyed by `last_success_at`, so the 10:00 window
    /// file was never found and a schedule that needed a retry reported plain
    /// `ok`. The summary could not reach `degraded` at all.
    #[test]
    fn degraded_when_the_due_window_needed_a_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let agents = vec![agent_from(tmp.path(), CRON_MANIFEST)];

        state
            .write_heartbeat(&heartbeat("disk-alert", at(10, 12, 0)))
            .unwrap();
        state
            .write_window(
                &window("disk-alert", "morning", at(10, 0, 0), 2, 0),
                "default",
                at(10, 0, 0),
            )
            .unwrap();

        let summary = collect(&agents, &state, at(10, 30, 0));
        assert_eq!(summary.ok, 0, "{summary:?}");
        assert_eq!(summary.degraded.len(), 1, "{summary:?}");
        assert!(
            summary.degraded[0].contains('1'),
            "one failed attempt before the success: {}",
            summary.degraded[0]
        );
    }

    /// The counterpart: `attempts` counts the successful dispatch too, so a
    /// first-try success must stay `ok`. The summary reads the count through
    /// `health_state`, which already discounts it — this pins that it does not
    /// grow its own raw-`attempts` reader.
    #[test]
    fn ok_when_the_first_attempt_succeeded() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let agents = vec![agent_from(tmp.path(), CRON_MANIFEST)];

        state
            .write_heartbeat(&heartbeat("disk-alert", at(10, 0, 51)))
            .unwrap();
        state
            .write_window(
                &window("disk-alert", "morning", at(10, 0, 0), 1, 0),
                "default",
                at(10, 0, 0),
            )
            .unwrap();

        let summary = collect(&agents, &state, at(10, 30, 0));
        assert_eq!(summary.ok, 1, "{summary:?}");
        assert!(summary.degraded.is_empty(), "{summary:?}");
    }

    /// A failing window must carry its attempt count into the message, which
    /// only happens if the window file was actually found.
    ///
    /// Asserting on `1 attempt` rather than on `attempt`: the no-window
    /// fallback reads `window due Nmin ago, no attempt`, which a bare
    /// substring check would accept while proving nothing.
    #[test]
    fn failing_window_reports_its_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let agents = vec![agent_from(tmp.path(), CRON_MANIFEST)];

        let yesterday = at(10, 0, 0) - chrono::Duration::days(1);
        state
            .write_heartbeat(&heartbeat("disk-alert", yesterday))
            .unwrap();
        state
            .write_window(
                &window("disk-alert", "morning", at(10, 0, 0), 1, 1),
                "default",
                at(10, 0, 0),
            )
            .unwrap();

        let summary = collect(&agents, &state, at(10, 30, 0));
        assert_eq!(summary.failing.len(), 1, "{summary:?}");
        assert!(
            summary.failing[0].contains("1 attempt"),
            "window state was not read: {}",
            summary.failing[0]
        );
    }

    /// Interval windows roll forward; the key has to roll with them.
    /// Ticks are 09:00, 10:30, 12:00 — at 11:00 the due window is 10:30.
    #[test]
    fn interval_reads_the_rolled_window() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let agents = vec![agent_from(tmp.path(), INTERVAL_MANIFEST)];

        state
            .write_heartbeat(&heartbeat("hn-digest", at(9, 0, 0)))
            .unwrap();
        state
            .write_window(
                &window("hn-digest", "digest-90min", at(10, 30, 0), 1, 1),
                "default",
                at(10, 30, 0),
            )
            .unwrap();

        let summary = collect(&agents, &state, at(11, 0, 0));
        assert_eq!(summary.failing.len(), 1, "{summary:?}");
        assert!(
            summary.failing[0].contains("1 attempt"),
            "rolled window was not read: {}",
            summary.failing[0]
        );
    }

    #[test]
    fn unmonitored_agents_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let manifest = CRON_MANIFEST.replace("timeout_seconds", "monitor = false\ntimeout_seconds");
        let agents = vec![agent_from(tmp.path(), &manifest)];

        let summary = collect(&agents, &state, at(10, 30, 0));
        assert_eq!(summary, Summary::default());
    }

    #[test]
    fn render_lists_every_bucket() {
        let summary = Summary {
            ok: 1,
            degraded: vec!["a/x — recovered after 1 attempt".into()],
            failing: vec!["b/y — 2 attempts, will retry".into()],
            stale: vec!["c/z — never ran".into()],
        };
        let body = render(&summary, at(22, 45, 0));
        assert!(body.contains("1/4 ok"), "{body}");
        assert!(body.contains("a/x — recovered after 1 attempt"), "{body}");
        assert!(body.contains("b/y — 2 attempts, will retry"), "{body}");
        assert!(body.contains("c/z — never ran"), "{body}");
    }

    #[test]
    fn render_omits_empty_buckets() {
        let summary = Summary {
            ok: 3,
            ..Default::default()
        };
        let body = render(&summary, at(22, 45, 0));
        assert!(body.contains("3/3 ok"), "{body}");
        assert!(!body.contains("Failing"), "{body}");
        assert!(!body.contains("Degraded"), "{body}");
        assert!(!body.contains("Stale"), "{body}");
    }

    #[test]
    fn render_headers_are_english() {
        let summary = Summary {
            ok: 0,
            degraded: vec!["a/x — d".into()],
            failing: vec!["b/y — f".into()],
            stale: vec!["c/z — s".into()],
        };
        let body = render(&summary, at(22, 45, 0));
        assert!(body.contains("❌ Failing:"), "{body}");
        assert!(body.contains("⚠️ Degraded:"), "{body}");
        assert!(body.contains("🕑 Stale:"), "{body}");
        for pt in ["Falhando", "Degradado", "tentativas"] {
            assert!(!body.contains(pt), "leftover pt-BR {pt}: {body}");
        }
    }

    // --- destination resolution: the half that was a hardcoded phone number
    // nobody owned. ---

    #[test]
    fn no_configured_notifier_falls_back_to_desktop() {
        // Not to nothing. "Nothing" is what this feature did for its whole
        // life, and it did it without an error anywhere.
        let entries = resolve_notifiers(&DailySummaryConfig::default());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].driver_name(), "desktop");
    }

    #[test]
    fn the_fallback_is_never_a_hardcoded_address() {
        // Regression pin for the original bug: the summary used to go to
        // `notify-imessage` at a placeholder number compiled into the binary.
        let entries = resolve_notifiers(&DailySummaryConfig::default());
        assert!(
            entries.iter().all(|e| e.driver_name() != "imessage"),
            "no address may be baked into the default path"
        );
    }

    fn cfg_with(toml_str: &str) -> DailySummaryConfig {
        toml::from_str(toml_str).expect("test config must parse")
    }

    #[test]
    fn configured_notifiers_replace_the_fallback() {
        let cfg = cfg_with(
            r#"
[[notifiers]]
driver = "telegram"
bot_token = "${TG}"
chat_id = "42"

[[notifiers]]
driver = "slack"
webhook_url = "https://hooks.slack.com/x"
"#,
        );
        let entries = resolve_notifiers(&cfg);
        assert_eq!(driver_list(&entries), "telegram, slack");
    }

    #[test]
    fn an_event_filter_cannot_silence_the_summary() {
        // Copying an entry out of a manifest brings its `events` along. Left
        // alone it would match no event the summary fires and drop the whole
        // delivery without a word — the exact failure mode being fixed.
        let cfg = cfg_with(
            r#"
[[notifiers]]
driver = "slack"
webhook_url = "https://hooks.slack.com/x"
events = ["given_up"]
"#,
        );
        let entries = resolve_notifiers(&cfg);
        assert!(
            entries[0].matches_event(SUMMARY_EVENT),
            "the summary's own event must reach a notifier the user listed"
        );
    }

    #[test]
    fn a_plugin_event_filter_cannot_silence_the_summary_either() {
        // The plugin escape hatch carries a second `events` list inside its
        // own config, and `matches_event` consults it. Clearing only the outer
        // one would leave the same silent drop reachable.
        let cfg = cfg_with(
            r#"
[[notifiers]]
driver = "plugin"
name = "notify-discord"
events = ["given_up"]
[notifiers.config]
webhook = "x"
"#,
        );
        let entries = resolve_notifiers(&cfg);
        assert_eq!(entries[0].driver_name(), "plugin");
        assert!(entries[0].matches_event(SUMMARY_EVENT));
    }

    #[test]
    fn the_plugin_escape_hatch_survives_resolution() {
        // `driver = "plugin"` must reach the dispatcher intact, config and
        // all — dropping it would be a new silent failure in place of the old.
        let cfg = cfg_with(
            r#"
[[notifiers]]
driver = "plugin"
name = "notify-discord"
[notifiers.config]
webhook = "https://discord/x"
"#,
        );
        let entries = resolve_notifiers(&cfg);
        let plugin = entries[0].as_plugin().expect("plugin entry must survive");
        assert_eq!(plugin.name, "notify-discord");
        assert_eq!(plugin.config["webhook"], "https://discord/x");
    }
}
