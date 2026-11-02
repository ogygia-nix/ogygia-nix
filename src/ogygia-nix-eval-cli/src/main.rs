//! The `ogygia-nix-eval` command, which takes a subset of `nix`'s arguments
//! and prints what `nix` would.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use ogygia_nix_eval::Fetcher;
use ogygia_nix_eval::FlakeEnv;
use ogygia_nix_eval::Settings;

mod stats;

#[global_allocator]
static ALLOC: stats::Counting = stats::Counting;

/// Evaluate with ogygia-nix-eval as `nix` does. Flags `nix` has but this
/// does not are refused.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Evaluate INSTALLABLE and print it as JSON, like `nix eval --json`.
    Eval {
        installable: String,
        /// Print the value as JSON, the only output this supports.
        #[arg(long, required = true)]
        json: bool,
        /// A function to apply to the value before printing it.
        #[arg(long, value_name = "EXPR")]
        apply: Option<String>,
        /// Fail rather than change a lock file that is out of date. Lock
        /// files are never written, so this is the behaviour without it too.
        #[arg(long)]
        no_update_lock_file: bool,
        /// The store to read inputs from: `local?store=DIR`, or
        /// `dummy://?read-only=false` to fetch every input into memory.
        /// Defaults to `/nix/store`.
        #[arg(long, value_name = "URL", value_parser = store)]
        store: Option<Store>,
    },
}

/// A store directory, or `None` for a store in memory.
#[derive(Clone)]
struct Store(Option<PathBuf>);

fn store(url: &str) -> Result<Store, String> {
    if url == "dummy://?read-only=false" {
        return Ok(Store(None));
    }
    match url.strip_prefix("local?store=") {
        Some(dir) if !dir.contains('&') => Ok(Store(Some(PathBuf::from(dir)))),
        _ => Err("unsupported store".to_owned()),
    }
}

/// Downloads over HTTP.
#[derive(Default)]
struct Http(std::sync::OnceLock<reqwest::blocking::Client>);

impl Fetcher for Http {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let response = self
            .0
            .get_or_init(reqwest::blocking::Client::new)
            .get(url)
            .send()?
            .error_for_status()?;
        Ok(response.bytes()?.to_vec())
    }
}

/// Prints the statistics if `NIX_SHOW_STATS` is set and not `0`: to the
/// file `NIX_SHOW_STATS_PATH` names if it is set and not `-`, otherwise to
/// stderr.
fn show_stats(instructions: Option<&stats::Instructions>) {
    if *stats::ENABLED {
        let stats = stats::json(instructions).to_string();
        match std::env::var("NIX_SHOW_STATS_PATH") {
            Ok(path) if path != "-" => {
                std::fs::write(&path, stats).expect("writing NIX_SHOW_STATS_PATH")
            }
            _ => eprintln!("{stats}"),
        }
    }
}

fn main() -> ExitCode {
    let instructions = stats::ENABLED.then(stats::Instructions::start).flatten();
    let Command::Eval {
        installable,
        apply,
        store,
        ..
    } = Cli::parse().command;
    let env = FlakeEnv {
        cwd: std::env::current_dir().expect("current directory"),
        store: store.map_or_else(|| Some(PathBuf::from("/nix/store")), |s| s.0),
        fetcher: Some(Box::<Http>::default()),
    };
    let apply = apply.as_deref().unwrap_or("x: x");
    let code =
        match ogygia_nix_eval::eval_flake_json(&installable, apply, &env, Settings::default()) {
            Ok(s) => {
                println!("{s}");
                ExitCode::SUCCESS
            }
            Err(ogygia_nix_eval::Error::Eval(d)) => {
                eprintln!("{}", d.render(std::io::stderr().is_terminal()));
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    show_stats(instructions.as_ref());
    code
}
