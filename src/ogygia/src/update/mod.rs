//! Client for the ogygia-updated daemon: manual updates and canaries.

use std::env;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use chrono::Utc;
use clap::Args;
use clap::Subcommand;
use clap_complete::engine::ArgValueCompleter;
use clap_complete::engine::CompletionCandidate;
use ogygia_updated::control;
use ogygia_updated::control::Reply;
use ogygia_updated::control::Request;

use self::render::Format;
use self::render::Renderer;

mod render;

/// Environment override for the daemon configuration the branch completer
/// reads, mirroring `OGYGIA_CONFIG` for the CLI's own config.
const CONFIG_OVERRIDE_ENV: &str = "OGYGIA_UPDATED_CONFIG";

/// Drive the ogygia-updated daemon (root only). With no subcommand, reset
/// the host to the tracked branch tip now.
#[derive(Args)]
pub struct UpdateArgs {
    /// Control socket of the ogygia-updated daemon
    #[arg(long, default_value = ogygia_updated::control::DEFAULT_SOCKET_PATH)]
    socket: PathBuf,

    /// How to show progress: nom if it is installed and stderr is a
    /// terminal, otherwise plain lines; json prints the daemon's frames
    #[arg(long, value_enum, default_value = "auto", global = true)]
    format: Format,

    #[command(subcommand)]
    command: Option<UpdateCommand>,
}

#[derive(Subcommand)]
enum UpdateCommand {
    /// Run one update cycle now and wait for it to finish, holding any
    /// active canary. Exits non-zero if the cycle failed.
    Tick,
    /// Trial a branch on this host, or report the active trial
    Canary(CanaryArgs),
}

/// `canary <branch>` starts a trial; `canary status` reports one.
#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct CanaryArgs {
    /// Branch to trial; its tip is pinned onto this host now
    #[arg(add = ArgValueCompleter::new(complete_branch))]
    branch: Option<String>,

    /// How long to hold the trial before reverting (e.g. 24h, 90m, 2d)
    #[arg(
        long = "for",
        value_name = "DURATION",
        default_value = "24h",
        conflicts_with = "forever"
    )]
    duration: humantime::Duration,

    /// Hold the trial with no timeout
    #[arg(long)]
    forever: bool,

    /// Make the trial the boot default, so a reboot keeps it instead of
    /// falling back to the tracked branch. Needed to trial a new kernel or
    /// new boot flags; recovery is by hand from the bootloader menu.
    #[arg(long)]
    persist: bool,

    #[command(subcommand)]
    command: Option<CanaryCommand>,
}

#[derive(Subcommand)]
enum CanaryCommand {
    /// Show the current or most-recent canary
    Status,
}

/// Branches the daemon could pin onto this host, for shell completion.
///
/// Reads the daemon's configuration for its remote rather than asking the
/// daemon itself, so completion works whatever state the service is in. Any
/// failure yields no candidates: this writes to the shell's completion
/// stream, where an error message would be offered as a branch name.
fn complete_branch(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };

    let path = env::var_os(CONFIG_OVERRIDE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(ogygia_updated::DEFAULT_CONFIG_PATH));
    let Ok(config) = ogygia_updated::Config::load(&path) else {
        return Vec::new();
    };

    ogygia_updated::branches::list(&config)
        .into_iter()
        .filter(|branch| branch.starts_with(prefix))
        .map(CompletionCandidate::new)
        .collect()
}

impl UpdateArgs {
    pub fn run(&self) -> Result<()> {
        let request = match &self.command {
            None => Request::Update,
            Some(UpdateCommand::Tick) => Request::Tick,
            Some(UpdateCommand::Canary(canary)) => canary.request()?,
        };

        let mut renderer = Renderer::new(self.format);
        let reply = control::send(&self.socket, &request, |frame| renderer.frame(frame));
        renderer.finish();
        let reply = reply?;

        // With --format json stdout carries only the frames, which already
        // include the reply.
        if self.format != Format::Json {
            println!("{}", render::reply(&reply, Utc::now()));
        }
        // A cycle that wanted to advance and could not has not served the
        // request, so a caller gating on being current sees a failure.
        if let Reply::Cycle { outcome } = &reply
            && !outcome.served()
        {
            bail!(render::outcome(outcome));
        }
        Ok(())
    }
}

impl CanaryArgs {
    fn request(&self) -> Result<Request> {
        match &self.command {
            Some(CanaryCommand::Status) => Ok(Request::CanaryStatus),
            None => {
                let branch = self
                    .branch
                    .clone()
                    .context("a branch to trial is required (or use `canary status`)")?;
                let timeout = (!self.forever).then(|| Duration::from(self.duration).as_secs());
                Ok(Request::Canary {
                    branch,
                    timeout,
                    persist: self.persist,
                })
            }
        }
    }
}
