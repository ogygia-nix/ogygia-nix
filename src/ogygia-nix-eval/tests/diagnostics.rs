//! How evaluation errors are reported. Nix's messages are not matched here,
//! so these are snapshots to review rather than equivalence checks.

use insta::assert_snapshot;
use ogygia_nix_eval::Error;
use ogygia_nix_eval::Settings;

/// The report of the error evaluating `expr` fails with.
fn report(expr: &str) -> String {
    match ogygia_nix_eval::eval_to_string(expr, "/", true, Settings::default()) {
        Ok(v) => panic!("{expr} evaluated to {v}"),
        Err(Error::Eval(d)) => d.to_string(),
        Err(e) => panic!("{expr} failed without a report: {e}"),
    }
}

#[test]
fn syntax_error() {
    assert_snapshot!(report("{\n  a = 1\n  b = 2;\n}"), @"
    error: syntax error: unexpected TOKEN_ASSIGN at 14..17, wanted any of [TOKEN_SEMICOLON]
       ╭─[ «string»:3:5 ]
       │
     3 │   b = 2;
       │     ─┬─
       │      ╰─── here
    ───╯
    ");
}

#[test]
fn undefined_variable() {
    assert_snapshot!(report("let a = 1; in b"), @"
    error: undefined variable 'b'
       ╭─[ «string»:1:15 ]
       │
     1 │ let a = 1; in b
       │               ┬
       │               ╰── not defined
    ───╯
    ");
}

#[test]
fn duplicate_attribute() {
    assert_snapshot!(report("{ a = 1; a = 2; }"), @"
    error: attribute 'a' already defined
       ╭─[ «string»:1:10 ]
       │
     1 │ { a = 1; a = 2; }
       │          ┬
       │          ╰── already defined
    ───╯
    ");
}

#[test]
fn missing_attribute() {
    assert_snapshot!(report("let\n  cfg = { a = 1; b = 2; };\nin cfg.c"), @"
    error: attribute 'c' missing
       ╭─[ «string»:3:8 ]
       │
     3 │ in cfg.c
       │        ┬
       │        ╰── no such attribute
    ───╯
    ");
}

#[test]
fn select_from_non_set() {
    assert_snapshot!(report("builtins.map (x: x.foo) [ 1 ]"), @"
    error: expected a set but found an integer: 1 while selecting attribute 'foo'
       ╭─[ «string»:1:20 ]
       │
     1 │ builtins.map (x: x.foo) [ 1 ]
       │                    ─┬─
       │                     ╰─── selected from an integer
    ───╯
    note:
       ╭─[ «string»:1:15 ]
       │
     1 │ builtins.map (x: x.foo) [ 1 ]
       │               ────┬───
       │                   ╰───── while calling 'anonymous lambda'
    ───╯
    ");
}

#[test]
fn operator_type_error_in_call() {
    let expr = r#"let
  f = { name, ... }: name + 1;
in
{
  x = f { name = "hello"; };
}"#;
    assert_snapshot!(report(expr), @r#"
    error: cannot coerce an integer to a string: 1
       ╭─[ «string»:2:27 ]
       │
     2 │   f = { name, ... }: name + 1;
       │                           ┬
       │                           ╰── in this '+'
    ───╯
    note:
       ╭─[ «string»:5:7 ]
       │
     5 │   x = f { name = "hello"; };
       │       ──────────┬──────────
       │                 ╰──────────── while calling 'f'
    ───╯
    note:
       ╭─[ «string»:5:3 ]
       │
     5 │   x = f { name = "hello"; };
       │   ┬
       │   ╰── while evaluating the attribute 'x'
    ───╯
    "#);
}

#[test]
fn throw_at_call() {
    assert_snapshot!(report(r#"throw "boom""#), @r#"
    error: boom
       ╭─[ «string»:1:1 ]
       │
     1 │ throw "boom"
       │ ──────┬─────
       │       ╰─────── here
    ───╯
    "#);
}

#[test]
fn assertion() {
    assert_snapshot!(report("assert 1 == 2; null"), @"
    error: assertion failed
       ╭─[ «string»:1:8 ]
       │
     1 │ assert 1 == 2; null
       │        ───┬──
       │           ╰──── the condition is false
    ───╯
    ");
}

#[test]
fn missing_argument_of_multi_line_function() {
    let expr = "let\n  f = { a, b }:\n    a + b;\nin f { a = 1; }";
    assert_snapshot!(report(expr), @"
    error: function 'f' called without required argument 'b'
       ╭─[ «string»:2:7 ]
       │
     2 │   f = { a, b }:
       │       ────┬────
       │           ╰────── requires 'b'
    ───╯
    ");
}

#[test]
fn unlocated() {
    assert_snapshot!(report(r#""${1}""#), @"error: cannot coerce an integer to a string: 1");
}
