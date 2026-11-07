//! Features of what Nix did with an input, for libFuzzer to keep inputs on
//! which Nix behaves in a way no earlier input made it behave, though
//! ogygia-nix-eval reaches no new code: whether each step succeeded, the
//! templates of its error messages and trace frames, the buckets of its
//! evaluation statistics, and the token bigrams of what it parsed.
//!
//! Features are drawn only from Nix's output, never from timing or the
//! names of temporary files, so the same input always has the same ones.

use std::cell::RefCell;
use std::collections::BTreeSet;

use crate::check::Run;

/// The most features one input has.
const CAP: usize = 1024;

thread_local! {
    static FEATURES: RefCell<BTreeSet<u64>> = const { RefCell::new(BTreeSet::new()) };
}

/// Forget the features of the last input checked on this thread.
pub(crate) fn reset() {
    FEATURES.with_borrow_mut(BTreeSet::clear);
}

/// The features of the last input checked on this thread, as hashes.
pub fn take() -> Vec<u64> {
    FEATURES
        .with_borrow_mut(std::mem::take)
        .into_iter()
        .collect()
}

/// FNV-1a, so that a feature's hash does not depend on the build.
fn hash(kind: &str, text: &str) -> u64 {
    [kind.as_bytes(), &[0], text.as_bytes()]
        .concat()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

fn add(kind: &str, text: &str) {
    FEATURES.with_borrow_mut(|f| {
        if f.len() < CAP {
            f.insert(hash(kind, text));
        }
    });
}

/// Record what Nix did at `step`: its output or error, and the statistics
/// it wrote, if any. On a successful `parse` step the output is the
/// expression as Nix parsed it.
pub(crate) fn observe(step: &str, run: &Run, stats: Option<&str>) {
    add(
        "step",
        &format!("{step}:{}", if run.is_ok() { "ok" } else { "err" }),
    );
    match run {
        Ok(out) if step == "parse" => {
            let tokens = parse_tokens(out);
            for pair in tokens.windows(2) {
                add("parse", &format!("{} {}", pair[0], pair[1]));
            }
        }
        Ok(_) => {}
        Err(stderr) => {
            let lines = error_templates(stderr);
            for line in &lines {
                add("error", line);
            }
            for pair in lines.windows(2) {
                add("errors", &format!("{}\n{}", pair[0], pair[1]));
            }
        }
    }
    if let Some(stats) = stats {
        for (name, bucket) in stat_buckets(stats) {
            add("stat", &format!("{name}:{bucket}"));
        }
    }
}

/// `s` with ANSI escape sequences removed.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Where a value Nix prints in a message may start.
fn starts_value(s: &str) -> bool {
    s.starts_with(|c: char| "{[(\"'«</0123456789-".contains(c))
        || ["true", "false", "null"].iter().any(|v| s.starts_with(v))
}

/// The templates of the lines of Nix's standard error that say what went
/// wrong: neither the source it quotes nor where, and with quoted strings,
/// paths, numbers and printed values replaced by placeholders, so that
/// the same failure on different data has the same template.
fn error_templates(stderr: &str) -> Vec<String> {
    strip_ansi(stderr)
        .lines()
        .map(str::trim)
        .filter(|l| {
            let quoted = l.trim_start_matches(|c: char| c.is_ascii_digit());
            !l.is_empty() && !quoted.starts_with('|') && !(l.starts_with("at ") && l.ends_with(':'))
        })
        .map(template)
        .collect()
}

fn template(line: &str) -> String {
    for prefix in ["trace:", "evaluation warning:", "Did you mean"] {
        if line.starts_with(prefix) {
            return prefix.to_owned();
        }
    }
    let mut out = String::new();
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        if let Some(value) = rest.strip_prefix(": ")
            && starts_value(value)
        {
            out.push_str(": V");
            break;
        }
        let len = match c {
            '\'' | '"' | '‘' | '`' => {
                let close = if c == '‘' { '’' } else { c };
                let body = &rest[c.len_utf8()..];
                match body.find(close) {
                    Some(end) => {
                        out.push_str("'S'");
                        c.len_utf8() + end + close.len_utf8()
                    }
                    None => {
                        out.push(c);
                        c.len_utf8()
                    }
                }
            }
            '/' if out.is_empty() || out.ends_with([' ', '(', '\t']) => {
                let end = rest
                    .find(|c: char| c.is_whitespace() || "'\"‘’:,;()[]{}".contains(c))
                    .unwrap_or(rest.len());
                out.push_str(if rest.starts_with("/nix/store/") {
                    "<store>"
                } else {
                    "<path>"
                });
                end
            }
            '0'..='9' => {
                out.push('N');
                rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                    .unwrap_or(rest.len())
            }
            _ => {
                out.push(c);
                c.len_utf8()
            }
        };
        rest = &rest[len..];
    }
    out
}

/// Evaluation statistics that describe the evaluation rather than how long
/// it took or the machine it ran on.
const STAT_IGNORED: &[&str] = &["cpuTime", "time", "gc", "sizes", "functions", "attributes"];

/// Every integer of the statistics Nix writes as JSON with
/// `NIX_SHOW_STATS`, named by its path, with the bucket of its value:
/// `ilog2` of it, or `-` for 0.
fn stat_buckets(stats: &str) -> Vec<(String, String)> {
    fn walk(name: &str, v: &serde_json::Value, out: &mut Vec<(String, String)>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    if name.is_empty() && STAT_IGNORED.contains(&k.as_str()) {
                        continue;
                    }
                    let name = if name.is_empty() {
                        k.clone()
                    } else {
                        format!("{name}.{k}")
                    };
                    walk(&name, v, out);
                }
            }
            serde_json::Value::Number(n) => {
                if let Some(n) = n.as_u64() {
                    let bucket = n.checked_ilog2().map_or("-".to_owned(), |b| b.to_string());
                    out.push((name.to_owned(), bucket));
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str(stats) {
        walk("", &v, &mut out);
    }
    out
}

const KEYWORDS: &[&str] = &[
    "assert", "else", "if", "in", "inherit", "let", "or", "rec", "then", "with",
];

/// The tokens of an expression as `nix-instantiate --parse` prints it, with
/// strings, numbers and paths replaced by placeholders, and identifiers but
/// for keywords and names starting `__`, which Nix desugars operators to.
fn parse_tokens(out: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = out;
    while let Some(c) = rest.chars().next() {
        let len = if c.is_whitespace() {
            c.len_utf8()
        } else if c == '"' {
            let mut escaped = false;
            let end = rest[1..]
                .find(|c| {
                    let close = c == '"' && !escaped;
                    escaped = c == '\\' && !escaped;
                    close
                })
                .map_or(rest.len(), |e| e + 2);
            tokens.push("S".to_owned());
            end
        } else if c.is_ascii_digit() {
            tokens.push("N".to_owned());
            rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                .unwrap_or(rest.len())
        } else if c == '/' || c == '<' && rest[1..].starts_with(|c: char| c.is_alphanumeric()) {
            tokens.push("P".to_owned());
            rest.find(|c: char| c.is_whitespace() || c == ')' || c == ';' || c == ']')
                .unwrap_or(rest.len())
        } else if c.is_alphabetic() || c == '_' {
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || "_'-".contains(c)))
                .unwrap_or(rest.len());
            let word = &rest[..end];
            tokens.push(if KEYWORDS.contains(&word) || word.starts_with("__") {
                word.to_owned()
            } else {
                "I".to_owned()
            });
            end
        } else {
            let op = [
                "${", "...", "//", "++", "==", "!=", "<=", ">=", "&&", "||", "->",
            ]
            .into_iter()
            .find(|op| rest.starts_with(op));
            let op = op.unwrap_or(&rest[..c.len_utf8()]);
            tokens.push(op.to_owned());
            op.len()
        };
        rest = &rest[len..];
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const STDERR: &str = "error:
       … while evaluating the attribute 'a'
         at «stdin»:1:41:
            1| let f = x: throw \"no ${toString x}\"; in { a = f 3; }.a
             |                                         ^
            2|

       … from call site
         at /tmp/ogygia-nix-eval-fuzz/flake-AbCd/root/flake.nix:12:3:

       error: no 3 in /nix/store/p1v2hkzib64izpkmp5zr6gn81rrk8pv1-x/a: { a = 1; }
";

    fn features(f: impl FnOnce()) -> Vec<u64> {
        reset();
        f();
        take()
    }

    #[test]
    fn normalises_errors() {
        assert_eq!(
            error_templates(STDERR),
            [
                "error:",
                "… while evaluating the attribute 'S'",
                "… from call site",
                "error: no N in <store>: V",
            ]
        );
        let other = STDERR
            .replace("'a'", "'b'")
            .replace(":41:", ":7:")
            .replace("AbCd", "XyZw")
            .replace("no 3", "no 12")
            .replace("{ a = 1; }", "[ \"x\" ]")
            .replace("error:\n", "\x1b[31;1merror:\x1b[0m\n");
        assert_eq!(error_templates(&other), error_templates(STDERR));
        assert_eq!(
            features(|| observe("eval", &Err(other), None)),
            features(|| observe("eval", &Err(STDERR.to_owned()), None))
        );
    }

    #[test]
    fn a_new_frame_is_a_new_feature() {
        let before = features(|| observe("eval", &Err(STDERR.to_owned()), None));
        let deeper = STDERR.replace(
            "       … from call site",
            "       … while calling the 'map' builtin\n\n       … from call site",
        );
        let after = features(|| observe("eval", &Err(deeper), None));
        assert!(after.iter().any(|f| !before.contains(f)));
    }

    #[test]
    fn buckets_stats() {
        let stats = r#"{ "cpuTime": 0.01, "envs": { "number": 3, "bytes": 1032 },
            "nrThunks": 0, "primops": { "map": 1 }, "gc": { "cycles": 1 },
            "time": { "cpu": 0.01 } }"#;
        let mut buckets = stat_buckets(stats);
        buckets.sort();
        let want = [
            ("envs.bytes", "10"),
            ("envs.number", "1"),
            ("nrThunks", "-"),
            ("primops.map", "0"),
        ];
        assert_eq!(buckets, want.map(|(n, b)| (n.to_owned(), b.to_owned())));
    }

    #[test]
    fn tokenises_parse_output() {
        assert_eq!(
            parse_tokens(
                "(let a = 1; in [ ((__sub a 2.5)) ((\"x\\\"\" + (toString a))) (/p/q) <nixpkgs> { b = { }; } ])"
            )
            .join(" "),
            "( let I = N ; in [ ( ( __sub I N ) ) ( ( S + ( I I ) ) ) ( P ) P { I = { } ; } ] )"
        );
    }

    #[test]
    fn caps_features() {
        let word = |i: u32| -> String {
            (0..4)
                .map(|d| char::from(b'a' + (i / 26u32.pow(d) % 26) as u8))
                .collect()
        };
        let stderr: String = (0..2000).map(|i| format!("error: {}\n", word(i))).collect();
        let f = features(|| observe("eval", &Err(stderr), None));
        assert_eq!(f.len(), CAP);
    }
}
