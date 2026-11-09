//! The evaluator: forcing thunks, evaluating expressions, calling functions.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;

use bumpalo::Bump;
use enum_map::EnumMap;

use crate::builtins;
use crate::builtins::Builtin;
use crate::context::Context;
use crate::ir::Expr;
use crate::ir::ExprRef;
use crate::ir::Pos;
use crate::path::canon_path;
use crate::store::StorePath;
use crate::symbol::Sym;
use crate::value::Attrs;
use crate::value::Closure;
use crate::value::Ctx;
use crate::value::Entry;
use crate::value::Env;
use crate::value::ErrorKind;
use crate::value::List;
use crate::value::NixStr;
use crate::value::PathV;
use crate::value::R;
use crate::value::Thunk;
use crate::value::ThunkState;
use crate::value::Value;
use crate::value::anyhow_err;
use crate::value::error;
use crate::value::eval_err;
use crate::value::type_err;

mod machine;

use machine::Frame;
use machine::Step;

/// Nix's default `max-call-depth`.
const MAX_CALL_DEPTH: usize = 10_000;

/// Evaluation settings that are fixed for a session.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `builtins.currentSystem`, which pure evaluation leaves out.
    pub current_system: String,
    /// Search path entries for `<...>` lookups, as `(prefix, path)`.
    pub nix_path: Vec<(String, String)>,
    /// Features whose use fails evaluation with [`crate::Error::Disabled`],
    /// which `builtins.tryEval` does not catch.
    pub disabled: Vec<Feature>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            current_system: crate::CURRENT_SYSTEM.to_owned(),
            nix_path: Vec::new(),
            disabled: Vec::new(),
        }
    }
}

/// A part of the language that [`Settings::disabled`] can switch off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// Regular expressions, in `builtins.match` and `builtins.split`.
    Regex,
}

impl std::fmt::Display for Feature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Feature::Regex => "regex",
        })
    }
}

/// One evaluation session. Values are allocated in `bump` and live until the
/// session's arena is dropped.
pub struct Eval<'a> {
    pub ctx: &'a Context,
    pub bump: &'a Bump,
    pub settings: Settings,
    /// The value of every builtin.
    globals: EnumMap<Builtin, Value<'a>>,
    import_cache: RefCell<HashMap<String, Value<'a>>>,
    depth: Cell<usize>,
    /// Emptied frame stacks of finished evaluation loops, for reuse.
    spare_stacks: RefCell<Vec<Vec<Frame<'a>>>>,
    /// The depth the innermost evaluation loop started at.
    run_depth: Cell<Option<usize>>,
    pub(crate) empty_attrs: &'a Attrs<'a>,
    pub(crate) empty_list: &'a List<'a>,
}

/// Whether string coercion copies paths to the store, and whether it accepts
/// the extra types `toString` does.
#[derive(Clone, Copy)]
pub struct Coerce {
    pub more: bool,
    pub copy: bool,
}

/// An arithmetic operator.
#[derive(Clone, Copy)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    Div,
}

impl Coerce {
    pub const INTERP: Coerce = Coerce {
        more: false,
        copy: true,
    };
    pub const PLAIN: Coerce = Coerce {
        more: false,
        copy: false,
    };
}

impl<'a> Eval<'a> {
    /// Fail unless `feature` is enabled.
    pub(crate) fn require(&self, feature: Feature) -> R<'a, ()> {
        if self.settings.disabled.contains(&feature) {
            return Err(error(
                ErrorKind::Disabled(feature),
                format!("disabled feature: {feature}"),
            ));
        }
        Ok(())
    }

    pub fn new(ctx: &'a Context, bump: &'a Bump, settings: Settings) -> Eval<'a> {
        let mut ev = Eval {
            ctx,
            bump,
            settings,
            globals: EnumMap::from_fn(|_| Value::Null),
            import_cache: RefCell::new(HashMap::new()),
            depth: Cell::new(0),
            spare_stacks: RefCell::new(Vec::new()),
            run_depth: Cell::new(None),
            empty_attrs: bump.alloc(Attrs { entries: &[] }),
            empty_list: bump.alloc(List { items: &[] }),
        };
        ev.globals = builtins::make_globals(&ev);
        ev
    }

    pub fn sym(&self, name: &str) -> Sym {
        self.ctx.intern(name)
    }

    pub fn name(&self, sym: Sym) -> &'a str {
        self.ctx.name(sym)
    }

    // ---- allocation -------------------------------------------------------

    pub fn alloc<T>(&self, t: T) -> &'a T {
        self.bump.alloc(t)
    }

    pub fn str_val(&self, s: &[u8], ctx: &[Ctx<'a>]) -> Value<'a> {
        let s = self.bump.alloc_slice_copy(s);
        let ctx: &'a [Ctx<'a>] = if ctx.is_empty() {
            &[]
        } else {
            self.bump.alloc_slice_copy(ctx)
        };
        Value::Str(self.bump.alloc(NixStr { s, ctx }))
    }

    pub fn string(&self, s: &str) -> Value<'a> {
        self.str_val(s.as_bytes(), &[])
    }

    pub fn path_val(&self, p: &str) -> Value<'a> {
        Value::Path(self.bump.alloc(PathV(self.bump.alloc_str(p))))
    }

    pub fn list(&self, items: &[Value<'a>]) -> Value<'a> {
        if items.is_empty() {
            return Value::List(self.empty_list);
        }
        Value::List(self.bump.alloc(List {
            items: self.bump.alloc_slice_copy(items),
        }))
    }

    /// An attribute set from entries in any order; later duplicates win.
    pub fn attrs(&self, mut entries: Vec<Entry<'a>>) -> Value<'a> {
        if entries.is_empty() {
            return Value::Attrs(self.empty_attrs);
        }
        // Stable sort keeps duplicates in insertion order; keep the last.
        entries.sort_by_key(|e| e.name);
        let mut out: Vec<Entry<'a>> = Vec::with_capacity(entries.len());
        for e in entries {
            match out.last_mut() {
                Some(last) if last.name == e.name => *last = e,
                _ => out.push(e),
            }
        }
        self.attrs_sorted(&out)
    }

    /// An attribute set from entries already sorted by symbol, without
    /// duplicates.
    pub fn attrs_sorted(&self, entries: &[Entry<'a>]) -> Value<'a> {
        if entries.is_empty() {
            return Value::Attrs(self.empty_attrs);
        }
        Value::Attrs(self.bump.alloc(Attrs {
            entries: self.bump.alloc_slice_copy(entries),
        }))
    }

    pub fn entry(&self, name: &str, value: Value<'a>) -> Entry<'a> {
        Entry {
            name: self.sym(name),
            value,
            pos: None,
        }
    }

    pub fn thunk(&self, state: ThunkState<'a>) -> Value<'a> {
        Value::Thunk(self.bump.alloc(Thunk(Cell::new(state))))
    }

    /// A lazy application `f x`.
    pub fn lazy_app(&self, f: Value<'a>, x: Value<'a>) -> Value<'a> {
        self.thunk(ThunkState::App(f, x))
    }

    // ---- forcing ----------------------------------------------------------

    pub fn force(&self, v: Value<'a>) -> R<'a> {
        match machine::whnf(v) {
            Some(v) => Ok(v),
            None => self.run(Step::Force(v)),
        }
    }

    /// Force `v` and everything reachable from it.
    pub fn deep_force(&self, v: Value<'a>) -> R<'a> {
        let mut seen = HashSet::new();
        self.deep_force_inner(v, &mut seen)
    }

    fn deep_force_inner(&self, v: Value<'a>, seen: &mut HashSet<*const ()>) -> R<'a> {
        self.deeper(|| self.deep_force_level(v, seen))
    }

    fn deep_force_level(&self, v: Value<'a>, seen: &mut HashSet<*const ()>) -> R<'a> {
        let v = self.force(v)?;
        match v {
            Value::Attrs(a) => {
                if seen.insert(a as *const _ as *const ()) {
                    for e in a.entries {
                        self.deep_force_inner(e.value, seen).map_err(|mut err| {
                            err.push_trace(
                                format!("while evaluating the attribute '{}'", self.name(e.name)),
                                e.pos,
                            );
                            err
                        })?;
                    }
                }
            }
            Value::List(l) if seen.insert(l as *const _ as *const ()) => {
                for item in l.items {
                    self.deep_force_inner(*item, seen)?;
                }
            }
            _ => {}
        }
        Ok(v)
    }

    pub fn type_error<T>(&self, expected: &str, got: Value<'a>) -> R<'a, T> {
        type_err(format!(
            "expected {expected} but found {}",
            self.describe(got)
        ))
    }

    pub fn force_attrs(&self, v: Value<'a>) -> R<'a, &'a Attrs<'a>> {
        match self.force(v)? {
            Value::Attrs(a) => Ok(a),
            other => self.type_error("a set", other),
        }
    }

    pub fn force_list(&self, v: Value<'a>) -> R<'a, &'a [Value<'a>]> {
        match self.force(v)? {
            Value::List(l) => Ok(l.items),
            other => self.type_error("a list", other),
        }
    }

    pub fn force_int(&self, v: Value<'a>) -> R<'a, i64> {
        match self.force(v)? {
            Value::Int(i) => Ok(i),
            other => self.type_error("an integer", other),
        }
    }

    pub fn force_bool(&self, v: Value<'a>) -> R<'a, bool> {
        match self.force(v)? {
            Value::Bool(b) => Ok(b),
            other => self.type_error("a Boolean", other),
        }
    }

    pub fn force_function(&self, v: Value<'a>) -> R<'a> {
        let v = self.force(v)?;
        if v.is_function() || self.functor(v).is_some() {
            Ok(v)
        } else {
            self.type_error("a function", v)
        }
    }

    /// The `__functor` of `v`, if `v` is a set that has one.
    pub(crate) fn functor(&self, v: Value<'a>) -> Option<Value<'a>> {
        match v {
            Value::Attrs(a) => a.get(self.ctx.syms.functor),
            _ => None,
        }
    }

    /// A string value without coercion.
    pub fn force_str(&self, v: Value<'a>) -> R<'a, &'a NixStr<'a>> {
        match self.force(v)? {
            Value::Str(s) => Ok(s),
            other => self.type_error("a string", other),
        }
    }

    /// A string without context, without coercion.
    pub fn force_str_no_ctx(&self, v: Value<'a>) -> R<'a, &'a [u8]> {
        let s = self.force_str(v)?;
        if !s.ctx.is_empty() {
            return eval_err(format!(
                "the string '{}' is not allowed to refer to a store path",
                s.as_str_lossy()
            ));
        }
        Ok(s.s)
    }

    pub fn describe(&self, v: Value<'a>) -> String {
        format!("{}: {}", v.show_type(), crate::print::short(self, v))
    }

    // ---- evaluation -------------------------------------------------------

    /// A value for `e` in `env` without evaluating it, if that is cheap.
    pub fn lazy(&self, e: ExprRef<'a>, env: &'a Env<'a>) -> Value<'a> {
        match e {
            Expr::Null => Value::Null,
            Expr::Bool(b) => Value::Bool(*b),
            Expr::Int(i) => Value::Int(*i),
            Expr::Float(f) => Value::Float(*f),
            Expr::Str(s) => Value::Str(s),
            Expr::Path(p) => Value::Path(p),
            Expr::Var(d, i) => env.up(*d).slots[*i as usize].get(),
            Expr::Lambda(def) => Value::Lambda(self.bump.alloc(Closure { def, env })),
            Expr::Global(b) => self.globals[*b],
            _ => self.thunk(ThunkState::Expr(e, env)),
        }
    }

    /// Whether `v` is the value of a builtin, which Nix holds as a value
    /// even where it is a thunk here.
    pub fn is_builtin(&self, v: Value<'a>) -> bool {
        v.ptr()
            .is_some_and(|p| self.globals.values().any(|g| g.ptr() == Some(p)))
    }

    /// [`Self::lazy`] for `e` filling in a slot of `env` while only its
    /// first `filled` slots are filled in.
    fn lazy_slot(&self, e: ExprRef<'a>, env: &'a Env<'a>, filled: usize) -> Value<'a> {
        match e {
            Expr::Var(0, i) if *i as usize >= filled => self.thunk(ThunkState::Expr(e, env)),
            _ => self.lazy(e, env),
        }
    }

    pub fn eval(&self, e: ExprRef<'a>, env: &'a Env<'a>) -> R<'a> {
        self.run(Step::Eval(e, env))
    }

    /// `{ file, line, column }` for `pos`, or null for positions not in a file.
    pub fn pos_value(&self, pos: Pos<'a>) -> Value<'a> {
        let Some(path) = pos.source.path else {
            return Value::Null;
        };
        let (line, col) = pos.line_col();
        let s = &self.ctx.syms;
        self.attrs(vec![
            Entry {
                name: s.file,
                value: self.string(path),
                pos: None,
            },
            Entry {
                name: s.line,
                value: Value::Int(line as i64),
                pos: None,
            },
            Entry {
                name: s.column,
                value: Value::Int(col as i64),
                pos: None,
            },
        ])
    }

    pub fn str_with_ctx(&self, buf: &[u8], mut ctx: Vec<Ctx<'a>>) -> Value<'a> {
        ctx.sort();
        ctx.dedup();
        self.str_val(buf, &ctx)
    }

    /// A frame of `n` slots under `parent`, to be filled in.
    fn alloc_env(&self, n: usize, parent: &'a Env<'a>) -> &'a Env<'a> {
        let slots = self
            .bump
            .alloc_slice_fill_with(n, |_| Cell::new(Value::Null));
        self.bump.alloc(Env {
            parent: Some(parent),
            slots,
        })
    }

    /// A frame of `inits` under `parent`, filled in order. With `share`, a
    /// slot naming one before it holds that slot's value.
    pub(crate) fn new_frame(
        &self,
        inits: &'a [ExprRef<'a>],
        parent: &'a Env<'a>,
        share: bool,
    ) -> &'a Env<'a> {
        let env = self.alloc_env(inits.len(), parent);
        for (i, (slot, e)) in env.slots.iter().zip(inits).enumerate() {
            slot.set(self.lazy_slot(e, env, if share { i } else { 0 }));
        }
        env
    }

    // ---- function calls ---------------------------------------------------

    /// Apply `f` to each of `args` in turn. `pos` is where the application
    /// is written, if it is written anywhere.
    pub fn call(&self, f: Value<'a>, args: &[Value<'a>], pos: Option<Pos<'a>>) -> R<'a> {
        let mut f = f;
        for &arg in args {
            f = self.run(Step::Call(f, arg, pos))?;
        }
        Ok(f)
    }

    /// Run `f` one level down a walk over a value (deep forcing, comparison,
    /// coercion, serialisation), which counts towards `max-call-depth` like
    /// a call.
    pub(crate) fn deeper<T>(&self, f: impl FnOnce() -> R<'a, T>) -> R<'a, T> {
        let depth = self.enter()?;
        let r = f();
        self.depth.set(depth);
        r
    }

    /// Count one level towards `max-call-depth`, returning the depth to
    /// restore on leaving it.
    fn enter(&self) -> R<'a, usize> {
        let depth = self.depth.get();
        // Like Nix, fail only once the depth is already past the limit.
        if depth > MAX_CALL_DEPTH {
            return eval_err("stack overflow; max-call-depth exceeded");
        }
        self.depth.set(depth + 1);
        Ok(depth)
    }

    fn lambda_name(&self, name: Option<Sym>) -> &'a str {
        match name {
            Some(n) => self.name(n),
            None => "anonymous lambda",
        }
    }

    // ---- operators --------------------------------------------------------

    /// `a // b`.
    pub fn update(&self, a: &'a Attrs<'a>, b: &'a Attrs<'a>) -> Value<'a> {
        if a.is_empty() {
            return Value::Attrs(b);
        }
        if b.is_empty() {
            return Value::Attrs(a);
        }
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut i, mut j) = (0, 0);
        let (ae, be) = (a.entries, b.entries);
        while i < ae.len() && j < be.len() {
            match ae[i].name.cmp(&be[j].name) {
                std::cmp::Ordering::Less => {
                    out.push(ae[i]);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push(be[j]);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    out.push(be[j]);
                    i += 1;
                    j += 1;
                }
            }
        }
        out.extend_from_slice(&ae[i..]);
        out.extend_from_slice(&be[j..]);
        self.attrs_sorted(&out)
    }

    /// `a op b` on numbers. `pos` is where the operator is written, if it is
    /// written anywhere.
    pub fn arith(&self, op: Arith, a: Value<'a>, b: Value<'a>, pos: Option<Pos<'a>>) -> R<'a> {
        let a = self.force(a)?;
        let b = self.force(b)?;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                let (r, verb, sym) = match op {
                    Arith::Add => (x.checked_add(y), "adding", "+"),
                    Arith::Sub => (x.checked_sub(y), "subtracting", "-"),
                    Arith::Mul => (x.checked_mul(y), "multiplying", "*"),
                    Arith::Div => {
                        if y == 0 {
                            return Err(error(ErrorKind::Eval, "division by zero")
                                .at_labelled(pos, "the divisor is zero"));
                        }
                        (x.checked_div(y), "dividing", "/")
                    }
                };
                match r {
                    Some(r) => Ok(Value::Int(r)),
                    None => Err(error(
                        ErrorKind::Eval,
                        format!("integer overflow in {verb} {x} {sym} {y}"),
                    )
                    .at_labelled(pos, "overflows")),
                }
            }
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                let (x, y) = (as_f64(a), as_f64(b));
                Ok(Value::Float(match op {
                    Arith::Add => x + y,
                    Arith::Sub => x - y,
                    Arith::Mul => x * y,
                    Arith::Div => {
                        if y == 0.0 {
                            return Err(error(ErrorKind::Eval, "division by zero")
                                .at_labelled(pos, "the divisor is zero"));
                        }
                        x / y
                    }
                }))
            }
            (Value::Int(_) | Value::Float(_), other) | (other, _) => {
                self.type_error("an integer", other)
            }
        }
    }

    /// Structural equality, as `==`.
    pub fn eq(&self, a: Value<'a>, b: Value<'a>) -> R<'a, bool> {
        self.deeper(|| {
            let a = self.force(a)?;
            let b = self.force(b)?;
            self.eq_forced(a, b)
        })
    }

    fn eq_forced(&self, a: Value<'a>, b: Value<'a>) -> R<'a, bool> {
        Ok(match (a, b) {
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                as_f64(a) == as_f64(b)
            }
            (Value::Str(x), Value::Str(y)) => x.s == y.s,
            (Value::Path(x), Value::Path(y)) => x.0 == y.0,
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::List(x), Value::List(y)) => {
                if x.items.len() != y.items.len() {
                    return Ok(false);
                }
                for (p, q) in x.items.iter().zip(y.items) {
                    if !self.eq_elem(*p, *q)? {
                        return Ok(false);
                    }
                }
                true
            }
            (Value::Attrs(x), Value::Attrs(y)) => {
                if self.is_derivation(x)? && self.is_derivation(y)? {
                    let out_path = self.ctx.syms.out_path;
                    if let (Some(p), Some(q)) = (x.get(out_path), y.get(out_path)) {
                        return self.eq_elem(p, q);
                    }
                }
                if x.len() != y.len() {
                    return Ok(false);
                }
                if x.entries
                    .iter()
                    .zip(y.entries)
                    .any(|(p, q)| p.name != q.name)
                {
                    return Ok(false);
                }
                for (p, q) in x.entries.iter().zip(y.entries) {
                    if !self.eq_elem(p.value, q.value)? {
                        return Ok(false);
                    }
                }
                true
            }
            _ => false,
        })
    }

    /// Equality of values stored in a list or set, or passed as arguments:
    /// both are forced, then a stored value equals itself, even if it is a
    /// function.
    pub(crate) fn eq_elem(&self, a: Value<'a>, b: Value<'a>) -> R<'a, bool> {
        self.deeper(|| {
            let forced_a = self.force(a)?;
            let forced_b = self.force(b)?;
            let same = match (a, b) {
                (Value::PrimOp(x), Value::PrimOp(y)) => x == y,
                _ => {
                    matches!((a.ptr(), b.ptr()), (Some(p), Some(q)) if p == q)
                        && std::mem::discriminant(&a) == std::mem::discriminant(&b)
                }
            };
            if same {
                return Ok(true);
            }
            self.eq_forced(forced_a, forced_b)
        })
    }

    pub fn is_derivation(&self, a: &'a Attrs<'a>) -> R<'a, bool> {
        match a.get(self.ctx.syms.type_) {
            Some(t) => match self.force(t)? {
                Value::Str(s) => Ok(s.s == b"derivation"),
                _ => Ok(false),
            },
            None => Ok(false),
        }
    }

    pub fn less_than(&self, a: Value<'a>, b: Value<'a>) -> R<'a, bool> {
        let a = self.force(a)?;
        let b = self.force(b)?;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(x < y),
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                Ok(as_f64(a) < as_f64(b))
            }
            (Value::Str(x), Value::Str(y)) => Ok(x.s < y.s),
            (Value::Path(x), Value::Path(y)) => Ok(crate::path::abs(x.0) < crate::path::abs(y.0)),
            (Value::List(x), Value::List(y)) => {
                for (p, q) in x.items.iter().zip(y.items) {
                    if !self.eq_elem(*p, *q)? {
                        return self.less_than(*p, *q);
                    }
                }
                Ok(x.items.len() < y.items.len())
            }
            _ => eval_err(format!(
                "cannot compare {} with {}",
                a.show_type(),
                b.show_type()
            )),
        }
    }

    // ---- string coercion --------------------------------------------------

    pub fn coerce_into(
        &self,
        v: Value<'a>,
        c: Coerce,
        buf: &mut Vec<u8>,
        ctx: &mut Vec<Ctx<'a>>,
    ) -> R<'a, ()> {
        self.deeper(|| self.coerce_level(v, c, buf, ctx))
    }

    /// [`Self::coerce_into`] at the current level of a walk over a value.
    pub(crate) fn coerce_level(
        &self,
        v: Value<'a>,
        c: Coerce,
        buf: &mut Vec<u8>,
        ctx: &mut Vec<Ctx<'a>>,
    ) -> R<'a, ()> {
        let v = self.force(v)?;
        match v {
            Value::Str(s) => {
                buf.extend_from_slice(s.s);
                ctx.extend_from_slice(s.ctx);
            }
            Value::Path(p) => {
                if c.copy {
                    let sp = self.copy_path_to_store(p.0)?;
                    buf.extend_from_slice(sp.as_bytes());
                    ctx.push(Ctx::Opaque(sp));
                } else {
                    buf.extend_from_slice(crate::path::abs(p.0).as_bytes());
                }
            }
            Value::Attrs(a) => {
                let s = &self.ctx.syms;
                if let Some(f) = a.get(s.to_string) {
                    let r = self.call(f, &[v], None)?;
                    return self.coerce_into(r, c, buf, ctx);
                }
                if let Some(out) = a.get(s.out_path) {
                    return self.coerce_into(self.force(out)?, c, buf, ctx);
                }
                return type_err(format!(
                    "cannot coerce a set to a string: {}",
                    crate::print::short(self, v)
                ));
            }
            Value::Null | Value::Bool(false) if c.more => {}
            Value::Bool(true) if c.more => buf.push(b'1'),
            Value::Int(i) if c.more => buf.extend_from_slice(i.to_string().as_bytes()),
            Value::Float(f) if c.more => {
                buf.extend_from_slice(crate::print::float_to_string(f).as_bytes())
            }
            Value::List(l) if c.more => {
                // Nix omits the separator after an element that is an empty list.
                for (i, item) in l.items.iter().enumerate() {
                    self.coerce_into(*item, c, buf, ctx)?;
                    let empty = matches!(self.force(*item)?, Value::List(l) if l.items.is_empty());
                    if i + 1 < l.items.len() && !empty {
                        buf.push(b' ');
                    }
                }
            }
            other => {
                return type_err(format!(
                    "cannot coerce {} to a string: {}",
                    other.show_type(),
                    crate::print::short(self, other)
                ));
            }
        }
        Ok(())
    }

    /// Coerce to a string with context.
    pub fn coerce_to_string(&self, v: Value<'a>, c: Coerce) -> R<'a, (Vec<u8>, Vec<Ctx<'a>>)> {
        let mut buf = Vec::new();
        let mut ctx = Vec::new();
        self.coerce_into(v, c, &mut buf, &mut ctx)?;
        Ok((buf, ctx))
    }

    /// Coerce to a filesystem path: a path value, or a string holding an
    /// absolute path.
    pub fn coerce_to_path(&self, v: Value<'a>) -> R<'a, String> {
        self.coerce_to_path_realising(v, false)
    }

    /// [`Eval::coerce_to_path`] for a path to read. Nix first builds the
    /// derivation outputs the string refers to; this evaluator cannot build,
    /// so it fails unless they are already in the store.
    pub fn realise_path(&self, v: Value<'a>) -> R<'a, String> {
        self.coerce_to_path_realising(v, true)
    }

    fn coerce_to_path_realising(&self, v: Value<'a>, realise: bool) -> R<'a, String> {
        let v = self.force(v)?;
        match v {
            Value::Path(p) => return Ok(p.0.to_owned()),
            // A set stands for what it coerces to, so a path it gives keeps
            // the filesystem it belongs to rather than becoming a string.
            Value::Attrs(a) => {
                let s = &self.ctx.syms;
                let inner = match a.get(s.to_string) {
                    Some(f) => Some(self.call(f, &[v], None)?),
                    None => a.get(s.out_path),
                };
                if let Some(inner) = inner {
                    let inner = self.force(inner)?;
                    return self.deeper(|| self.coerce_to_path_realising(inner, realise));
                }
            }
            _ => {}
        }
        let (buf, ctx) = self.coerce_to_string(v, Coerce::PLAIN)?;
        if realise {
            self.realise(&ctx)?;
        }
        let s = String::from_utf8_lossy(&buf).into_owned();
        if !s.starts_with('/') {
            return eval_err(format!("string '{s}' doesn't represent an absolute path"));
        }
        Ok(canon_path(&s))
    }

    /// Fail unless every derivation output in `ctx` is in the store, as
    /// [`Eval::realise_path`] requires.
    pub fn realise(&self, ctx: &[Ctx<'a>]) -> R<'a, ()> {
        for c in ctx {
            let Ctx::Built { drv, output } = *c else {
                continue;
            };
            let path = match self.ctx.drvs.borrow().get(drv) {
                Some((d, _)) => match d.outputs.get(output) {
                    Some(o) => Some(o.path.clone()),
                    None => {
                        return eval_err(format!(
                            "derivation '{drv}' does not have wanted outputs '{output}'"
                        ));
                    }
                },
                None => None,
            };
            let valid = match path.as_deref().and_then(StorePath::parse) {
                Some(sp) if self.ctx.io.is_valid(&sp).map_err(anyhow_err)? => {
                    self.ctx.io.allow_valid(&sp);
                    true
                }
                _ => false,
            };
            if !valid {
                return eval_err(format!(
                    "cannot build '{drv}': this evaluator does not build derivations"
                ));
            }
        }
        Ok(())
    }

    pub fn copy_path_to_store(&self, p: &str) -> R<'a, &'a str> {
        let sp = self.ctx.io.add_path_to_store(p).map_err(anyhow_err)?;
        Ok(self.bump.alloc_str(&sp))
    }

    // ---- import -----------------------------------------------------------

    /// Evaluate the file at `path` (a directory means its `default.nix`),
    /// sharing the result with every other import of it in this session.
    pub fn import(&self, path: &str) -> R<'a> {
        let path = self.ctx.io.resolve_import(path).map_err(anyhow_err)?;
        let cached = self.import_cache.borrow().get(&path).copied();
        if let Some(v) = cached {
            return self.force(v);
        }
        let expr = self.ctx.compile_file(&path)?;
        let v = self.thunk(ThunkState::Expr(expr, self.root_env()));
        self.import_cache.borrow_mut().insert(path.clone(), v);
        self.force(v).map_err(|mut e| {
            e.push_trace(format!("while importing '{path}'"), None);
            e
        })
    }

    pub fn root_env(&self) -> &'a Env<'a> {
        self.bump.alloc(Env {
            parent: None,
            slots: &[],
        })
    }

    /// Parse and evaluate `text`, resolving relative paths against `base_dir`.
    pub fn eval_string(&self, text: &str, base_dir: &str) -> R<'a> {
        let expr = self.ctx.compile_str(text, base_dir)?;
        self.eval(expr, self.root_env())
    }
}

pub fn as_f64(v: Value<'_>) -> f64 {
    match v {
        Value::Int(i) => i as f64,
        Value::Float(f) => f,
        _ => 0.0,
    }
}
