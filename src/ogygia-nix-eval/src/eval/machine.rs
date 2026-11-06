//! The evaluation loop.
//!
//! Evaluating an expression, forcing a thunk and applying a function each
//! run as a sequence of [`Step`]s. Work that waits for a value is pushed as
//! a [`Frame`] on a stack on the heap, so the depth of a Nix evaluation
//! never grows the native stack. Native code (a builtin, or a walk over a
//! value) re-enters the loop only inside a call or a walk, each of which
//! counts towards max-call-depth, which bounds how many loops are nested on
//! the native stack.

use super::Arith;
use super::Coerce;
use super::Eval;
use crate::builtins::Builtin;
use crate::builtins::Kind;
use crate::ir::AttrKey;
use crate::ir::AttrValue;
use crate::ir::AttrsDef;
use crate::ir::BinOp;
use crate::ir::Expr;
use crate::ir::ExprRef;
use crate::ir::LambdaDef;
use crate::ir::Param;
use crate::ir::Pos;
use crate::path::canon_path;
use crate::symbol::Sym;
use crate::value::Attrs;
use crate::value::Closure;
use crate::value::Ctx;
use crate::value::Entry;
use crate::value::Env;
use crate::value::ErrorKind;
use crate::value::EvalError;
use crate::value::List;
use crate::value::PathV;
use crate::value::PrimOpApp;
use crate::value::PrimOpFn;
use crate::value::R;
use crate::value::Thunk;
use crate::value::ThunkState;
use crate::value::Value;
use crate::value::error;
use crate::value::eval_err;

/// What the loop does next.
pub(super) enum Step<'a> {
    /// Evaluate an expression to weak head normal form.
    Eval(ExprRef<'a>, &'a Env<'a>),
    /// Force a value to weak head normal form.
    Force(Value<'a>),
    /// Apply a function to an argument.
    Call(Value<'a>, Value<'a>, Option<Pos<'a>>),
    /// Hand a value in weak head normal form to the frame on top of the
    /// stack.
    Return(Value<'a>),
}

/// Work waiting for the value of the steps above it.
pub(super) enum Frame<'a> {
    /// A thunk being forced, with its state from before. The value is
    /// stored in it; a failure restores the state.
    Thunk(&'a Thunk<'a>, ThunkState<'a>),
    /// A call, with the depth from before it, which is restored on leaving
    /// it either way, the lambda called once its body is evaluated, which a
    /// failure is traced to, and where the call is written, if anywhere.
    Leave(usize, Option<&'a LambdaDef<'a>>, Option<Pos<'a>>),
    /// Apply the function to the remaining arguments of an application.
    Apply {
        args: &'a [ExprRef<'a>],
        env: &'a Env<'a>,
        pos: Pos<'a>,
    },
    /// Apply the function, once forced, to `arg`.
    Call {
        arg: Value<'a>,
        pos: Option<Pos<'a>>,
    },
    /// Bind the formals of a lambda to its argument, once forced.
    Pattern {
        closure: &'a Closure<'a>,
        arg: Value<'a>,
    },
    If {
        then: ExprRef<'a>,
        else_: ExprRef<'a>,
        env: &'a Env<'a>,
    },
    Assert {
        body: ExprRef<'a>,
        env: &'a Env<'a>,
        pos: Pos<'a>,
    },
    Not,
    /// The operand of a binary operator evaluated first, with the other.
    Operand1 {
        op: BinOp,
        other: ExprRef<'a>,
        env: &'a Env<'a>,
        pos: Pos<'a>,
    },
    /// The operand of `==`, `!=`, `++` or a numeric `+` evaluated second,
    /// with the first.
    Operand2 {
        op: BinOp,
        first: Value<'a>,
        pos: Pos<'a>,
    },
    /// The right operand of `&&`, `||` or `->`, which must be a Boolean. Each
    /// operator frame ends with where the operator is written.
    Bool(BinOp, Pos<'a>),
    /// The left operand of `//`, with the right.
    UpdateLeft(&'a Attrs<'a>, Pos<'a>),
    /// The right operand of `+` after a path.
    AddPath(&'a PathV<'a>, Pos<'a>),
    /// The right operand of `+` after a left operand coerced to a string.
    AddStr(Box<StrAcc<'a>>, Coerce, Pos<'a>),
    /// A namespace of a `with` that a variable may come from.
    WithVar(WithVar<'a>, u32),
    /// A part of an interpolated string or path.
    Interp(Box<Interp<'a>>),
    /// The value an attribute path is selected from.
    SelectBase(Select<'a>),
    /// The name of the attribute at `i` in a path, selected from `set`.
    SelectName {
        sel: Select<'a>,
        i: u32,
        set: Value<'a>,
    },
    /// The set the attribute at `i` in a path, named `name`, is selected
    /// from.
    SelectSet {
        sel: Select<'a>,
        i: u32,
        name: Sym,
    },
    /// The set the attribute at `i` in a path is looked up in.
    HasSet(HasAttr<'a>, u32),
    /// The name of the attribute at `i` in a path, looked up in `set`.
    HasName {
        has: HasAttr<'a>,
        i: u32,
        set: Value<'a>,
    },
    /// The name of a dynamic attribute of a set.
    Dynamic(Box<Dynamic<'a>>),
}

/// A string being built, with its context.
#[derive(Default)]
pub(super) struct StrAcc<'a> {
    buf: Vec<u8>,
    ctx: Vec<Ctx<'a>>,
}

#[derive(Clone, Copy)]
pub(super) struct WithVar<'a> {
    name: Sym,
    withs: &'a [u32],
    env: &'a Env<'a>,
    pos: Pos<'a>,
}

pub(super) struct Interp<'a> {
    path: bool,
    parts: &'a [ExprRef<'a>],
    env: &'a Env<'a>,
    i: usize,
    acc: StrAcc<'a>,
}

#[derive(Clone, Copy)]
pub(super) struct Select<'a> {
    path: &'a [AttrKey<'a>],
    default: Option<ExprRef<'a>>,
    env: &'a Env<'a>,
    pos: Pos<'a>,
}

#[derive(Clone, Copy)]
pub(super) struct HasAttr<'a> {
    path: &'a [AttrKey<'a>],
    env: &'a Env<'a>,
}

pub(super) struct Dynamic<'a> {
    def: &'a AttrsDef<'a>,
    env: &'a Env<'a>,
    entries: Vec<Entry<'a>>,
    i: usize,
}

/// `v` if it is in weak head normal form, or the value of a thunk already
/// forced.
pub(super) fn whnf<'a>(v: Value<'a>) -> Option<Value<'a>> {
    match v {
        Value::Thunk(t) => match t.0.get() {
            ThunkState::Done(v) => Some(v),
            _ => None,
        },
        v => Some(v),
    }
}

impl<'a> Eval<'a> {
    /// Run the loop from `step` until it returns a value.
    pub(super) fn run(&self, step: Step<'a>) -> R<'a> {
        let depth = self.depth.get();
        let outer = self.run_depth.replace(Some(depth));
        // Native code starts a loop only one call or walk level deeper than
        // the loop it runs in, so nesting is bounded by max-call-depth.
        assert!(
            outer.is_none_or(|d| depth > d),
            "evaluation started natively without counting depth"
        );
        let mut stack = self.spare_stacks.borrow_mut().pop().unwrap_or_default();
        let r = self.run_on(&mut stack, step);
        self.spare_stacks.borrow_mut().push(stack);
        self.run_depth.set(outer);
        r
    }

    fn run_on(&self, stack: &mut Vec<Frame<'a>>, mut step: Step<'a>) -> R<'a> {
        loop {
            let next = match step {
                Step::Eval(e, env) => self.eval_step(stack, e, env),
                Step::Force(v) => self.force_step(stack, v),
                Step::Call(f, arg, pos) => self.call_step(stack, f, arg, pos),
                Step::Return(v) => match stack.pop() {
                    Some(frame) => self.resume(stack, frame, v),
                    None => return Ok(v),
                },
            };
            step = match next {
                Ok(step) => step,
                Err(e) => return Err(self.unwind(stack, e)),
            };
        }
    }

    /// Drop the frames of `stack` after `err`.
    fn unwind(
        &self,
        stack: &mut Vec<Frame<'a>>,
        mut err: Box<EvalError<'a>>,
    ) -> Box<EvalError<'a>> {
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Thunk(t, state) => t.0.set(state),
                Frame::Leave(depth, lambda, pos) => {
                    self.depth.set(depth);
                    if let Some(def) = lambda {
                        err.push_trace(
                            format!("while calling '{}'", self.lambda_name(def.name)),
                            Some(pos.unwrap_or(def.pos)),
                        );
                    }
                }
                _ => {}
            }
        }
        err
    }

    fn force_step(&self, stack: &mut Vec<Frame<'a>>, v: Value<'a>) -> R<'a, Step<'a>> {
        let Value::Thunk(t) = v else {
            return Ok(Step::Return(v));
        };
        let state = t.0.get();
        let step = match state {
            ThunkState::Done(v) => return Ok(Step::Return(v)),
            ThunkState::Blackhole => {
                return Err(error(
                    ErrorKind::InfiniteRecursion,
                    "infinite recursion encountered",
                ));
            }
            ThunkState::Expr(e, env) => Step::Eval(e, env),
            ThunkState::App(f, x) => Step::Call(f, x, None),
            ThunkState::Native(f) => {
                t.0.set(ThunkState::Blackhole);
                stack.push(Frame::Thunk(t, state));
                // Like a primop, one call deeper.
                let depth = self.enter()?;
                stack.push(Frame::Leave(depth, None, None));
                return Ok(Step::Force(f(self)?));
            }
        };
        t.0.set(ThunkState::Blackhole);
        stack.push(Frame::Thunk(t, state));
        Ok(step)
    }

    fn eval_step(
        &self,
        stack: &mut Vec<Frame<'a>>,
        e: ExprRef<'a>,
        env: &'a Env<'a>,
    ) -> R<'a, Step<'a>> {
        Ok(match e {
            Expr::Null => Step::Return(Value::Null),
            Expr::Bool(b) => Step::Return(Value::Bool(*b)),
            Expr::Int(i) => Step::Return(Value::Int(*i)),
            Expr::Float(f) => Step::Return(Value::Float(*f)),
            Expr::Str(s) => Step::Return(Value::Str(s)),
            Expr::Path(p) => Step::Return(Value::Path(p)),
            Expr::Var(d, i) => return self.force_step(stack, env.up(*d).slots[*i as usize].get()),
            Expr::Global(b) => return self.force_step(stack, self.globals[*b]),
            Expr::WithVar { name, withs, pos } => {
                let w = WithVar {
                    name: *name,
                    withs,
                    env,
                    pos: *pos,
                };
                return self.with_var(stack, w, 0, None);
            }
            Expr::Lambda(def) => Step::Return(Value::Lambda(self.bump.alloc(Closure { def, env }))),
            Expr::List([]) => Step::Return(Value::List(self.empty_list)),
            Expr::List(items) => {
                let items = self
                    .bump
                    .alloc_slice_fill_iter(items.iter().map(|i| self.lazy(i, env)));
                Step::Return(Value::List(self.bump.alloc(List { items })))
            }
            Expr::Attrs(def) => return self.eval_attrs(stack, def, env),
            Expr::Select {
                expr,
                path,
                default,
                pos,
            } => {
                stack.push(Frame::SelectBase(Select {
                    path,
                    default: *default,
                    env,
                    pos: *pos,
                }));
                Step::Eval(expr, env)
            }
            Expr::HasAttr { expr, path } => {
                // Only the sets along the path are forced, never the value of
                // the last attribute.
                return self.has_attr(stack, HasAttr { path, env }, 0, self.lazy(expr, env), None);
            }
            Expr::Apply { func, args, pos } => {
                stack.push(Frame::Apply {
                    args,
                    env,
                    pos: *pos,
                });
                Step::Eval(func, env)
            }
            Expr::Let(def) => Step::Eval(def.body, self.new_frame(def.slots, env)),
            Expr::With { namespace, body } => {
                let ns = self.lazy(namespace, env);
                let env = self.alloc_env(1, env);
                env.slots[0].set(ns);
                Step::Eval(body, env)
            }
            Expr::If(c, t, f) => {
                stack.push(Frame::If {
                    then: t,
                    else_: f,
                    env,
                });
                Step::Eval(c, env)
            }
            Expr::Assert { cond, body, pos } => {
                stack.push(Frame::Assert {
                    body,
                    env,
                    pos: *pos,
                });
                Step::Eval(cond, env)
            }
            Expr::Not(e) => {
                stack.push(Frame::Not);
                Step::Eval(e, env)
            }
            Expr::Bin(op, l, r, pos) => {
                // Nix evaluates the right operand of `//` first.
                let (first, other) = match op {
                    BinOp::Update => (r, l),
                    _ => (l, r),
                };
                stack.push(Frame::Operand1 {
                    op: *op,
                    other,
                    env,
                    pos: *pos,
                });
                Step::Eval(first, env)
            }
            Expr::InterpStr(parts) | Expr::InterpPath(parts) => {
                let s = Interp {
                    path: matches!(e, Expr::InterpPath(..)),
                    parts,
                    env,
                    i: 0,
                    acc: StrAcc::default(),
                };
                return self.interp(stack, Box::new(s));
            }
            Expr::CurPos(pos) => Step::Return(self.pos_value(*pos)),
        })
    }

    /// Continue `frame` with the value `v` of the steps above it.
    fn resume(
        &self,
        stack: &mut Vec<Frame<'a>>,
        frame: Frame<'a>,
        v: Value<'a>,
    ) -> R<'a, Step<'a>> {
        match frame {
            Frame::Thunk(t, _) => {
                t.0.set(ThunkState::Done(v));
                Ok(Step::Return(v))
            }
            Frame::Leave(depth, ..) => {
                self.depth.set(depth);
                Ok(Step::Return(v))
            }
            Frame::Apply { args, env, pos } => {
                let Some((arg, rest)) = args.split_first() else {
                    return Ok(Step::Return(v));
                };
                if !rest.is_empty() {
                    stack.push(Frame::Apply {
                        args: rest,
                        env,
                        pos,
                    });
                }
                Ok(Step::Call(v, self.lazy(arg, env), Some(pos)))
            }
            Frame::Call { arg, pos } => self.apply(stack, v, arg, pos),
            Frame::Pattern { closure, arg } => self.bind_pattern(stack, closure, arg, v),
            Frame::If { then, else_, env } => Ok(Step::Eval(
                if self.force_bool(v)? { then } else { else_ },
                env,
            )),
            Frame::Assert { body, env, pos } => match self.force_bool(v)? {
                true => Ok(Step::Eval(body, env)),
                false => Err(error(ErrorKind::Assert, "assertion failed")
                    .at_labelled(pos, "the condition is false")),
            },
            Frame::Not => Ok(Step::Return(Value::Bool(!self.force_bool(v)?))),
            Frame::Bool(op, pos) => {
                let b = self.force_bool(v).map_err(|e| in_op(e, op, pos))?;
                Ok(Step::Return(Value::Bool(b)))
            }
            Frame::Operand1 {
                op,
                other,
                env,
                pos,
            } => self
                .operand1(stack, op, v, other, env, pos)
                .map_err(|e| in_op(e, op, pos)),
            Frame::Operand2 { op, first, pos } => self
                .operand2(op, first, v, pos)
                .map_err(|e| in_op(e, op, pos)),
            Frame::UpdateLeft(right, pos) => {
                let left = self
                    .force_attrs(v)
                    .map_err(|e| in_op(e, BinOp::Update, pos))?;
                Ok(Step::Return(self.update(left, right)))
            }
            Frame::AddPath(p, pos) => {
                let mut buf = crate::path::abs(p.0).as_bytes().to_vec();
                let mut ctx = Vec::new();
                self.coerce_into(v, Coerce::PLAIN, &mut buf, &mut ctx)
                    .map_err(|e| in_op(e, BinOp::Add, pos))?;
                self.appended_path(&buf, &ctx)
                    .map_err(|e| in_op(e, BinOp::Add, pos))
            }
            Frame::AddStr(mut acc, how, pos) => {
                self.coerce_into(v, how, &mut acc.buf, &mut acc.ctx)
                    .map_err(|e| in_op(e, BinOp::Add, pos))?;
                Ok(Step::Return(self.str_with_ctx(&acc.buf, acc.ctx)))
            }
            Frame::WithVar(w, i) => self.with_var(stack, w, i as usize, Some(v)),
            Frame::Interp(mut s) => {
                let acc = &mut s.acc;
                match (s.path, s.i, v) {
                    (true, 0, Value::Path(p)) => {
                        acc.buf.extend_from_slice(crate::path::abs(p.0).as_bytes())
                    }
                    (true, ..) => self.coerce_into(v, Coerce::PLAIN, &mut acc.buf, &mut acc.ctx)?,
                    (false, ..) => {
                        self.coerce_into(v, Coerce::INTERP, &mut acc.buf, &mut acc.ctx)?
                    }
                }
                s.i += 1;
                self.interp(stack, s)
            }
            Frame::SelectBase(sel) => self.select(stack, sel, 0, v, None),
            Frame::SelectName { sel, i, set } => {
                let name = self.attr_name(v)?;
                self.select(stack, sel, i as usize, set, Some(name))
            }
            Frame::SelectSet { sel, i, name } => self.select(stack, sel, i as usize, v, Some(name)),
            Frame::HasSet(has, i) => self.has_attr(stack, has, i as usize, v, None),
            Frame::HasName { has, i, set } => {
                let name = self.attr_name(v)?;
                self.has_attr(stack, has, i as usize, set, Some(name))
            }
            Frame::Dynamic(mut d) => {
                let attr = &d.def.dynamics[d.i];
                if !matches!(v, Value::Null) {
                    let name = self.attr_name(v)?;
                    if d.entries.iter().any(|e| e.name == name) {
                        return Err(error(
                            ErrorKind::Eval,
                            format!("dynamic attribute '{}' already defined", self.name(name)),
                        )
                        .at_labelled(attr.pos, "already defined"));
                    }
                    d.entries.push(Entry {
                        name,
                        value: self.lazy(attr.value, d.env),
                        pos: Some(attr.pos),
                    });
                }
                d.i += 1;
                self.dynamic_attrs(stack, d)
            }
        }
    }

    // ---- function calls ---------------------------------------------------

    fn call_step(
        &self,
        stack: &mut Vec<Frame<'a>>,
        f: Value<'a>,
        arg: Value<'a>,
        pos: Option<Pos<'a>>,
    ) -> R<'a, Step<'a>> {
        match whnf(f) {
            Some(f) => self.apply(stack, f, arg, pos),
            None => {
                stack.push(Frame::Call { arg, pos });
                Ok(Step::Force(f))
            }
        }
    }

    /// Apply the forced function `f` to `arg`, one call deeper.
    fn apply(
        &self,
        stack: &mut Vec<Frame<'a>>,
        f: Value<'a>,
        arg: Value<'a>,
        pos: Option<Pos<'a>>,
    ) -> R<'a, Step<'a>> {
        let depth = self.enter()?;
        stack.push(Frame::Leave(depth, None, pos));
        match f {
            Value::Lambda(c) => self.enter_lambda(stack, c, arg),
            Value::PrimOp(op) => self.call_primop(op, &[], arg, pos),
            Value::PrimOpApp(app) => self.call_primop(app.op, app.args, arg, pos),
            _ if let Some(functor) = self.functor(f) => {
                stack.push(Frame::Call { arg, pos });
                Ok(Step::Call(functor, f, None))
            }
            other => {
                let e = error(
                    ErrorKind::Type,
                    format!(
                        "attempt to call something which is not a function but {}",
                        self.describe(other)
                    ),
                );
                Err(e.at_labelled(pos, "not a function"))
            }
        }
    }

    /// Apply `op`, already given `prev`, to one more argument, in a call
    /// written at `pos` if anywhere. A failure that happened nowhere else is
    /// put there.
    fn call_primop(
        &self,
        op: Builtin,
        prev: &[Value<'a>],
        arg: Value<'a>,
        pos: Option<Pos<'a>>,
    ) -> R<'a, Step<'a>> {
        let locate = |e: Box<EvalError<'a>>| e.at(pos);
        let Kind::PrimOp(f) = op.kind() else {
            unreachable!("only a primop is applied")
        };
        let r = match (f, prev) {
            (PrimOpFn::F1(f), []) => f(self, arg).map_err(locate)?,
            (PrimOpFn::F2(f), [a]) => f(self, *a, arg).map_err(locate)?,
            (PrimOpFn::F3(f), [a, b]) => f(self, *a, *b, arg).map_err(locate)?,
            _ => {
                let args = self
                    .bump
                    .alloc_slice_fill_with(prev.len() + 1, |i| prev.get(i).copied().unwrap_or(arg));
                return Ok(Step::Return(Value::PrimOpApp(
                    self.bump.alloc(PrimOpApp { op, args }),
                )));
            }
        };
        Ok(Step::Force(r))
    }

    fn enter_lambda(
        &self,
        stack: &mut Vec<Frame<'a>>,
        c: &'a Closure<'a>,
        arg: Value<'a>,
    ) -> R<'a, Step<'a>> {
        match &c.def.param {
            Param::Ident(_) => {
                let env = self.alloc_env(c.def.nslots(), c.env);
                env.slots[0].set(arg);
                self.body(stack, c.def, env)
            }
            Param::Pattern { .. } => match whnf(arg) {
                Some(set) => self.bind_pattern(stack, c, arg, set),
                None => {
                    stack.push(Frame::Pattern { closure: c, arg });
                    Ok(Step::Force(arg))
                }
            },
        }
    }

    /// Bind the formals of `c` to `set`, the forced value of `arg`.
    fn bind_pattern(
        &self,
        stack: &mut Vec<Frame<'a>>,
        c: &'a Closure<'a>,
        arg: Value<'a>,
        set: Value<'a>,
    ) -> R<'a, Step<'a>> {
        let def = c.def;
        let Param::Pattern {
            formals,
            ellipsis,
            at,
        } = &def.param
        else {
            unreachable!("only a pattern is bound")
        };
        let attrs = match set {
            Value::Attrs(a) => a,
            other => {
                return Err(error(
                    ErrorKind::Type,
                    format!(
                        "expected a set but found {} while evaluating the value passed for the lambda argument",
                        self.describe(other)
                    ),
                )
                .at_labelled(def.pos, "this function expects a set"));
            }
        };
        let env = self.alloc_env(c.def.nslots(), c.env);
        let mut used = 0;
        for (slot, formal) in env.slots.iter().zip(formals.iter()) {
            match attrs.get(formal.name) {
                Some(v) => {
                    used += 1;
                    slot.set(v);
                }
                None => match formal.default {
                    Some(d) => slot.set(self.lazy_slot(d, env)),
                    None => {
                        return Err(error(
                            ErrorKind::Type,
                            format!(
                                "function '{}' called without required argument '{}'",
                                self.lambda_name(def.name),
                                self.name(formal.name)
                            ),
                        )
                        .at_labelled(def.pos, format!("requires '{}'", self.name(formal.name))));
                    }
                },
            }
        }
        if !ellipsis && used != attrs.len() {
            let bad = attrs
                .sorted(self.ctx)
                .into_iter()
                .find(|e| !formals.iter().any(|f| f.name == e.name));
            if let Some(bad) = bad {
                return Err(error(
                    ErrorKind::Type,
                    format!(
                        "function '{}' called with unexpected argument '{}'",
                        self.lambda_name(def.name),
                        self.name(bad.name)
                    ),
                )
                .at_labelled(def.pos, format!("does not take '{}'", self.name(bad.name))));
            }
        }
        if at.is_some() {
            env.slots[formals.len()].set(arg);
        }
        self.body(stack, def, env)
    }

    fn body(
        &self,
        stack: &mut Vec<Frame<'a>>,
        def: &'a LambdaDef<'a>,
        env: &'a Env<'a>,
    ) -> R<'a, Step<'a>> {
        // The call's frame is on top again once the argument is bound.
        let Some(Frame::Leave(_, lambda, _)) = stack.last_mut() else {
            unreachable!("a lambda body is evaluated in its call")
        };
        *lambda = Some(def);
        Ok(Step::Eval(def.body, env))
    }

    // ---- operators --------------------------------------------------------

    /// Continue a binary operator after the operand evaluated first, `a`.
    fn operand1(
        &self,
        stack: &mut Vec<Frame<'a>>,
        op: BinOp,
        a: Value<'a>,
        other: ExprRef<'a>,
        env: &'a Env<'a>,
        pos: Pos<'a>,
    ) -> R<'a, Step<'a>> {
        let frame = match (op, a) {
            (BinOp::And, Value::Bool(false)) => return Ok(Step::Return(Value::Bool(false))),
            (BinOp::Or, Value::Bool(true)) => return Ok(Step::Return(Value::Bool(true))),
            (BinOp::Impl, Value::Bool(false)) => return Ok(Step::Return(Value::Bool(true))),
            (BinOp::And | BinOp::Or | BinOp::Impl, Value::Bool(_)) => Frame::Bool(op, pos),
            (BinOp::And | BinOp::Or | BinOp::Impl, other) => {
                return self.type_error("a Boolean", other);
            }
            (BinOp::Eq | BinOp::Neq | BinOp::Concat, _)
            | (BinOp::Add, Value::Int(_) | Value::Float(_)) => {
                Frame::Operand2 { op, first: a, pos }
            }
            (BinOp::Update, _) => Frame::UpdateLeft(self.force_attrs(a)?, pos),
            (BinOp::Add, Value::Path(p)) => Frame::AddPath(p, pos),
            // Only a string on the left copies paths to the store; a set
            // that coerces to a string concatenates without copying.
            (BinOp::Add, _) => {
                let how = match a {
                    Value::Str(_) => Coerce::INTERP,
                    _ => Coerce::PLAIN,
                };
                let mut acc = Box::<StrAcc>::default();
                self.coerce_into(a, how, &mut acc.buf, &mut acc.ctx)?;
                Frame::AddStr(acc, how, pos)
            }
        };
        stack.push(frame);
        Ok(Step::Eval(other, env))
    }

    /// Finish `==`, `!=`, `++` or a numeric `+` with both operands.
    fn operand2(&self, op: BinOp, a: Value<'a>, b: Value<'a>, pos: Pos<'a>) -> R<'a, Step<'a>> {
        Ok(Step::Return(match op {
            BinOp::Eq => Value::Bool(self.eq(a, b)?),
            BinOp::Neq => Value::Bool(!self.eq(a, b)?),
            BinOp::Concat => {
                let a = self.force_list(a)?;
                let b = self.force_list(b)?;
                if a.is_empty() {
                    self.list(b)
                } else if b.is_empty() {
                    self.list(a)
                } else {
                    let mut v = Vec::with_capacity(a.len() + b.len());
                    v.extend_from_slice(a);
                    v.extend_from_slice(b);
                    self.list(&v)
                }
            }
            // Unlike `builtins.add`, `+` fails on a number and a non-number
            // with a plain evaluation error.
            _ => self.arith(Arith::Add, a, b, Some(pos)).map_err(|mut e| {
                if e.kind == ErrorKind::Type {
                    e.kind = ErrorKind::Eval;
                }
                e
            })?,
        }))
    }

    // ---- variables, attributes and strings --------------------------------

    /// Look `w` up in the namespaces from the one at `i`, whose forced value
    /// is `ns` if it has been forced.
    fn with_var(
        &self,
        stack: &mut Vec<Frame<'a>>,
        w: WithVar<'a>,
        mut i: usize,
        mut ns: Option<Value<'a>>,
    ) -> R<'a, Step<'a>> {
        while let Some(&depth) = w.withs.get(i) {
            let forced = match ns.take() {
                Some(v) => v,
                None => {
                    let v = w.env.up(depth).slots[0].get();
                    match whnf(v) {
                        Some(v) => v,
                        None => {
                            stack.push(Frame::WithVar(w, i as u32));
                            return Ok(Step::Force(v));
                        }
                    }
                }
            };
            if let Some(v) = self.force_attrs(forced)?.get(w.name) {
                return Ok(Step::Force(v));
            }
            i += 1;
        }
        Err(error(
            ErrorKind::Eval,
            format!("undefined variable '{}'", self.name(w.name)),
        )
        .at_labelled(w.pos, "not defined"))
    }

    /// Select the path of `sel` from the attribute at `i` on, out of `v`,
    /// given the name of that attribute if it has been evaluated. Each name
    /// is evaluated before the value it selects from is forced.
    fn select(
        &self,
        stack: &mut Vec<Frame<'a>>,
        sel: Select<'a>,
        mut i: usize,
        mut v: Value<'a>,
        mut name: Option<Sym>,
    ) -> R<'a, Step<'a>> {
        while let Some(key) = sel.path.get(i) {
            let name = match (name.take(), key) {
                (Some(name), _) => name,
                (None, AttrKey::Static(name)) => *name,
                (None, AttrKey::Dynamic(e)) => {
                    stack.push(Frame::SelectName {
                        sel,
                        i: i as u32,
                        set: v,
                    });
                    return Ok(Step::Eval(e, sel.env));
                }
            };
            let Some(set) = whnf(v) else {
                stack.push(Frame::SelectSet {
                    sel,
                    i: i as u32,
                    name,
                });
                return Ok(Step::Force(v));
            };
            let found = match set {
                Value::Attrs(a) => a.get(name),
                _ => None,
            };
            match found {
                Some(x) => v = x,
                None => {
                    if let Some(d) = sel.default {
                        return Ok(Step::Eval(d, sel.env));
                    }
                    return match set {
                        Value::Attrs(_) => Err(error(
                            ErrorKind::Eval,
                            format!("attribute '{}' missing", self.name(name)),
                        )
                        .at_labelled(sel.pos, "no such attribute")),
                        other => Err(error(
                            ErrorKind::Type,
                            format!(
                                "expected a set but found {} while selecting attribute '{}'",
                                self.describe(other),
                                self.name(name)
                            ),
                        )
                        .at_labelled(sel.pos, format!("selected from {}", other.show_type()))),
                    };
                }
            }
            i += 1;
        }
        Ok(Step::Force(v))
    }

    /// Whether the path of `has` from the attribute at `i` on is in `v`,
    /// given the name of that attribute if it has been evaluated, in which
    /// case `v` is forced. Each set is forced before the name looked up in
    /// it is evaluated.
    fn has_attr(
        &self,
        stack: &mut Vec<Frame<'a>>,
        has: HasAttr<'a>,
        mut i: usize,
        mut v: Value<'a>,
        mut name: Option<Sym>,
    ) -> R<'a, Step<'a>> {
        while let Some(key) = has.path.get(i) {
            let Some(set) = whnf(v) else {
                stack.push(Frame::HasSet(has, i as u32));
                return Ok(Step::Force(v));
            };
            let name = match (name.take(), key) {
                (Some(name), _) => name,
                (None, AttrKey::Static(name)) => *name,
                (None, AttrKey::Dynamic(e)) => {
                    stack.push(Frame::HasName {
                        has,
                        i: i as u32,
                        set,
                    });
                    return Ok(Step::Eval(e, has.env));
                }
            };
            match set {
                Value::Attrs(a) => match a.get(name) {
                    Some(x) => v = x,
                    None => return Ok(Step::Return(Value::Bool(false))),
                },
                _ => return Ok(Step::Return(Value::Bool(false))),
            }
            i += 1;
        }
        Ok(Step::Return(Value::Bool(true)))
    }

    /// The attribute name the value of a dynamic name stands for.
    fn attr_name(&self, v: Value<'a>) -> R<'a, Sym> {
        let s = self.force_str_no_ctx(v)?;
        Ok(self.ctx.interner.intern_bytes(s))
    }

    fn eval_attrs(
        &self,
        stack: &mut Vec<Frame<'a>>,
        def: &'a AttrsDef<'a>,
        env: &'a Env<'a>,
    ) -> R<'a, Step<'a>> {
        let env = match def.frame {
            Some(inits) => self.new_frame(inits, env),
            None => env,
        };
        let entries: Vec<Entry<'a>> = def
            .statics
            .iter()
            .map(|a| Entry {
                name: a.name,
                pos: Some(a.pos),
                value: match &a.value {
                    AttrValue::Expr(e) => self.lazy(e, env),
                    AttrValue::Slot(i) => env.slots[*i as usize].get(),
                },
            })
            .collect();
        if def.dynamics.is_empty() {
            return Ok(Step::Return(self.attrs_sorted(&entries)));
        }
        self.dynamic_attrs(
            stack,
            Box::new(Dynamic {
                def,
                env,
                entries,
                i: 0,
            }),
        )
    }

    /// Add the dynamic attributes of a set from the one at `d.i` on.
    fn dynamic_attrs(
        &self,
        stack: &mut Vec<Frame<'a>>,
        mut d: Box<Dynamic<'a>>,
    ) -> R<'a, Step<'a>> {
        match d.def.dynamics.get(d.i) {
            Some(attr) => {
                let (name, env) = (attr.name, d.env);
                stack.push(Frame::Dynamic(d));
                Ok(Step::Eval(name, env))
            }
            None => {
                d.entries.sort_by_key(|e| e.name);
                Ok(Step::Return(self.attrs_sorted(&d.entries)))
            }
        }
    }

    /// Continue an interpolated string or path from the part at `s.i`.
    fn interp(&self, stack: &mut Vec<Frame<'a>>, s: Box<Interp<'a>>) -> R<'a, Step<'a>> {
        if let Some(&part) = s.parts.get(s.i) {
            let env = s.env;
            stack.push(Frame::Interp(s));
            return Ok(Step::Eval(part, env));
        }
        let StrAcc { buf, ctx } = s.acc;
        if !s.path {
            return Ok(Step::Return(self.str_with_ctx(&buf, ctx)));
        }
        self.appended_path(&buf, &ctx)
    }

    /// The path `buf`, built by appending strings with context `ctx` to a
    /// path.
    fn appended_path(&self, buf: &[u8], ctx: &[Ctx<'a>]) -> R<'a, Step<'a>> {
        if !ctx.is_empty() {
            return eval_err("a string that refers to a store path cannot be appended to a path");
        }
        Ok(Step::Return(
            self.path_val(&canon_path(&String::from_utf8_lossy(buf))),
        ))
    }
}

/// `e`, at the operator `op` written at `pos` unless it is somewhere more
/// precise.
fn in_op<'a>(e: Box<EvalError<'a>>, op: BinOp, pos: Pos<'a>) -> Box<EvalError<'a>> {
    e.at_labelled(pos, format!("in this '{}'", op.symbol()))
}

#[cfg(test)]
mod tests {
    use bumpalo::Bump;

    use crate::Context;
    use crate::Eval;
    use crate::Io;
    use crate::Settings;
    use crate::print_strict;

    /// A chain of thunks far deeper than a default thread's stack could
    /// hold native frames for.
    #[test]
    fn deep_thunk_chain_on_default_stack() {
        let expr = r#"
            let
              s = builtins.listToAttrs (builtins.genList (i: {
                name = toString i;
                value = if i == 0 then 0 else s.${toString (i - 1)} + 1;
              }) 100000);
            in
            s."99999"
        "#;
        let out = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let ctx = Context::new(Io::pure());
                let bump = Bump::new();
                let ev = Eval::new(&ctx, &bump, Settings::default());
                let v = ev.eval_string(expr, "/").unwrap();
                print_strict(&ev, v).unwrap()
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(out, b"99999");
    }
}
