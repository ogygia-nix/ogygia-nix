//! The `ogygia-nix-eval-fuzz` command; README.md describes its use.

use std::ffi::CString;
use std::ffi::c_char;
use std::ffi::c_int;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::PipeReader;
use std::io::PipeWriter;
use std::io::Write;
use std::num::NonZero;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use clap::Parser;
use libfuzzer_sys::fuzz_crossover;
use libfuzzer_sys::fuzz_mutator;
use libfuzzer_sys::fuzz_target;
use ogygia_nix_eval_fuzz::Budget;
use ogygia_nix_eval_fuzz::Outcome;
use ogygia_nix_eval_fuzz::Summary;

/// Differential fuzzing of ogygia-nix-eval against Nix. Arguments that are
/// not a subcommand are passed to libFuzzer.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Recheck the findings in DIR and minimise its corpus, then fuzz until
    /// stopped.
    Run {
        dir: PathBuf,
        /// Findings to recheck, corpus shards to minimise and fuzzing
        /// processes to run at once; defaults to one per CPU.
        #[arg(long)]
        jobs: Option<NonZero<usize>>,
    },
    /// Check every finding in DIR/findings again, keeping only those that
    /// still diverge. Must not run while DIR is being fuzzed.
    Recheck {
        dir: PathBuf,
        /// Findings to recheck at once; defaults to one per CPU.
        #[arg(long)]
        jobs: Option<NonZero<usize>>,
    },
    /// Shrink DIR/corpus to the inputs that add coverage, keeping those that
    /// crash. Must not run while DIR is being fuzzed.
    Minimise {
        dir: PathBuf,
        /// Corpus shards to minimise at once; defaults to one per CPU.
        #[arg(long)]
        jobs: Option<NonZero<usize>>,
    },
    /// Compare ogygia-nix-eval with Nix on the Nix expression or flake
    /// bundle in FILE.
    Check { file: PathBuf },
}

struct Options {
    findings: Option<PathBuf>,
    stats: Option<PathBuf>,
}

static OPTIONS: OnceLock<Options> = OnceLock::new();

/// The original standard error, once [`quiet`] has pointed descriptor 2 at
/// /dev/null.
static LOG: OnceLock<File> = OnceLock::new();

/// `eprintln!`, to [`LOG`] once it is set.
macro_rules! report {
    ($($arg:tt)*) => {
        match LOG.get() {
            Some(mut log) => writeln!(log, $($arg)*),
            None => writeln!(std::io::stderr(), $($arg)*),
        }
        .expect("writing to standard error")
    };
}

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };
    let options = OPTIONS.get().expect("options are set before fuzzing");
    let outcome = ogygia_nix_eval_fuzz::check(src, Budget::Fuzzing);
    if let Outcome::Diverged(report) = &outcome {
        match &options.findings {
            Some(dir) => {
                ogygia_nix_eval_fuzz::record(dir, src, report).expect("recording a finding")
            }
            None => panic!("ogygia-nix-eval and Nix disagree\n{report}"),
        }
    }
    ogygia_nix_eval_fuzz::count(&outcome, options.stats.as_deref());
});

unsafe extern "C" {
    fn LLVMFuzzerRunDriver(
        argc: *mut c_int,
        argv: *mut *mut *mut c_char,
        callback: extern "C" fn(*const u8, usize) -> c_int,
    ) -> c_int;
}

extern "C" fn test_one_input(data: *const u8, size: usize) -> c_int {
    // SAFETY: libFuzzer passes a buffer of `size` bytes.
    unsafe { libfuzzer_sys::test_input_wrap(data, size) }
}

/// Hand `args`, program name first, to libFuzzer.
fn fuzz(args: Vec<String>) -> ExitCode {
    let flag = |name: &str| {
        args.iter()
            .find_map(|a| a.strip_prefix(name))
            .map(PathBuf::from)
    };
    let options = Options {
        findings: flag("--findings="),
        stats: flag("--stats="),
    };
    if OPTIONS.set(options).is_err() {
        unreachable!("fuzzing starts once");
    }
    let args: Vec<CString> = args
        .into_iter()
        .map(|a| CString::new(a).expect("arguments cannot contain NUL"))
        .collect();
    let mut argv: Vec<*mut c_char> = args
        .iter()
        .map(|a| a.as_ptr().cast_mut())
        .chain([std::ptr::null_mut()])
        .collect();
    let mut argc = c_int::try_from(args.len()).expect("argument count fits in an int");
    let mut argv = argv.as_mut_ptr();
    // SAFETY: argv is a NULL-terminated array of argc C strings, all of
    // which outlive the call.
    let code = unsafe { LLVMFuzzerRunDriver(&mut argc, &mut argv, test_one_input) };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn recheck(findings: &Path, jobs: NonZero<usize>) -> bool {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            report!("locating this program: {e}");
            return false;
        }
    };
    match ogygia_nix_eval_fuzz::recheck(
        findings,
        jobs,
        |src| {
            check_in_child(&exe, src).unwrap_or_else(|e| {
                // Left as it was, as when Nix cannot finish.
                report!("rechecking a finding: {e}");
                Outcome::Skipped
            })
        },
        |line| report!("{line}"),
    ) {
        Ok(r) => {
            report!(
                "rechecked findings: {} kept, {} removed, {} inconclusive",
                r.kept,
                r.removed,
                r.inconclusive
            );
            true
        }
        Err(e) => {
            report!("rechecking {}: {e}", findings.display());
            false
        }
    }
}

/// [`ogygia_nix_eval_fuzz::check`] `src` with no limit on Nix, in a `check`
/// process of its own: the coverage instrumentation this program is built
/// with has every thread of a process update the same counters, so checks
/// on threads of one process slow each other down rather than running in
/// parallel.
fn check_in_child(exe: &Path, src: &str) -> std::io::Result<Outcome> {
    let mut child = std::process::Command::new(exe)
        .args(["check", "/dev/stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    // The child reads all of its input before it writes anything.
    child
        .stdin
        .take()
        .expect("standard input is piped")
        .write_all(src.as_bytes())?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let outcome = if output.status.success() {
        Outcome::from_name(stdout.trim_end())
    } else if stdout.starts_with("step: ") {
        Some(Outcome::Diverged(stdout.into_owned()))
    } else {
        None
    };
    outcome.ok_or_else(|| std::io::Error::other(format!("check {}", output.status)))
}

/// Nix gets 10 seconds, so a timeout means ours is far slower.
const TIMEOUT: &str = "-timeout=60";

/// The seed inputs this build was made with.
const SEEDS: Option<&str> = option_env!("OGYGIA_NIX_EVAL_FUZZ_SEEDS");

/// libFuzzer writes every crashing, slow, timed-out or memory-hungry input
/// here, so none is kept: a panic of ours is a finding, and the rest say
/// nothing about equivalence.
const NO_ARTIFACTS: &str = "-exact_artifact_path=/dev/null";

/// Minimise `dir`'s corpus, the merges reporting their outcome counts to
/// `stats`.
fn minimise(dir: &Path, jobs: NonZero<usize>, stats: &Path) -> bool {
    let corpus = dir.join("corpus");
    let crashed = dir.join("crashed");
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            report!("locating this program: {e}");
            return false;
        }
    };
    let seeds = SEEDS.map(|s| Path::new(s).join("seeds"));
    let findings = format!("--findings={}", dir.join("findings").display());
    let stats = format!("--stats={}", stats.display());
    report!("minimising corpus in {}", corpus.display());
    let start = Instant::now();
    match ogygia_nix_eval_fuzz::minimise(
        &corpus,
        &crashed,
        seeds.as_deref().as_slice(),
        jobs,
        |inputs, kept, control| {
            let status = std::process::Command::new(&exe)
                .args(["-merge=1", TIMEOUT, NO_ARTIFACTS, &findings, &stats])
                .arg(format!("-merge_control_file={}", control.display()))
                .arg(kept)
                .arg(inputs)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "merging {}: {status}",
                    inputs.display()
                )))
            }
        },
    ) {
        Ok(m) => {
            report!(
                "minimised {} inputs in {}s: {} kept in the corpus, {} that crashed set aside in {}",
                m.inputs,
                start.elapsed().as_secs(),
                m.kept,
                m.crashed,
                crashed.display()
            );
            true
        }
        Err(e) => {
            report!("minimising {}: {e}", corpus.display());
            false
        }
    }
}

/// Point descriptor 2 at /dev/null, returning the original standard error.
fn silence_stderr() -> std::io::Result<File> {
    let original = std::io::stderr().as_fd().try_clone_to_owned()?;
    let null = File::options().write(true).open("/dev/null")?;
    // SAFETY: dup2 only replaces descriptor 2, which stays open.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(original.into())
}

/// Send standard error to [`LOG`], and descriptor 2 to /dev/null:
/// libFuzzer reports every job, and evaluation traces and warns with bytes
/// of the input, so only our own lines go to standard error.
fn quiet() -> bool {
    match silence_stderr() {
        Ok(log) => {
            LOG.set(log).expect("standard error is silenced once");
            std::panic::set_hook(Box::new(|info| report!("{info}")));
            true
        }
        Err(e) => {
            eprintln!("silencing standard error: {e}");
            false
        }
    }
}

fn run(dir: &Path, jobs: NonZero<usize>) -> ExitCode {
    let Some(seeds) = SEEDS else {
        eprintln!("this build has no seed inputs; build it with Nix");
        return ExitCode::FAILURE;
    };
    let findings = dir.join("findings");
    let corpus = dir.join("corpus");
    if let Err(e) = std::fs::create_dir_all(&corpus) {
        report!("creating {}: {e}", corpus.display());
        return ExitCode::FAILURE;
    }
    if !quiet() {
        return ExitCode::FAILURE;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            report!("locating this program: {e}");
            return ExitCode::FAILURE;
        }
    };
    // libFuzzer limits the peak memory use of this process, which every
    // process it starts inherits, so the recheck, which can use far more,
    // runs in a process of its own.
    let rechecked = LOG
        .get()
        .expect("standard error is silenced")
        .try_clone()
        .and_then(|log| {
            std::process::Command::new(&exe)
                .arg("recheck")
                .arg(dir)
                .arg(format!("--jobs={jobs}"))
                .stderr(log)
                .status()
        });
    match rechecked {
        Ok(status) if status.success() => {}
        Ok(_) => return ExitCode::FAILURE,
        Err(e) => {
            report!("running the recheck: {e}");
            return ExitCode::FAILURE;
        }
    }
    let summary = match Summary::new(&findings) {
        Ok(summary) => Arc::new(Mutex::new(summary)),
        Err(e) => {
            report!("reading {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    };
    // Every process the run starts inherits the pipe's write end and
    // reports its outcome counts to it.
    let (counts, writer) = match outcome_pipe() {
        Ok(pipe) => pipe,
        Err(e) => {
            report!("creating the outcome pipe: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stats = PathBuf::from(format!("/dev/fd/{}", writer.as_raw_fd()));
    std::thread::spawn({
        let summary = Arc::clone(&summary);
        move || {
            for line in BufReader::new(counts).lines() {
                let line = line.expect("reading outcome counts");
                summary.lock().unwrap().add(&line);
            }
        }
    });
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(60));
            let lines = summary.lock().unwrap().update();
            for line in lines.expect("summarising the run") {
                report!("{line}");
            }
        }
    });
    let minimised = minimise(dir, jobs, &stats);
    if !minimised {
        report!("fuzzing the corpus and seeds as they were");
    }
    report!("fuzzing with {jobs} jobs");
    let path = |p: &Path| p.display().to_string();
    let mut args = vec![
        path(&exe),
        format!("-fork={jobs}"),
        "-ignore_crashes=1".to_owned(),
        "-ignore_timeouts=1".to_owned(),
        "-ignore_ooms=1".to_owned(),
        TIMEOUT.to_owned(),
        NO_ARTIFACTS.to_owned(),
        format!("-dict={seeds}/nix.dict"),
        format!("--findings={}", path(&findings)),
        format!("--stats={}", path(&stats)),
        path(&corpus),
    ];
    if minimised {
        // Fuzz the inputs as they are, rather than first replaying them,
        // one at a time, to learn which add coverage: the minimisation did
        // that in parallel, set aside those that crash, and kept every seed
        // that adds coverage.
        args.push("-keep_seed=1".to_owned());
    } else {
        args.push(format!("{seeds}/seeds"));
    }
    fuzz(args)
}

/// A pipe whose write end, unlike the read end, is inherited by the
/// processes this one starts.
fn outcome_pipe() -> std::io::Result<(PipeReader, PipeWriter)> {
    let (reader, writer) = std::io::pipe()?;
    // SAFETY: F_SETFD only changes the flags of a descriptor we own.
    if unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((reader, writer))
}

/// `jobs`, defaulting to one per CPU.
fn or_cpus(jobs: Option<NonZero<usize>>) -> NonZero<usize> {
    jobs.unwrap_or_else(|| std::thread::available_parallelism().unwrap_or(NonZero::<usize>::MIN))
}

fn check(file: &Path) -> ExitCode {
    let src = match std::fs::read(file).map(String::from_utf8) {
        Ok(Ok(src)) => src,
        Ok(Err(_)) => {
            eprintln!("{} is not UTF-8", file.display());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("reading {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    match ogygia_nix_eval_fuzz::check(&src, Budget::Unlimited) {
        Outcome::Diverged(report) => {
            print!("{report}");
            ExitCode::FAILURE
        }
        outcome => {
            println!("{outcome}");
            if let Outcome::DeliberateDivergence(skip) = outcome {
                eprint!("{}", skip.reason());
            }
            ExitCode::SUCCESS
        }
    }
}

fn main() -> ExitCode {
    // SAFETY: no other thread has started yet.
    unsafe { ogygia_nix_eval_fuzz::block_network() };
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let subcommand = args.get(1).map(String::as_str);
    // Everything else may fuzz, or start processes that do.
    if !matches!(
        subcommand,
        Some("recheck" | "check" | "help" | "-h" | "--help")
    ) && let Err(e) = ogygia_nix_eval_fuzz::limit_nix_memory()
    {
        eprintln!("limiting Nix's memory: {e}");
        return ExitCode::FAILURE;
    }
    if !matches!(
        subcommand,
        Some("run" | "recheck" | "minimise" | "check" | "help" | "-h" | "--help")
    ) {
        return fuzz(args);
    }
    match Cli::parse().command {
        Command::Run { dir, jobs } => run(&dir, or_cpus(jobs)),
        Command::Recheck { dir, jobs } => {
            if quiet() && recheck(&dir.join("findings"), or_cpus(jobs)) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Command::Minimise { dir, jobs } => {
            if minimise(&dir, or_cpus(jobs), Path::new("/dev/null")) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Command::Check { file } => check(&file),
    }
}

fuzz_mutator!(|data: &mut [u8], size: usize, max_size: usize, seed: u32| {
    ogygia_nix_eval_fuzz::mutate(data, size, max_size, seed, libfuzzer_sys::fuzzer_mutate)
});

fuzz_crossover!(|data1: &[u8], data2: &[u8], out: &mut [u8], seed: u32| {
    ogygia_nix_eval_fuzz::crossover(data1, data2, out, seed)
});
