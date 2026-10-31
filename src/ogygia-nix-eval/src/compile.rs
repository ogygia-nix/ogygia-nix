//! Lowering of the rnix syntax tree into [`crate::ir`].

use std::borrow::Cow;
use std::collections::HashMap;

use num_bigint::BigUint;
use rnix::ast;
use rnix::ast::AstToken;
use rnix::ast::HasEntry;
use rnix::ast::InterpolPart;
use rowan::ast::AstNode;

use crate::builtins::Builtin;
use crate::context::Context;
use crate::ir::AttrKey;
use crate::ir::AttrValue;
use crate::ir::AttrsDef;
use crate::ir::BinOp;
use crate::ir::DynamicAttr;
use crate::ir::Expr;
use crate::ir::ExprRef;
use crate::ir::Formal;
use crate::ir::LambdaDef;
use crate::ir::LetDef;
use crate::ir::Param;
use crate::ir::Pos;
use crate::ir::Source;
use crate::ir::StaticAttr;
use crate::path::canon_path;
use crate::symbol::Sym;
use crate::value::NixStr;
use crate::value::PathV;

/// A parse or scoping error.
#[derive(Debug)]
pub struct CompileError {
    pub msg: String,
}

type CResult<T> = Result<T, CompileError>;

fn err<T>(msg: impl Into<String>) -> CResult<T> {
    Err(CompileError { msg: msg.into() })
}

enum Scope {
    Names(HashMap<Sym, u32>),
    With,
}

struct Compiler<'a> {
    ctx: &'a Context,
    source: &'a Source<'a>,
    /// Offsets of the bytes [`match_nix_tokens`] inserted.
    inserted: Vec<u32>,
    /// Directory relative path literals are resolved against.
    base_dir: &'a str,
    pure: bool,
    scopes: Vec<Scope>,
}

/// Parse and compile `source`. `extra_scope` names are bound in an outermost
/// frame (`scopedImport`). In `pure` mode, `~/` paths are a compile error, as
/// Nix rejects them while parsing.
pub fn compile<'a>(
    ctx: &'a Context,
    source: &'a Source<'a>,
    base_dir: &str,
    pure: bool,
    extra_scope: Option<&[Sym]>,
) -> CResult<ExprRef<'a>> {
    let text = end_comments_at_cr(source.text);
    let (text, inserted) = match_nix_tokens(&text);
    let (text, inserted) = parenthesise_legacy_lets(text, inserted);
    let (text, inserted) = parenthesise_inverts(text, inserted);
    let pos = |offset: rnix::TextSize| Pos {
        source,
        offset: source_offset(&inserted, u32::from(offset)),
    };
    if let Some(at) = invalid_whitespace(&text) {
        return err(format!(
            "syntax error: unexpected invalid token at {}",
            pos(at.into())
        ));
    }
    let parse = rnix::Root::parse(&text);
    if let Some(e) = parse.errors().first() {
        let range = match e {
            rnix::ParseError::Unexpected(r)
            | rnix::ParseError::UnexpectedExtra(r)
            | rnix::ParseError::UnexpectedWanted(_, r, _)
            | rnix::ParseError::UnexpectedDoubleBind(r)
            | rnix::ParseError::DuplicatedArgs(r, _) => Some(*r),
            _ => None,
        };
        let at = range
            .map(|r| format!(" at {}", pos(r.start())))
            .unwrap_or_default();
        return err(format!("syntax error: {e}{at}"));
    }
    // rnix closes a parenthesised expression with whatever token follows
    // it, without reporting an error when that is not `)`.
    if let Some(t) = parse
        .syntax()
        .descendants()
        .filter(|n| n.kind() == rnix::SyntaxKind::NODE_PAREN)
        .filter_map(|n| n.last_token())
        .find(|t| t.kind() != rnix::SyntaxKind::TOKEN_R_PAREN)
    {
        return err(format!(
            "syntax error: unexpected {}, expecting ')' at {}",
            t.text(),
            pos(t.text_range().start())
        ));
    }
    // rnix parses expressions in `${ ... }` in an attribute name until `}`,
    // without reporting an error when there is more than one.
    if let Some(n) = parse
        .syntax()
        .descendants()
        .filter(|n| n.kind() == rnix::SyntaxKind::NODE_DYNAMIC)
        .find_map(|n| n.children().nth(1))
    {
        let t = n.first_token().map(|t| t.to_string()).unwrap_or_default();
        return err(format!(
            "syntax error: unexpected {t}, expecting '}}' at {}",
            pos(n.text_range().start())
        ));
    }
    // rnix parses a function wherever it parses an operand, but Nix only
    // allows one unparenthesised outside operators, lists and selects.
    if let Some(n) = parse.syntax().descendants().find(|n| {
        n.kind() == rnix::SyntaxKind::NODE_LAMBDA
            && n.parent().is_some_and(|p| {
                matches!(
                    p.kind(),
                    rnix::SyntaxKind::NODE_LIST
                        | rnix::SyntaxKind::NODE_BIN_OP
                        | rnix::SyntaxKind::NODE_UNARY_OP
                        | rnix::SyntaxKind::NODE_SELECT
                        | rnix::SyntaxKind::NODE_HAS_ATTR
                        | rnix::SyntaxKind::NODE_APPLY
                )
            })
    }) {
        return err(format!(
            "syntax error: unexpected function at {}",
            pos(n.text_range().start())
        ));
    }
    if let Some(at) = parse
        .syntax()
        .descendants()
        .find_map(|n| interpolation_after_path_trivia(&n))
    {
        return err(format!("syntax error: unexpected '${{' at {}", pos(at)));
    }
    if let Some(at) = parse
        .syntax()
        .descendants()
        .filter_map(ast::Path::cast)
        .find_map(|p| path_segments_before_interpolation(&p))
    {
        return err(format!(
            "syntax error: unexpected string, expecting '${{' at {}",
            pos(at)
        ));
    }
    intern_names(ctx, &parse.syntax());
    let root = parse.tree();
    let expr = child(root.expr())?;
    let mut c = Compiler {
        ctx,
        source,
        inserted,
        base_dir: ctx.alloc_str(base_dir),
        pure,
        scopes: Vec::new(),
    };
    if let Some(names) = extra_scope {
        c.scopes.push(scope_of(names));
    }
    c.expr(&expr)
}

/// Attribute sets are ordered by symbol, so names are interned in the order
/// Nix's parser interns them: in source order, except that a function's
/// parameter name follows its body and a formal's name follows its default.
fn intern_names(ctx: &Context, node: &rnix::SyntaxNode) {
    use rnix::SyntaxKind::*;
    let intern_ident = |n: &rnix::SyntaxNode| {
        if let Some(tok) = n.first_token() {
            ctx.intern(tok.text());
        }
    };
    match node.kind() {
        NODE_IDENT => intern_ident(node),
        NODE_STRING | NODE_DYNAMIC
            if node
                .parent()
                .is_some_and(|p| matches!(p.kind(), NODE_ATTRPATH | NODE_INHERIT)) =>
        {
            let lit = match ast::Attr::cast(node.clone()) {
                Some(ast::Attr::Str(s)) => static_str(&s),
                Some(ast::Attr::Dynamic(d)) => d.expr().and_then(|e| literal_name(&e)),
                _ => None,
            };
            match lit {
                Some(lit) => {
                    ctx.intern(&lit);
                }
                None => node.children().for_each(|c| intern_names(ctx, &c)),
            }
        }
        NODE_LAMBDA => {
            let mut params = Vec::new();
            for c in node.children() {
                match c.kind() {
                    NODE_IDENT_PARAM => params.extend(c.children()),
                    NODE_PATTERN => {
                        for p in c.children() {
                            match p.kind() {
                                NODE_PAT_BIND => params.extend(p.children()),
                                _ => intern_names(ctx, &p),
                            }
                        }
                    }
                    _ => intern_names(ctx, &c),
                }
            }
            params.iter().for_each(intern_ident);
        }
        NODE_PAT_ENTRY => {
            let mut children = node.children();
            let name = children.next();
            children.for_each(|c| intern_names(ctx, &c));
            name.iter().for_each(intern_ident);
        }
        _ => node.children().for_each(|c| intern_names(ctx, &c)),
    }
}

/// The name of a `${...}` attribute that Nix treats as static: a string
/// literal such as `${"lit"}`, `${(f:o)}` or `${''lit''}`, or an indented string
/// whose only piece interpolates one, such as `${''${"lit"}''}`.
fn literal_name(e: &ast::Expr) -> Option<String> {
    match e {
        ast::Expr::Paren(p) => literal_name(&p.expr()?),
        ast::Expr::Str(s) if s.syntax().first_token().is_some_and(|t| t.text() == "''") => {
            match <[_; 1]>::try_from(str_parts(s)) {
                Ok([InterpolPart::Interpolation(i)]) => literal_name(&i.expr()?),
                _ => static_str(s),
            }
        }
        ast::Expr::Str(s) => static_str(s),
        ast::Expr::Literal(l) => match l.kind() {
            ast::LiteralKind::Uri(u) => Some(u.syntax().text().to_owned()),
            _ => None,
        },
        _ => None,
    }
}

/// rnix lexes any Unicode whitespace between tokens as whitespace, but Nix
/// only space, tab, `\r` and `\n`, and any other character there is an invalid
/// token. Returns the offset of the first other whitespace character.
fn invalid_whitespace(text: &str) -> Option<u32> {
    let mut start = 0;
    rnix::tokenize(text).find_map(|(kind, s)| {
        let at = start;
        start += s.len();
        if kind != rnix::SyntaxKind::TOKEN_WHITESPACE {
            return None;
        }
        s.find(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
            .map(|i| (at + i) as u32)
    })
}

/// Nix ends a `#` comment at `\r` as well as `\n`, but rnix only at `\n`.
/// Rewrites each `\r` that ends a comment to `\n`, which keeps offsets and
/// changes nothing else, then retokenises as the text after it is now code.
fn end_comments_at_cr(text: &str) -> Cow<'_, str> {
    let mut text = Cow::Borrowed(text);
    if !text.contains('\r') {
        return text;
    }
    loop {
        let mut start = 0;
        let cr = rnix::tokenize(&text).find_map(|(kind, s)| {
            let at = start;
            start += s.len();
            if kind == rnix::SyntaxKind::TOKEN_COMMENT && s.starts_with('#') {
                s.find('\r').map(|i| at + i)
            } else {
                None
            }
        });
        match cr {
            Some(i) => text.to_mut().replace_range(i..=i, "\n"),
            None => return text,
        }
    }
}

/// Inserts text wherever rnix lexes the source differently from Nix: a space
/// where rnix lexes one token but Nix lexes several or rnix parses a token as
/// part of the path before it but Nix does not, `./` before an ellipsis
/// Nix lexes as the start of a path, and an empty interpolation between the
/// slashes of `//` in a path continued after an interpolation. Retokenises after each as the text after
/// it changes, and returns the offsets of the inserted bytes in the result.
fn match_nix_tokens(text: &str) -> (Cow<'_, str>, Vec<u32>) {
    let mut text = Cow::Borrowed(text);
    let mut inserted = Vec::new();
    if !text.contains(['/', '.', '<']) {
        return (text, inserted);
    }
    loop {
        let mut start = 0;
        let mut prev = None;
        // Whether each open interpolation is part of a path.
        let mut interpols = Vec::new();
        let mut closed_path = false;
        let insert = rnix::tokenize(&text).find_map(|(kind, s)| {
            use rnix::SyntaxKind::*;
            let at = start;
            start += s.len();
            let before = prev.replace(kind);
            let continues_path = before == Some(TOKEN_INTERPOL_END) && closed_path;
            match kind {
                TOKEN_INTERPOL_START => interpols.push(
                    matches!(
                        before,
                        Some(TOKEN_PATH_ABS | TOKEN_PATH_REL | TOKEN_PATH_HOME)
                    ) || continues_path,
                ),
                TOKEN_INTERPOL_END => closed_path = interpols.pop().unwrap_or(false),
                _ => {}
            }
            split_division(kind, s, continues_path)
                .or_else(|| split_number(kind, s))
                .or_else(|| split_less(kind, s))
                .or_else(|| split_home_path(kind, s, before))
                .map(|i| (at + i, " "))
                .or_else(|| ellipsis_path(kind, &text[at..]).then_some((at, "./")))
                .or_else(|| {
                    empty_path_segment(kind, s, continues_path).map(|i| (at + i, "${\"\"}"))
                })
        });
        match insert {
            Some((i, s)) => {
                text.to_mut().insert_str(i, s);
                inserted.extend((i..i + s.len()).map(|i| i as u32));
            }
            None => return (text, inserted),
        }
    }
}

/// Nix parses `let { ... }` as an operand, so it can be selected from,
/// applied or used with an operator unparenthesised, but rnix only parses it
/// where it parses a function. Wraps each in parentheses, and returns the
/// result with `inserted` updated to also hold the offsets of the parentheses.
/// One right after `inherit` is left alone, as Nix rejects it there but
/// accepts the parenthesised one as the source of an inherit-from.
fn parenthesise_legacy_lets(text: Cow<'_, str>, inserted: Vec<u32>) -> (Cow<'_, str>, Vec<u32>) {
    let tokens = significant_tokens(&text);
    let mut parens = Vec::new();
    for (i, w) in tokens.windows(2).enumerate() {
        let [
            (rnix::SyntaxKind::TOKEN_LET, at, _),
            (rnix::SyntaxKind::TOKEN_L_BRACE, _, _),
        ] = *w
        else {
            continue;
        };
        if i.checked_sub(1).map(|j| tokens[j].0) == Some(rnix::SyntaxKind::TOKEN_INHERIT) {
            continue;
        }
        let mut depth = 0usize;
        let close = tokens[i + 1..].iter().find_map(|&(kind, _, end)| {
            match kind {
                rnix::SyntaxKind::TOKEN_L_BRACE | rnix::SyntaxKind::TOKEN_INTERPOL_START => {
                    depth += 1
                }
                rnix::SyntaxKind::TOKEN_R_BRACE | rnix::SyntaxKind::TOKEN_INTERPOL_END => {
                    depth -= 1
                }
                _ => {}
            }
            (depth == 0).then_some(end)
        });
        if let Some(close) = close {
            parens.extend([(at, "("), (close, ")")]);
        }
    }
    insert_parens(text, inserted, parens)
}

/// Nix parses `!` as the operand of an operator that binds tighter than it
/// (`+`, `-`, `*`, `/` and `++`, and unary `-`), with the operand of `!`
/// extending over every operator that also binds tighter, so `a + !b + c` is
/// `a + !(b + c)`, but rnix rejects `!` there. Wraps each such `!` and its
/// operand in parentheses, and returns the result with `inserted` updated to
/// also hold the offsets of the parentheses. The operand includes error
/// tokens, as rnix lexes one such as the trailing `/` of `./a/${b}/ c` as
/// division after a closing parenthesis.
fn parenthesise_inverts(text: Cow<'_, str>, inserted: Vec<u32>) -> (Cow<'_, str>, Vec<u32>) {
    use rnix::SyntaxKind::*;
    let tokens = significant_tokens(&text);
    let mut parens = Vec::new();
    for (i, w) in tokens.windows(2).enumerate() {
        let [
            (TOKEN_ADD | TOKEN_SUB | TOKEN_MUL | TOKEN_DIV | TOKEN_CONCAT, _, _),
            (TOKEN_INVERT, at, _),
        ] = *w
        else {
            continue;
        };
        let mut depth = 0usize;
        let operand = tokens[i + 2..]
            .iter()
            .take_while(|&&(kind, _, _)| match kind {
                TOKEN_L_PAREN | TOKEN_L_BRACK | TOKEN_L_BRACE | TOKEN_INTERPOL_START
                | TOKEN_STRING_START => {
                    depth += 1;
                    true
                }
                TOKEN_R_PAREN | TOKEN_R_BRACK | TOKEN_R_BRACE | TOKEN_INTERPOL_END
                | TOKEN_STRING_END => depth.checked_sub(1).map(|d| depth = d).is_some(),
                _ if depth > 0 => true,
                TOKEN_IDENT | TOKEN_INTEGER | TOKEN_FLOAT | TOKEN_URI | TOKEN_CUR_POS
                | TOKEN_PATH_ABS | TOKEN_PATH_HOME | TOKEN_PATH_REL | TOKEN_PATH_SEARCH
                | TOKEN_REC | TOKEN_DOT | TOKEN_OR | TOKEN_QUESTION | TOKEN_CONCAT | TOKEN_MUL
                | TOKEN_DIV | TOKEN_ADD | TOKEN_SUB | TOKEN_INVERT | TOKEN_ERROR => true,
                _ => false,
            })
            .last();
        if let Some(&(_, _, close)) = operand {
            parens.extend([(at, "("), (close, ")")]);
        }
    }
    insert_parens(text, inserted, parens)
}

/// The kind and start and end offsets of each token of `text` that is not
/// trivia.
fn significant_tokens(text: &str) -> Vec<(rnix::SyntaxKind, usize, usize)> {
    let mut tokens = Vec::new();
    let mut start = 0;
    for (kind, s) in rnix::tokenize(text) {
        if !kind.is_trivia() {
            tokens.push((kind, start, start + s.len()));
        }
        start += s.len();
    }
    tokens
}

/// Inserts each of `parens` into `text` at its offset, and returns the result
/// with `inserted` updated to also hold their offsets.
fn insert_parens<'a>(
    text: Cow<'a, str>,
    mut inserted: Vec<u32>,
    mut parens: Vec<(usize, &str)>,
) -> (Cow<'a, str>, Vec<u32>) {
    if parens.is_empty() {
        return (text, inserted);
    }
    // Inserting from the end keeps the offsets still to insert at valid, and
    // `(` before `)` at the same offset leaves `)(` there.
    parens.sort_by_key(|&(i, s)| (std::cmp::Reverse(i), s));
    let mut text = text.into_owned();
    for (i, s) in parens {
        text.insert_str(i, s);
        let i = i as u32;
        inserted
            .iter_mut()
            .filter(|o| **o >= i)
            .for_each(|o| *o += 1);
        inserted.push(i);
    }
    inserted.sort_unstable();
    (Cow::Owned(text), inserted)
}

/// Nix only continues a path with an interpolation directly after it, so
/// `./a ${b}` is a syntax error, but rnix parses whitespace and comments
/// between them as part of the path. Returns where such an interpolation
/// starts.
fn interpolation_after_path_trivia(node: &rnix::SyntaxNode) -> Option<rnix::TextSize> {
    use rnix::SyntaxKind::*;
    if !matches!(node.kind(), NODE_PATH_ABS | NODE_PATH_REL | NODE_PATH_HOME) {
        return None;
    }
    node.children_with_tokens()
        .skip_while(|c| !c.kind().is_trivia())
        .find(|c| c.kind() == NODE_INTERPOL)
        .map(|c| c.text_range().start())
}

/// Nix lexes a path followed by an interpolation as the longest prefix that
/// is a path without `//`, then the rest before the interpolation as strings
/// split at each `//`, and only accepts one string there. So `./a//b/${x}` is
/// `./a/` `/b/` `x`, but `./a///b/${x}` and `./a//b//c/${x}` are syntax
/// errors, which rnix accepts. Returns where the second such string starts.
fn path_segments_before_interpolation(path: &ast::Path) -> Option<rnix::TextSize> {
    let mut parts = path.parts().into_iter();
    let InterpolPart::Literal(first) = parts.next()? else {
        return None;
    };
    parts.next()?;
    let text = first.syntax().text();
    let mut slashes = (0..text.len()).filter(|&i| text[i..].starts_with("//"));
    slashes.next()?;
    let second = slashes.next()?;
    Some(first.syntax().text_range().start() + rnix::TextSize::from(second as u32 + 1))
}

/// Nix lexes each `/` that ends a segment of a path continued after an
/// interpolation as its own string, so `./a/${b}//c` is `./a/` `b` `/` `/c`,
/// but rnix lexes the rest of the path as one token, an error when it contains
/// `//`. Returns the offset of the second `/` in such a token, where an empty
/// interpolation makes rnix lex the same strings.
fn empty_path_segment(kind: rnix::SyntaxKind, s: &str, continues_path: bool) -> Option<usize> {
    (kind == rnix::SyntaxKind::TOKEN_ERROR && continues_path)
        .then(|| s.find("//"))
        .flatten()
        .map(|i| i + 1)
}

/// Nix lexes `...` followed by path characters and a `/` that continues the
/// path, as in `.../a`, as a relative path, but rnix lexes `...` as an
/// ellipsis. Returns whether `rest` starts with such an ellipsis token, which
/// `./` before it makes rnix lex as the same relative path.
fn ellipsis_path(kind: rnix::SyntaxKind, rest: &str) -> bool {
    let path_char = |c: &u8| c.is_ascii_alphanumeric() || b"._-+".contains(c);
    let b = rest.as_bytes();
    kind == rnix::SyntaxKind::TOKEN_ELLIPSIS
        && b.iter().position(|c| !path_char(c)).is_some_and(|i| {
            b[i] == b'/' && (b.get(i + 1).is_some_and(path_char) || b[i + 1..].starts_with(b"${"))
        })
}

/// rnix lexes `a/` followed by a character that cannot continue a path, as in
/// `a/(b)` or `a/"b"`, as a path with a trailing slash, but Nix only lexes a
/// path when a path character or `${` follows the `/`, so this is `a / ...`.
/// Returns the offset of that `/` in the token.
fn split_division(kind: rnix::SyntaxKind, s: &str, continues_path: bool) -> Option<usize> {
    // A path continued after an interpolation, as in `./a${b}c/`, has a
    // trailing slash in Nix too. After any other interpolation, as in
    // `a.${b}.c/(d)`, the token starts a new path, as `a/(b)` does.
    (kind == rnix::SyntaxKind::TOKEN_ERROR
        && !continues_path
        && !s.starts_with('~')
        && s.find('/') == Some(s.len() - 1))
    .then_some(s.len() - 1)
}

/// rnix parses a home path directly after a path continued after an
/// interpolation, as in `./a/${b}~/c` or `./a/${b}.~/c`, as the rest of that
/// path, but `~` cannot continue a path in Nix, so this is `./a/${b}` (or
/// `./a/${b}.`) then `~/c`. Returns the offset of such a home path, where a
/// space makes rnix parse the same two paths.
fn split_home_path(
    kind: rnix::SyntaxKind,
    s: &str,
    prev: Option<rnix::SyntaxKind>,
) -> Option<usize> {
    use rnix::SyntaxKind::*;
    (kind == TOKEN_PATH_HOME
        && s.starts_with('~')
        && matches!(
            prev,
            Some(TOKEN_INTERPOL_END | TOKEN_PATH_ABS | TOKEN_PATH_REL | TOKEN_PATH_HOME)
        ))
    .then_some(0)
}

/// Nix lexes `<` as the start of a search path only when path segments
/// separated by single slashes and then `>` follow it, and otherwise as less
/// than. rnix also lexes a search path with an empty segment, as in `<a//b>`,
/// and an error token when another `<` follows, as in `<a/b<c>`, which Nix
/// lexes as `< a/b <c>`. Returns the offset after such a `<`.
fn split_less(kind: rnix::SyntaxKind, s: &str) -> Option<usize> {
    let search_path = s
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .is_some_and(|s| {
            s.split('/').all(|seg| {
                !seg.is_empty()
                    && seg
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-+".contains(&c))
            })
        });
    (matches!(
        kind,
        rnix::SyntaxKind::TOKEN_PATH_SEARCH | rnix::SyntaxKind::TOKEN_ERROR
    ) && s.starts_with('<')
        && !search_path)
        .then_some(1)
}

/// Nix lexes a float as `(([1-9][0-9]*\.[0-9]*)|(0?\.[0-9]+))([Ee][+-]?[0-9]+)?`
/// and an integer as `[0-9]+`, but rnix lexes `0.` followed by a non-digit as
/// a float and an exponent marker without digits, as in `1.5e`, as an error,
/// where Nix lexes `0 .x` and `1.5 e`. Returns where Nix's number ends in a
/// number token rnix lexed longer.
fn split_number(kind: rnix::SyntaxKind, s: &str) -> Option<usize> {
    let numeric = match kind {
        rnix::SyntaxKind::TOKEN_FLOAT => true,
        rnix::SyntaxKind::TOKEN_ERROR => {
            s.starts_with(|c: char| c.is_ascii_digit() || c == '.')
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || b".eE+-".contains(&c))
        }
        _ => false,
    };
    if !numeric {
        return None;
    }
    let b = s.as_bytes();
    let digits = |from: usize| from + b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let int = digits(0);
    let mantissa = if b.first().is_some_and(|c| (b'1'..=b'9').contains(c)) {
        (b.get(int) == Some(&b'.')).then(|| digits(int + 1))
    } else {
        let dot = usize::from(b.first() == Some(&b'0'));
        (b.get(dot) == Some(&b'.') && b.get(dot + 1).is_some_and(u8::is_ascii_digit))
            .then(|| digits(dot + 1))
    };
    let len = match mantissa {
        Some(m) if matches!(b.get(m), Some(b'e' | b'E')) => {
            let sign = m + 1 + usize::from(matches!(b.get(m + 1), Some(b'+' | b'-')));
            match digits(sign) {
                end if end > sign => end,
                _ => m,
            }
        }
        Some(m) => m,
        None => int,
    };
    (0 < len && len < s.len()).then_some(len)
}

/// Maps an offset in the text [`match_nix_tokens`] returned to the source.
fn source_offset(inserted: &[u32], offset: u32) -> u32 {
    offset - inserted.partition_point(|&s| s < offset) as u32
}

/// The parts of an attribute set or `let` before compilation, with nested
/// attribute paths merged.
#[derive(Default)]
struct PreSet {
    statics: Vec<(Sym, u32, PreAttr)>,
    dynamics: Vec<(ast::Expr, u32, PreAttr)>,
    /// `inherit (from) names;` sources.
    froms: Vec<ast::Expr>,
}

enum PreAttr {
    Expr(ast::Expr),
    /// A set merged from several definitions, and whether it is `rec`.
    Nested(PreSet, bool),
    /// `inherit name;`, resolved in the enclosing scope.
    Inherit(Sym, u32),
    /// `inherit (froms[i]) name;`
    InheritFrom(usize, Sym, u32),
}

enum AttrName {
    Static(Sym),
    Dynamic(ast::Expr),
}

fn offset(node: &impl AstNode<Language = rnix::NixLanguage>) -> u32 {
    u32::from(node.syntax().text_range().start())
}

impl<'a> Compiler<'a> {
    fn at(&self, offset: u32) -> Pos<'a> {
        Pos {
            source: self.source,
            offset: source_offset(&self.inserted, offset),
        }
    }

    fn pos(&self, node: &impl AstNode<Language = rnix::NixLanguage>) -> Pos<'a> {
        self.at(offset(node))
    }

    fn leak(&self, e: Expr<'a>) -> ExprRef<'a> {
        self.ctx.alloc(e)
    }

    fn lit_str(&self, b: &[u8]) -> &'a NixStr<'a> {
        self.ctx.alloc(NixStr {
            s: self.ctx.alloc_bytes(b),
            ctx: &[],
        })
    }

    fn name(&self, s: Sym) -> &'a str {
        self.ctx.name(s)
    }

    fn resolve(&self, name: Sym, skip: usize, pos: Pos<'a>) -> CResult<Expr<'a>> {
        let n = self.scopes.len();
        let mut withs = Vec::new();
        for (depth, scope) in self.scopes[..n - skip.min(n)].iter().rev().enumerate() {
            let depth = depth + skip;
            match scope {
                Scope::Names(names) => {
                    if let Some(&idx) = names.get(&name) {
                        return Ok(Expr::Var(depth as u32, idx));
                    }
                }
                Scope::With => withs.push(depth as u32),
            }
        }
        // The global scope is static, so `with` cannot shadow it.
        let s = &self.ctx.syms;
        if name == s.true_ {
            return Ok(Expr::Bool(true));
        } else if name == s.false_ {
            return Ok(Expr::Bool(false));
        } else if name == s.null {
            return Ok(Expr::Null);
        }
        if let Some(b) = Builtin::in_scope(self.name(name), self.pure) {
            return Ok(Expr::Global(b));
        }
        if withs.is_empty() {
            return err(format!("undefined variable '{}' at {pos}", self.name(name)));
        }
        Ok(Expr::WithVar {
            name,
            withs: self.ctx.alloc_slice(withs),
            pos,
        })
    }

    fn expr(&mut self, e: &ast::Expr) -> CResult<ExprRef<'a>> {
        let e = self.expr_inner(e)?;
        Ok(self.leak(e))
    }

    fn exprs(&mut self, es: &[ast::Expr]) -> CResult<&'a [ExprRef<'a>]> {
        let v = es
            .iter()
            .map(|e| self.expr(e))
            .collect::<CResult<Vec<_>>>()?;
        Ok(self.ctx.alloc_slice(v))
    }

    fn expr_inner(&mut self, e: &ast::Expr) -> CResult<Expr<'a>> {
        let pos = self.pos(e);
        Ok(match e {
            ast::Expr::Paren(p) => return self.expr_inner(&child(p.expr())?),
            ast::Expr::Root(r) => return self.expr_inner(&child(r.expr())?),
            ast::Expr::Error(_) => return err(format!("syntax error at {pos}")),
            ast::Expr::Literal(l) => match l.kind() {
                ast::LiteralKind::Integer(i) => match i.value() {
                    Ok(v) => Expr::Int(v),
                    Err(_) => {
                        return err(format!("invalid integer '{}' at {pos}", i.syntax().text()));
                    }
                },
                ast::LiteralKind::Float(f) => match parse_float(f.syntax().text()) {
                    Some(v) => Expr::Float(v),
                    None => {
                        return err(format!("invalid float '{}' at {pos}", f.syntax().text()));
                    }
                },
                ast::LiteralKind::Uri(u) => Expr::Str(self.lit_str(u.syntax().text().as_bytes())),
            },
            ast::Expr::Ident(i) => {
                if i.syntax().text() == "__curPos" {
                    return Ok(Expr::CurPos(pos));
                }
                let name = self.ident(i)?;
                self.resolve(name, 0, pos)?
            }
            ast::Expr::CurPos(_) => Expr::CurPos(pos),
            ast::Expr::Str(s) => self.string(s)?,
            ast::Expr::PathAbs(_) | ast::Expr::PathRel(_) | ast::Expr::PathHome(_) => {
                let p = ast::Path::cast(e.syntax().clone()).unwrap();
                self.path(&p, pos)?
            }
            ast::Expr::PathSearch(p) => {
                // `<name>` is `__findFile __nixPath "name"`, resolved lexically.
                let text = p.content().map(|c| c.text().to_owned()).unwrap_or_default();
                let name = text.trim_start_matches('<').trim_end_matches('>');
                let s = &self.ctx.syms;
                let nix_path = self.leak(self.resolve(s.nix_path, 0, pos)?);
                let name = self.leak(Expr::Str(self.lit_str(name.as_bytes())));
                self.call(s.find_file, &[nix_path, name], pos)?
            }
            ast::Expr::List(l) => {
                let items: Vec<ast::Expr> = l.items().collect();
                Expr::List(self.exprs(&items)?)
            }
            ast::Expr::Apply(a) => {
                // Flatten `f a b c` into one application node.
                let mut args = vec![child(a.argument())?];
                let mut func = child(a.lambda())?;
                while let ast::Expr::Apply(inner) = &func {
                    args.push(child(inner.argument())?);
                    func = child(inner.lambda())?;
                }
                args.reverse();
                Expr::Apply {
                    func: self.expr(&func)?,
                    args: self.exprs(&args)?,
                    pos,
                }
            }
            ast::Expr::Select(s) => {
                let expr = self.expr(&child(s.expr())?)?;
                let attrpath = child(s.attrpath())?;
                let path = self.attrpath(&attrpath)?;
                let default = match s.default_expr() {
                    Some(d) => Some(self.expr(&d)?),
                    None => None,
                };
                Expr::Select {
                    expr,
                    path,
                    default,
                    pos: self.pos(&attrpath),
                }
            }
            ast::Expr::HasAttr(h) => Expr::HasAttr {
                expr: self.expr(&child(h.expr())?)?,
                path: self.attrpath(&child(h.attrpath())?)?,
            },
            ast::Expr::IfElse(i) => Expr::If(
                self.expr(&child(i.condition())?)?,
                self.expr(&child(i.body())?)?,
                self.expr(&child(i.else_body())?)?,
            ),
            ast::Expr::Assert(a) => Expr::Assert {
                cond: self.expr(&child(a.condition())?)?,
                body: self.expr(&child(a.body())?)?,
                pos,
            },
            ast::Expr::With(w) => {
                let namespace = self.expr(&child(w.namespace())?)?;
                self.scopes.push(Scope::With);
                let body = child(w.body()).and_then(|b| self.expr(&b));
                self.scopes.pop();
                Expr::With {
                    namespace,
                    body: body?,
                }
            }
            ast::Expr::UnaryOp(u) => {
                let inner = self.expr(&child(u.expr())?)?;
                match u.operator() {
                    Some(ast::UnaryOpKind::Invert) => Expr::Not(inner),
                    // `-e` is `__sub 0 e`.
                    Some(ast::UnaryOpKind::Negate) => {
                        let zero = self.leak(Expr::Int(0));
                        self.call(self.ctx.syms.sub, &[zero, inner], pos)?
                    }
                    None => return err(format!("syntax error: unknown unary operator at {pos}")),
                }
            }
            ast::Expr::BinOp(b) => self.binop(b, pos)?,
            ast::Expr::Lambda(l) => Expr::Lambda(self.lambda(l, None)?),
            ast::Expr::AttrSet(a) => {
                let set = self.preset(a.entries())?;
                self.attrs(set, a.rec_token().is_some())?
            }
            ast::Expr::LetIn(l) => {
                let set = self.preset(l.entries())?;
                if !set.dynamics.is_empty() {
                    return err(format!("dynamic attributes not allowed in let at {pos}"));
                }
                let b = self.bindings(set, true, |c| c.expr(&child(l.body())?))?;
                Expr::Let(self.ctx.alloc(LetDef {
                    slots: b.frame.expect("a let has a frame"),
                    body: b.body,
                }))
            }
            ast::Expr::LegacyLet(l) => {
                // `let { ...; body = e; }` is `rec { ...; body = e; }.body`.
                let set = self.preset(l.entries())?;
                let attrs = self.attrs(set, true)?;
                let body = self.ctx.syms.body;
                Expr::Select {
                    expr: self.leak(attrs),
                    path: self.ctx.alloc_slice(vec![AttrKey::Static(body)]),
                    default: None,
                    pos,
                }
            }
        })
    }

    fn binop(&mut self, b: &ast::BinOp, pos: Pos<'a>) -> CResult<Expr<'a>> {
        let Some(op) = b.operator() else {
            return err(format!("syntax error: unknown operator at {pos}"));
        };
        let lhs = self.expr(&child(b.lhs())?)?;
        let rhs = self.expr(&child(b.rhs())?)?;
        let op_pos = b
            .syntax()
            .children_with_tokens()
            .filter_map(|t| t.into_token())
            .find(|t| ast::BinOpKind::from_kind(t.kind()).is_some())
            .map(|t| self.at(u32::from(t.text_range().start())))
            .unwrap_or(pos);
        let s = &self.ctx.syms;
        // Nix desugars these operators to calls of builtins named by
        // variables, which lexical bindings can shadow. `a > b` is
        // `__lessThan b a`, which evaluates `b` first, and `a <= b` and
        // `a >= b` are the negations of `a > b` and `a < b`.
        let op = match op {
            ast::BinOpKind::Sub => return self.call(s.sub, &[lhs, rhs], op_pos),
            ast::BinOpKind::Mul => return self.call(s.mul, &[lhs, rhs], op_pos),
            ast::BinOpKind::Div => return self.call(s.div, &[lhs, rhs], op_pos),
            ast::BinOpKind::Less => return self.call(s.less_than, &[lhs, rhs], op_pos),
            ast::BinOpKind::More => return self.call(s.less_than, &[rhs, lhs], op_pos),
            ast::BinOpKind::LessOrEq => {
                let gt = self.call(s.less_than, &[rhs, lhs], op_pos)?;
                return Ok(Expr::Not(self.leak(gt)));
            }
            ast::BinOpKind::MoreOrEq => {
                let lt = self.call(s.less_than, &[lhs, rhs], op_pos)?;
                return Ok(Expr::Not(self.leak(lt)));
            }
            ast::BinOpKind::Concat => BinOp::Concat,
            ast::BinOpKind::Update => BinOp::Update,
            ast::BinOpKind::Add => BinOp::Add,
            ast::BinOpKind::And => BinOp::And,
            ast::BinOpKind::Equal => BinOp::Eq,
            ast::BinOpKind::Implication => BinOp::Impl,
            ast::BinOpKind::NotEqual => BinOp::Neq,
            ast::BinOpKind::Or => BinOp::Or,
            ast::BinOpKind::PipeRight | ast::BinOpKind::PipeLeft => {
                return err(format!(
                    "experimental Nix feature 'pipe-operators' is disabled at {op_pos}"
                ));
            }
        };
        Ok(Expr::Bin(op, lhs, rhs, op_pos))
    }

    /// A call of the function the variable `name` resolves to.
    fn call(&self, name: Sym, args: &[ExprRef<'a>], pos: Pos<'a>) -> CResult<Expr<'a>> {
        let func = self.resolve(name, 0, pos)?;
        Ok(Expr::Apply {
            func: self.leak(func),
            args: self.ctx.alloc_slice(args.to_vec()),
            pos,
        })
    }

    fn lambda(&mut self, l: &ast::Lambda, name: Option<Sym>) -> CResult<&'a LambdaDef<'a>> {
        let pos = self.pos(l);
        let (param, names) = match child(l.param())? {
            ast::Param::IdentParam(i) => {
                let n = self.ident(&child(i.ident())?)?;
                (Param::Ident(n), vec![n])
            }
            ast::Param::Pattern(p) => {
                let mut names = Vec::new();
                let mut entries = Vec::new();
                for entry in p.pat_entries() {
                    let n = self.ident(&child(entry.ident())?)?;
                    if names.contains(&n) {
                        return err(format!(
                            "duplicate formal function argument '{}' at {}",
                            self.name(n),
                            self.pos(&entry)
                        ));
                    }
                    names.push(n);
                    entries.push(entry);
                }
                let at = match p.pat_bind() {
                    Some(b) => {
                        let n = self.ident(&child(b.ident())?)?;
                        if names.contains(&n) {
                            return err(format!(
                                "duplicate formal function argument '{}' at {}",
                                self.name(n),
                                self.pos(&b)
                            ));
                        }
                        Some(n)
                    }
                    None => None,
                };
                let mut all = names.clone();
                all.extend(at);
                // Defaults see every formal and the `@` binding.
                self.scopes.push(scope_of(&all));
                let formals = entries
                    .iter()
                    .zip(&names)
                    .map(|(entry, &name)| {
                        Ok(Formal {
                            name,
                            default: match entry.default() {
                                Some(d) => Some(self.expr(&d)?),
                                None => None,
                            },
                        })
                    })
                    .collect::<CResult<Vec<_>>>();
                self.scopes.pop();
                (
                    Param::Pattern {
                        formals: self.ctx.alloc_slice(formals?),
                        ellipsis: p.ellipsis_token().is_some(),
                        at,
                    },
                    all,
                )
            }
        };
        self.scopes.push(scope_of(&names));
        let body = child(l.body()).and_then(|b| self.expr(&b));
        self.scopes.pop();
        Ok(self.ctx.alloc(LambdaDef {
            param,
            body: body?,
            pos,
            name,
        }))
    }

    fn string(&mut self, s: &ast::Str) -> CResult<Expr<'a>> {
        let mut out = Vec::new();
        let mut lit: Option<String> = None;
        for part in str_parts(s) {
            match part {
                InterpolPart::Literal(l) => match &mut lit {
                    Some(prev) => prev.push_str(&l),
                    None => lit = Some(l),
                },
                InterpolPart::Interpolation(i) => {
                    if let Some(l) = lit.take().filter(|l| !l.is_empty()) {
                        out.push(self.leak(Expr::Str(self.lit_str(l.as_bytes()))));
                    }
                    out.push(self.expr(&child(i.expr())?)?);
                }
            }
        }
        if let Some(l) = lit.filter(|l| !l.is_empty()) {
            out.push(self.leak(Expr::Str(self.lit_str(l.as_bytes()))));
        }
        Ok(match out.as_slice() {
            [] => Expr::Str(self.lit_str(b"")),
            [Expr::Str(s)] => Expr::Str(s),
            _ => Expr::InterpStr(self.ctx.alloc_slice(out)),
        })
    }

    fn path(&mut self, p: &ast::Path, pos: Pos<'a>) -> CResult<Expr<'a>> {
        let mut out: Vec<ExprRef<'a>> = Vec::new();
        for (i, part) in p.parts().into_iter().enumerate() {
            match part {
                InterpolPart::Literal(l) => {
                    let text = l.text();
                    if i == 0 {
                        if l.is_home() && self.pure {
                            return err(format!(
                                "the path '{text}' can not be resolved in pure mode at {pos}"
                            ));
                        }
                        let resolved = self.resolve_path_literal(text, l.is_home());
                        let path = self.ctx.alloc(PathV(self.ctx.alloc_str(&resolved)));
                        out.push(self.leak(Expr::Path(path)));
                    } else {
                        out.push(self.leak(Expr::Str(self.lit_str(text.as_bytes()))));
                    }
                }
                InterpolPart::Interpolation(i) => out.push(self.expr(&child(i.expr())?)?),
            }
        }
        Ok(match out.as_slice() {
            [Expr::Path(p)] => Expr::Path(p),
            _ => Expr::InterpPath(self.ctx.alloc_slice(out)),
        })
    }

    fn resolve_path_literal(&self, text: &str, home: bool) -> String {
        // A literal prefix followed by interpolation keeps its trailing
        // slash, which canonicalisation must not drop: `./a/${x}`.
        let trailing = text.ends_with('/');
        let p = if home {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}{}", &text[1..])
        } else if text.starts_with('/') {
            text.to_owned()
        } else {
            format!("{}/{}", self.base_dir, text)
        };
        let mut p = canon_path(&p);
        if trailing && !p.ends_with('/') {
            p.push('/');
        }
        p
    }

    fn attrpath(&mut self, p: &ast::Attrpath) -> CResult<&'a [AttrKey<'a>]> {
        let keys = p
            .attrs()
            .map(|a| {
                Ok(match self.attr_name(&a)? {
                    AttrName::Static(s) => AttrKey::Static(s),
                    AttrName::Dynamic(e) => AttrKey::Dynamic(self.expr(&e)?),
                })
            })
            .collect::<CResult<Vec<_>>>()?;
        Ok(self.ctx.alloc_slice(keys))
    }

    fn attr_name(&self, a: &ast::Attr) -> CResult<AttrName> {
        Ok(match a {
            ast::Attr::Ident(i) => AttrName::Static(self.ident(i)?),
            // Nix's grammar only allows `"` strings as attribute names.
            ast::Attr::Str(s) if s.syntax().first_token().is_some_and(|t| t.text() == "''") => {
                return err(format!(
                    "syntax error: unexpected start of an indented string at {}",
                    self.pos(s)
                ));
            }
            ast::Attr::Str(s) => match static_str(s) {
                Some(lit) => AttrName::Static(self.ctx.intern(&lit)),
                None => AttrName::Dynamic(ast::Expr::Str(s.clone())),
            },
            ast::Attr::Dynamic(d) => {
                let inner = child(d.expr())?;
                match literal_name(&inner) {
                    Some(name) => AttrName::Static(self.ctx.intern(&name)),
                    None => AttrName::Dynamic(inner),
                }
            }
        })
    }

    fn ident(&self, i: &ast::Ident) -> CResult<Sym> {
        let Some(tok) = i.syntax().first_token() else {
            return err("syntax error: empty identifier");
        };
        Ok(self.ctx.intern(tok.text()))
    }

    fn preset(&mut self, entries: impl Iterator<Item = ast::Entry>) -> CResult<PreSet> {
        let mut set = PreSet::default();
        for entry in entries {
            self.merge_entry(&mut set, entry)?;
        }
        Ok(set)
    }

    fn merge_entry(&self, set: &mut PreSet, entry: ast::Entry) -> CResult<()> {
        match entry {
            ast::Entry::AttrpathValue(av) => {
                let path = child(av.attrpath())?;
                let attrs: Vec<ast::Attr> = path.attrs().collect();
                self.insert_path(set, &attrs, child(av.value())?)
            }
            ast::Entry::Inherit(inh) => {
                let from = match inh.from() {
                    Some(f) => {
                        set.froms.push(child(f.expr())?);
                        Some(set.froms.len() - 1)
                    }
                    None => None,
                };
                for attr in inh.attrs() {
                    let off = offset(&attr);
                    let AttrName::Static(name) = self.attr_name(&attr)? else {
                        return err(format!(
                            "dynamic attributes not allowed in inherit at {}",
                            self.at(off)
                        ));
                    };
                    if set.statics.iter().any(|(n, _, _)| *n == name) {
                        return err(format!(
                            "attribute '{}' already defined at {}",
                            self.name(name),
                            self.at(off)
                        ));
                    }
                    let value = match from {
                        Some(i) => PreAttr::InheritFrom(i, name, off),
                        None => PreAttr::Inherit(name, off),
                    };
                    set.statics.push((name, off, value));
                }
                Ok(())
            }
        }
    }

    fn insert_path(&self, set: &mut PreSet, path: &[ast::Attr], value: ast::Expr) -> CResult<()> {
        let off = offset(&path[0]);
        let rest = &path[1..];
        let leaf = |c: &Self, value: ast::Expr| -> CResult<PreAttr> {
            if rest.is_empty() {
                Ok(PreAttr::Expr(value))
            } else {
                let mut nested = PreSet::default();
                c.insert_path(&mut nested, rest, value)?;
                Ok(PreAttr::Nested(nested, false))
            }
        };
        let name = match self.attr_name(&path[0])? {
            AttrName::Dynamic(e) => {
                let attr = leaf(self, value)?;
                set.dynamics.push((e, off, attr));
                return Ok(());
            }
            AttrName::Static(name) => name,
        };
        let Some((_, _, existing)) = set.statics.iter_mut().find(|(n, _, _)| *n == name) else {
            let attr = leaf(self, value)?;
            set.statics.push((name, off, attr));
            return Ok(());
        };
        let already = || {
            err(format!(
                "attribute '{}' already defined at {}",
                self.name(name),
                self.at(off)
            ))
        };
        // Two definitions merge only when both are attribute sets:
        // `a.b = 1; a.c = 2;` or `a = { b = 1; }; a.c = 2;`. The merged set
        // is `rec` exactly when the first definition is a `rec` literal.
        if let PreAttr::Expr(ast::Expr::AttrSet(lit)) = existing {
            let mut nested = PreSet::default();
            for entry in lit.entries() {
                self.merge_entry(&mut nested, entry)?;
            }
            *existing = PreAttr::Nested(nested, lit.rec_token().is_some());
        }
        let PreAttr::Nested(nested, _) = existing else {
            return already();
        };
        if !rest.is_empty() {
            return self.insert_path(nested, rest, value);
        }
        match value {
            ast::Expr::AttrSet(lit) => {
                for entry in lit.entries() {
                    self.merge_entry(nested, entry)?;
                }
                Ok(())
            }
            _ => already(),
        }
    }

    /// Compile a value of a set or `let`, other than an `inherit`.
    fn pre_attr(&mut self, attr: PreAttr, name: Option<Sym>) -> CResult<ExprRef<'a>> {
        Ok(match attr {
            PreAttr::Expr(ast::Expr::Lambda(l)) => {
                let def = self.lambda(&l, name)?;
                self.leak(Expr::Lambda(def))
            }
            PreAttr::Expr(e) => self.expr(&e)?,
            PreAttr::Nested(set, recursive) => {
                let e = self.attrs(set, recursive)?;
                self.leak(e)
            }
            PreAttr::Inherit(..) | PreAttr::InheritFrom(..) => {
                unreachable!("inherit only binds a name of its own set")
            }
        })
    }

    /// An attribute set.
    fn attrs(&mut self, set: PreSet, recursive: bool) -> CResult<Expr<'a>> {
        let b = self.bindings(set, recursive, |_| Ok(()))?;
        Ok(Expr::Attrs(self.ctx.alloc(AttrsDef {
            frame: b.frame,
            statics: self.ctx.alloc_slice(b.statics),
            dynamics: self.ctx.alloc_slice(b.dynamics),
        })))
    }

    /// Compile the bindings of a set or `let`, and then `body` in their scope.
    ///
    /// Recursive bindings (`rec` and `let`) are the first slots of a new
    /// frame, in which every value is compiled. The `inherit (from)` sources
    /// take the slots after them, so that each is evaluated once; a
    /// non-recursive set has a frame only if it has such sources.
    fn bindings<T>(
        &mut self,
        set: PreSet,
        recursive: bool,
        body: impl FnOnce(&mut Self) -> CResult<T>,
    ) -> CResult<Bindings<'a, T>> {
        let has_frame = recursive || !set.froms.is_empty();
        if has_frame {
            let names: Vec<Sym> = match recursive {
                true => set.statics.iter().map(|(n, _, _)| *n).collect(),
                false => Vec::new(),
            };
            self.scopes.push(scope_of(&names));
        }
        let result = self.bindings_in_scope(set, recursive, has_frame, body);
        if has_frame {
            self.scopes.pop();
        }
        result
    }

    fn bindings_in_scope<T>(
        &mut self,
        set: PreSet,
        recursive: bool,
        has_frame: bool,
        body: impl FnOnce(&mut Self) -> CResult<T>,
    ) -> CResult<Bindings<'a, T>> {
        let froms_base = if recursive {
            set.statics.len() as u32
        } else {
            0
        };
        let mut slots = Vec::new();
        let mut statics = Vec::new();
        for (name, off, attr) in set.statics {
            let value = match attr {
                // `inherit x;` refers to the scope around the bindings.
                PreAttr::Inherit(n, off) => {
                    let e = self.resolve(n, usize::from(has_frame), self.at(off))?;
                    self.leak(e)
                }
                PreAttr::InheritFrom(i, n, off) => {
                    let from = self.leak(Expr::Var(0, froms_base + i as u32));
                    self.leak(Expr::Select {
                        expr: from,
                        path: self.ctx.alloc_slice(vec![AttrKey::Static(n)]),
                        default: None,
                        pos: self.at(off),
                    })
                }
                other => self.pre_attr(other, Some(name))?,
            };
            let value = if recursive {
                slots.push(value);
                AttrValue::Slot(slots.len() as u32 - 1)
            } else {
                AttrValue::Expr(value)
            };
            statics.push(StaticAttr {
                name,
                pos: self.at(off),
                value,
            });
        }
        for from in &set.froms {
            slots.push(self.expr(from)?);
        }
        let mut dynamics = Vec::new();
        for (name, off, attr) in set.dynamics {
            dynamics.push(DynamicAttr {
                name: self.expr(&name)?,
                value: self.pre_attr(attr, None)?,
                pos: self.at(off),
            });
        }
        let body = body(self)?;
        statics.sort_by_key(|s| s.name);
        Ok(Bindings {
            frame: has_frame.then(|| self.ctx.alloc_slice(slots)),
            statics,
            dynamics,
            body,
        })
    }
}

/// Compiled bindings of a set or `let`.
struct Bindings<'a, T> {
    /// The frame the bindings introduce, if any.
    frame: Option<&'a [ExprRef<'a>]>,
    /// Sorted by symbol.
    statics: Vec<StaticAttr<'a>>,
    dynamics: Vec<DynamicAttr<'a>>,
    body: T,
}

fn scope_of(names: &[Sym]) -> Scope {
    Scope::Names(
        names
            .iter()
            .enumerate()
            .map(|(i, s)| (*s, i as u32))
            .collect(),
    )
}

/// The literal text and interpolations of a string, with escapes resolved.
/// rnix keeps a raw `\r\n` or `\r` in a `"` string, but Nix reads each as
/// `\n`; an escaped `\r` is kept, as are both in an indented string.
fn str_parts(s: &ast::Str) -> Vec<InterpolPart<String>> {
    let indented = s.syntax().first_token().is_some_and(|t| t.text() == "''");
    if indented {
        return indented_parts(s);
    }
    s.parts()
        .map(|part| match part {
            InterpolPart::Literal(l) => InterpolPart::Literal(unescape_str(l.syntax().text())),
            InterpolPart::Interpolation(i) => InterpolPart::Interpolation(i),
        })
        .collect()
}

/// A piece of an indented string as Nix's lexer splits it.
enum IndPart {
    /// Literal text, and whether it is plain text rather than an escape,
    /// `$` or `'`.
    Str(String, bool),
    Interpolation(ast::Interpol),
}

/// The parts of an indented string with escapes resolved and indentation
/// stripped as Nix's parser does: lines are found by the newlines in plain
/// text when measuring indentation, but by every newline, escaped or not,
/// when stripping it, and escaped text is stripped like any other. Each
/// token is its own part, and those left empty are dropped.
fn indented_parts(s: &ast::Str) -> Vec<InterpolPart<String>> {
    let mut parts = Vec::new();
    for (i, part) in s.parts().enumerate() {
        match part {
            InterpolPart::Literal(l) => {
                let mut text = l.syntax().text();
                // The opening `''` takes spaces and a newline after it.
                if i == 0
                    && let Some((first, rest)) = text.split_once('\n')
                    && first.bytes().all(|b| b == b' ')
                {
                    text = rest;
                }
                ind_str_tokens(text, &mut parts);
            }
            InterpolPart::Interpolation(i) => parts.push(IndPart::Interpolation(i)),
        }
    }

    let mut at_start_of_line = true;
    let mut min_indent = usize::MAX;
    let mut cur_indent = 0;
    for part in &parts {
        let IndPart::Str(text, true) = part else {
            if at_start_of_line {
                at_start_of_line = false;
                min_indent = min_indent.min(cur_indent);
            }
            continue;
        };
        for b in text.bytes() {
            if at_start_of_line {
                match b {
                    b' ' => cur_indent += 1,
                    b'\n' => cur_indent = 0,
                    _ => {
                        at_start_of_line = false;
                        min_indent = min_indent.min(cur_indent);
                    }
                }
            } else if b == b'\n' {
                at_start_of_line = true;
                cur_indent = 0;
            }
        }
    }

    let mut out: Vec<InterpolPart<String>> = Vec::new();
    let mut at_start_of_line = true;
    let mut cur_dropped = 0;
    let n = parts.len();
    for (i, part) in parts.into_iter().enumerate() {
        let text = match part {
            IndPart::Interpolation(e) => {
                at_start_of_line = false;
                cur_dropped = 0;
                out.push(InterpolPart::Interpolation(e));
                continue;
            }
            IndPart::Str(text, _) => text,
        };
        let mut s = String::with_capacity(text.len());
        for c in text.chars() {
            if at_start_of_line {
                match c {
                    ' ' => {
                        if cur_dropped >= min_indent {
                            s.push(c);
                        }
                        cur_dropped += 1;
                    }
                    '\n' => {
                        cur_dropped = 0;
                        s.push(c);
                    }
                    _ => {
                        at_start_of_line = false;
                        cur_dropped = 0;
                        s.push(c);
                    }
                }
            } else {
                s.push(c);
                if c == '\n' {
                    at_start_of_line = true;
                }
            }
        }
        // A final line of only spaces is dropped.
        if i + 1 == n
            && let Some(p) = s.rfind('\n')
            && s[p + 1..].bytes().all(|b| b == b' ')
        {
            s.truncate(p + 1);
        }
        if !s.is_empty() {
            out.push(InterpolPart::Literal(s));
        }
    }
    out
}

/// Splits the raw text of an indented string literal into the tokens Nix's
/// lexer reads it as, resolving escapes.
fn ind_str_tokens(text: &str, out: &mut Vec<IndPart>) {
    let mut plain = String::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let next = rest[c.len_utf8()..].chars().next();
        let (token, len) = match (c, next) {
            ('\'', Some('\'')) => {
                let after = &rest[2..];
                match after.chars().next() {
                    Some('\'') => ("''".to_owned(), 3),
                    Some('$') => ("$".to_owned(), 3),
                    Some('\\') => match after[1..].chars().next() {
                        Some(e) => (unescape_str(&format!("\\{e}")), 3 + e.len_utf8()),
                        None => break,
                    },
                    _ => break,
                }
            }
            ('\'', None | Some('$')) => ("'".to_owned(), 1),
            ('$', None | Some('{' | '\'')) => ("$".to_owned(), 1),
            _ => {
                plain.push(c);
                rest = &rest[c.len_utf8()..];
                continue;
            }
        };
        if !plain.is_empty() {
            out.push(IndPart::Str(std::mem::take(&mut plain), true));
        }
        out.push(IndPart::Str(token, false));
        rest = &rest[len..];
    }
    if !plain.is_empty() {
        out.push(IndPart::Str(plain, true));
    }
}

/// Resolves the escapes in the text of a `"` string literal.
fn unescape_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(c) => out.push(c),
                None => {}
            },
            '\r' => {
                out.push('\n');
                if chars.as_str().starts_with('\n') {
                    chars.next();
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// The text of a string literal that is a single piece of text.
fn static_str(s: &ast::Str) -> Option<String> {
    match <[_; 1]>::try_from(str_parts(s)) {
        Ok([InterpolPart::Literal(lit)]) => Some(lit),
        Ok(_) => None,
        Err(parts) => parts.is_empty().then(String::new),
    }
}

fn child<T>(o: Option<T>) -> CResult<T> {
    o.ok_or_else(|| CompileError {
        msg: "syntax error: incomplete expression".into(),
    })
}

/// Parses a float literal as Nix does with glibc's `strtod`, rejecting a
/// literal whose value overflows to infinity or underflows.
fn parse_float(text: &str) -> Option<f64> {
    let v: f64 = text.parse().ok()?;
    if v.is_infinite() || (v <= f64::MIN_POSITIVE && underflows(text)) {
        return None;
    }
    Some(v)
}

// Whether glibc detects tininess after rounding on this architecture, as
// its sysdeps/*/tininess.h say; on the others it detects it before.
const TININESS_AFTER_ROUNDING: bool = cfg!(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "riscv32",
    target_arch = "riscv64",
    target_arch = "mips",
    target_arch = "mips32r6",
    target_arch = "mips64",
    target_arch = "mips64r6",
    target_arch = "loongarch64",
    target_arch = "csky",
));

/// Whether glibc's `strtod` reports a range error for the literal `text`:
/// its value is tiny (below the smallest normal double) and not exactly
/// representable. Where tininess is detected after rounding, a value that
/// rounds to the smallest normal double at full precision is not tiny.
fn underflows(text: &str) -> bool {
    let Some((digits, scale)) = decimal(text) else {
        return true;
    };
    if digits.is_empty() {
        return false;
    }
    // Below 10^-330 the value is under half the smallest subnormal.
    if digits.len() as i64 + scale < -330 {
        return true;
    }
    if scale >= 0 {
        return false;
    }
    // The value is d / 10^e; compare it against powers of two exactly.
    let d: BigUint = digits.parse().expect("decimal digits");
    let ten_e = BigUint::from(10u32).pow((-scale) as u32);
    if (&d << 1022u32) >= ten_e {
        return false;
    }
    if (&d << 1074u32) % &ten_e == BigUint::ZERO {
        return false;
    }
    // 2^-1022 - 2^-1076 is halfway to the next double below at full
    // precision, and rounds to even, up to 2^-1022.
    !(TININESS_AFTER_ROUNDING && (&d << 1076u32) >= BigUint::from((1u64 << 54) - 1) * &ten_e)
}

/// A decimal literal as significant digits and the power of ten they are
/// scaled by, `None` for an exponent out of range.
fn decimal(text: &str) -> Option<(String, i64)> {
    let (mantissa, exp) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int}{frac}");
    let digits = digits.trim_start_matches('0');
    let significant = digits.trim_end_matches('0');
    if significant.is_empty() {
        return Some((String::new(), 0));
    }
    let scale =
        exp.parse::<i64>().ok()? - frac.len() as i64 + (digits.len() - significant.len()) as i64;
    Some((significant.to_owned(), scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_float_smallest_normal() {
        assert_eq!(
            parse_float("2.2250738585072014e-308"),
            Some(f64::MIN_POSITIVE)
        );
        // Tiny, and rounds to the smallest normal only at subnormal precision.
        assert_eq!(parse_float("2.2250738585072012e-308"), None);
        // Tiny before rounding, but rounds to the smallest normal at full
        // precision; 2^-1022 - 2^-1076 is the boundary.
        let after = TININESS_AFTER_ROUNDING.then_some(f64::MIN_POSITIVE);
        assert_eq!(parse_float("2.2250738585072013e-308"), after);
        assert_eq!(parse_float("2.225073858507201259574e-308"), after);
        assert_eq!(parse_float("2.225073858507201259573e-308"), None);
    }

    #[test]
    fn parse_float_exact_subnormal() {
        assert_eq!(parse_float("0.0e-999"), Some(0.0));
        assert_eq!(parse_float("1.0e-320"), None);
        let min = format!("{:.1100e}", f64::from_bits(1));
        assert_eq!(parse_float(&min), Some(f64::from_bits(1)));
    }
}
