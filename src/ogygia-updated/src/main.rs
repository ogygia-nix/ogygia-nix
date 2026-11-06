use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use chrono::Utc;
use clap::Parser;
use ogygia_updated::CanaryState;
use ogygia_updated::Config;
use ogygia_updated::Outcome;
use ogygia_updated::Trigger;
use ogygia_updated::control;
use ogygia_updated::control::Events;
use ogygia_updated::control::Failure;
use ogygia_updated::control::Reply;
use ogygia_updated::control::Request;
use rand::RngExt;
use tracing::error;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Ogygia automatic update daemon.
#[derive(Parser)]
#[command(
    name = "ogygia-updated",
    version,
    about = "Ogygia automatic update daemon"
)]
struct Args {
    /// Path to the TOML configuration file
    #[arg(long)]
    config: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ogygia_updated=info,warn")),
        )
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;

    let listener = control::bind(&config.daemon.socket_path)?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || control::listen(listener, sender));

    // Wait out the initial delay or the interval between cycles, but run
    // immediately when a command arrives — and never sleep past an active
    // canary's deadline, so it is reverted on time.
    let mut wait = wait_duration(
        &config,
        Duration::from_secs(config.daemon.initial_delay_seconds),
    );
    loop {
        let received = match receiver.recv_timeout(wait) {
            Ok(pair) => Some(pair),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => bail!("control socket listener exited"),
        };

        let (trigger, events) = match received {
            None => (Trigger::Scheduled, Events::none()),
            Some((request, events)) => match request {
                // A status query is answered from persisted state, without
                // running a cycle.
                Request::CanaryStatus => {
                    events.finish(canary_status(&config).map_err(|error| Failure::from(&error)));
                    wait = wait_duration(&config, interval(&config));
                    continue;
                }
                Request::Update => {
                    info!("manual update requested over the control socket");
                    (Trigger::Manual, events)
                }
                Request::Tick => {
                    info!("tick requested over the control socket");
                    (Trigger::Scheduled, events)
                }
                Request::Canary {
                    branch,
                    timeout,
                    persist,
                } => {
                    info!(
                        branch,
                        ?timeout,
                        persist,
                        "canary requested over the control socket"
                    );
                    (
                        Trigger::StartCanary {
                            branch,
                            timeout: timeout.map(Duration::from_secs),
                            persist,
                        },
                        events,
                    )
                }
            },
        };

        let result = ogygia_updated::run_once(&config, trigger, &events);
        match &result {
            Ok(outcome) => info!(?outcome, "update cycle finished"),
            Err(error) => error!(message = format!("{error:#}"), "update cycle failed"),
        }
        // Whether a cycle that could not advance counts as a failure is the
        // client's call (`Outcome::served`); the daemon's own interval has
        // no client and simply retries on the next cycle.
        events.finish(match &result {
            Ok(outcome) => Ok(Reply::Cycle {
                outcome: outcome.clone(),
            }),
            Err(error) => Err(Failure::from(error)),
        });

        // Activating a configuration may have replaced this unit's
        // definition, which switch-to-configuration never restarts
        // (restartIfChanged is off so it cannot kill its own parent
        // mid-cycle); exit so systemd brings the daemon back under the
        // new definition.
        if result.as_ref().is_ok_and(Outcome::activated) {
            info!("activated a new configuration; exiting to adopt its unit definition");
            return Ok(());
        }

        wait = wait_duration(&config, interval(&config));
    }
}

/// The base gap between scheduled cycles, with jitter applied.
fn interval(config: &Config) -> Duration {
    let jitter = match config.daemon.jitter_seconds {
        0 => 0,
        bound => rand::rng().random_range(0..bound),
    };
    Duration::from_secs(config.daemon.interval_seconds + jitter)
}

/// Clamp `base` to an active canary's remaining time so the daemon wakes
/// to revert it on schedule. A lapsed deadline yields a zero wait, running
/// the reverting cycle at once.
fn wait_duration(config: &Config, base: Duration) -> Duration {
    if let Ok(Some(CanaryState::Active {
        expires_at: Some(deadline),
        ..
    })) = CanaryState::load(&config.canary_state_path())
    {
        let remaining = (deadline - Utc::now()).to_std().unwrap_or(Duration::ZERO);
        base.min(remaining)
    } else {
        base
    }
}

/// The current or most-recent canary, for `canary status`.
fn canary_status(config: &Config) -> Result<Reply> {
    Ok(Reply::CanaryStatus {
        state: CanaryState::load(&config.canary_state_path())?,
        running: read_revision(&config.host.current_revision_path),
        next_boot: read_revision(&config.next_boot_revision_path()),
    })
}

/// Best-effort read of a build-revision file for display.
fn read_revision(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|raw| raw.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}
