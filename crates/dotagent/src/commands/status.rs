//! Textual health dashboard.
//!
//! Read-only: never dispatches anything, never writes to audit. Substitutes
//! `agent-orchestrator --status` from the legacy Fish framework.

use anyhow::Result;
use chrono::{Local, TimeZone};
use dotagent_core::{AgentManifest, Heartbeat, Schedule};
use dotagent_scheduler::{health_state, HealthState, ResolvedPolicy};
use dotagent_state::{slug_from_args, StateStore};

use crate::discovery::{self, DiscoveredAgent};

struct Row {
    agent: String,
    schedule: String,
    state: HealthState,
    last_run: String,
    reason: String,
}

pub async fn run() -> Result<()> {
    let agents = discovery::discover_all()?;
    if agents.is_empty() {
        println!("no agents discovered");
        return Ok(());
    }
    let state = StateStore::from_home()?;
    let now = Local::now();

    let mut rows: Vec<Row> = Vec::new();
    for agent in &agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        for sched in &agent.manifest.schedules {
            rows.push(compute_row(agent, sched, &state, now));
        }
    }

    print_dashboard(&rows, now);
    Ok(())
}

fn compute_row(
    agent: &DiscoveredAgent,
    sched: &Schedule,
    state: &StateStore,
    now: chrono::DateTime<Local>,
) -> Row {
    let policy = ResolvedPolicy::resolve(&agent.manifest, sched);
    let hb = read_hb(&agent.manifest, sched, state);
    let slug = slug_from_args(sched.args());

    let last_run = hb
        .as_ref()
        .and_then(|h| h.finished_at_iso.clone())
        .unwrap_or_else(|| "never".into());

    let expected = hb
        .as_ref()
        .and_then(|h| h.last_success_at)
        .and_then(|s| Local.timestamp_opt(s, 0).single());

    let window = expected.and_then(|exp| {
        state
            .read_window(&agent.manifest.agent.name, &slug, exp)
            .ok()
            .flatten()
    });
    let (state_val, reason) = health_state(sched, &policy, hb.as_ref(), window.as_ref(), now);

    Row {
        agent: agent.manifest.agent.name.clone(),
        schedule: sched.id().to_string(),
        state: state_val,
        last_run,
        reason,
    }
}

fn read_hb(manifest: &AgentManifest, sched: &Schedule, state: &StateStore) -> Option<Heartbeat> {
    let slug = slug_from_args(sched.args());
    state
        .read_heartbeat(&manifest.agent.name, &slug)
        .ok()
        .flatten()
}

fn print_dashboard(rows: &[Row], now: chrono::DateTime<Local>) {
    let mut ok = 0;
    let mut deg = 0;
    let mut fail = 0;
    let mut stale = 0;
    for r in rows {
        match r.state {
            HealthState::Ok => ok += 1,
            HealthState::Degraded => deg += 1,
            HealthState::Failing => fail += 1,
            HealthState::Stale => stale += 1,
        }
    }
    let total = ok + deg + fail + stale;
    println!();
    println!("═══ Agent Health · {} ═══", now.format("%Y-%m-%d %H:%M"));
    println!();
    println!("  ✅ ok       {ok}/{total}");
    println!("  ⚠️  degraded {deg}");
    println!("  ❌ failing  {fail}");
    println!("  🕑 stale    {stale}");
    println!();
    println!(
        "{:<36} {:<11} {:<26} REASON",
        "AGENT/SCHEDULE", "STATE", "LAST RUN"
    );
    let sep = "─".repeat(100);
    println!("{sep}");

    // Order: failing → degraded → stale → ok (most-urgent-first).
    let order = [
        HealthState::Failing,
        HealthState::Degraded,
        HealthState::Stale,
        HealthState::Ok,
    ];
    for state in order {
        for row in rows.iter().filter(|r| r.state == state) {
            let icon = match row.state {
                HealthState::Ok => "✅ ok      ",
                HealthState::Degraded => "⚠️  degraded",
                HealthState::Failing => "❌ failing ",
                HealthState::Stale => "🕑 stale   ",
            };
            println!(
                "{:<36} {}  {:<26} {}",
                format!("{}/{}", row.agent, row.schedule),
                icon,
                row.last_run,
                row.reason
            );
        }
    }
    println!();
    let home = dotagent_state::paths::home();
    println!("Logs:    {}/logs/", home.display());
    println!("State:   {}/state/agents/", home.display());
    println!("Audit:   {}/audit.log", home.display());
}
