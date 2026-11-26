//! `toJSON` and `fromJSON`.

use super::grisu2::grisu2;
use crate::eval::Coerce;
use crate::eval::Eval;
use crate::value::Ctx;
use crate::value::Entry;
use crate::value::R;
use crate::value::Value;
use crate::value::eval_err;

pub fn to_json<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    let mut out = Vec::new();
    let mut ctx = Vec::new();
    write_json(ev, v, &mut out, &mut ctx)?;
    Ok(ev.str_with_ctx(&out, ctx))
}

pub fn json_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
}

/// A float the way Nix's JSON writer prints it: digits that round-trip, as
/// chosen by Grisu2, always with a fraction or exponent. Values from 1e-4 up
/// to but excluding 1e15 are in fixed notation, others in exponent notation
/// such as `1.5e+15` or `1e-05`.
pub fn json_float(f: f64) -> String {
    if !f.is_finite() {
        return "null".to_owned();
    }
    let sign = if f.is_sign_negative() { "-" } else { "" };
    let (digits, exp) = if f == 0.0 {
        (vec![b'0'], 0)
    } else {
        grisu2(f.abs())
    };
    let digits = String::from_utf8(digits).expect("digits are ASCII");
    // The decimal point goes after `point` digits of `digits`.
    let point = exp + digits.len() as i32;
    let e = point - 1;
    if !(-3..=15).contains(&point) {
        let (first, rest) = digits.split_at(1);
        let dot = if rest.is_empty() { "" } else { "." };
        let esign = if e < 0 { '-' } else { '+' };
        return format!("{sign}{first}{dot}{rest}e{esign}{:02}", e.unsigned_abs());
    }
    let len = digits.len() as i32;
    if point <= 0 {
        let zeros = "0".repeat(point.unsigned_abs() as usize);
        format!("{sign}0.{zeros}{digits}")
    } else if point >= len {
        let zeros = "0".repeat((point - len) as usize);
        format!("{sign}{digits}{zeros}.0")
    } else {
        let (int, frac) = digits.split_at(point as usize);
        format!("{sign}{int}.{frac}")
    }
}

pub fn write_json<'a>(
    ev: &Eval<'a>,
    v: Value<'a>,
    out: &mut Vec<u8>,
    ctx: &mut Vec<Ctx<'a>>,
) -> R<'a, ()> {
    ev.deeper(|| write_json_level(ev, v, out, ctx))
}

fn write_json_level<'a>(
    ev: &Eval<'a>,
    v: Value<'a>,
    out: &mut Vec<u8>,
    ctx: &mut Vec<Ctx<'a>>,
) -> R<'a, ()> {
    let v = ev.force(v)?;
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Float(f) => out.extend_from_slice(json_float(f).as_bytes()),
        Value::Str(s) => {
            json_string(out, s.s);
            ctx.extend_from_slice(s.ctx);
        }
        Value::Path(_) => {
            let (s, c) = ev.coerce_to_string(v, Coerce::INTERP)?;
            json_string(out, &s);
            ctx.extend(c);
        }
        Value::Attrs(a) => {
            let s = &ev.ctx.syms;
            if a.get(s.to_string).is_some() {
                let mut st = Vec::new();
                ev.coerce_level(v, Coerce::PLAIN, &mut st, ctx)?;
                json_string(out, &st);
                return Ok(());
            }
            if let Some(p) = a.get(s.out_path) {
                return write_json_level(ev, p, out, ctx);
            }
            out.push(b'{');
            for (i, e) in a.sorted(ev.ctx).iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                json_string(out, ev.name(e.name).as_bytes());
                out.push(b':');
                write_json(ev, e.value, out, ctx)?;
            }
            out.push(b'}');
        }
        Value::List(l) => {
            out.push(b'[');
            for (i, item) in l.items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json(ev, *item, out, ctx)?;
            }
            out.push(b']');
        }
        other => {
            return eval_err(format!("cannot convert {} to JSON", other.show_type()));
        }
    }
    Ok(())
}

pub fn from_json<'a>(ev: &Eval<'a>, s: Value<'a>) -> R<'a> {
    let s = ev.force_str_no_ctx(s)?;
    let Ok(text) = std::str::from_utf8(s) else {
        return eval_err("JSON parse error: invalid UTF-8");
    };
    JsonParser {
        s: text.strip_prefix('\u{feff}').unwrap_or(text),
        i: 0,
    }
    .parse(ev)
}

enum JsonFrame<'a> {
    List(Vec<Value<'a>>),
    Attrs(Vec<Entry<'a>>, crate::symbol::Sym),
}

/// A JSON parser with the semantics of the nlohmann parser Nix uses: no
/// nesting limit, `-0` is an integer, and integers that overflow are floats.
struct JsonParser<'s> {
    s: &'s str,
    i: usize,
}

impl JsonParser<'_> {
    fn parse<'a>(&mut self, ev: &Eval<'a>) -> R<'a> {
        let mut stack: Vec<JsonFrame<'a>> = Vec::new();
        loop {
            self.ws();
            let mut v = match self.peek() {
                Some(b'[') => {
                    self.i += 1;
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        ev.list(&[])
                    } else {
                        stack.push(JsonFrame::List(Vec::new()));
                        continue;
                    }
                }
                Some(b'{') => {
                    self.i += 1;
                    self.ws();
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                        ev.attrs(Vec::new())
                    } else {
                        let key = self.key(ev)?;
                        stack.push(JsonFrame::Attrs(Vec::new(), key));
                        continue;
                    }
                }
                Some(b'"') => ev.string(&self.string()?),
                Some(b't') => self.literal("true", Value::Bool(true))?,
                Some(b'f') => self.literal("false", Value::Bool(false))?,
                Some(b'n') => self.literal("null", Value::Null)?,
                Some(b'-' | b'0'..=b'9') => self.number()?,
                _ => return self.err(),
            };
            loop {
                self.ws();
                match stack.last_mut() {
                    None if self.i == self.s.len() => return Ok(v),
                    None => return self.err(),
                    Some(JsonFrame::List(items)) => {
                        items.push(v);
                        match self.next() {
                            Some(b',') => break,
                            Some(b']') => {}
                            _ => return self.err(),
                        }
                    }
                    Some(JsonFrame::Attrs(entries, key)) => {
                        entries.push(Entry {
                            name: *key,
                            value: v,
                            pos: None,
                        });
                        match self.next() {
                            Some(b',') => {
                                *key = self.key(ev)?;
                                break;
                            }
                            Some(b'}') => {}
                            _ => return self.err(),
                        }
                    }
                }
                v = match stack.pop() {
                    Some(JsonFrame::List(items)) => ev.list(&items),
                    Some(JsonFrame::Attrs(entries, _)) => ev.attrs(entries),
                    None => unreachable!("a value was just added to the top frame"),
                };
            }
        }
    }

    fn err<T>(&self) -> R<'static, T> {
        eval_err(format!("JSON parse error at byte {}", self.i))
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek();
        self.i += 1;
        c
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.i += 1;
        }
    }

    fn literal<'a>(&mut self, word: &str, v: Value<'a>) -> R<'a> {
        if !self.s[self.i..].starts_with(word) {
            return self.err();
        }
        self.i += word.len();
        Ok(v)
    }

    fn key(&mut self, ev: &Eval<'_>) -> R<'static, crate::symbol::Sym> {
        self.ws();
        if self.peek() != Some(b'"') {
            return self.err();
        }
        let key = self.string()?;
        self.ws();
        if self.next() != Some(b':') {
            return self.err();
        }
        Ok(ev.sym(&key))
    }

    fn string(&mut self) -> R<'static, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let rest = &self.s[self.i..];
            let end = rest
                .find(|c: char| c == '"' || c == '\\' || c < ' ')
                .unwrap_or(rest.len());
            out.push_str(&rest[..end]);
            self.i += end;
            match self.next() {
                Some(b'"') => break,
                Some(b'\\') => {}
                _ => return self.err(),
            }
            let c = match self.next() {
                Some(b'"') => '"',
                Some(b'\\') => '\\',
                Some(b'/') => '/',
                Some(b'b') => '\u{8}',
                Some(b'f') => '\u{c}',
                Some(b'n') => '\n',
                Some(b'r') => '\r',
                Some(b't') => '\t',
                Some(b'u') => {
                    let hi = self.hex4()?;
                    let code = match hi {
                        0xd800..=0xdbff => {
                            if !self.s[self.i..].starts_with("\\u") {
                                return self.err();
                            }
                            self.i += 2;
                            let lo = self.hex4()?;
                            if !(0xdc00..=0xdfff).contains(&lo) {
                                return self.err();
                            }
                            0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                        }
                        0xdc00..=0xdfff => return self.err(),
                        c => c,
                    };
                    char::from_u32(code).expect("surrogates were excluded")
                }
                _ => return self.err(),
            };
            out.push(c);
        }
        if out.contains('\0') {
            return eval_err(format!(
                "input string '{out}' cannot be represented as Nix string because it contains null bytes"
            ));
        }
        Ok(out)
    }

    fn hex4(&mut self) -> R<'static, u32> {
        let digits = self.s.get(self.i..self.i + 4).unwrap_or("");
        if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return self.err();
        }
        self.i += 4;
        Ok(u32::from_str_radix(digits, 16).expect("validated hex digits"))
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while let Some(b'0'..=b'9') = self.peek() {
            self.i += 1;
        }
        self.i - start
    }

    fn number<'a>(&mut self) -> R<'a> {
        let start = self.i;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return self.err(),
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            self.i += 1;
            if self.digits() == 0 {
                return self.err();
            }
            float = true;
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.i += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.i += 1;
            }
            if self.digits() == 0 {
                return self.err();
            }
            float = true;
        }
        let text = &self.s[start..self.i];
        if !float {
            if negative {
                if let Ok(i) = text.parse::<i64>() {
                    return Ok(Value::Int(i));
                }
            } else if let Ok(u) = text.parse::<u64>() {
                return match i64::try_from(u) {
                    Ok(i) => Ok(Value::Int(i)),
                    Err(_) => eval_err(format!(
                        "unsigned json number {u} outside of Nix integer range"
                    )),
                };
            }
        }
        let f: f64 = text.parse().expect("validated JSON number");
        if !f.is_finite() {
            return eval_err(format!("number overflow parsing '{text}'"));
        }
        Ok(Value::Float(f))
    }
}

pub fn from_toml<'a>(ev: &Eval<'a>, s: Value<'a>) -> R<'a> {
    let s = ev.force_str_no_ctx(s)?;
    let text = String::from_utf8_lossy(s);
    if let Some(e) = toml_1_1_syntax(&text) {
        return eval_err(format!("while parsing TOML: {e}"));
    }
    let table: toml::Table = match split_headers_after_values(&text).parse() {
        Ok(t) => t,
        Err(e) => return eval_err(format!("while parsing TOML: {e}")),
    };
    toml_to_value(ev, &toml::Value::Table(table))
}

/// Nix parses TOML 1.0, but the `toml` crate accepts TOML 1.1, which adds
/// newlines and a trailing comma inside inline tables and the `\e` and
/// `\xHH` escapes in basic strings. Report the first of these, if any.
fn toml_1_1_syntax(text: &str) -> Option<&'static str> {
    use toml_parser::lexer::TokenKind;

    let mut open = Vec::new();
    let mut after_comma = false;
    for token in toml_parser::Source::new(text).lex() {
        let kind = token.kind();
        let in_inline_table = open.last() == Some(&TokenKind::LeftCurlyBracket);
        match kind {
            TokenKind::LeftSquareBracket | TokenKind::LeftCurlyBracket => open.push(kind),
            TokenKind::RightSquareBracket => {
                open.pop();
            }
            TokenKind::RightCurlyBracket => {
                if in_inline_table && after_comma {
                    return Some("trailing comma in inline table");
                }
                open.pop();
            }
            TokenKind::Newline if in_inline_table => {
                return Some("newline in inline table");
            }
            TokenKind::BasicString | TokenKind::MlBasicString => {
                let span = token.span();
                let mut bytes = text.as_bytes()[span.start()..span.end()].iter();
                while let Some(b) = bytes.next() {
                    if *b == b'\\' && matches!(bytes.next(), Some(b'e' | b'x')) {
                        return Some("unknown escape sequence");
                    }
                }
            }
            _ => {}
        }
        match kind {
            TokenKind::Comma => after_comma = true,
            TokenKind::Whitespace => {}
            _ => after_comma = false,
        }
    }
    None
}

/// toml11 ends a table's key/value pairs as soon as the next non-whitespace
/// is a `[`, without requiring the newline TOML does after a value, so a
/// header may follow a value on the same line (`a = 1 [b]`). Insert that
/// newline so the `toml` crate sees the same document.
fn split_headers_after_values(text: &str) -> std::borrow::Cow<'_, str> {
    use toml_parser::lexer::TokenKind;

    enum Line {
        Start,
        Header,
        Key,
        Value { depth: usize, done: bool },
    }

    let mut splits = Vec::new();
    let mut line = Line::Start;
    for token in toml_parser::Source::new(text).lex() {
        let kind = token.kind();
        line = match (line, kind) {
            (
                Line::Value { depth: 0, .. } | Line::Start | Line::Header | Line::Key,
                TokenKind::Newline,
            ) => Line::Start,
            (
                l,
                TokenKind::Whitespace | TokenKind::Comment | TokenKind::Newline | TokenKind::Eof,
            ) => l,
            (Line::Start, TokenKind::LeftSquareBracket) => Line::Header,
            (Line::Start, _) => Line::Key,
            (Line::Header, _) => Line::Header,
            (Line::Key, TokenKind::Equals) => Line::Value {
                depth: 0,
                done: false,
            },
            (Line::Key, _) => Line::Key,
            (
                Line::Value {
                    depth: 0,
                    done: true,
                },
                TokenKind::LeftSquareBracket,
            ) => {
                splits.push(token.span().start());
                Line::Header
            }
            (
                Line::Value { depth, .. },
                TokenKind::LeftSquareBracket | TokenKind::LeftCurlyBracket,
            ) => Line::Value {
                depth: depth + 1,
                done: false,
            },
            (
                Line::Value { depth, .. },
                TokenKind::RightSquareBracket | TokenKind::RightCurlyBracket,
            ) => Line::Value {
                depth: depth.saturating_sub(1),
                done: true,
            },
            (Line::Value { depth, .. }, _) => Line::Value { depth, done: true },
        };
    }
    if splits.is_empty() {
        return text.into();
    }
    let mut out = String::with_capacity(text.len() + splits.len());
    let mut prev = 0;
    for at in splits {
        out.push_str(&text[prev..at]);
        out.push('\n');
        prev = at;
    }
    out.push_str(&text[prev..]);
    out.into()
}

fn check_toml_string(s: &str) -> R<'static, ()> {
    if s.contains('\0') {
        return eval_err(
            "while parsing TOML: input string cannot be represented as Nix string because it contains null bytes",
        );
    }
    Ok(())
}

fn toml_to_value<'a>(ev: &Eval<'a>, t: &toml::Value) -> R<'a> {
    Ok(match t {
        toml::Value::String(s) => {
            check_toml_string(s)?;
            ev.string(s)
        }
        toml::Value::Integer(i) => Value::Int(*i),
        toml::Value::Float(f) => Value::Float(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(_) => {
            return eval_err("while parsing TOML: dates and times are not supported");
        }
        toml::Value::Array(items) => {
            let items = items
                .iter()
                .map(|i| toml_to_value(ev, i))
                .collect::<R<'a, Vec<_>>>()?;
            ev.list(&items)
        }
        toml::Value::Table(map) => ev.attrs(
            map.iter()
                .map(|(k, v)| {
                    check_toml_string(k)?;
                    Ok(Entry {
                        name: ev.sym(k),
                        value: toml_to_value(ev, v)?,
                        pos: None,
                    })
                })
                .collect::<R<'a, Vec<_>>>()?,
        ),
    })
}
