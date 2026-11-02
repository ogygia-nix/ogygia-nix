//! Runtime values.
//!
//! Every value lives in the evaluation session's arena, so a [`Value`] is a
//! small `Copy` handle and nothing is freed until the session ends.

use std::cell::Cell;

use crate::builtins::Builtin;
use crate::eval::Eval;
use crate::ir::ExprRef;
use crate::ir::LambdaDef;
use crate::ir::Pos;
use crate::symbol::Sym;

#[derive(Clone, Copy)]
pub enum Value<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a NixStr<'a>),
    Path(&'a PathV<'a>),
    Attrs(&'a Attrs<'a>),
    List(&'a List<'a>),
    Lambda(&'a Closure<'a>),
    PrimOp(Builtin),
    PrimOpApp(&'a PrimOpApp<'a>),
    Thunk(&'a Thunk<'a>),
}

/// An element of a string's context: a store path the string refers to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Ctx<'a> {
    /// A plain store path.
    Opaque(&'a str),
    /// A derivation and its whole closure (`=` prefix).
    DrvDeep(&'a str),
    /// An output of a derivation (`!out!` prefix).
    Built { drv: &'a str, output: &'a str },
}

pub struct NixStr<'a> {
    pub s: &'a [u8],
    /// Sorted and deduplicated.
    pub ctx: &'a [Ctx<'a>],
}

impl NixStr<'_> {
    pub fn as_str_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.s)
    }
}

pub struct PathV<'a>(pub &'a str);

#[derive(Clone, Copy)]
pub struct Entry<'a> {
    pub name: Sym,
    pub value: Value<'a>,
    pub pos: Option<Pos<'a>>,
}

/// An attribute set; `entries` is sorted by symbol.
pub struct Attrs<'a> {
    pub entries: &'a [Entry<'a>],
}

impl<'a> Attrs<'a> {
    pub fn get(&self, name: Sym) -> Option<Value<'a>> {
        self.entry(name).map(|e| e.value)
    }

    pub fn entry(&self, name: Sym) -> Option<&Entry<'a>> {
        self.entries
            .binary_search_by_key(&name, |e| e.name)
            .ok()
            .map(|i| &self.entries[i])
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries in lexicographic name order, the order Nix exposes.
    pub fn sorted(&self, ctx: &crate::Context) -> Vec<Entry<'a>> {
        let mut v = self.entries.to_vec();
        v.sort_by(|a, b| ctx.name(a.name).cmp(ctx.name(b.name)));
        v
    }
}

pub struct List<'a> {
    pub items: &'a [Value<'a>],
}

pub struct Closure<'a> {
    pub def: &'a LambdaDef<'a>,
    pub env: &'a Env<'a>,
}

/// A builtin function, taking its arguments by position.
#[derive(Clone, Copy)]
pub enum PrimOpFn {
    F1(for<'a> fn(&Eval<'a>, Value<'a>) -> R<'a>),
    F2(for<'a> fn(&Eval<'a>, Value<'a>, Value<'a>) -> R<'a>),
    F3(for<'a> fn(&Eval<'a>, Value<'a>, Value<'a>, Value<'a>) -> R<'a>),
}

pub struct PrimOpApp<'a> {
    pub op: Builtin,
    pub args: &'a [Value<'a>],
}

#[derive(Clone, Copy)]
pub enum ThunkState<'a> {
    Expr(ExprRef<'a>, &'a Env<'a>),
    /// A lazy function application.
    App(Value<'a>, Value<'a>),
    /// Computed by native code, as part of evaluating something other than
    /// an expression, such as a flake.
    Native(&'a dyn Fn(&Eval<'a>) -> R<'a>),
    Blackhole,
    Done(Value<'a>),
}

pub struct Thunk<'a>(pub Cell<ThunkState<'a>>);

pub struct Env<'a> {
    pub parent: Option<&'a Env<'a>>,
    pub slots: &'a [Cell<Value<'a>>],
}

impl<'a> Env<'a> {
    pub fn up(&'a self, depth: u32) -> &'a Env<'a> {
        let mut env = self;
        for _ in 0..depth {
            env = env
                .parent
                .expect("variable depth exceeds environment chain");
        }
        env
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// `throw`; caught by `tryEval`.
    Throw,
    /// A failed `assert`; caught by `tryEval`.
    Assert,
    /// `abort`.
    Abort,
    InfiniteRecursion,
    /// A value of the wrong type, a function called with the wrong
    /// arguments, or a set given to a builtin without an attribute the
    /// builtin requires.
    Type,
    /// Any other evaluation error.
    Eval,
    /// Use of a feature in [`crate::Settings::disabled`].
    Disabled(crate::Feature),
    /// Use of a feature this evaluator deliberately leaves out, by name.
    Unimplemented(&'static str),
}

#[derive(Debug)]
pub struct EvalError<'a> {
    pub kind: ErrorKind,
    pub msg: String,
    /// Where the error happened, if known.
    pub pos: Option<Pos<'a>>,
    /// What is wrong at `pos`, in a few words.
    pub label: Option<String>,
    /// What evaluation was doing when it happened, innermost first.
    pub trace: Vec<TraceFrame<'a>>,
}

/// A step of evaluation an [`EvalError`] happened in.
#[derive(Debug)]
pub struct TraceFrame<'a> {
    pub msg: String,
    pub pos: Option<Pos<'a>>,
}

impl<'a> EvalError<'a> {
    /// This error, at `pos` unless it already has a position or happened in
    /// a step of evaluation inside the one at `pos`.
    pub fn at(mut self: Box<Self>, pos: impl Into<Option<Pos<'a>>>) -> Box<Self> {
        if self.pos.is_none() && self.trace.is_empty() {
            self.pos = pos.into();
        }
        self
    }

    /// This error, at `pos` where `label` says what is wrong, unless it
    /// already has a position or happened in a step of evaluation inside the
    /// one at `pos`.
    pub fn at_labelled(
        mut self: Box<Self>,
        pos: impl Into<Option<Pos<'a>>>,
        label: impl Into<String>,
    ) -> Box<Self> {
        if self.pos.is_none() && self.trace.is_empty() {
            self.pos = pos.into();
            self.label = Some(label.into());
        }
        self
    }

    /// Record that the error happened while doing what `msg` says, at `pos`.
    pub fn push_trace(&mut self, msg: impl Into<String>, pos: Option<Pos<'a>>) {
        self.trace.push(TraceFrame {
            msg: msg.into(),
            pos,
        });
    }
}

pub type R<'a, T = Value<'a>> = Result<T, Box<EvalError<'a>>>;

pub fn error<'a>(kind: ErrorKind, msg: impl Into<String>) -> Box<EvalError<'a>> {
    Box::new(EvalError {
        kind,
        msg: msg.into(),
        pos: None,
        label: None,
        trace: Vec::new(),
    })
}

pub fn eval_err<'a, T>(msg: impl Into<String>) -> Result<T, Box<EvalError<'a>>> {
    Err(error(ErrorKind::Eval, msg))
}

/// An [`ErrorKind::Unimplemented`] error for `feature`, which
/// `builtins.tryEval` does not catch.
pub fn unimplemented<'a, T>(feature: &'static str) -> Result<T, Box<EvalError<'a>>> {
    Err(error(
        ErrorKind::Unimplemented(feature),
        format!("{feature} is not implemented by this evaluator"),
    ))
}

/// An [`ErrorKind::Type`] error.
pub fn type_err<'a, T>(msg: impl Into<String>) -> Result<T, Box<EvalError<'a>>> {
    Err(error(ErrorKind::Type, msg))
}

/// An [`ErrorKind::Eval`] error whose message is `e` and its causes.
pub fn anyhow_err<'a>(e: anyhow::Error) -> Box<EvalError<'a>> {
    error(ErrorKind::Eval, format!("{e:#}"))
}

impl<'a> Value<'a> {
    /// The type name as returned by `builtins.typeOf`.
    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Path(_) => "path",
            Value::Attrs(_) => "set",
            Value::List(_) => "list",
            Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_) => "lambda",
            Value::Thunk(_) => "thunk",
        }
    }

    /// The type described with an article, as in Nix's error messages.
    pub fn show_type(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "a Boolean",
            Value::Int(_) => "an integer",
            Value::Float(_) => "a float",
            Value::Str(_) => "a string",
            Value::Path(_) => "a path",
            Value::Attrs(_) => "a set",
            Value::List(_) => "a list",
            Value::Lambda(_) => "a function",
            Value::PrimOp(_) => "a built-in function",
            Value::PrimOpApp(_) => "a partially applied built-in function",
            Value::Thunk(_) => "a thunk",
        }
    }

    pub fn is_function(&self) -> bool {
        matches!(
            self,
            Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_)
        )
    }

    /// Identity of the heap object behind this value, if any.
    pub fn ptr(&self) -> Option<*const ()> {
        Some(match self {
            Value::Str(s) => *s as *const _ as *const (),
            Value::Path(p) => *p as *const _ as *const (),
            Value::Attrs(a) => *a as *const _ as *const (),
            Value::List(l) => *l as *const _ as *const (),
            Value::Lambda(c) => *c as *const _ as *const (),
            Value::PrimOpApp(p) => *p as *const _ as *const (),
            Value::Thunk(t) => *t as *const _ as *const (),
            _ => return None,
        })
    }
}
