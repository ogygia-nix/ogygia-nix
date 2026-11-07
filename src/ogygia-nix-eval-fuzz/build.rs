//! libFuzzer links the C++ standard library dynamically. Where it lives
//! outside the default library path, as in Nix, record its directory as a
//! runpath so the fuzzer starts without `LD_LIBRARY_PATH`.
//!
//! The dictionary also has the identifier-like string literals of
//! ogygia-nix-eval's source, which this collects into `$OUT_DIR/literals`,
//! one per line.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn main() {
    link_cxx();
    collect_literals();
}

fn link_cxx() {
    println!("cargo:rerun-if-env-changed=CXX");
    let cxx = std::env::var("CXX").unwrap_or_else(|_| "c++".to_owned());
    let Ok(out) = Command::new(cxx)
        .arg("-print-file-name=libstdc++.so")
        .output()
    else {
        return;
    };
    let file = String::from_utf8_lossy(&out.stdout);
    // The compiler echoes the bare name back when it does not know the path.
    if let Some(dir) = Path::new(file.trim()).parent().filter(|d| d.is_absolute()) {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{}", dir.display());
    }
}

fn collect_literals() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ogygia-nix-eval/src");
    println!("cargo:rerun-if-changed={}", src.display());
    let mut literals = BTreeSet::new();
    let mut dirs = vec![src];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("reading ogygia-nix-eval's source") {
            let path = entry.expect("reading ogygia-nix-eval's source").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text =
                    std::fs::read_to_string(&path).expect("reading ogygia-nix-eval's source");
                literals.extend(identifier_literals(&text).map(str::to_owned));
            }
        }
    }
    let out = Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("literals");
    let lines: String = literals.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(out, lines).expect("writing the literals");
}

/// The contents of every `"…"` in `text` that starts with a letter or `_`
/// and holds only those, digits and `_.+'-`.
fn identifier_literals(text: &str) -> impl Iterator<Item = &str> {
    let word = |c: char| c.is_ascii_alphanumeric() || "_.+'-".contains(c);
    let mut rest = text;
    std::iter::from_fn(move || {
        loop {
            let start = rest.find('"')? + 1;
            rest = &rest[start..];
            let len = rest.find(|c: char| !word(c)).unwrap_or(rest.len());
            let (literal, after) = rest.split_at(len);
            if after.starts_with('"')
                && literal.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            {
                rest = &after[1..];
                return Some(literal);
            }
        }
    })
}
