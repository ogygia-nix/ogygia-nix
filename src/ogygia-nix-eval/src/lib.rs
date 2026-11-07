//! A pure Rust evaluator for the Nix language.
//!
//! A [`Context`] owns compiled sources and filesystem mappings and may be
//! reused across evaluations; each evaluation runs in a session ([`Eval`])
//! whose values live in an arena dropped when the session ends.
//!
//! Compiling recurses on the native stack, and builtins and walks over
//! values nest evaluation on it a bounded number of times, so run both
//! through [`run_with_stack`] rather than on a thread with a default-sized
//! stack.
//! Equivalence with the real Nix evaluator is checked by the tests in
//! `tests/equiv`.

mod accessor;
mod archive;
mod builtins;
mod compile;
mod context;
mod derivation;
mod diagnostic;
mod eval;
mod fetch;
mod flake;
mod io;
mod ir;
mod nar;
mod path;
mod print;
mod store;
mod symbol;
mod value;

pub use builtins::Builtin;
pub use context::Context;
pub use diagnostic::Diagnostic;
pub use eval::Eval;
pub use eval::Feature;
pub use eval::Settings;
pub use fetch::Fetcher;
pub use flake::FlakeEnv;
pub use io::Io;
pub use print::print_strict;
pub use value::EvalError;
pub use value::Value;

/// The system this binary was built for, as a Nix system double.
pub const CURRENT_SYSTEM: &str = current_system();

const fn current_system() -> &'static str {
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-linux"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-linux"
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-darwin"
    } else {
        "unknown"
    }
}

/// Stack size for evaluation threads. Compiling deeply nested syntax (long
/// chains of binary operators) needs far more than the default.
const STACK_SIZE: usize = 1 << 30;

/// Run `f` on a fresh thread with a stack large enough for evaluation.
pub fn run_with_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("nix-eval".to_owned())
            .stack_size(STACK_SIZE)
            .spawn_scoped(s, f)
            .expect("failed to spawn evaluation thread")
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}

/// Why [`eval_to_string`] failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Evaluation failed.
    #[error("{0}")]
    Eval(Box<Diagnostic>),
    /// The expression used a feature in [`Settings::disabled`].
    #[error("disabled feature: {0}")]
    Disabled(Feature),
    /// The expression used a feature this evaluator deliberately leaves
    /// out, which the string names.
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
}

/// Evaluate the expression in `text`, deeply forcing it, and render it the
/// way `nix-instantiate --eval --strict` does, in pure evaluation mode if
/// `pure`. Relative paths resolve against `base_dir`.
pub fn eval_to_string(
    text: &str,
    base_dir: &str,
    pure: bool,
    settings: Settings,
) -> Result<String, Error> {
    run_with_stack(|| {
        let ctx = Context::new(if pure { Io::pure() } else { Io::default() });
        let bump = bumpalo::Bump::new();
        let ev = Eval::new(&ctx, &bump, settings);
        printed(
            ev.eval_string(text, base_dir)
                .and_then(|v| print_strict(&ev, v)),
        )
    })
}

fn printed(result: Result<Vec<u8>, Box<EvalError<'_>>>) -> Result<String, Error> {
    match result {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => Err(match e.kind {
            value::ErrorKind::Disabled(feature) => Error::Disabled(feature),
            value::ErrorKind::Unimplemented(feature) => Error::Unimplemented(feature),
            _ => Error::Eval(Box::new(Diagnostic::new(&e))),
        }),
    }
}

/// Evaluate the installable `installable`, a flake reference and an
/// attribute path, as `nix eval --json --no-update-lock-file --apply apply
/// installable` does, in `env`, and return what it prints, without the
/// final newline.
pub fn eval_flake_json(
    installable: &str,
    apply: &str,
    env: &FlakeEnv,
    settings: Settings,
) -> Result<String, Error> {
    run_with_stack(|| {
        let ctx = Context::flakes(Io::pure().with_store_dir(env.store.clone()));
        let bump = bumpalo::Bump::new();
        let flakes = std::cell::OnceCell::new();
        let ev = Eval::new(&ctx, &bump, settings);
        printed(flake::eval_json(&ev, installable, apply, env, &flakes))
    })
}
