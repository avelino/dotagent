//! dotagent CLI entry point.
//!
//! Subcommands map to thin wrappers around the supporting crates. The actual
//! orchestration logic for `tick`, `status`, `daily-summary`, `run`, etc.
//! lives in `commands/`.

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

mod commands;
mod discovery;

#[derive(Parser, Debug)]
#[command(name = "dotagent", about = "Polyglot agent orchestrator.", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run an agent (invoked by launchd/systemd).
    Run {
        /// Agent name (matches the manifest's `agent.name`).
        name: String,
        /// Schedule id (matches one of the manifest's `[[schedules]].id`).
        #[arg(long)]
        schedule: String,
        /// Dry run — do not emit side effects, do not write heartbeat.
        #[arg(long)]
        dry_run: bool,
    },

    /// Detect missed windows, run retries, fire notifications (one-shot).
    Tick {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        verbose: bool,
    },

    /// Run the long-lived daemon (adaptive scheduler). Invoked by launchd /
    /// systemd from the unit installed by `dotagent install`.
    Daemon,

    /// Print a textual health dashboard.
    Status,

    /// Send the daily summary via the configured notification plugin.
    DailySummary {
        #[arg(long)]
        dry_run: bool,
    },

    /// Mark every schedule's current window as ok (one-shot, after initial install).
    Bootstrap,

    /// Generate and install launchd plists / systemd unit files.
    Install {
        /// Install all agents in the search root. Mutually exclusive with `name`.
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Install a single agent by name.
        name: Option<String>,
    },

    /// Remove launchd plists / systemd unit files.
    Uninstall {
        #[arg(long, conflicts_with = "name")]
        all: bool,
        name: Option<String>,
    },

    /// Validate manifests + discover plugins + check platform deps.
    Doctor,

    /// Plugin helpers.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },

    /// Tail the daemon-captured logs for an agent.
    ///
    /// Omit `name` to tail logs from every agent at once (each line is
    /// prefixed by `tail` with the originating file).
    Logs {
        name: Option<String>,
        #[arg(long)]
        schedule: Option<String>,
        /// Number of trailing lines to print (default 50).
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Follow new output (`tail -F`). Survives log rotation.
        #[arg(short = 'f', long)]
        follow: bool,
    },

    /// Print heartbeat, window state, manifest hash for an agent.
    Inspect { name: String },

    /// Send SIGHUP to the running daemon (reload manifests + plugins).
    Reload,

    /// Force-run an agent now, ignoring schedule windows.
    RunNow {
        name: String,
        #[arg(long)]
        schedule: Option<String>,
    },

    /// Print a shell completion script.
    ///
    /// Includes dynamic completion of agent names (via `dotagent _list-agents`)
    /// for subcommands that take an agent name (`run`, `inspect`, `run-now`,
    /// `logs`, `install`, `uninstall`).
    ///
    /// Examples:
    ///   dotagent completions fish | source
    ///   dotagent completions zsh  > ~/.zfunc/_dotagent
    ///   dotagent completions bash > ~/.local/share/bash-completion/completions/dotagent
    Completions {
        /// Target shell (bash, zsh, fish, elvish, powershell).
        shell: Shell,
    },

    /// (internal) Print discovered agent names, one per line. Used by the
    /// completion scripts emitted by `dotagent completions`.
    #[command(name = "_list-agents", hide = true)]
    ListAgents,
}

#[derive(Subcommand, Debug)]
enum PluginCommand {
    /// List discovered plugins.
    List,
    /// Invoke a plugin manually (debug).
    Invoke {
        name: String,
        /// JSON payload on stdin if `-`, else literal value.
        payload: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Telemetry is opt-in by subcommand: the daemon initializes structured
    // logging (JSON file + stderr + optional OTel). Other subcommands
    // (`run`, `status`, etc.) use a lightweight stderr subscriber so they
    // don't fight over the daemon's log files.
    let cli = Cli::parse();
    let _telemetry_guard = match &cli.command {
        Command::Daemon => Some(
            dotagent_telemetry::init_from_default_config()
                .map_err(|e| anyhow::anyhow!("telemetry init failed: {e}"))?,
        ),
        _ => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .with_writer(std::io::stderr)
                .init();
            None
        }
    };

    match cli.command {
        Command::Run {
            name,
            schedule,
            dry_run,
        } => commands::run(name, schedule, dry_run).await,
        Command::Tick { dry_run, verbose } => commands::tick(dry_run, verbose).await,
        Command::Daemon => commands::daemon_cmd().await,
        Command::Status => commands::status().await,
        Command::DailySummary { dry_run } => commands::daily_summary(dry_run).await,
        Command::Bootstrap => commands::bootstrap().await,
        Command::Install { all, name } => commands::install(all, name).await,
        Command::Uninstall { all, name } => commands::uninstall(all, name).await,
        Command::Doctor => commands::doctor().await,
        Command::Plugin { command } => match command {
            PluginCommand::List => commands::plugin_list().await,
            PluginCommand::Invoke { name, payload } => commands::plugin_invoke(name, payload).await,
        },
        Command::Logs {
            name,
            schedule,
            lines,
            follow,
        } => commands::utility::logs(name, schedule, lines, follow).await,
        Command::Inspect { name } => commands::utility::inspect(name).await,
        Command::Reload => commands::utility::reload().await,
        Command::RunNow { name, schedule } => commands::utility::run_now(name, schedule).await,
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            commands::completions::print(shell, &mut cmd);
            Ok(())
        }
        Command::ListAgents => commands::list_agents::run(),
    }
}
