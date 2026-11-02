//! The compiled form of a Nix expression.
//!
//! Variables are resolved when compiling: a lexically bound name becomes a
//! (depth, slot) pair into the environment chain, so evaluation never looks a
//! name up by string except through `with`.
//!
//! Every node is allocated in a [`crate::Context`]'s arena and holds only
//! references into it, so nothing here owns heap memory of its own.

use crate::builtins::Builtin;
use crate::symbol::Sym;
use crate::value::NixStr;
use crate::value::PathV;

pub type ExprRef<'a> = &'a Expr<'a>;

/// A source file (or string) that expressions were compiled from.
pub struct Source<'a> {
    /// `None` for expressions that were not read from a file.
    pub path: Option<&'a str>,
    pub text: &'a str,
    pub line_starts: &'a [u32],
}

impl Source<'_> {
    /// The 1-based line and column of byte `offset`.
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.line_starts[line] as usize;
        let end = (offset as usize).min(self.text.len());
        let col = self
            .text
            .get(start..end)
            .map_or(end - start, |s| s.chars().count());
        (line as u32 + 1, col as u32 + 1)
    }
}

/// A span of a source file, from byte `offset` to byte `end`. Where a
/// position is reported as a line and column, it is that of `offset`.
#[derive(Clone, Copy)]
pub struct Pos<'a> {
    pub source: &'a Source<'a>,
    pub offset: u32,
    pub end: u32,
}

impl Pos<'_> {
    pub fn line_col(self) -> (u32, u32) {
        self.source.line_col(self.offset)
    }
}

impl std::fmt::Display for Pos<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (line, col) = self.line_col();
        match self.source.path {
            Some(p) => write!(f, "{p}:{line}:{col}"),
            None => write!(f, "«string»:{line}:{col}"),
        }
    }
}

impl std::fmt::Debug for Pos<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    Neq,
    And,
    Or,
    Impl,
    Add,
    Concat,
    Update,
}

impl BinOp {
    /// The operator as written.
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Eq => "==",
            BinOp::Neq => "!=",
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::Impl => "->",
            BinOp::Add => "+",
            BinOp::Concat => "++",
            BinOp::Update => "//",
        }
    }
}

pub enum AttrKey<'a> {
    Static(Sym),
    Dynamic(ExprRef<'a>),
}

pub enum AttrValue<'a> {
    /// Evaluate in the attribute set's environment.
    Expr(ExprRef<'a>),
    /// The value of a slot in the attribute set's own frame.
    Slot(u32),
}

pub struct StaticAttr<'a> {
    pub name: Sym,
    pub pos: Pos<'a>,
    pub value: AttrValue<'a>,
}

pub struct DynamicAttr<'a> {
    pub name: ExprRef<'a>,
    pub value: ExprRef<'a>,
    pub pos: Pos<'a>,
}

pub struct AttrsDef<'a> {
    /// The frame the attribute set introduces, or `None` if its values are
    /// evaluated directly in the enclosing environment.
    pub frame: Option<&'a [ExprRef<'a>]>,
    /// Sorted by symbol.
    pub statics: &'a [StaticAttr<'a>],
    pub dynamics: &'a [DynamicAttr<'a>],
}

pub struct LetDef<'a> {
    pub slots: &'a [ExprRef<'a>],
    pub body: ExprRef<'a>,
}

pub struct Formal<'a> {
    pub name: Sym,
    pub default: Option<ExprRef<'a>>,
}

pub enum Param<'a> {
    Ident(Sym),
    Pattern {
        formals: &'a [Formal<'a>],
        ellipsis: bool,
        at: Option<Sym>,
    },
}

pub struct LambdaDef<'a> {
    pub param: Param<'a>,
    pub body: ExprRef<'a>,
    pub pos: Pos<'a>,
    /// The name the lambda was bound to, for error messages.
    pub name: Option<Sym>,
}

impl LambdaDef<'_> {
    pub fn nslots(&self) -> usize {
        match &self.param {
            Param::Ident(_) => 1,
            Param::Pattern { formals, at, .. } => formals.len() + usize::from(at.is_some()),
        }
    }
}

pub enum Expr<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a NixStr<'a>),
    Path(&'a PathV<'a>),
    /// A path with interpolations; the first part evaluates to a path.
    InterpPath(&'a [ExprRef<'a>]),
    /// String interpolation; the result is always a string.
    InterpStr(&'a [ExprRef<'a>]),
    Var(u32, u32),
    WithVar {
        name: Sym,
        /// Depths of the enclosing `with` frames, innermost first.
        withs: &'a [u32],
        pos: Pos<'a>,
    },
    Global(Builtin),
    Select {
        expr: ExprRef<'a>,
        path: &'a [AttrKey<'a>],
        default: Option<ExprRef<'a>>,
        pos: Pos<'a>,
    },
    HasAttr {
        expr: ExprRef<'a>,
        path: &'a [AttrKey<'a>],
    },
    Attrs(&'a AttrsDef<'a>),
    List(&'a [ExprRef<'a>]),
    Lambda(&'a LambdaDef<'a>),
    Apply {
        func: ExprRef<'a>,
        args: &'a [ExprRef<'a>],
        pos: Pos<'a>,
    },
    Let(&'a LetDef<'a>),
    With {
        namespace: ExprRef<'a>,
        body: ExprRef<'a>,
    },
    If(ExprRef<'a>, ExprRef<'a>, ExprRef<'a>),
    Assert {
        cond: ExprRef<'a>,
        body: ExprRef<'a>,
        pos: Pos<'a>,
    },
    Bin(BinOp, ExprRef<'a>, ExprRef<'a>, Pos<'a>),
    Not(ExprRef<'a>),
    CurPos(Pos<'a>),
}
