//! Mutations that edit whole nodes and tokens of rnix's syntax tree, insert
//! Nix's keywords and operators between them, replace expressions with
//! ones that fail, wrap them in `let`s binding `__findFile` and
//! `__nixPath`, or give attribute sets attributes Nix treats specially or
//! toggle their `rec`, so changes to an input stay meaningful Nix more
//! often than byte edits do. They only make some inputs likelier:
//! libFuzzer's byte-level mutations, which can turn any input into any
//! other, run alongside them. In a flake bundle they edit the Nix files it
//! holds, one at a time.

use std::ops::Range;

use ogygia_nix_eval::run_with_stack;
use rnix::SyntaxKind;
use rnix::SyntaxKind::*;
use rnix::TextRange;
use rnix::TextSize;
use rowan::ast::AstNode;

struct Rng(u32);

impl Rng {
    fn new(seed: u32) -> Rng {
        Rng(seed | 1)
    }

    fn below(&mut self, n: usize) -> usize {
        // xorshift32
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as usize % n
    }
}

/// A node or token of rnix's syntax tree.
struct Element {
    range: TextRange,
    kind: SyntaxKind,
    /// Whether it is an expression that any other may replace: not, say,
    /// an attribute name or a function's parameter.
    expr: bool,
}

/// Every node and token in rnix's syntax tree of `text`, in preorder, so
/// never empty: the first is the root, which spans all of it.
fn elements(text: &str) -> Vec<Element> {
    run_with_stack(|| {
        rnix::Root::parse(text)
            .syntax()
            .descendants_with_tokens()
            .map(|e| Element {
                range: e.text_range(),
                kind: e.kind(),
                expr: rnix::ast::Expr::can_cast(e.kind())
                    && !e.parent().is_some_and(|p| {
                        matches!(
                            p.kind(),
                            NODE_ATTRPATH
                                | NODE_INHERIT
                                | NODE_IDENT_PARAM
                                | NODE_PAT_BIND
                                | NODE_PAT_ENTRY
                        )
                    }),
            })
            .collect()
    })
}

/// An expression of `elements`, picked at random, or the root, which spans
/// all of the text, when it has none.
fn expr<'a>(elements: &'a [Element], rng: &mut Rng) -> &'a Element {
    let exprs: Vec<&Element> = elements.iter().filter(|e| e.expr).collect();
    if exprs.is_empty() {
        &elements[0]
    } else {
        exprs[rng.below(exprs.len())]
    }
}

/// One of the parts of `text` holding Nix source, picked at random.
fn nix_part(text: &str, rng: &mut Rng) -> Option<Range<usize>> {
    let parts = crate::bundle::nix_ranges(text);
    (!parts.is_empty()).then(|| parts[rng.below(parts.len())].clone())
}

/// The keywords and operators of Nix.
const TOKENS: &[&str] = &[
    "assert", "else", "if", "in", "inherit", "let", "or", "rec", "then", "with", "->", "||", "&&",
    "==", "!=", "<", "<=", ">", ">=", "//", "++", "+", "-", "*", "/", "!", "?", ".", "|>", "<|",
    ":", "@", "...", "=", ";", ",", "{", "}", "[", "]", "(", ")",
];

/// Expressions that fail when evaluated, some in ways `builtins.tryEval`
/// catches and some not.
const ERRORS: &[&str] = &[
    "(throw \"t\")",
    "(abort \"a\")",
    "(assert false; null)",
    "(1 + \"\")",
    "(let x = x; in x)",
];

/// Values the mutations bind, besides the input's own expressions.
const VALUES: &[&str] = &[
    "null",
    "true",
    "0",
    "1.5",
    "\"a\"",
    "./.",
    "[ ]",
    "[ 1 \"a\" ]",
    "{ }",
    "{ a = 1; }",
    "(x: x)",
    "(a: b: a)",
    "(a: b: a < b)",
];

/// Bindings Nix gives a meaning to in an attribute set, with `{}` standing
/// for a value.
const MAGIC: &[&str] = &[
    "__functor = self: x: {};",
    "__toString = self: {};",
    "outPath = {};",
    "type = \"derivation\";",
    "__ignoreNulls = true;",
    "__structuredAttrs = true;",
    "outputHashMode = \"text\";",
];

/// Bindings of `__findFile`, which `<NAME>` calls, with `{}` standing for a
/// value.
const FIND_FILE: &[&str] = &[
    "__findFile = builtins.findFile;",
    "__findFile = path: name: [ path name ];",
    "__findFile = _: _: {};",
];

/// Bindings of `__nixPath`, which `<NAME>` passes to `__findFile`, with
/// `{}` standing for a value.
const NIX_PATH: &[&str] = &[
    "__nixPath = [ ];",
    "__nixPath = [ { prefix = \"nixpkgs\"; path = ./.; } ];",
    "__nixPath = [ { prefix = \"\"; path = \"/\"; } ];",
    "__nixPath = {};",
];

fn splice(text: &str, range: TextRange, with: &str) -> String {
    let (start, end) = (usize::from(range.start()), usize::from(range.end()));
    [&text[..start], with, &text[end..]].concat()
}

/// The text of `e`, parenthesised unless it can stand as an argument as it
/// is.
fn argument(text: &str, e: &Element) -> String {
    let s = &text[e.range];
    let atom = matches!(
        e.kind,
        NODE_LITERAL
            | NODE_STRING
            | NODE_LIST
            | NODE_ATTR_SET
            | NODE_PAREN
            | NODE_PATH_ABS
            | NODE_PATH_HOME
            | NODE_PATH_REL
            | NODE_PATH_SEARCH
    ) || (e.kind == NODE_IDENT && s != "or");
    if atom { s.to_owned() } else { format!("({s})") }
}

/// An expression of `text` as an argument, or one of [`VALUES`].
fn value(text: &str, elements: &[Element], rng: &mut Rng) -> String {
    if rng.below(2) == 0 {
        VALUES[rng.below(VALUES.len())].to_owned()
    } else {
        argument(text, expr(elements, rng))
    }
}

/// An attribute set of the text.
struct AttrSet {
    range: TextRange,
    rec: Option<TextRange>,
    /// Where its bindings start, just after its `{`.
    open: TextSize,
    /// The attribute paths within it.
    names: Vec<TextRange>,
}

fn attr_sets(elements: &[Element]) -> Vec<AttrSet> {
    elements
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == NODE_ATTR_SET)
        .filter_map(|(i, set)| {
            let (mut rec, mut open, mut names) = (None, None, Vec::new());
            for e in elements[i + 1..]
                .iter()
                .take_while(|e| set.range.contains_range(e.range))
            {
                match e.kind {
                    TOKEN_REC if open.is_none() => rec = Some(e.range),
                    TOKEN_L_BRACE if open.is_none() => open = Some(e.range.end()),
                    NODE_ATTRPATH => names.push(e.range),
                    _ => {}
                }
            }
            Some(AttrSet {
                range: set.range,
                rec,
                open: open?,
                names,
            })
        })
        .collect()
}

/// Replace an expression of `text` with one that fails.
fn inject_error(text: &str, elements: &[Element], rng: &mut Rng) -> String {
    let at = expr(elements, rng).range;
    splice(text, at, ERRORS[rng.below(ERRORS.len())])
}

/// Add one of [`MAGIC`] to an attribute set of `text`, or `__overrides` of
/// one of its attributes, making the set `rec`. With no set, replace an
/// expression with a set of one of [`MAGIC`].
fn add_magic(text: &str, elements: &[Element], rng: &mut Rng) -> String {
    let sets = attr_sets(elements);
    let value = value(text, elements, rng);
    let pick = rng.below(MAGIC.len() + 1);
    if sets.is_empty() {
        let binding = MAGIC[pick % MAGIC.len()].replace("{}", &value);
        return splice(text, expr(elements, rng).range, &format!("{{ {binding} }}"));
    }
    let set = &sets[rng.below(sets.len())];
    let at = TextRange::empty(set.open);
    if let Some(magic) = MAGIC.get(pick) {
        return splice(text, at, &format!(" {}", magic.replace("{}", &value)));
    }
    let name = if set.names.is_empty() {
        "a"
    } else {
        &text[set.names[rng.below(set.names.len())]]
    };
    let out = splice(
        text,
        at,
        &format!(" __overrides = {{ {name} = {value}; }};"),
    );
    match set.rec {
        Some(_) => out,
        None => splice(&out, TextRange::empty(set.range.start()), "rec "),
    }
}

/// Make an attribute set of `text` `rec` or not, or add one of [`MAGIC`]
/// when it has none.
fn toggle_rec(text: &str, elements: &[Element], rng: &mut Rng) -> String {
    let sets = attr_sets(elements);
    if sets.is_empty() {
        return add_magic(text, elements, rng);
    }
    let set = &sets[rng.below(sets.len())];
    match set.rec {
        Some(rec) => splice(text, rec, ""),
        None => splice(text, TextRange::empty(set.range.start()), "rec "),
    }
}

/// Replace an expression of `text` with a `let` that binds `__findFile`,
/// `__nixPath` or both around it, and around a `<nixpkgs>` alongside it
/// half the time.
fn wrap_find_file(text: &str, elements: &[Element], rng: &mut Rng) -> String {
    let target = expr(elements, rng);
    let which = rng.below(3);
    let mut bindings = Vec::new();
    if which != 0 {
        let value = value(text, elements, rng);
        bindings.push(FIND_FILE[rng.below(FIND_FILE.len())].replace("{}", &value));
    }
    if which != 1 {
        let value = value(text, elements, rng);
        bindings.push(NIX_PATH[rng.below(NIX_PATH.len())].replace("{}", &value));
    }
    let body = if rng.below(2) == 0 {
        text[target.range].to_owned()
    } else {
        format!("[ {} <nixpkgs> ]", argument(text, target))
    };
    let wrapped = format!("(let {} in {body})", bindings.join(" "));
    splice(text, target.range, &wrapped)
}

/// Replace, delete, copy or swap whole nodes and tokens of `text`, insert a
/// keyword or operator after one, or edit an expression or attribute set
/// in one of the ways above.
fn mutate_tree(text: &str, rng: &mut Rng) -> String {
    let elements = elements(text);
    let a = elements[rng.below(elements.len())].range;
    let b = elements[rng.below(elements.len())].range;
    let piece = &text[b];
    match rng.below(9) {
        0 => splice(text, a, piece),
        1 => splice(text, a, ""),
        2 => splice(text, TextRange::empty(a.end()), &format!(" {piece}")),
        3 => {
            let token = TOKENS[rng.below(TOKENS.len())];
            splice(text, TextRange::empty(a.end()), &format!(" {token} "))
        }
        4 => {
            let (first, second) = if a.start() <= b.start() {
                (a, b)
            } else {
                (b, a)
            };
            if first.end() > second.start() {
                return splice(text, a, piece);
            }
            let between = TextRange::new(first.end(), second.start());
            [
                &text[..usize::from(first.start())],
                &text[second],
                &text[between],
                &text[first],
                &text[usize::from(second.end())..],
            ]
            .concat()
        }
        5 => inject_error(text, &elements, rng),
        6 => add_magic(text, &elements, rng),
        7 => toggle_rec(text, &elements, rng),
        _ => wrap_find_file(text, &elements, rng),
    }
}

/// libFuzzer's custom mutator: mutate the input `data[..size]` in place
/// into at most `max_size` bytes, returning the new size. Half the time,
/// and whenever the input is not UTF-8 or the result would not fit, this is
/// `fallback`, libFuzzer's own mutation, with the same arguments.
pub fn mutate(
    data: &mut [u8],
    size: usize,
    max_size: usize,
    seed: u32,
    fallback: impl FnOnce(&mut [u8], usize, usize) -> usize,
) -> usize {
    let mut rng = Rng::new(seed);
    if rng.below(2) == 0
        && let Ok(text) = std::str::from_utf8(&data[..size])
        && let Some(part) = nix_part(text, &mut rng)
    {
        let out = [
            &text[..part.start],
            mutate_tree(&text[part.clone()], &mut rng).as_str(),
            &text[part.end..],
        ]
        .concat();
        if out.len() <= max_size {
            data[..out.len()].copy_from_slice(out.as_bytes());
            return out.len();
        }
    }
    fallback(data, size, max_size)
}

/// libFuzzer's custom crossover: write a combination of `data1` and
/// `data2` to `out`, truncated to fit, returning its length. When both are
/// UTF-8 it is `data1` with one of its nodes or tokens replaced by one of
/// `data2`'s; otherwise a prefix of one joined to a suffix of the other.
pub fn crossover(data1: &[u8], data2: &[u8], out: &mut [u8], seed: u32) -> usize {
    let mut rng = Rng::new(seed);
    let texts = std::str::from_utf8(data1)
        .ok()
        .zip(std::str::from_utf8(data2).ok());
    let parts = texts.and_then(|(a, b)| {
        let to = nix_part(a, &mut rng)?;
        let from = nix_part(b, &mut rng)?;
        Some((a, to, &b[from]))
    });
    let spliced = match parts {
        Some((a, to, b)) => {
            let into = elements(&a[to.clone()]);
            let from = elements(b);
            let piece = &b[from[rng.below(from.len())].range];
            let at = into[rng.below(into.len())].range + TextSize::from(to.start as u32);
            splice(a, at, piece).into_bytes()
        }
        None => {
            let cut1 = rng.below(data1.len() + 1);
            let cut2 = rng.below(data2.len() + 1);
            [&data1[..cut1], &data2[cut2..]].concat()
        }
    };
    let len = spliced.len().min(out.len());
    out[..len].copy_from_slice(&spliced[..len]);
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUTS: &[&str] = &[
        "",
        "1",
        "let a = { b = [ 1 2.5 ]; c = \"x${y}z\"; }; in a.b ++ [ (a.c or null) ]",
        "{ x, ... }@args: ''\n  é ${toString x} ''$\n''",
        "(null} ,; ''",
        "日本語 /* unterminated",
    ];

    /// Inputs that parse, with an expression in every kind of place.
    const VALID: &[&str] = &[
        "1",
        "let a = { b = [ 1 2.5 ]; c = \"x${y}z\"; }; in a.b ++ [ (a.c or null) ]",
        "{ x, y ? 1, ... }@args: ''\n  é ${toString x} ''$\n''",
        "rec { a = ./x; b = a; inherit (a) c; \"d\" = -1; ${e} = a ? f.g; }",
        "x: { f = y: x.${y} or <nixpkgs>; inherit x; } // { or = 1; }",
    ];

    /// Every output of `mutation` on [`VALID`] over a few seeds, checking
    /// that each still parses. The seeds are spread out, as the first draws
    /// from small ones are alike.
    fn valid_outputs(mutation: fn(&str, &[Element], &mut Rng) -> String) -> Vec<String> {
        let mut outs = Vec::new();
        for input in VALID {
            assert!(rnix::Root::parse(input).errors().is_empty(), "{input:?}");
            let elements = elements(input);
            for seed in 0..200u32 {
                let out = mutation(
                    input,
                    &elements,
                    &mut Rng::new(seed.wrapping_mul(0x9e37_79b9)),
                );
                let errors = rnix::Root::parse(&out).errors().to_vec();
                assert!(errors.is_empty(), "{input:?} -> {out:?}: {errors:?}");
                outs.push(out);
            }
        }
        outs
    }

    #[test]
    fn injected_errors_parse() {
        let outs = valid_outputs(inject_error);
        for error in ERRORS {
            assert!(outs.iter().any(|o| o.contains(error)), "never {error}");
        }
    }

    #[test]
    fn magic_attributes_parse() {
        let outs = valid_outputs(add_magic);
        for magic in MAGIC {
            let name = magic.split(" = ").next().unwrap();
            assert!(outs.iter().any(|o| o.contains(name)), "never {name}");
        }
        let overrides: Vec<_> = outs.iter().filter(|o| o.contains("__overrides")).collect();
        assert!(!overrides.is_empty());
        assert!(
            overrides.iter().all(|o| o.contains("rec ")),
            "{overrides:?}"
        );
    }

    #[test]
    fn rec_toggles() {
        valid_outputs(toggle_rec);
        let toggle = |s: &str| toggle_rec(s, &elements(s), &mut Rng::new(1));
        assert_eq!(toggle("{ a = 1; }"), "rec { a = 1; }");
        assert_eq!(toggle("rec { a = 1; }"), " { a = 1; }");
    }

    #[test]
    fn find_file_bindings_parse() {
        let outs = valid_outputs(wrap_find_file);
        for binding in [FIND_FILE, NIX_PATH].concat() {
            let name = binding.replace("{}", "");
            assert!(
                outs.iter().any(|o| o.contains(name.trim_end_matches(';'))),
                "never {binding}"
            );
        }
    }

    fn no_fallback(_: &mut [u8], _: usize, _: usize) -> usize {
        panic!("fell back to libFuzzer's mutation")
    }

    #[test]
    fn tree_mutations_keep_text_valid() {
        for input in INPUTS {
            for seed in 0..500 {
                let mut data = input.as_bytes().to_vec();
                data.resize(4096, 0);
                // Seeds whose first draw picks the fallback leave the input alone.
                let size = mutate(&mut data, input.len(), 4096, seed, |_, size, _| size);
                assert!(size <= 4096);
                std::str::from_utf8(&data[..size])
                    .unwrap_or_else(|e| panic!("{input:?} with seed {seed}: {e}"));
            }
        }
    }

    #[test]
    fn tree_mutations_insert_every_token() {
        let mut missing = TOKENS.to_vec();
        for seed in 0..20000 {
            let out = mutate_tree("1", &mut Rng::new(seed));
            missing.retain(|token| out != format!("1 {token} "));
        }
        assert!(missing.is_empty(), "never inserted: {missing:?}");
    }

    #[test]
    fn mutation_respects_max_size() {
        let input = INPUTS[2];
        for seed in 0..500 {
            let mut data = input.as_bytes().to_vec();
            let max_size = input.len();
            let size = mutate(&mut data, input.len(), max_size, seed, |_, size, _| size);
            assert!(size <= max_size, "seed {seed}");
        }
    }

    #[test]
    fn non_utf8_falls_back() {
        let mut data = vec![0xff, 0xfe, 0, 0];
        for seed in 0..100 {
            assert_eq!(mutate(&mut data, 2, 4, seed, |_, _, _| 3), 3);
        }
        // A UTF-8 input whose first draw picks a tree mutation never falls back.
        let mut data = b"1 + 2".to_vec();
        data.resize(4096, 0);
        let seed = (0..)
            .find(|&s| Rng::new(s).below(2) == 0)
            .expect("some seed picks a tree mutation");
        mutate(&mut data, 5, 4096, seed, no_fallback);
    }

    #[test]
    fn tree_mutations_keep_bundle_directives() {
        let bundle = "#~flake path:@dir(r)#x\n#~tree r path\n#~file flake.nix\n{ outputs = _: { x = [ 1 2 ]; }; }\n#~file flake.lock\n{ }\n";
        let directives = |s: &str| -> Vec<String> {
            s.lines()
                .filter(|l| l.starts_with("#~"))
                .map(str::to_owned)
                .collect()
        };
        for seed in 0..500 {
            let mut data = bundle.as_bytes().to_vec();
            data.resize(4096, 0);
            let size = mutate(&mut data, bundle.len(), 4096, seed, |_, size, _| size);
            let out = std::str::from_utf8(&data[..size]).unwrap();
            assert_eq!(directives(out), directives(bundle), "seed {seed}");
            assert!(out.ends_with("{ }\n"), "seed {seed}: {out:?}");
        }
    }

    #[test]
    fn crossover_fits_and_splices_text() {
        for a in INPUTS {
            for b in INPUTS {
                for seed in 0..50 {
                    let mut out = vec![0; 4096];
                    let len = crossover(a.as_bytes(), b.as_bytes(), &mut out, seed);
                    std::str::from_utf8(&out[..len])
                        .unwrap_or_else(|e| panic!("{a:?} x {b:?} with seed {seed}: {e}"));
                    let mut small = vec![0; 3];
                    assert!(crossover(a.as_bytes(), b.as_bytes(), &mut small, seed) <= 3);
                }
            }
        }
        let mut out = vec![0; 16];
        let len = crossover(&[0xff, 1], &[2, 0xfe], &mut out, 7);
        assert!(len <= 4);
    }
}
