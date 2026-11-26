//! Decoded TOML strings must be representable as Nix strings, even when
//! only the outer result is forced. These errors are not caught by tryEval.

use ogygia_nix_eval::Settings;

#[test]
fn nul_strings_are_uncatchable() {
    // Pinned Nix accepts each expression's syntax, but fails during evaluation
    // (including under tryEval) for each of these key and value shapes.
    for document in [
        r#""\u0000"=2"#,
        r#""a\u0000b"=2"#,
        r#""\U00000000"=2"#,
        r#""\u0000".a=2"#,
        r#"a."\u0000"=2"#,
        "[\"\\u0000\"]\na=2",
        "[a.\"\\u0000\"]\nb=2",
        "[[\"\\u0000\"]]\na=2",
        r#"a={"\u0000"=2}"#,
        r#"a=[{b={"\u0000"=2}}]"#,
        r#"a="\u0000""#,
        r#"a="\U00000000""#,
        r#"a="""a\u0000b""""#,
        r#"a=["\u0000"]"#,
        r#"a={b="\u0000"}"#,
        "[[a]]\nb=[{c=\"\\u0000\"}]",
    ] {
        // Debug quoting escapes these ASCII TOML documents as Nix strings.
        let from_toml = format!("builtins.fromTOML {document:?}");
        for expression in [
            format!("builtins.seq ({from_toml}) true"),
            format!("builtins.tryEval ({from_toml})"),
        ] {
            let result =
                ogygia_nix_eval::eval_to_string(&expression, "/", false, Settings::default());
            let error = result.expect_err(&expression).to_string();
            assert!(
                error.contains("while parsing TOML:") && error.contains("contains null bytes"),
                "{expression}: {error}"
            );
        }
    }
}
