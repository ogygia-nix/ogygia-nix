//! `builtins.match` and `builtins.split`: POSIX extended regular expressions,
//! translated to `regex` syntax.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use regex_automata::Anchored;
use regex_automata::Input;
use regex_automata::MatchKind;
use regex_automata::meta;
use regex_automata::util::syntax;

use crate::eval::Eval;
use crate::eval::Feature;
use crate::value::R;
use crate::value::Value;

/// Compiled patterns by source.
#[derive(Default)]
pub(crate) struct Cache {
    matches: RefCell<HashMap<Vec<u8>, Rc<regex::bytes::Regex>>>,
    splits: RefCell<HashMap<Vec<u8>, Rc<SplitRegex>>>,
}

/// Translate a POSIX extended regular expression to `regex` syntax for
/// matching bytes with Unicode disabled, writing the end anchor `$` as `end`.
/// As in `std::regex`, each byte of `re` is one character.
fn translate_regex(re: &[u8], end: &str) -> Result<String, String> {
    let chars: Vec<char> = re.iter().map(|&b| char::from(b)).collect();
    let mut out = String::with_capacity(re.len() + 8);
    // Where each open group starts in `out`.
    let mut groups = Vec::new();
    // Where the last atom, with any quantifiers, starts in `out`.
    let mut atom = None;
    let mut quantified = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let start = out.len();
        let mut quantifier = false;
        match c {
            '?' if quantified => {
                // A POSIX quantifier is never lazy: `x??` is `(x?)?`.
                if let Some(s) = atom {
                    out.insert_str(s, "(?:");
                    out.push(')');
                }
                out.push('?');
                quantifier = true;
                i += 1;
            }
            '*' | '+' | '?' => {
                out.push(c);
                quantifier = true;
                i += 1;
            }
            '[' => {
                i = translate_bracket(&chars, i + 1, &mut out)?;
            }
            '\\' => {
                // libstdc++ only lets an extended regex escape its special
                // characters.
                match chars.get(i + 1) {
                    Some(&n) if "$()*+.?[\\^{|".contains(n) => {
                        out.push('\\');
                        out.push(n);
                    }
                    _ => return Err("invalid escape".into()),
                }
                i += 2;
            }
            '^' | '$' => {
                // libstdc++ parses an assertion as a whole term, so a
                // quantifier cannot follow one.
                if chars.get(i + 1).is_some_and(|n| "*+?{".contains(*n)) {
                    return Err("quantifier after an assertion".into());
                }
                if c == '$' {
                    out.push_str(end);
                } else {
                    out.push(c);
                }
                i += 1;
            }
            '{' => {
                // `regex` allows whitespace in an interval; libstdc++ does not.
                let end = interval_end(&chars, i + 1).ok_or("invalid interval")?;
                out.extend(&chars[i..end]);
                quantifier = true;
                i = end;
            }
            '(' => {
                groups.push(start);
                out.push('(');
                i += 1;
            }
            ')' => {
                let open = groups.pop().ok_or("unmatched ')'")?;
                out.push(')');
                i += 1;
                atom = Some(open);
                quantified = false;
                continue;
            }
            c => {
                push_byte(&mut out, c);
                i += 1;
            }
        }
        if !quantifier {
            atom = (!matches!(c, '(' | '|' | '^' | '$')).then_some(start);
        }
        quantified = quantifier;
    }
    if !groups.is_empty() {
        return Err("unmatched '('".into());
    }
    Ok(out)
}

/// The names `std::regex_traits<char>::lookup_collatename` accepts in `[.x.]`
/// and `[=x=]`, indexed by the character each names.
const COLLATE_NAMES: [&str; 128] = [
    "NUL",
    "SOH",
    "STX",
    "ETX",
    "EOT",
    "ENQ",
    "ACK",
    "alert",
    "backspace",
    "tab",
    "newline",
    "vertical-tab",
    "form-feed",
    "carriage-return",
    "SO",
    "SI",
    "DLE",
    "DC1",
    "DC2",
    "DC3",
    "DC4",
    "NAK",
    "SYN",
    "ETB",
    "CAN",
    "EM",
    "SUB",
    "ESC",
    "IS4",
    "IS3",
    "IS2",
    "IS1",
    "space",
    "exclamation-mark",
    "quotation-mark",
    "number-sign",
    "dollar-sign",
    "percent-sign",
    "ampersand",
    "apostrophe",
    "left-parenthesis",
    "right-parenthesis",
    "asterisk",
    "plus-sign",
    "comma",
    "hyphen",
    "period",
    "slash",
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "colon",
    "semicolon",
    "less-than-sign",
    "equals-sign",
    "greater-than-sign",
    "question-mark",
    "commercial-at",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "left-square-bracket",
    "backslash",
    "right-square-bracket",
    "circumflex",
    "underscore",
    "grave-accent",
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "left-curly-bracket",
    "vertical-line",
    "right-curly-bracket",
    "tilde",
    "DEL",
];

/// A token inside a bracket expression, as libstdc++'s POSIX scanner reads it.
enum BracketToken {
    Char(char),
    Dash,
    End,
    Collate(String),
    Equiv(String),
    Class(String),
}

/// Push the byte `c` as is, or as an escape if it is not ASCII.
fn push_byte(out: &mut String, c: char) {
    if c.is_ascii() {
        out.push(c);
    } else {
        out.push_str(&format!("\\x{:02X}", u32::from(c)));
    }
}

/// Push the byte `c` as a literal member of a `regex` character class.
fn push_class_char(out: &mut String, c: char) {
    if c.is_ascii() {
        out.push_str(&regex::escape(c.encode_utf8(&mut [0; 4])));
    } else {
        push_byte(out, c);
    }
}

/// Push `c`, if any, as a literal member of a `regex` character class.
fn push_pending(out: &mut String, c: Option<char>) {
    if let Some(c) = c {
        push_class_char(out, c);
    }
}

/// The index after the `}` of the `min}`, `min,}` or `min,max}` interval body
/// starting at `chars[i]`, if it is one with `min <= max`.
fn interval_end(chars: &[char], i: usize) -> Option<usize> {
    let number = |i: usize| -> (Option<u64>, usize) {
        let len = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        let v = chars[i..i + len].iter().fold(0u64, |v, c| {
            v.saturating_mul(10)
                .saturating_add(u64::from(c.to_digit(10).unwrap_or(0)))
        });
        ((len > 0).then_some(v), i + len)
    };
    let (min, i) = number(i);
    let min = min?;
    let (max, i) = match chars.get(i) {
        Some(',') => number(i + 1),
        _ => (Some(min), i),
    };
    (chars.get(i) == Some(&'}') && max.is_none_or(|max| min <= max)).then_some(i + 1)
}

/// Translate the bracket expression whose body starts at `chars[i]`, returning
/// the index after its closing `]`. Follows libstdc++'s POSIX bracket
/// grammar: `]` first is a member, `-` is a member only first or last,
/// a range starts at a character or `[.x.]` and ends at a character or `-`,
/// and `[:x:]`, `[.x.]` and `[=x=]` must name a known class or character.
fn translate_bracket(chars: &[char], mut i: usize, out: &mut String) -> Result<usize, String> {
    let next = |i: &mut usize, start: bool| -> Result<BracketToken, String> {
        let c = *chars.get(*i).ok_or("unterminated bracket expression")?;
        *i += 1;
        Ok(match c {
            '-' => BracketToken::Dash,
            ']' if !start => BracketToken::End,
            '[' => match chars.get(*i) {
                None => return Err("unterminated bracket expression".into()),
                Some(&close @ ('.' | ':' | '=')) => {
                    *i += 1;
                    let name_start = *i;
                    while *i < chars.len() && chars[*i] != close {
                        *i += 1;
                    }
                    let name: String = chars[name_start..*i].iter().collect();
                    if chars.get(*i + 1) != Some(&']') {
                        return Err(format!("unterminated [{close}{name}{close}]"));
                    }
                    *i += 2;
                    match close {
                        '.' => BracketToken::Collate(name),
                        ':' => BracketToken::Class(name),
                        _ => BracketToken::Equiv(name),
                    }
                }
                Some(_) => BracketToken::Char('['),
            },
            c => BracketToken::Char(c),
        })
    };
    let collate = |name: &str| -> Result<char, String> {
        COLLATE_NAMES
            .iter()
            .position(|n| *n == name)
            .map(|c| char::from(c as u8))
            .ok_or_else(|| format!("invalid collating element '{name}'"))
    };

    out.push('[');
    if chars.get(i) == Some(&'^') {
        out.push('^');
        i += 1;
    }
    // The character that may start a range, if the last term was one.
    let mut last: Option<char> = None;
    let mut start = true;
    loop {
        let tok = next(&mut i, start)?;
        let first = std::mem::replace(&mut start, false);
        match tok {
            BracketToken::End => break,
            BracketToken::Char(c) => {
                push_pending(out, last);
                last = Some(c);
            }
            BracketToken::Collate(name) => {
                push_pending(out, last);
                last = Some(collate(&name)?);
            }
            BracketToken::Equiv(name) => {
                push_pending(out, last.take());
                let c = collate(&name)?;
                push_class_char(out, c.to_ascii_lowercase());
                if c.is_ascii_alphabetic() {
                    out.push(c.to_ascii_uppercase());
                }
            }
            BracketToken::Class(name) => {
                push_pending(out, last.take());
                let lower = name.to_ascii_lowercase();
                let class = match lower.as_str() {
                    "d" => "digit",
                    "w" => "word",
                    "s" => "space",
                    n @ ("alnum" | "alpha" | "blank" | "cntrl" | "digit" | "graph" | "lower"
                    | "print" | "punct" | "space" | "upper" | "xdigit") => n,
                    _ => return Err(format!("invalid character class '{name}'")),
                };
                out.push_str("[:");
                out.push_str(class);
                out.push_str(":]");
            }
            BracketToken::Dash if first => last = Some('-'),
            BracketToken::Dash => {
                let save = i;
                if let BracketToken::End = next(&mut i, false)? {
                    push_pending(out, last);
                    push_class_char(out, '-');
                    break;
                }
                i = save;
                let l = last.take().ok_or("invalid '-' in bracket expression")?;
                let r = match next(&mut i, false)? {
                    BracketToken::Char(r) => r,
                    BracketToken::Dash => '-',
                    _ => return Err("invalid end of range".into()),
                };
                // libstdc++ orders a range's ends as `char`, which is signed
                // on some platforms, so a range may run from a non-ASCII byte
                // round to an ASCII one.
                let as_c_char = |c: char| std::ffi::c_char::from_ne_bytes([c as u8]);
                if as_c_char(l) > as_c_char(r) {
                    return Err(format!("invalid range '{l}-{r}'"));
                }
                push_class_char(out, l);
                if l > r {
                    out.push_str("-\\xFF\\x00");
                }
                out.push('-');
                push_class_char(out, r);
            }
        }
    }
    push_pending(out, last);
    out.push(']');
    Ok(i)
}

fn regex_error(re: &[u8], e: impl std::fmt::Display) -> Box<crate::value::EvalError<'static>> {
    crate::value::error(
        crate::value::ErrorKind::Eval,
        format!(
            "invalid regular expression '{}': {e}",
            String::from_utf8_lossy(re)
        ),
    )
}

fn compile_match_regex<'a>(ev: &Eval<'a>, re: &[u8]) -> R<'a, Rc<regex::bytes::Regex>> {
    ev.require(Feature::Regex)?;
    if let Some(r) = ev.ctx.regexes.matches.borrow().get(re) {
        return Ok(r.clone());
    }
    let pattern = format!(
        "^(?:{})$",
        translate_regex(re, "$").map_err(|e| regex_error(re, e))?
    );
    let r = regex::bytes::RegexBuilder::new(&pattern)
        .unicode(false)
        .dot_matches_new_line(true)
        .build()
        .map_err(|e| regex_error(re, e))?;
    let r = Rc::new(r);
    ev.ctx
        .regexes
        .matches
        .borrow_mut()
        .insert(re.to_vec(), r.clone());
    Ok(r)
}

fn captures_list<'a>(ev: &Eval<'a>, caps: &regex::bytes::Captures) -> Value<'a> {
    let groups: Vec<Value<'a>> = (1..caps.len())
        .map(|i| match caps.get(i) {
            Some(m) => ev.str_val(m.as_bytes(), &[]),
            None => Value::Null,
        })
        .collect();
    ev.list(&groups)
}

pub fn match_<'a>(ev: &Eval<'a>, re: Value<'a>, s: Value<'a>) -> R<'a> {
    let re = ev.force_str_no_ctx(re)?;
    let r = compile_match_regex(ev, re)?;
    let s = ev.force_str(s)?;
    match r.captures(s.s) {
        Some(caps) => Ok(captures_list(ev, &caps)),
        None => Ok(Value::Null),
    }
}

/// A `builtins.split` pattern, searched for as Nix's POSIX `std::regex` does:
/// each match starts as early as possible and is then as long as possible,
/// with the groups of the first such match in order of alternatives and
/// repetitions.
struct SplitRegex {
    /// Finds where the next match starts.
    start: meta::Regex,
    /// Finds where the longest match from a start ends.
    longest: meta::Regex,
    /// Captures a match that ends at the end of the haystack.
    to_end: meta::Regex,
    /// Captures a match that ends at the end of a haystack cut short, where
    /// `$` cannot match.
    to_cut: meta::Regex,
}

fn compile_split_regex<'a>(ev: &Eval<'a>, re: &[u8]) -> R<'a, Rc<SplitRegex>> {
    ev.require(Feature::Regex)?;
    if let Some(r) = ev.ctx.regexes.splits.borrow().get(re) {
        return Ok(r.clone());
    }
    let build = |pattern: &str, kind| {
        meta::Builder::new()
            .configure(meta::Config::new().match_kind(kind).utf8_empty(false))
            .syntax(
                syntax::Config::new()
                    .unicode(false)
                    .utf8(false)
                    .dot_matches_new_line(true),
            )
            .build(pattern)
            .map_err(|e| regex_error(re, e))
    };
    let translated = translate_regex(re, "$").map_err(|e| regex_error(re, e))?;
    let r = Rc::new(SplitRegex {
        start: build(&translated, MatchKind::LeftmostFirst)?,
        longest: build(&translated, MatchKind::All)?,
        to_end: build(&format!("(?:{translated})\\z"), MatchKind::LeftmostFirst)?,
        to_cut: build(
            &format!(
                "(?:{})\\z",
                translate_regex(re, "[a&&b]").map_err(|e| regex_error(re, e))?
            ),
            MatchKind::LeftmostFirst,
        )?,
    });
    ev.ctx
        .regexes
        .splits
        .borrow_mut()
        .insert(re.to_vec(), r.clone());
    Ok(r)
}

pub fn split<'a>(ev: &Eval<'a>, re: Value<'a>, s: Value<'a>) -> R<'a> {
    let re = ev.force_str_no_ctx(re)?;
    let r = compile_split_regex(ev, re)?;
    let s = ev.force_str(s)?;
    let hay = s.s;
    let mut caps = r.to_end.create_captures();
    let mut out = Vec::new();
    let mut last = 0;
    let mut at = 0;
    while at <= hay.len() {
        let Some(m) = r.start.find(Input::new(hay).range(at..)) else {
            break;
        };
        let start = m.start();
        let end = r
            .longest
            .find(Input::new(hay).range(start..).anchored(Anchored::Yes))
            .expect("a match starts here")
            .end();
        // Cutting the haystack at the end of the match makes `\z` find the
        // groups of a match that ends there, but would let `$` match early.
        let to = if end == hay.len() {
            &r.to_end
        } else {
            &r.to_cut
        };
        to.captures(
            Input::new(&hay[..end])
                .range(start..)
                .anchored(Anchored::Yes),
            &mut caps,
        );
        let groups: Vec<Value<'a>> = (1..caps.group_len())
            .map(|i| match caps.get_group(i) {
                Some(g) => ev.str_val(&hay[g.range()], &[]),
                None => Value::Null,
            })
            .collect();
        out.push(ev.str_val(&hay[last..start], &[]));
        out.push(ev.list(&groups));
        last = end;
        // As with `std::regex_iterator`, an empty match may directly follow
        // another match, but the next search after one starts a byte later.
        at = if start == end { end + 1 } else { end };
    }
    if out.is_empty() {
        // Nix returns the string itself, context included, when nothing
        // matches; pieces around a match carry no context.
        return Ok(ev.list(&[Value::Str(s)]));
    }
    out.push(ev.str_val(&hay[last..], &[]));
    Ok(ev.list(&out))
}
