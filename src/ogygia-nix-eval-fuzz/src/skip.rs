//! Differences from Nix that ogygia-nix-eval keeps on purpose, which the
//! fuzzer does not report.

use ogygia_nix_eval::Feature;
use strum::Display;
use strum::EnumIter;
use strum::EnumMessage;
use strum::EnumString;
use strum::IntoEnumIterator;

use crate::check::Run;
use crate::check::Store;

/// What a [`Skip`] is matched against, each at the point of a check where
/// it is known.
pub(crate) enum Evidence<'a> {
    /// The input, before either evaluator sees it.
    Input(&'a str),
    /// Nix's standard error at any step, once Nix has finished without
    /// running out of time, memory or stack.
    NixStderr(&'a str),
    /// ogygia-nix-eval reached a feature that [`disabled`] switches off.
    Disabled(Feature),
    /// ogygia-nix-eval reached a feature it deliberately leaves out.
    Unimplemented,
    /// A flake bundle that is not well formed, or whose trees could not be
    /// written out or added to Nix's store.
    MalformedBundle,
    /// The two disagree at the evaluation or catch step, where Nix ran with
    /// `store`, if it ran.
    Disagreement {
        theirs: &'a Run,
        store: Option<&'a Store>,
        ours: &'a Run,
    },
}

/// A difference from Nix that is not a finding. Each variant's
/// documentation is the reason, which [`EnumMessage::get_documentation`]
/// returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Display, EnumIter, EnumMessage, EnumString)]
#[strum(serialize_all = "kebab-case")]
pub enum Skip {
    /// The input contains a NUL byte. Nix stops reading at the first one
    /// outside a string, so it evaluates a prefix of the input rather than
    /// the input.
    NulByte,
    /// The input is a flake bundle that is not well formed, or whose trees
    /// cannot be written out or added to Nix's store, so there is no flake
    /// for either evaluator to see.
    MalformedBundle,
    /// The input uses a regular expression. Nix's are libstdc++'s
    /// `std::regex`, whose semantics are too broken to reasonably match, so
    /// ogygia-nix-eval runs with them disabled.
    Regex,
    /// Nix hit libstdc++'s memory limit for a regular expression, a fixed
    /// limit of its implementation that it hits on every run.
    /// `builtins.tryEval` cannot catch it, so no expression can depend on
    /// it.
    RegexMemoryLimit,
    /// Nix failed reading back a `.drv` file it wrote whose platform holds
    /// a `"` or `\`, where everything else in the file is well formed. Nix
    /// writes the platform without escaping it, so it cannot parse the
    /// file, and fails where ogygia-nix-eval succeeds or catches the
    /// failure, until NixOS/nix#16525
    /// (<https://github.com/NixOS/nix/issues/16525>) is fixed.
    UnescapedPlatform,
    /// ogygia-nix-eval failed reading `/` or `/nix/store`, as forbidden in
    /// pure evaluation or as not existing, where Nix succeeded after adding
    /// a path to its store. Once it has added one, Nix lets pure evaluation
    /// see the parents of store paths, `/` as an empty directory; that is a
    /// harmless artefact of its implementation, which ogygia-nix-eval does
    /// not match and which is not reported upstream.
    StoreRoot,
    /// ogygia-nix-eval reached a feature it deliberately leaves out, such
    /// as fetching, and failed with an error naming it where Nix may not.
    Unimplemented,
}

impl Skip {
    /// The first skip that `evidence` matches.
    pub(crate) fn find(evidence: &Evidence) -> Option<Skip> {
        Skip::iter().find(|skip| skip.matches(evidence))
    }

    fn matches(self, evidence: &Evidence) -> bool {
        match self {
            Skip::NulByte => matches!(evidence, Evidence::Input(src) if src.contains('\0')),
            Skip::MalformedBundle => matches!(evidence, Evidence::MalformedBundle),
            Skip::Regex => matches!(evidence, Evidence::Disabled(Feature::Regex)),
            Skip::RegexMemoryLimit => matches!(
                evidence,
                Evidence::NixStderr(stderr)
                    if stderr.contains("memory limit exceeded by regular expression")
            ),
            Skip::UnescapedPlatform => matches!(
                evidence,
                Evidence::Disagreement { theirs, store, .. } if misread_platform(theirs, *store)
            ),
            Skip::StoreRoot => matches!(
                evidence,
                Evidence::Disagreement { theirs, store, ours } if hidden_root(theirs, *store, ours)
            ),
            Skip::Unimplemented => matches!(evidence, Evidence::Unimplemented),
        }
    }

    /// The feature ogygia-nix-eval runs without for this skip.
    fn disables(self) -> Option<Feature> {
        match self {
            Skip::Regex => Some(Feature::Regex),
            _ => None,
        }
    }

    /// The explanation of this skip.
    pub fn reason(self) -> &'static str {
        self.get_documentation().expect("every skip is documented")
    }
}

/// The features ogygia-nix-eval runs without.
pub(crate) fn disabled() -> Vec<Feature> {
    Skip::iter().filter_map(Skip::disables).collect()
}

/// Whether Nix failed as [`Skip::UnescapedPlatform`] describes, reading
/// back a `.drv` file in its store.
fn misread_platform(theirs: &Run, store: Option<&Store>) -> bool {
    let (Err(stderr), Some(store)) = (theirs, store) else {
        return false;
    };
    stderr
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .and_then(|l| {
            l.trim_start()
                .strip_prefix("error: error parsing derivation '/nix/store/")
        })
        .and_then(|l| l.split_once("': "))
        .and_then(|(name, _)| std::fs::read_to_string(store.dir().join(name)).ok())
        .is_some_and(|drv| unescaped_platform(&drv))
}

/// Whether `drv` is a `.drv` file that is well formed but for its platform,
/// which holds an unescaped `"` or `\`.
///
/// The fields before the platform are read from the start and those after
/// it from the end, as only they are escaped.
fn unescaped_platform(drv: &str) -> bool {
    let mut rest = drv.strip_prefix("Derive(");
    // Outputs, input derivations and input sources.
    for _ in 0..3 {
        rest = rest.and_then(skip_aterm).and_then(|r| r.strip_prefix(','));
    }
    let platform = rest
        .and_then(|r| r.strip_prefix('"'))
        .and_then(|r| r.strip_suffix(')'))
        // Environment, arguments and builder.
        .and_then(|r| strip_list(r, strip_pair))
        .and_then(|r| r.strip_suffix(','))
        .and_then(|r| strip_list(r, strip_string))
        .and_then(|r| r.strip_suffix(','))
        .and_then(strip_string)
        .and_then(|r| r.strip_suffix("\","));
    platform.is_some_and(|p| p.contains(['"', '\\']))
}

/// `s` after the escaped ATerm string, list or tuple it starts with.
fn skip_aterm(s: &str) -> Option<&str> {
    if let Some(mut s) = s.strip_prefix('"') {
        loop {
            let i = s.find(['"', '\\'])?;
            let (special, rest) = s[i..].split_at(1);
            if special == "\"" {
                return Some(rest);
            }
            let mut escaped = rest.chars();
            escaped.next()?;
            s = escaped.as_str();
        }
    }
    let (close, mut s) = if let Some(s) = s.strip_prefix('[') {
        (']', s)
    } else {
        (')', s.strip_prefix('(')?)
    };
    if let Some(s) = s.strip_prefix(close) {
        return Some(s);
    }
    loop {
        s = skip_aterm(s)?;
        if let Some(s) = s.strip_prefix(close) {
            return Some(s);
        }
        s = s.strip_prefix(',')?;
    }
}

/// `s` before the escaped ATerm string it ends with.
fn strip_string(s: &str) -> Option<&str> {
    let mut s = s.strip_suffix('"')?;
    // Within the string, a `"` follows an odd number of backslashes.
    loop {
        let i = s.rfind('"')?;
        let before = &s[..i];
        if (before.len() - before.trim_end_matches('\\').len()) % 2 == 0 {
            return Some(before);
        }
        s = before;
    }
}

/// `s` before the ATerm pair of escaped strings it ends with.
fn strip_pair(s: &str) -> Option<&str> {
    let s = strip_string(s.strip_suffix(')')?)?;
    strip_string(s.strip_suffix(',')?)?.strip_suffix('(')
}

/// `s` before the ATerm list it ends with, whose elements `strip_elem`
/// strips.
fn strip_list(s: &str, strip_elem: impl Fn(&str) -> Option<&str>) -> Option<&str> {
    let mut s = s.strip_suffix(']')?;
    if let Some(s) = s.strip_suffix('[') {
        return Some(s);
    }
    loop {
        s = strip_elem(s)?;
        if let Some(s) = s.strip_suffix('[') {
            return Some(s);
        }
        s = s.strip_suffix(',')?;
    }
}

/// Whether the two disagree only as [`Skip::StoreRoot`] describes: Nix
/// succeeded after adding a path to its store, and ogygia-nix-eval's
/// error is that pure evaluation forbids `/` or `/nix/store`, or that it
/// does not exist.
fn hidden_root(theirs: &Run, store: Option<&Store>, ours: &Run) -> bool {
    let Err(err) = ours else { return false };
    let Some(message) = err.lines().next().and_then(|l| l.strip_prefix("error: ")) else {
        return false;
    };
    let hidden = |path| {
        message
            == format!(
                "access to absolute path '{path}' is forbidden in pure evaluation mode \
             (use '--impure' to override)"
            )
            || message == format!("path '{path}' does not exist")
    };
    theirs.is_ok() && store.is_some_and(Store::stored) && (hidden("/") || hidden("/nix/store"))
}

#[cfg(test)]
mod tests {
    use ogygia_nix_eval::Error;
    use ogygia_nix_eval::Settings;
    use ogygia_nix_eval::eval_to_string;
    use ogygia_nix_eval::run_with_stack;

    use super::*;
    use crate::Budget;
    use crate::Outcome;
    use crate::check::Theirs;
    use crate::check::check_with;
    use crate::check::compile;
    use crate::check::run_nix;
    use crate::check::tests::outcome;
    use crate::check::tests::report;

    #[test]
    fn every_skip_has_a_reason() {
        for skip in Skip::iter() {
            assert!(!skip.reason().trim().is_empty(), "{skip}");
        }
    }

    #[test]
    fn ignores_malformed_bundles() {
        assert_eq!(
            outcome("#~flake x\n"),
            "deliberate-divergence malformed-bundle"
        );
        assert_eq!(
            outcome("#~flake x\n#~tree a path\n#~file ../b\n"),
            "deliberate-divergence malformed-bundle"
        );
    }

    #[test]
    fn limits_what_nix_never_finishes() {
        assert_eq!(
            outcome(
                "builtins.match (\"(a{2}b){2}\" + \
                 builtins.concatStringsSep \"\" (builtins.genList (_: \"a\") 99970)) \"\""
            ),
            "deliberate-divergence regex-memory-limit"
        );
    }

    #[test]
    fn ignores_nul_bytes() {
        assert_eq!(outcome("1\0+"), "deliberate-divergence nul-byte");
        assert_eq!(outcome("\"a\0b\""), "deliberate-divergence nul-byte");
    }

    #[test]
    fn ignores_regex() {
        assert_eq!(
            outcome("builtins.match \"a\" \"a\""),
            "deliberate-divergence regex"
        );
        assert_eq!(
            outcome("builtins.tryEval (builtins.split \"a\" \"a\")"),
            "deliberate-divergence regex"
        );
        assert_eq!(
            outcome("builtins.addErrorContext (throw \"x\") (builtins.match \"a\" \"a\")"),
            "deliberate-divergence regex"
        );
        assert_eq!(outcome("builtins.typeOf builtins.match"), "values");
    }

    #[test]
    fn ignores_unimplemented_features() {
        for expr in [
            r#"builtins.fetchTarball "x""#,
            r#"builtins.tryEval (builtins.fetchurl { url = "a"; })"#,
            r#"fetchGit "/x""#,
            r#"fetchMercurial "/x""#,
            r#"builtins.flakeRefToString { type = "git"; url = "https://a/b"; }"#,
        ] {
            assert_eq!(
                outcome(expr),
                "deliberate-divergence unimplemented",
                "{expr}"
            );
        }
        assert_eq!(outcome("builtins.typeOf builtins.fetchTarball"), "values");
        assert_eq!(outcome("builtins ? fetchTree"), "values");
        assert_eq!(
            outcome(r#"builtins.flakeRefToString { type = "github"; owner = "a"; repo = "b"; }"#),
            "uncaught"
        );
    }

    #[test]
    fn ignores_unimplemented_flake_features() {
        let bundle = |reference: &str, outputs: &str| {
            format!(
                "#~flake path:@dir(root){reference}\n#~tree root path\n#~file flake.nix\n\
                 {{ outputs = _: {{ {outputs} }}; }}\n"
            )
        };
        for src in [
            bundle(
                "#x",
                r#"x = builtins.fetchTree { type = "path"; path = "/x"; };"#,
            ),
            bundle("#x", r#"x = builtins.getFlake "github:a/b";"#),
            bundle(
                "#x",
                r#"x = builtins.flakeRefToString { type = "github"; owner = "a"; repo = "b"; };"#,
            ),
            bundle("#x^out", "x = 1;"),
            bundle("#^", "x = 1;"),
        ] {
            assert_eq!(
                outcome(&src),
                "deliberate-divergence unimplemented",
                "{src}"
            );
        }
        assert_eq!(
            outcome(&bundle("#x", "x = builtins.attrNames builtins;")),
            "values"
        );
        assert_eq!(outcome(&bundle("#x%5Eout", r#""x^out" = 1;"#)), "values");
    }

    #[test]
    fn skips_misread_platforms() {
        let drv = |system: &str| {
            format!(r#"(derivation {{ name = "d"; system = {system}; builder = "b"; }}).drvPath"#)
        };
        for system in [
            // expected string ','
            r#""\"""#,
            r#""a\"b""#,
            r#"builtins.toXML 1"#,
            // unexpected escape sequence in unquoted string
            r#""\\""#,
            r#""a\\b""#,
            r#""a\\""#,
            // expected string '['
            r#""p\",\"b""#,
            // expected end of file
            r#""p\",\"b\",[],[])""#,
        ] {
            assert_eq!(
                outcome(&drv(system)),
                "deliberate-divergence unescaped-platform",
                "{system}"
            );
        }
        // Where both fail, but only ours catches it.
        assert_eq!(
            outcome(&format!(r#"[ {} (throw "t") ]"#, drv(r#""\"""#))),
            "deliberate-divergence unescaped-platform"
        );
        assert_eq!(
            outcome(&format!(
                r#"builtins.tryEval (builtins.seq {} null)"#,
                drv(r#""\\""#)
            )),
            "deliberate-divergence unescaped-platform"
        );
    }

    #[test]
    fn skips_misread_platforms_among_other_fields() {
        assert_eq!(
            outcome(
                r#"let a = derivation { name = "a"; system = "s"; builder = "b"; outputs = [ "out" "dev" ]; };
                in (derivation { name = "d"; system = "p\"q\\r"; builder = "b\"\\"; args = [ "x\"" "y" ];
                  x = a.dev; y = builtins.toFile "t" "t"; }).drvPath"#
            ),
            "deliberate-divergence unescaped-platform"
        );
        assert_eq!(
            outcome(
                r#"(derivation { name = "d"; system = "\\"; builder = "b"; __structuredAttrs = true; }).drvPath"#
            ),
            "deliberate-divergence unescaped-platform"
        );
    }

    #[test]
    fn finds_only_unescaped_platforms() {
        let drv = |platform: &str, builder: &str| {
            format!(
                r#"Derive([("out","/nix/store/93rc1g704vn26ic977ampaap558r8wal-d","","")],[("/nix/store/c3vjpd08x2wgxxgkvran4kip7a97c05a-a.drv",["dev"])],["/nix/store/l6ah1wqqzy1brqig2l2swzpfb5yfzwnv-t"],"{platform}","{builder}",["x\"",""],[("builder","b\"\\"),("system","p\"q\\r"),("e","")])"#
            )
        };
        assert!(unescaped_platform(&drv(r#"p"q\r"#, r#"b\"\\"#)));
        assert!(unescaped_platform(&drv(r#"""#, "b")));
        assert!(unescaped_platform(&drv(r#"\"#, "b")));
        assert!(unescaped_platform(&drv(r#"",""#, "b")));
        assert!(!unescaped_platform(&drv("p", r#"b\"\\"#)));
        assert!(!unescaped_platform(&drv("p", r#"b"\"#)));
        assert!(!unescaped_platform(&drv(r#"p"q"#, r#"b"\"#)));
        assert!(!unescaped_platform(&format!("{})", drv(r#"""#, "b"))));
        assert!(!unescaped_platform(r#"Derive([],[],[],"\")"#));
    }

    #[test]
    fn reports_divergences_beside_misread_platforms() {
        let nix = |mode: &[&str], expr: &str| run_nix(mode, expr, Budget::Unlimited);
        let bad = r#"derivation { name = "d"; system = "\""; builder = "b"; }"#;
        // Never read back.
        let unread = report(check_with(
            compile,
            |_| Ok("2".to_owned()),
            nix,
            &format!("({bad}).name"),
        ));
        assert!(unread.contains("--- nix\n\"d\"\n--- ours\n2\n"), "{unread}");
        // Read back, where ours panics.
        let panicked = report(check_with(
            compile,
            |_| run_with_stack(|| -> Result<String, Error> { panic!("eval failed") }),
            nix,
            &format!("({bad}).drvPath"),
        ));
        assert!(panicked.starts_with("step: panic\n"), "{panicked}");
    }

    #[test]
    fn skips_the_root_once_nix_has_stored_a_path() {
        let stored = r#"builtins.seq (builtins.toFile "x" "y")"#;
        for expr in [
            format!("{stored} (builtins.readDir /.)"),
            format!(r#"{stored} "${{/sub/..}}""#),
            format!("{stored} (builtins.filterSource (p: t: true) /.)"),
            format!("{stored} (builtins.readFileType /.)"),
            format!("{stored} (builtins.readFileType /nix/store)"),
            r#"builtins.seq "${<nix/fetchurl.nix>}" (builtins.readDir /.)"#.to_owned(),
            // Where both fail, but only ours fails uncatchably.
            format!(r#"[ ({stored} (builtins.readDir /.)) (throw "t") ]"#),
        ] {
            assert_eq!(outcome(&expr), "deliberate-divergence store-root", "{expr}");
        }
        // Nix fails listing or copying the store directory uncatchably, as
        // ours does.
        assert_eq!(
            outcome(&format!("{stored} (builtins.readDir /nix/store)")),
            "uncaught"
        );
        assert_eq!(
            outcome(&format!(r#"{stored} "${{/nix/store}}""#)),
            "uncaught"
        );
    }

    #[test]
    fn reports_other_failures_beside_the_root() {
        let stored = r#"builtins.seq (builtins.toFile "x" "y")"#;
        assert_eq!(outcome("builtins.readDir /."), "uncaught");
        assert_eq!(
            outcome(&format!("{stored} (builtins.readDir /etc)")),
            "uncaught"
        );
        let nix = |stored| {
            move |mode: &[&str], _: &str| -> Result<Theirs, Outcome> {
                Ok(Theirs {
                    run: Ok(String::new()),
                    store: (stored && mode != ["--parse"]).then(|| {
                        let store = Store::new();
                        std::fs::create_dir_all(store.dir().join("x")).unwrap();
                        store
                    }),
                })
            }
        };
        let diverges = |stored, err: &str| {
            let throw = format!("let dc = _: throw {err:?}; in dc 1");
            let ours = |_: &str| eval_to_string(&throw, "/", true, Settings::default());
            matches!(
                check_with(compile, ours, nix(stored), "1"),
                Outcome::Diverged(_)
            )
        };
        let forbidden = |path: &str| {
            format!(
                "access to absolute path '{path}' is forbidden in pure evaluation mode \
                 (use '--impure' to override)"
            )
        };
        for path in ["/", "/nix/store"] {
            assert!(!diverges(true, &forbidden(path)), "{path}");
            assert!(
                !diverges(true, &format!("path '{path}' does not exist")),
                "{path}"
            );
            assert!(diverges(false, &forbidden(path)), "{path}");
        }
        for path in ["/etc", "/nix", "/nix/store/x", "//"] {
            assert!(diverges(true, &forbidden(path)), "{path}");
            assert!(
                diverges(true, &format!("path '{path}' does not exist")),
                "{path}"
            );
        }
        assert!(diverges(true, "path '/' is not a valid store path"));
        assert!(diverges(true, &format!("{} x", forbidden("/"))));
    }
}
