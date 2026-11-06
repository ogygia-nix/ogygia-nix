//! Evaluation errors as reports for people, quoting the source they point at.

use std::fmt;
use std::ops::Range;

use ariadne::Color;
use ariadne::Config;
use ariadne::Fmt;
use ariadne::IndexType;
use ariadne::Label;
use ariadne::Report;
use ariadne::ReportKind;

use crate::ir::Pos;
use crate::ir::Source;
use crate::value::EvalError;

/// The most steps of evaluation a report lists, innermost first.
const MAX_TRACE: usize = 30;

/// An evaluation error, with the source it points at and the steps of
/// evaluation it happened in. [`Display`](fmt::Display) renders it without
/// colour.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    message: String,
    span: Option<Span>,
    /// What is wrong at `span`.
    label: String,
    /// Innermost first.
    trace: Vec<(String, Option<Span>)>,
    /// Steps of evaluation left out of `trace`.
    omitted: usize,
    /// The names and text of the sources spans point into.
    sources: Vec<(String, String)>,
}

/// A range of bytes in one of a [`Diagnostic`]'s sources.
#[derive(Clone, Debug)]
struct Span {
    source: usize,
    range: Range<usize>,
}

impl Diagnostic {
    pub(crate) fn new<'a>(e: &EvalError<'a>) -> Diagnostic {
        let mut sources: Vec<&Source<'a>> = Vec::new();
        let mut span = |pos: Option<Pos<'a>>| {
            let pos = pos?;
            let source = match sources.iter().position(|s| std::ptr::eq(*s, pos.source)) {
                Some(i) => i,
                None => {
                    sources.push(pos.source);
                    sources.len() - 1
                }
            };
            Some(Span {
                source,
                range: pos.offset as usize..pos.end as usize,
            })
        };
        let main = span(e.pos);
        let trace = e
            .trace
            .iter()
            .take(MAX_TRACE)
            .map(|t| (t.msg.clone(), span(t.pos)))
            .collect();
        Diagnostic {
            message: e.msg.clone(),
            span: main,
            label: e.label.clone().unwrap_or_else(|| "here".to_owned()),
            trace,
            omitted: e.trace.len().saturating_sub(MAX_TRACE),
            sources: sources
                .into_iter()
                .map(|s| {
                    let name = s.path.unwrap_or("«string»");
                    (name.to_owned(), s.text.to_owned())
                })
                .collect(),
        }
    }

    /// The report, with ANSI colours if `color`.
    pub fn render(&self, color: bool) -> String {
        let mut cache = Sources(
            self.sources
                .iter()
                .map(|(name, text)| (name.as_str(), ariadne::Source::from(text.as_str())))
                .collect(),
        );
        let mut out = Vec::new();
        let config = Config::default()
            .with_color(color)
            .with_index_type(IndexType::Byte);
        // A report with a span is headed by `header` and says what is at the
        // span in `label`; one without is the single line `kind: label`.
        let mut report =
            |kind: &str, hue: Color, header: &str, label: &str, span: &Option<Span>| {
                let Some(span) = span else {
                    let kind = match color {
                        true => kind.fg(hue).to_string(),
                        false => kind.to_owned(),
                    };
                    out.extend_from_slice(format!("{kind}: {label}\n").as_bytes());
                    return;
                };
                let range = self.shown(span);
                Report::build(ReportKind::Custom(kind, hue), (span.source, range.clone()))
                    .with_config(config)
                    .with_message(header)
                    .with_label(
                        Label::new((span.source, range))
                            .with_message(label)
                            .with_color(hue),
                    )
                    .finish()
                    .write(&mut cache, &mut out)
                    .expect("writing to a Vec cannot fail");
            };
        match &self.span {
            Some(_) => report("error", Color::Red, &self.message, &self.label, &self.span),
            None => report("error", Color::Red, "", &self.message, &None),
        }
        for (msg, span) in &self.trace {
            report("note", Color::Blue, "", msg, span);
        }
        if self.omitted > 0 {
            let msg = format!("{} more steps of evaluation are not shown", self.omitted);
            report("note", Color::Blue, "", &msg, &None);
        }
        if !color {
            // ariadne colours the kind of a report even without colour.
            out = strip_ansi(&out);
        }
        let out = String::from_utf8_lossy(&out);
        let lines: Vec<_> = out.lines().map(str::trim_end).collect();
        lines.join("\n").trim_end().to_owned()
    }

    /// The part of `span` to underline: its first line, and at least one
    /// character where there is one.
    fn shown(&self, span: &Span) -> Range<usize> {
        let text = &self.sources[span.source].1;
        let start = span.range.start.min(text.len());
        let end = span.range.end.clamp(start, text.len());
        let end = match text[start..end].find('\n') {
            Some(nl) => start + nl,
            None => end,
        };
        let end = match text[start..].chars().next() {
            Some(c) if end == start && c != '\n' => start + c.len_utf8(),
            _ => end,
        };
        start..end
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(false))
    }
}

impl std::error::Error for Diagnostic {}

/// A [`Diagnostic`]'s sources, by index, for ariadne.
struct Sources<'d>(Vec<(&'d str, ariadne::Source<&'d str>)>);

impl<'d> ariadne::Cache<usize> for Sources<'d> {
    type Storage = &'d str;

    fn fetch(&mut self, id: &usize) -> Result<&ariadne::Source<&'d str>, impl fmt::Debug> {
        self.0.get(*id).map(|(_, s)| s).ok_or("no such source")
    }

    fn display<'a>(&self, id: &'a usize) -> Option<impl fmt::Display + 'a> {
        self.0.get(*id).map(|(name, _)| name.to_string())
    }
}

/// `bytes` without ANSI escape sequences.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}
