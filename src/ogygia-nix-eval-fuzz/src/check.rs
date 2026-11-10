//! Comparing ogygia-nix-eval with `nix-instantiate` on one input.

use std::io::Read;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use ogygia_nix_eval::Context;
use ogygia_nix_eval::Error;
use ogygia_nix_eval::Io;
use ogygia_nix_eval::Settings;
use ogygia_nix_eval::run_with_stack;
use strum::EnumCount;
use strum::EnumDiscriminants;
use strum::EnumIter;
use strum::EnumString;
use strum::IntoStaticStr;

use crate::cgroup::Cgroup;
use crate::skip;
use crate::skip::Evidence;
use crate::skip::Skip;

const DEEP_COPY: &str = "let dc = v: if builtins.isAttrs v then builtins.mapAttrs (_: dc) v \
                         else if builtins.isList v then map dc v else v; in dc";

/// How long Nix may spend on one expression of a new input.
const NIX_TIMEOUT: Duration = Duration::from_secs(10);

/// Memory Nix may use on a new input.
const NIX_MEMORY: u64 = 1 << 30;

/// What Nix may use on an input before [`check`] skips it.
#[derive(Clone, Copy)]
pub enum Budget {
    /// 10 seconds per expression and 1 GiB of memory, for new inputs while
    /// fuzzing. Nix runs in a cgroup of its own, which needs this process
    /// [set up](crate::limit_nix_memory).
    Fuzzing,
    /// No limit, for findings: each already finished within
    /// [`Budget::Fuzzing`] once, so a limit would only make the result
    /// depend on how busy the machine is.
    Unlimited,
}

/// What one Nix process may use.
struct Limit {
    time: Duration,
    memory: u64,
}

impl Budget {
    fn limit(self) -> Option<Limit> {
        match self {
            Budget::Fuzzing => Some(Limit {
                time: NIX_TIMEOUT,
                memory: NIX_MEMORY,
            }),
            Budget::Unlimited => None,
        }
    }
}

const DEAD_PROXY: &str = "http://127.0.0.1:9";

const PROXY_VARS: &[&str] = &[
    "http_proxy",
    "https_proxy",
    "ftp_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "FTP_PROXY",
    "ALL_PROXY",
];

// `nix-instantiate --eval` only writes what it adds to the store, such as
// `toFile` results and `.drv` files, in read-write mode; without that, or
// with a `dummy://` store, reading them back fails where ogygia-nix-eval
// succeeds.
const NIX_ARGS: &[&str] = &["--read-write-mode", "--option", "pure-eval", "true"];

/// Point this process, and the Nix processes it starts, at a proxy that
/// refuses connections, so fetches fail without reaching the network.
///
/// # Safety
///
/// Must be called before any other thread exists.
pub unsafe fn block_network() {
    for var in PROXY_VARS {
        // SAFETY: the caller guarantees no other thread exists.
        unsafe { std::env::set_var(var, DEAD_PROXY) };
    }
    for var in ["no_proxy", "NO_PROXY"] {
        // SAFETY: as above.
        unsafe { std::env::remove_var(var) };
    }
}

/// What [`check`] made of an input.
#[derive(EnumDiscriminants)]
#[strum_discriminants(
    name(Kind),
    vis(pub(crate)),
    derive(EnumCount, EnumIter, EnumString, IntoStaticStr),
    strum(serialize_all = "kebab-case")
)]
pub enum Outcome {
    /// Both sides reject it.
    ParseRejected,
    /// Nix ran out of time, memory or stack.
    Skipped,
    /// It meets a difference from Nix that ogygia-nix-eval keeps on purpose,
    /// as the [`Skip`] describes, so the two are not compared further.
    DeliberateDivergence(Skip),
    /// Both sides evaluate it to the same value.
    #[strum_discriminants(strum(to_string = "values"))]
    Value,
    /// Both sides fail, and `builtins.tryEval` catches it on both.
    Caught,
    /// Both sides fail, and `builtins.tryEval` catches it on neither.
    Uncaught,
    /// The two disagree, or ogygia-nix-eval panicked; holds a report of the
    /// step that diverged, the input, and both sides' full output, or of
    /// the panic.
    Diverged(String),
}

impl Outcome {
    /// The outcome whose name, as [`Outcome`]'s `Display` gives it, is
    /// `name`, other than [`Outcome::Diverged`], which holds a report.
    pub fn from_name(name: &str) -> Option<Outcome> {
        let (kind, skip) = match name.split_once(' ') {
            Some((kind, skip)) => (kind, Some(skip.parse().ok()?)),
            None => (name, None),
        };
        Some(match (kind.parse().ok()?, skip) {
            (Kind::ParseRejected, None) => Outcome::ParseRejected,
            (Kind::Skipped, None) => Outcome::Skipped,
            (Kind::DeliberateDivergence, Some(skip)) => Outcome::DeliberateDivergence(skip),
            (Kind::Value, None) => Outcome::Value,
            (Kind::Caught, None) => Outcome::Caught,
            (Kind::Uncaught, None) => Outcome::Uncaught,
            _ => return None,
        })
    }
}

/// The outcome's kebab-case name, followed by the [`Skip`] of a deliberate
/// divergence.
impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(Kind::from(self).into())?;
        if let Outcome::DeliberateDivergence(skip) = self {
            write!(f, " {skip}")?;
        }
        Ok(())
    }
}

/// Standard output on success, standard error on failure.
pub(crate) type Run = Result<String, String>;

/// What Nix did with one expression.
pub(crate) struct Theirs {
    pub(crate) run: Run,
    /// The store it ran with, if it ran.
    pub(crate) store: Option<Store>,
}

impl From<Run> for Theirs {
    fn from(run: Run) -> Theirs {
        Theirs { run, store: None }
    }
}

pub(crate) fn scratch() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("ogygia-nix-eval-fuzz");
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    })
}

/// An empty chroot store for one Nix run, deleted on drop. Its store
/// directory is still `/nix/store`, so store paths match ogygia-nix-eval's.
pub(crate) struct Store(tempfile::TempDir);

impl Store {
    pub(crate) fn new() -> Store {
        Store(
            tempfile::Builder::new()
                .prefix("store-")
                .tempdir_in(scratch())
                .expect("create store dir"),
        )
    }

    fn uri(&self) -> String {
        format!("local?root={}", self.0.path().display())
    }

    /// Where the store directory, `/nix/store`, is on disk.
    pub(crate) fn dir(&self) -> PathBuf {
        self.0.path().join("nix/store")
    }

    /// Whether Nix added any path to the store.
    pub(crate) fn stored(&self) -> bool {
        std::fs::read_dir(self.dir()).is_ok_and(|mut entries| {
            entries.any(|entry| {
                let name = entry.expect("read store dir").file_name();
                !name.as_encoded_bytes().starts_with(b".")
            })
        })
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // Nix makes store paths read-only, which would stop the TempDir
        // removing them once this returns.
        make_writable(self.0.path());
    }
}

fn make_writable(path: &Path) {
    let meta = std::fs::symlink_metadata(path).expect("stat store path");
    if !meta.is_dir() {
        return;
    }
    let mut perms = meta.permissions();
    perms.set_mode(perms.mode() | 0o700);
    std::fs::set_permissions(path, perms).expect("make store path writable");
    for entry in std::fs::read_dir(path).expect("read store dir") {
        make_writable(&entry.expect("read store dir").path());
    }
}

pub(crate) fn nix_instantiate() -> String {
    std::env::var("OGYGIA_NIX_EVAL_NIX_INSTANTIATE")
        .ok()
        .or_else(|| option_env!("OGYGIA_NIX_INSTANTIATE_BIN").map(str::to_owned))
        .unwrap_or_else(|| "nix-instantiate".to_owned())
}

fn read_all(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).expect("reading Nix's output");
        buf
    })
}

/// `program`, one of the reference Nix's commands, set up to use `store` and
/// nothing of the environment it runs in.
pub(crate) fn nix_command(program: &str, store: &Store) -> Command {
    let s = scratch();
    let mut cmd = Command::new(program);
    cmd.arg("--store")
        .arg(store.uri())
        .current_dir("/")
        .env_clear()
        .env("HOME", s)
        .env("NIX_STATE_DIR", s.join("state"))
        .env("NIX_CONF_DIR", s.join("conf"))
        .env("NIX_LOG_DIR", s.join("log"))
        .env("XDG_CACHE_HOME", s.join("cache"))
        .envs(PROXY_VARS.iter().map(|v| (v, DEAD_PROXY)));
    cmd
}

/// What a Nix command did.
pub(crate) struct Ran {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run `cmd`, made by [`nix_command`], within `budget`, writing
/// `input` to its standard input. Fails with [`Outcome::Skipped`] or a
/// deliberate divergence when what Nix did says nothing about equivalence.
pub(crate) fn run_command(cmd: Command, input: &[u8], budget: Budget) -> Result<Ran, Outcome> {
    run_within(cmd, input, budget.limit())
}

/// [`run_command`] within `limit`, if any.
fn run_within(mut cmd: Command, input: &[u8], limit: Option<Limit>) -> Result<Ran, Outcome> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let cgroup = limit.as_ref().map(|limit| {
        let cgroup = Cgroup::new(limit.memory);
        cgroup.place(&mut cmd);
        cgroup
    });
    let mut child = cmd.spawn().expect("running Nix");
    let stdout = read_all(child.stdout.take().unwrap());
    let stderr = read_all(child.stderr.take().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let stdin = std::thread::spawn(move || {
        // Nix stops reading early when it is killed.
        if let Err(e) = stdin.write_all(&input)
            && e.kind() != std::io::ErrorKind::BrokenPipe
        {
            panic!("writing Nix's input: {e}");
        }
    });
    let deadline = limit.map(|limit| Instant::now() + limit.time);
    let status = loop {
        if let Some(status) = child.try_wait().expect("waiting for Nix") {
            break Ok(status);
        }
        if deadline.is_some_and(|d| Instant::now() > d) {
            child.kill().expect("killing Nix");
            child.wait().expect("waiting for Nix");
            break Err(Outcome::Skipped);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out_of_memory = cgroup.as_ref().is_some_and(Cgroup::exhausted);
    // Killing whatever Nix left running closes its output.
    drop(cgroup);
    stdin.join().unwrap();
    let stdout = String::from_utf8_lossy(&stdout.join().unwrap()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr.join().unwrap()).into_owned();
    let status = status?;
    if out_of_memory || exhausted(&stderr) {
        return Err(Outcome::Skipped);
    }
    if let Some(skip) = Skip::find(&Evidence::NixStderr(&stderr)) {
        return Err(Outcome::DeliberateDivergence(skip));
    }
    Ok(Ran {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// Fails with [`Outcome::Skipped`] or a deliberate divergence when what
/// Nix did says nothing about equivalence.
pub(crate) fn run_nix(mode: &[&str], expr: &str, budget: Budget) -> Result<Theirs, Outcome> {
    let store = Store::new();
    let mut cmd = nix_command(&nix_instantiate(), &store);
    // On standard input, the expression cannot be mistaken for a flag and
    // may contain NUL bytes.
    cmd.args(mode).args(NIX_ARGS).arg("-");
    let ran = run_command(cmd, expr.as_bytes(), budget)?;
    Ok(Theirs {
        run: if ran.success {
            Ok(ran
                .stdout
                .strip_suffix('\n')
                .unwrap_or(&ran.stdout)
                .to_owned())
        } else {
            Err(ran.stderr)
        },
        store: Some(store),
    })
}

/// Whether Nix's standard error says it ran out of memory or stack.
pub(crate) fn exhausted(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    [
        "out of memory",
        "bad_alloc",
        "stack overflow",
        "max-call-depth",
    ]
    .iter()
    .any(|e| stderr.contains(e))
}

pub(crate) fn settings() -> Settings {
    Settings {
        disabled: skip::disabled(),
        ..Settings::default()
    }
}

pub(crate) fn show(r: &Run) -> String {
    match r {
        Ok(s) => s.clone(),
        Err(e) => format!("error: {}", e.trim_end()),
    }
}

fn diverged(step: &str, src: &str, expr: &str, theirs: &Run, ours: &Run) -> Outcome {
    Outcome::Diverged(format!(
        "step: {step}\n--- input\n{src}\n--- expression\n{expr}\n--- nix\n{}\n--- ours\n{}\n",
        show(theirs),
        show(ours)
    ))
}

pub(crate) fn panicked(src: &str, expr: &str, panic: &str) -> Outcome {
    Outcome::Diverged(format!(
        "step: panic\n--- input\n{src}\n--- expression\n{expr}\n--- ours\n{panic}\n"
    ))
}

/// The name ogygia-nix-eval gives the threads it runs on, in
/// [`run_with_stack`].
const EVAL_THREAD: &str = "nix-eval";

/// Held while [`catch_ours`] runs ogygia-nix-eval, so that a panic on one of
/// its threads belongs to that call.
static CATCHING: Mutex<()> = Mutex::new(());

/// `Some` while [`catch_ours`] runs ogygia-nix-eval, holding the first panic
/// on one of its threads.
static PANIC: Mutex<Option<Option<String>>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `f`, which runs ogygia-nix-eval, failing with the location and
/// message of a panic on one of ogygia-nix-eval's threads, which is neither
/// reported nor, under libFuzzer, an abort. Any other panic is the
/// fuzzer's own, and propagates as it would without this.
pub(crate) fn catch_ours<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        // Set after libFuzzer's hook, which reports a panic and aborts, so
        // that a panic of ours never reaches that.
        let next = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let ours = std::thread::current().name() == Some(EVAL_THREAD)
                && lock(&PANIC)
                    .as_mut()
                    .map(|panic| panic.get_or_insert_with(|| info.to_string()))
                    .is_some();
            if !ours {
                next(info);
            }
        }));
    });
    let _catching = lock(&CATCHING);
    *lock(&PANIC) = Some(None);
    let result = std::panic::catch_unwind(AssertUnwindSafe(f));
    let panic = lock(&PANIC).take().flatten();
    match (result, panic) {
        (Ok(out), _) => Ok(out),
        (Err(_), Some(panic)) => Err(panic),
        (Err(payload), None) => std::panic::resume_unwind(payload),
    }
}

/// Fails with a deliberate divergence when evaluation reached a disabled
/// or unimplemented feature, and with a [divergence](Outcome::Diverged)
/// when it panicked.
pub(crate) fn run_ours(
    eval: impl Fn(&str) -> Result<String, Error>,
    src: &str,
    expr: &str,
) -> Result<Run, Outcome> {
    match catch_ours(|| eval(expr)) {
        Ok(Ok(out)) => Ok(Ok(out)),
        Ok(Err(Error::Disabled(feature))) => Err(Outcome::DeliberateDivergence(
            Skip::find(&Evidence::Disabled(feature)).expect("only skips disable features"),
        )),
        Ok(Err(Error::Unimplemented(_))) => Err(Outcome::DeliberateDivergence(
            Skip::find(&Evidence::Unimplemented).expect("a skip matches unimplemented features"),
        )),
        Ok(Err(Error::Eval(e))) => Ok(Err(e.to_string())),
        Err(panic) => Err(panicked(src, expr, &panic)),
    }
}

/// A [deliberate divergence](Outcome::DeliberateDivergence) where the two
/// disagree as a [`Skip`] describes, Nix having run with `store`, else a
/// divergence at `step`.
pub(crate) fn disagreed(
    step: &str,
    src: &str,
    expr: &str,
    theirs: &Run,
    store: Option<&Store>,
    ours: &Run,
) -> Outcome {
    match Skip::find(&Evidence::Disagreement {
        theirs,
        store,
        ours,
    }) {
        Some(skip) => Outcome::DeliberateDivergence(skip),
        None => diverged(step, src, expr, theirs, ours),
    }
}

/// Both sides succeed with the same output, or both fail.
pub(crate) fn agree(theirs: &Run, ours: &Run) -> bool {
    match (theirs, ours) {
        (Ok(a), Ok(b)) => a == b,
        (Err(_), Err(_)) => true,
        _ => false,
    }
}

/// Compare ogygia-nix-eval with Nix on the expression `src`, both in pure
/// mode, in up to three steps:
///
/// 1. Parse: whether ogygia-nix-eval compiles it must match whether
///    `nix-instantiate --parse` accepts it, undefined variables included.
/// 2. Evaluate: the deeply evaluated values must be equal, or both sides
///    must fail.
/// 3. When both fail, `builtins.tryEval` must catch the failure on both
///    sides or on neither. Error messages are not compared.
///
/// Nix runs first at each step, within `budget`. An input on which it runs
/// out of time, memory or stack is [skipped](Outcome::Skipped), and one
/// that matches a [`Skip`] is a
/// [deliberate divergence](Outcome::DeliberateDivergence).
///
/// A panic in ogygia-nix-eval diverges by itself, at whichever step it
/// happens, ending the check there. ogygia-nix-eval runs before Nix in the
/// parse step, so a panic there runs no Nix at all. A panic in the fuzzer
/// itself propagates.
///
/// A flake bundle is instead compared with `nix eval` in the evaluate and
/// catch steps, as the README describes.
pub fn check(src: &str, budget: Budget) -> Outcome {
    if let Some(skip) = Skip::find(&Evidence::Input(src)) {
        return Outcome::DeliberateDivergence(skip);
    }
    if crate::bundle::is_bundle(src) {
        return crate::flake::check(src, budget);
    }
    check_with(
        compile,
        |expr| ogygia_nix_eval::eval_to_string(expr, "/", true, settings()),
        |mode, expr| run_nix(mode, expr, budget),
        src,
    )
}

pub(crate) fn compile(src: &str) -> Run {
    run_with_stack(|| {
        Context::new(Io::pure())
            .compile_str(src, "/")
            .map(|_| String::new())
            .map_err(|e| e.msg)
    })
}

/// [`check`] on an expression that matches no [`Skip`] as input, with
/// `compile` and `eval` standing in for ogygia-nix-eval's parsing and
/// evaluation, and `nix` for running `nix-instantiate`.
pub(crate) fn check_with(
    compile: impl Fn(&str) -> Run,
    eval: impl Fn(&str) -> Result<String, Error>,
    nix: impl Fn(&[&str], &str) -> Result<Theirs, Outcome>,
    src: &str,
) -> Outcome {
    let ours = match catch_ours(|| compile(src)) {
        Ok(ours) => ours,
        Err(panic) => return panicked(src, src, &panic),
    };
    let theirs = match nix(&["--parse"], src) {
        Ok(theirs) => theirs.run,
        Err(outcome) => return outcome,
    };
    if theirs.is_ok() != ours.is_ok() {
        return diverged("parse", src, src, &theirs, &ours);
    }
    if ours.is_err() {
        return Outcome::ParseRejected;
    }

    // The input parses on its own, so parenthesised on lines of its own it
    // is the same expression.
    let expr = format!("{DEEP_COPY} (\n{src}\n)");
    let theirs = match nix(&["--eval", "--strict"], &expr) {
        Ok(theirs) => theirs,
        Err(outcome) => return outcome,
    };
    let ours = match run_ours(&eval, src, &expr) {
        Ok(ours) => ours,
        Err(outcome) => return outcome,
    };
    if !agree(&theirs.run, &ours) {
        return disagreed(
            "eval",
            src,
            &expr,
            &theirs.run,
            theirs.store.as_ref(),
            &ours,
        );
    }
    if ours.is_ok() {
        return Outcome::Value;
    }

    let expr = format!("builtins.tryEval (builtins.deepSeq (\n{src}\n) null)");
    let theirs = match nix(&["--eval", "--strict"], &expr) {
        Ok(theirs) => theirs,
        Err(outcome) => return outcome,
    };
    let ours = match run_ours(&eval, src, &expr) {
        Ok(ours) => ours,
        Err(outcome) => return outcome,
    };
    if !agree(&theirs.run, &ours) {
        return disagreed(
            "catch",
            src,
            &expr,
            &theirs.run,
            theirs.store.as_ref(),
            &ours,
        );
    }
    if ours.is_ok() {
        Outcome::Caught
    } else {
        Outcome::Uncaught
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use strum::IntoEnumIterator;

    use super::*;

    pub(crate) fn outcome(src: &str) -> String {
        check(src, Budget::Unlimited).to_string()
    }

    #[test]
    fn gc_out_of_memory_is_exhaustion() {
        assert!(exhausted(
            "GC Warning: Out of Memory! Heap size: 434 MiB. Returning NULL!\n\
             Insufficient space for initial table allocation\n"
        ));
    }

    #[test]
    fn equal_values() {
        assert_eq!(outcome("{ a = [ 1 2.5 \"x\" ]; }"), "values");
    }

    #[test]
    fn keeps_trailing_whitespace_in_paths() {
        assert_eq!(outcome("/a + \"b \""), "values");
    }

    #[test]
    fn reads_back_what_nix_adds_to_the_store() {
        assert_eq!(
            outcome("builtins.readDir (builtins.filterSource (p: k: false) <nix/.>)"),
            "values"
        );
    }

    #[test]
    fn drv_path_context_takes_the_closure() {
        assert_eq!(
            outcome(
                r#"let
                  b = builtins.toFile "b" "b";
                  x = derivation { name = "x"; system = "s"; builder = "/bin/sh";
                    src = builtins.toFile "a" "${b}"; outputs = [ "out" "dev" ]; };
                  y = derivation { name = "y"; system = "s"; builder = "/bin/sh"; c = x.dev; };
                  f = c: derivation { name = "d"; system = "s"; builder = "/bin/sh"; inherit c; };
                in map (d: { inherit (d) drvPath outPath; }) [ (f x.drvPath) (f y.drvPath) ]"#
            ),
            "values"
        );
    }

    #[test]
    fn append_context_requires_valid_paths() {
        assert_eq!(
            outcome(
                r#"builtins.appendContext "x" { "/nix/store/p1v2hkzib64izpkmp5zr6gn81rrk8pv1-x" = { }; }"#
            ),
            "uncaught"
        );
        assert_eq!(
            outcome(
                r#"let d = derivation { name = "d"; system = "s"; builder = "/bin/sh"; };
                in builtins.appendContext "x" { ${builtins.unsafeDiscardStringContext d.outPath} = throw "t"; }"#
            ),
            "uncaught"
        );
        assert_eq!(
            outcome(
                r#"let d = derivation { name = "d"; system = "s"; builder = "/bin/sh"; };
                in builtins.getContext (builtins.appendContext "x" {
                  ${builtins.unsafeDiscardStringContext d.drvPath} = { allOutputs = true; outputs = [ "out" ]; };
                })"#
            ),
            "values"
        );
        assert_eq!(
            outcome(
                r#"builtins.appendContext "x" {
                  ${builtins.unsafeDiscardStringContext (builtins.toFile "a" "b")} = { outputs = [ (throw "t") ]; };
                }"#
            ),
            "uncaught"
        );
    }

    #[test]
    fn reading_a_path_needs_its_derivation_outputs() {
        let d = r#"let d = derivation { name = "d"; system = "s"; builder = "/bin/sh"; };
            k = builtins.unsafeDiscardStringContext d.drvPath;
            t = builtins.toFile "t" "t";
            c = builtins.substring 0 0 "${d}";
            in"#;
        for expr in [
            "builtins.pathExists d",
            "builtins.pathExists (\"/nonexistent\" + c)",
            "builtins.readFile (t + c)",
            "builtins.readFileType (t + c)",
            "builtins.hashFile \"sha256\" (t + c)",
            "builtins.readDir (\"/\" + c)",
            "import d",
            "scopedImport { } d",
            "builtins.path { path = t + c; }",
            "builtins.filterSource (p: t: true) (t + c)",
            "builtins.findFile [ { path = \"/x\" + c; prefix = \"q\"; } ] \"nix/fetchurl.nix\"",
            "builtins.readFile (builtins.appendContext t { ${k} = { outputs = [ \"x\" ]; }; })",
        ] {
            assert_eq!(outcome(&format!("{d} {expr}")), "uncaught", "{expr}");
        }
        assert_eq!(
            outcome(&format!("{d} builtins.pathExists d.drvPath")),
            "values"
        );
        assert_eq!(
            outcome(&format!(
                "{d} builtins.readFile (builtins.appendContext t {{ ${{k}} = {{ allOutputs = true; }}; }})"
            )),
            "values"
        );
    }

    #[test]
    fn copying_a_store_path_with_context_keeps_its_references() {
        let p = r#"let p = builtins.toFile "x" (builtins.toFile "x" "2");
            q = builtins.toFile "q" "3";
            d = builtins.unsafeDiscardStringContext;
            in"#;
        for expr in [
            "builtins.path { path = p; }",
            "builtins.path { path = p; name = \"y\"; }",
            "builtins.path { path = builtins.substring 0 0 q + d p; }",
            "builtins.path { path = d p; }",
            "builtins.filterSource (p: t: true) p",
            "builtins.path { path = p; sha256 = \"0gbnj8lbw8gy5c709zy5zqqzmg0gss0ixmwxhfxc5b26sm3w6g0g\"; }",
            "builtins.path { path = d p; recursive = false; }",
        ] {
            assert_eq!(outcome(&format!("{p} {expr}")), "values", "{expr}");
        }
        assert_eq!(
            outcome(&format!(
                "{p} builtins.tryEval (builtins.path {{ path = p; recursive = false; }})"
            )),
            "uncaught"
        );
    }

    #[test]
    fn ignores_the_store_root() {
        assert_eq!(outcome("/nix/store/."), "values");
        assert_eq!(
            outcome("/nix/store/p1v2hkzib64izpkmp5zr6gn81rrk8pv1-x/a"),
            "values"
        );
    }

    #[test]
    fn both_reject_parse() {
        assert_eq!(outcome("1 +"), "parse-rejected");
        assert_eq!(outcome("undefined"), "parse-rejected");
        assert_eq!(outcome("~/a"), "parse-rejected");
        assert_eq!(outcome("__currentTime"), "parse-rejected");
    }

    #[test]
    fn both_catch() {
        assert_eq!(outcome("throw \"x\""), "caught");
    }

    #[test]
    fn neither_catches() {
        assert_eq!(outcome("abort \"x\""), "uncaught");
    }

    #[test]
    fn forbids_paths_outside_mounts() {
        assert_eq!(outcome("\"${/bin/sh}\""), "uncaught");
        assert_eq!(
            outcome("builtins.toJSON { outPath = /bin/sh; }"),
            "uncaught"
        );
        assert_eq!(outcome("builtins.readFile /nonexistent"), "uncaught");
        assert_eq!(outcome("builtins.pathExists /bin/sh"), "values");
        assert_eq!(
            outcome("builtins.findFile [ { path = \"/bin\"; prefix = \"\"; } ] \"sh\""),
            "uncaught"
        );
    }

    #[test]
    fn path_checks_sha256() {
        // The NAR hashes of `t` and of an empty directory.
        let t = "let t = builtins.toFile \"x\" \"y\"; in";
        let y = "jyiRGLH0P4VduRAhXme5qgKYjS+wp8Vv6s2yRrgV4bQ=";
        let empty = "sha256-pQpattmS9VmO3ZIQUFn66az8GSmB4IvYhTTCFn6SUmo=";
        let zero = "0000000000000000000000000000000000000000000000000000";
        for hash in [
            &format!("sha256-{y}"),
            y,
            "sha256:1d712nw4dcndx9pwb9xh5y6rh0map5kmw88hp5fqagzln4c92a4g",
            "8f289118b1f43f855db910215e67b9aa02988d2fb0a7c56feacdb246b815e1b4",
        ] {
            let expr = format!("{t} builtins.path {{ path = t; sha256 = \"{hash}\"; }}");
            assert_eq!(outcome(&expr), "values", "{expr}");
        }
        for expr in [
            &format!("{t} builtins.path {{ path = t; sha256 = \"{zero}\"; }}"),
            &format!("{t} builtins.path {{ path = t; name = \"q\"; sha256 = \"\"; }}"),
            &format!("{t} builtins.path {{ path = t; sha256 = \"garbage\"; }}"),
            &format!(
                "{t} builtins.path {{ path = t; sha256 = \"sha1:{}\"; }}",
                &zero[..32]
            ),
            &format!(
                "{t} builtins.path {{ path = t; sha256 = \"garbage\"; recursive = throw \"r\"; }}"
            ),
            &format!("builtins.path {{ path = <nix/.>; sha256 = \"{empty}\"; }}"),
            &format!(
                "builtins.path {{ path = /nonexistent; name = \"q\"; sha256 = \"sha256-{y}\"; }}"
            ),
        ] {
            assert_eq!(outcome(expr), "uncaught", "{expr}");
        }
        assert_eq!(
            outcome(&format!(
                "builtins.path {{ path = <nix/.>; filter = p: k: false; sha256 = \"{empty}\"; }}"
            )),
            "values"
        );
        // A path whose expected store path is already valid is not read.
        for (path, name) in [("/nonexistent", "name = \"q\";"), ("\"/nonexistent\"", "")] {
            let expr = format!(
                "{t} let a = builtins.path {{ path = t; name = \"{}\"; }}; in builtins.seq a \
                 (builtins.readFile (builtins.path {{ path = {path}; {name} \
                 sha256 = \"sha256-{y}\"; filter = p: k: throw \"f\"; }}))",
                if name.is_empty() { "nonexistent" } else { "q" }
            );
            assert_eq!(outcome(&expr), "values", "{expr}");
        }
    }

    #[test]
    fn path_adds_flat() {
        // The SHA-256 of `t`'s contents.
        let t = "let t = builtins.toFile \"x\" \"y\"; in";
        let y = "a1fce4363854ff888cff4b8e7875d600c2682390412a8cf79b37d0b11148b0fa";
        for args in [
            "path = t;",
            "path = t; name = \"q\";",
            &format!("path = t; sha256 = \"{y}\";"),
            "path = t; filter = p: k: throw \"f\";",
            "path = <nix/fetchurl.nix>;",
        ] {
            let p = format!("builtins.path {{ {args} recursive = false; }}");
            for expr in [
                format!("{t} {p}"),
                format!("{t} builtins.getContext ({p})"),
                format!("{t} builtins.readFile ({p})"),
            ] {
                assert_eq!(outcome(&expr), "values", "{expr}");
            }
        }
        for args in [
            "path = t; sha256 = \"\";",
            "path = <nix/.>;",
            &format!("path = <nix/.>; sha256 = \"{y}\";"),
        ] {
            let expr =
                format!("{t} builtins.tryEval (builtins.path {{ {args} recursive = false; }})");
            assert_eq!(outcome(&expr), "uncaught", "{expr}");
        }
        // A path whose expected store path is already valid is not read.
        let expr = format!(
            "{t} let a = builtins.path {{ path = t; name = \"q\"; recursive = false; }}; in \
             builtins.seq a (builtins.readFile (builtins.path {{ path = /nonexistent; \
             name = \"q\"; recursive = false; sha256 = \"{y}\"; }}))"
        );
        assert_eq!(outcome(&expr), "values", "{expr}");
    }

    #[test]
    fn skips_what_nix_cannot_finish() {
        assert_eq!(outcome("let f = x: f x; in f 1"), "skipped");
    }

    #[test]
    #[ignore = "needs a cgroup of its own: run the test binary under \
                `systemd-run --user --scope -p OOMPolicy=continue`"]
    fn skips_what_nix_cannot_finish_within_its_limit() {
        crate::limit_nix_memory().unwrap();
        let nix = |expr: &str, time, memory| {
            let store = Store::new();
            let mut cmd = nix_command(&nix_instantiate(), &store);
            cmd.args(["--eval", "--strict"]).args(NIX_ARGS).arg("-");
            run_within(cmd, expr.as_bytes(), Some(Limit { time, memory })).map(|ran| ran.stdout)
        };
        // Nix itself needs about 20 MiB.
        let memory = 32 << 20;
        assert_eq!(nix("1", NIX_TIMEOUT, memory).ok().as_deref(), Some("1\n"));
        assert!(matches!(
            nix(
                "builtins.length (builtins.genList (i: \"\") 1000000)",
                NIX_TIMEOUT,
                memory
            ),
            Err(Outcome::Skipped)
        ));
        assert!(matches!(
            nix("1", Duration::ZERO, memory),
            Err(Outcome::Skipped)
        ));
    }

    #[test]
    fn names_round_trip() {
        let mut outcomes = vec![
            Outcome::ParseRejected,
            Outcome::Skipped,
            Outcome::Value,
            Outcome::Caught,
            Outcome::Uncaught,
        ];
        outcomes.extend(Skip::iter().map(Outcome::DeliberateDivergence));
        for kind in Kind::iter() {
            assert!(
                kind == Kind::Diverged || outcomes.iter().any(|o| Kind::from(o) == kind),
                "{kind:?}"
            );
        }
        for outcome in outcomes {
            let name = outcome.to_string();
            assert_eq!(
                Outcome::from_name(&name).map(|o| o.to_string()),
                Some(name.clone())
            );
        }
        for name in [
            "diverged",
            "nonsense",
            "deliberate-divergence",
            "deliberate-divergence nonsense",
            "values nul-byte",
        ] {
            assert!(Outcome::from_name(name).is_none(), "{name}");
        }
    }

    #[test]
    fn reports_a_divergence() {
        let report = report(check_with(
            compile,
            |_| Ok("2".to_owned()),
            |mode, expr| run_nix(mode, expr, Budget::Unlimited),
            "1",
        ));
        assert!(report.starts_with("step: eval\n"), "{report}");
        assert!(report.contains("--- nix\n1\n--- ours\n2\n"), "{report}");
    }

    pub(crate) fn report(outcome: Outcome) -> String {
        let Outcome::Diverged(report) = outcome else {
            panic!("expected a divergence");
        };
        report
    }

    #[test]
    fn a_panic_in_parsing_diverges_without_nix() {
        let report = report(check_with(
            |_| run_with_stack(|| -> Run { panic!("parse failed") }),
            |_| unreachable!("evaluated"),
            |_, _| unreachable!("ran Nix"),
            "1",
        ));
        let ours = format!("--- ours\npanicked at {}:", file!());
        assert!(
            report.starts_with(&format!(
                "step: panic\n--- input\n1\n--- expression\n1\n{ours}"
            )),
            "{report}"
        );
        assert!(report.ends_with(":\nparse failed\n"), "{report}");
    }

    #[test]
    fn a_panic_in_evaluation_ends_the_check() {
        let report = report(check_with(
            |_| Ok(String::new()),
            |_| run_with_stack(|| -> Result<String, Error> { panic!("eval failed") }),
            |mode, expr| match mode {
                ["--parse"] => Ok(Ok(String::new()).into()),
                _ if expr.starts_with(DEEP_COPY) => Ok(Ok("1".to_owned()).into()),
                _ => unreachable!("ran Nix after the panic"),
            },
            "1",
        ));
        let ours = format!("--- ours\npanicked at {}:", file!());
        assert!(
            report.starts_with(&format!(
                "step: panic\n--- input\n1\n--- expression\n{DEEP_COPY} (\n1\n)\n{ours}"
            )),
            "{report}"
        );
        assert!(report.ends_with(":\neval failed\n"), "{report}");
    }

    #[test]
    fn a_panic_in_the_fuzzer_propagates() {
        // After a panic of ours, which must not be blamed for the others.
        check_with(
            |_| run_with_stack(|| -> Run { panic!("parse failed") }),
            |_| unreachable!("evaluated"),
            |_, _| unreachable!("ran Nix"),
            "1",
        );
        let nix = std::panic::catch_unwind(|| {
            check_with(
                |_| Ok(String::new()),
                |_| unreachable!("evaluated"),
                |_, _| panic!("Nix failed"),
                "1",
            )
        });
        assert_eq!(
            nix.map(drop).unwrap_err().downcast_ref::<&str>(),
            Some(&"Nix failed")
        );
        // Not on one of ogygia-nix-eval's threads.
        let eval = std::panic::catch_unwind(|| {
            check_with(
                |_| Ok(String::new()),
                |_| panic!("eval failed"),
                |_, _| Ok(Ok(String::new()).into()),
                "1",
            )
        });
        assert_eq!(
            eval.map(drop).unwrap_err().downcast_ref::<&str>(),
            Some(&"eval failed")
        );
    }
}
