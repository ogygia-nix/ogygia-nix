//! Comparing ogygia-nix-eval with `nix eval` on a flake bundle.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use ogygia_nix_eval::FlakeEnv;

use crate::bundle;
use crate::bundle::Bundle;
use crate::bundle::Kind;
use crate::bundle::State;
use crate::check::Budget;
use crate::check::Outcome;
use crate::check::Ran;
use crate::check::Run;
use crate::check::Store;
use crate::check::agree;
use crate::check::disagreed;
use crate::check::nix_command;
use crate::check::nix_instantiate;
use crate::check::run_command;
use crate::check::run_ours;
use crate::check::scratch;
use crate::check::settings;
use crate::features;
use crate::skip::Evidence;
use crate::skip::Skip;

/// Turns any value into one `--json` prints in full: every value becomes a
/// set naming its type, so that sets with an `outPath`, functions and paths
/// are kept apart, and string context is kept.
const ENCODE: &str = r#"let
  enc = v:
    let t = builtins.typeOf v; in
    if t == "set" then { inherit t; v = map (n: [ n (enc v.${n}) ]) (builtins.attrNames v); }
    else if t == "list" then { inherit t; v = map enc v; }
    else if t == "lambda" then { inherit t; }
    else if t == "path" then { inherit t; v = toString v; }
    else if t == "string" then { inherit t v; c = builtins.getContext v; }
    else { inherit t v; };
in enc"#;

/// The modification time of every file of every tree, so that neither
/// evaluator sees when the bundle was written.
const MTIME: i64 = 1_700_000_000;

/// The author and committer of every commit of a Git tree.
const SIGNATURE: (&str, &str) = ("fuzz", "fuzz@example.com");

/// `nix`, beside the `nix-instantiate` the fuzzer compares with.
fn nix() -> String {
    let instantiate = nix_instantiate();
    match Path::new(&instantiate).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("nix").display().to_string(),
        _ => "nix".to_owned(),
    }
}

/// The trees of a bundle, written out.
struct Trees {
    /// Every tree's directory, and every store tree's NAR hash.
    placeholders: Vec<(String, String)>,
    /// The tree evaluation starts in: the first.
    cwd: PathBuf,
    /// The store objects ogygia-nix-eval may read, which Nix has in its
    /// store too: the store trees.
    ours_store: PathBuf,
}

impl Trees {
    fn substitute(&self, s: &str) -> String {
        let mut s = s.to_owned();
        for (from, to) in &self.placeholders {
            s = s.replace(from, to);
        }
        s
    }
}

/// A `nix` command for flakes, with `cache` for its caches so that no run
/// sees what an earlier one fetched or evaluated.
fn flake_command(store: &Store, cache: &Path, cwd: &Path) -> std::process::Command {
    let mut cmd = nix_command(&nix(), store);
    cmd.env("XDG_CACHE_HOME", cache).current_dir(cwd).args([
        "--extra-experimental-features",
        "nix-command flakes",
        "--option",
        "flake-registry",
        "",
        "--option",
        "substituters",
        "",
        "--option",
        "eval-cache",
        "false",
    ]);
    cmd
}

/// Write `bundle`'s trees under `work`, adding its store trees to `store`.
/// Fails with a deliberate divergence if they cannot be written, or as
/// [`run_command`] does if Nix cannot finish.
fn materialise(
    bundle: &Bundle<'_>,
    work: &Path,
    store: &Store,
    budget: Budget,
) -> Result<Trees, Outcome> {
    let mut trees = Trees {
        placeholders: Vec::new(),
        cwd: work.join(bundle.trees[0].name),
        ours_store: work.join("ours-store"),
    };
    for tree in &bundle.trees {
        trees.placeholders.push((
            format!("@dir({})", tree.name),
            work.join(tree.name).display().to_string(),
        ));
    }
    std::fs::create_dir_all(&trees.ours_store).expect("create store directory");
    for tree in &bundle.trees {
        let dir = work.join(tree.name);
        std::fs::create_dir_all(&dir).expect("create tree directory");
        let written = match tree.kind {
            Kind::Git => write_git(tree, &dir, &trees).is_ok(),
            Kind::Path | Kind::Store => tree
                .files
                .iter()
                .try_for_each(|f| write_file(&dir, f, &trees))
                .is_ok(),
        };
        if !written {
            return Err(malformed());
        }
        set_mtimes(&dir);
        if tree.kind == Kind::Store {
            let path = add_to_store(&dir, store, work, budget)?;
            let hash = nar_hash(&dir, store, work, budget)?;
            let name = Path::new(&path).file_name().expect("store path has a name");
            copy_tree(&dir, &trees.ours_store.join(name)).expect("copy store tree");
            trees
                .placeholders
                .push((format!("@narHash({})", tree.name), hash));
        }
    }
    Ok(trees)
}

fn write_file(dir: &Path, file: &bundle::File<'_>, trees: &Trees) -> std::io::Result<()> {
    let path = dir.join(file.path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_dir()) {
        std::fs::remove_file(&path)?;
    }
    let contents = trees.substitute(file.contents);
    if file.symlink {
        std::os::unix::fs::symlink(contents.strip_suffix('\n').unwrap_or(&contents), &path)
    } else {
        std::fs::write(&path, contents)?;
        let mode = if file.executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
    }
}

/// Write a Git tree: a repository with one commit of its committed files,
/// then its staged files added to the index, then its unstaged files.
fn write_git(tree: &bundle::Tree<'_>, dir: &Path, trees: &Trees) -> Result<(), git2::Error> {
    let io = |e: std::io::Error| git2::Error::from_str(&e.to_string());
    let repo =
        git2::Repository::init_opts(dir, git2::RepositoryInitOptions::new().initial_head("main"))?;
    let mut index = repo.index()?;
    let files = |state| tree.files.iter().filter(move |f| f.state == state);
    for file in files(State::Committed) {
        write_file(dir, file, trees).map_err(io)?;
        index.add_path(Path::new(file.path))?;
    }
    let oid = index.write_tree()?;
    let time = git2::Time::new(MTIME, 0);
    let sig = git2::Signature::new(SIGNATURE.0, SIGNATURE.1, &time)?;
    repo.commit(Some("HEAD"), &sig, &sig, "fuzz", &repo.find_tree(oid)?, &[])?;
    for file in files(State::Staged) {
        write_file(dir, file, trees).map_err(io)?;
        index.add_path(Path::new(file.path))?;
    }
    index.write()?;
    for file in files(State::Unstaged) {
        write_file(dir, file, trees).map_err(io)?;
    }
    Ok(())
}

/// Set the modification time of everything in `dir` but Git's own files,
/// and of `dir` itself, to [`MTIME`].
fn set_mtimes(dir: &Path) {
    let times = [libc::timespec {
        tv_sec: MTIME,
        tv_nsec: 0,
    }; 2];
    let set = |path: &Path| {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
        // SAFETY: `c` is a NUL-terminated path and `times` holds two
        // timespecs, as utimensat requires.
        let r = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        assert_eq!(r, 0, "setting the time of {}", path.display());
    };
    fn walk(dir: &Path, set: &dyn Fn(&Path)) {
        for entry in std::fs::read_dir(dir).expect("read tree directory") {
            let entry = entry.expect("read tree directory");
            if entry.file_name() == ".git" {
                continue;
            }
            if entry.file_type().expect("stat tree entry").is_dir() {
                walk(&entry.path(), set);
            }
            set(&entry.path());
        }
    }
    walk(dir, &set);
    set(dir);
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
    } else if meta.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(drop)
    }
}

/// Run a `nix` command whose standard output is all that matters, failing
/// with a deliberate divergence if it fails.
fn nix_output(
    args: &[&str],
    store: &Store,
    work: &Path,
    budget: Budget,
) -> Result<String, Outcome> {
    let mut cmd = flake_command(store, &work.join("cache"), work);
    cmd.args(args);
    let ran = run_command(cmd, b"", store, budget)?;
    if !ran.success {
        return Err(malformed());
    }
    Ok(ran.stdout.trim_end().to_owned())
}

fn add_to_store(dir: &Path, store: &Store, work: &Path, budget: Budget) -> Result<String, Outcome> {
    let dir = dir.display().to_string();
    nix_output(
        &["store", "add", "--name", "source", &dir],
        store,
        work,
        budget,
    )
}

fn nar_hash(dir: &Path, store: &Store, work: &Path, budget: Budget) -> Result<String, Outcome> {
    let dir = dir.display().to_string();
    nix_output(&["hash", "path", &dir], store, work, budget)
}

/// Nix's standard error without the source lines it quotes, which for a
/// flake can be the Nix source of Nix itself.
fn strip_quotes(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| {
            let l = l.trim_start();
            let digits = l.trim_start_matches(|c: char| c.is_ascii_digit());
            !digits.starts_with('|')
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

/// What Nix did with one `nix eval`, at `step`.
fn nix_eval(
    step: &str,
    reference: &str,
    apply: &str,
    trees: &Trees,
    store: &Store,
    work: &Path,
    budget: Budget,
) -> Result<Run, Outcome> {
    let mut cmd = flake_command(store, &work.join("cache"), &trees.cwd);
    cmd.args([
        "eval",
        "--json",
        "--no-update-lock-file",
        "--apply",
        apply,
        reference,
    ]);
    let Ran {
        success,
        stdout,
        stderr,
        stats,
    } = run_command(cmd, b"", store, budget)?;
    let run = if success {
        Ok(stdout.strip_suffix('\n').unwrap_or(&stdout).to_owned())
    } else {
        Err(strip_quotes(&stderr))
    };
    features::observe(&format!("flake-{step}"), &run, stats.as_deref());
    Ok(run)
}

/// The [deliberate divergence](Outcome::DeliberateDivergence) of a bundle
/// that is not well formed or whose trees cannot be written out.
fn malformed() -> Outcome {
    Outcome::DeliberateDivergence(
        Skip::find(&Evidence::MalformedBundle).expect("a skip matches malformed bundles"),
    )
}

/// Compare ogygia-nix-eval with `nix eval --json --no-update-lock-file`
/// on the flake bundle `src`, in up to two steps, like the evaluate and
/// catch steps of [`crate::check`] for a Nix expression: the flake's
/// reference must evaluate to the same value, every value [encoded](ENCODE)
/// so that it prints in full, or both must fail; and when both fail,
/// `builtins.tryEval` must catch the failure on both sides or on neither.
///
/// The bundle's trees are written out first, and when it asks for it, the
/// flake is locked by Nix, which is not compared: an input on which locking
/// fails is still evaluated, and fails on both sides if the lock it needs
/// is missing. A bundle that is not well formed, or whose trees cannot be
/// written, is a [deliberate divergence](Outcome::DeliberateDivergence).
pub fn check(src: &str, budget: Budget) -> Outcome {
    let Some(bundle) = bundle::parse(src) else {
        return malformed();
    };
    let work = tempfile::Builder::new()
        .prefix("flake-")
        .tempdir_in(scratch())
        .expect("create flake directory");
    let store = Store::new();
    match check_bundle(src, &bundle, work.path(), &store, budget) {
        Ok(outcome) | Err(outcome) => outcome,
    }
}

fn check_bundle(
    src: &str,
    bundle: &Bundle<'_>,
    work: &Path,
    store: &Store,
    budget: Budget,
) -> Result<Outcome, Outcome> {
    let trees = materialise(bundle, work, store, budget)?;
    let reference = trees.substitute(bundle.reference);
    if bundle.lock {
        let flake = reference.split_once('#').map_or(&*reference, |(f, _)| f);
        let mut cmd = flake_command(store, &work.join("cache"), &trees.cwd);
        cmd.args(["flake", "lock", flake]);
        let ran = run_command(cmd, b"", store, budget)?;
        let run = if ran.success {
            Ok(ran.stdout)
        } else {
            Err(ran.stderr)
        };
        features::observe("lock", &run, ran.stats.as_deref());
        // Locking writes the lock file now.
        set_mtimes(&trees.cwd);
    }
    let env = FlakeEnv {
        cwd: trees.cwd.clone(),
        store: Some(trees.ours_store.clone()),
        // Nix downloads through a proxy that refuses every connection.
        fetcher: None,
    };
    let steps = [
        ("eval", ENCODE.to_owned()),
        (
            "catch",
            format!("v: ({ENCODE}) (builtins.tryEval (builtins.deepSeq v null))"),
        ),
    ];
    for (step, apply) in steps {
        let command =
            format!("nix eval --json --no-update-lock-file --apply '{apply}' '{reference}'");
        let theirs = nix_eval(step, &reference, &apply, &trees, store, work, budget)?;
        let ours = run_ours(
            |_| ogygia_nix_eval::eval_flake_json(&reference, &apply, &env, settings()),
            src,
            &command,
        )?;
        if !agree(&theirs, &ours) {
            return Ok(disagreed(step, src, &command, &theirs, Some(store), &ours));
        }
        if ours.is_ok() {
            return Ok(if step == "eval" {
                Outcome::Value
            } else {
                Outcome::Caught
            });
        }
    }
    Ok(Outcome::Uncaught)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_bundles_agree() {
        // The runtime value, not env!: a nextest archive runs away from the
        // tree it was built in.
        let dir =
            PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
                .join("flakes");
        for entry in std::fs::read_dir(&dir).expect("read flakes") {
            let path = entry.expect("read flakes").path();
            let src = std::fs::read_to_string(&path).expect("read bundle");
            match check(&src, Budget::Unlimited) {
                Outcome::Diverged(report) => panic!("{}:\n{report}", path.display()),
                Outcome::DeliberateDivergence(_) | Outcome::Skipped => {
                    panic!("{} is not compared", path.display())
                }
                _ => {}
            }
        }
    }

    #[test]
    fn strips_quoted_source() {
        let stderr = "error: e\n       at f.nix:1:2:\n            1| x\n             |  ^\n";
        assert_eq!(strip_quotes(stderr), "error: e\n       at f.nix:1:2:\n");
    }
}
