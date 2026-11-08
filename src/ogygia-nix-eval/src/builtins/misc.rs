//! Arithmetic, type predicates, control flow and other small builtins.

use crate::eval::Arith;
use crate::eval::Coerce;
use crate::eval::Eval;
use crate::ir::Param;
use crate::value::Entry;
use crate::value::ErrorKind;
use crate::value::R;
use crate::value::Value;
use crate::value::error;
use crate::value::eval_err;

pub fn add<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    ev.arith(Arith::Add, a, b, None)
}

pub fn sub<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    ev.arith(Arith::Sub, a, b, None)
}

pub fn mul<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    ev.arith(Arith::Mul, a, b, None)
}

pub fn div<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    ev.arith(Arith::Div, a, b, None)
}

fn bits<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>, f: fn(i64, i64) -> i64) -> R<'a> {
    let a = ev.force_int(a)?;
    let b = ev.force_int(b)?;
    Ok(Value::Int(f(a, b)))
}

pub fn bit_and<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    bits(ev, a, b, |a, b| a & b)
}

pub fn bit_or<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    bits(ev, a, b, |a, b| a | b)
}

pub fn bit_xor<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    bits(ev, a, b, |a, b| a ^ b)
}

/// The floats that convert to an integer without saturating.
const INT_RANGE: std::ops::Range<f64> = -9.223_372_036_854_776e18..9.223_372_036_854_776e18;

fn round<'a>(ev: &Eval<'a>, v: Value<'a>, f: fn(f64) -> f64, name: &str) -> R<'a> {
    match ev.force(v)? {
        Value::Int(i) => {
            let x = i as f64;
            if !INT_RANGE.contains(&x) || x as i64 != i {
                return eval_err(format!("{name} of {i} is not exact as a float"));
            }
            Ok(Value::Int(i))
        }
        Value::Float(x) => {
            let r = f(x);
            if !INT_RANGE.contains(&r) {
                return eval_err(format!(
                    "{name} of {} is out of the range of an integer",
                    crate::print::float_g(x)
                ));
            }
            Ok(Value::Int(r as i64))
        }
        other => ev.type_error("a float", other),
    }
}

pub fn ceil<'a>(ev: &Eval<'a>, x: Value<'a>) -> R<'a> {
    round(ev, x, f64::ceil, "ceil")
}

pub fn floor<'a>(ev: &Eval<'a>, x: Value<'a>) -> R<'a> {
    round(ev, x, f64::floor, "floor")
}

pub fn less_than<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    Ok(Value::Bool(ev.less_than(a, b)?))
}

pub fn seq<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    ev.force(a)?;
    ev.force(b)
}

pub fn deep_seq<'a>(ev: &Eval<'a>, a: Value<'a>, b: Value<'a>) -> R<'a> {
    ev.deep_force(a)?;
    ev.force(b)
}

fn message<'a>(ev: &Eval<'a>, v: Value<'a>, c: Coerce) -> R<'a, String> {
    let (s, _) = ev.coerce_to_string(v, c)?;
    Ok(String::from_utf8_lossy(&s).into_owned())
}

pub fn throw<'a>(ev: &Eval<'a>, msg: Value<'a>) -> R<'a> {
    let msg = message(ev, msg, Coerce::INTERP)?;
    Err(error(ErrorKind::Throw, msg))
}

pub fn abort<'a>(ev: &Eval<'a>, msg: Value<'a>) -> R<'a> {
    let msg = message(ev, msg, Coerce::INTERP)?;
    Err(error(
        ErrorKind::Abort,
        format!("evaluation aborted with the following error message: '{msg}'"),
    ))
}

pub fn add_error_context<'a>(ev: &Eval<'a>, context: Value<'a>, v: Value<'a>) -> R<'a> {
    ev.force(v).map_err(|mut e| {
        // Coercing the message may fail, and that failure only goes in the
        // trace: the error keeps its kind, so `tryEval` still sees it.
        let context = message(ev, context, Coerce::PLAIN).unwrap_or_else(|c| {
            format!(
                "while evaluating the error message passed to builtins.addErrorContext, \
                 another error occurred: {}",
                c.msg
            )
        });
        e.push_trace(context, None);
        e
    })
}

pub fn try_eval<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    let (success, value) = match ev.force(v) {
        Ok(v) => (true, v),
        Err(e) if matches!(e.kind, ErrorKind::Throw | ErrorKind::Assert) => {
            (false, Value::Bool(false))
        }
        Err(e) => return Err(e),
    };
    Ok(ev.attrs(vec![
        ev.entry("success", Value::Bool(success)),
        ev.entry("value", value),
    ]))
}

pub fn trace<'a>(ev: &Eval<'a>, msg: Value<'a>, v: Value<'a>) -> R<'a> {
    let text = match ev.force(msg)? {
        Value::Str(s) => s.as_str_lossy().into_owned(),
        other => crate::print::print_lazy(ev, other),
    };
    eprintln!("trace: {text}");
    ev.force(v)
}

pub fn trace_verbose<'a>(ev: &Eval<'a>, _msg: Value<'a>, v: Value<'a>) -> R<'a> {
    ev.force(v)
}

pub fn warn<'a>(ev: &Eval<'a>, msg: Value<'a>, v: Value<'a>) -> R<'a> {
    let msg = ev.force_str(msg)?;
    eprintln!("evaluation warning: {}", msg.as_str_lossy());
    ev.force(v)
}

pub fn break_<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    ev.force(v)
}

pub fn type_of<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    let v = ev.force(v)?;
    Ok(ev.string(v.type_of()))
}

fn is<'a>(ev: &Eval<'a>, v: Value<'a>, f: fn(&Value<'a>) -> bool) -> R<'a> {
    let v = ev.force(v)?;
    Ok(Value::Bool(f(&v)))
}

pub fn is_attrs<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| matches!(v, Value::Attrs(_)))
}

pub fn is_bool<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| matches!(v, Value::Bool(_)))
}

pub fn is_float<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| matches!(v, Value::Float(_)))
}

pub fn is_function<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| v.is_function())
}

pub fn is_int<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| matches!(v, Value::Int(_)))
}

pub fn is_list<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| matches!(v, Value::List(_)))
}

pub fn is_null<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| matches!(v, Value::Null))
}

pub fn is_path<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| matches!(v, Value::Path(_)))
}

pub fn is_string<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a> {
    is(ev, v, |v| matches!(v, Value::Str(_)))
}

pub fn function_args<'a>(ev: &Eval<'a>, f: Value<'a>) -> R<'a> {
    match ev.force(f)? {
        Value::Lambda(c) => match &c.def.param {
            Param::Ident(_) => Ok(Value::Attrs(ev.empty_attrs)),
            Param::Pattern { formals, .. } => Ok(ev.attrs(
                formals
                    .iter()
                    .map(|f| Entry {
                        name: f.name,
                        value: Value::Bool(f.default.is_some()),
                        pos: Some(c.def.pos),
                    })
                    .collect(),
            )),
        },
        Value::PrimOp(_) | Value::PrimOpApp(_) => Ok(Value::Attrs(ev.empty_attrs)),
        other => ev.type_error("a function", other),
    }
}

pub fn get_env<'a>(ev: &Eval<'a>, name: Value<'a>) -> R<'a> {
    let name = ev.force_str_no_ctx(name)?;
    if ev.ctx.io.pure {
        return Ok(ev.string(""));
    }
    let name = String::from_utf8_lossy(name);
    Ok(ev.string(&std::env::var(&*name).unwrap_or_default()))
}
