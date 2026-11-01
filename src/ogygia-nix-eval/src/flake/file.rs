//! `flake.nix`: a flake's description, inputs and outputs.
//!
//! Nix reads a flake's attributes without evaluating them: an attribute
//! whose expression is not a literal set, list, function or constant, or a
//! builtin, is a thunk to it, and is rejected wherever a value is expected.

use std::collections::BTreeMap;

use crate::eval::Eval;
use crate::flake::flakeref;
use crate::flake::flakeref::Attr;
use crate::flake::flakeref::Attrs;
use crate::ir::AttrValue;
use crate::ir::AttrsDef;
use crate::ir::Expr;
use crate::ir::ExprRef;
use crate::ir::Param;
use crate::symbol::Sym;
use crate::value::R;
use crate::value::Value;
use crate::value::anyhow_err;
use crate::value::eval_err;

/// An input as a flake's `flake.nix` declares it.
#[derive(Debug, Default)]
pub struct InputSpec {
    /// Its reference; `None` for an override that leaves it as it is.
    pub attrs: Option<Attrs>,
    pub follows: Option<Vec<String>>,
    /// Its own inputs, overridden.
    pub overrides: BTreeMap<String, InputSpec>,
}

/// A loaded `flake.nix`.
pub struct Flake<'a> {
    pub inputs: BTreeMap<String, InputSpec>,
    pub outputs: Value<'a>,
}

/// An attribute of a literal set, with its expression if it is known.
#[derive(Clone, Copy)]
struct Field<'a> {
    value: Value<'a>,
    expr: Option<ExprRef<'a>>,
}

/// Whether Nix evaluates `e` as part of the set it is an attribute of.
fn eager(e: ExprRef<'_>) -> bool {
    matches!(
        e,
        Expr::Attrs(_)
            | Expr::List(_)
            | Expr::Lambda(_)
            | Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Path(_)
            | Expr::Global(_)
    )
}

/// The expression of attribute `name` in the set `def` defines.
fn attr_expr<'a>(ev: &Eval<'a>, def: &'a AttrsDef<'a>, name: Sym) -> Option<ExprRef<'a>> {
    if let Ok(i) = def.statics.binary_search_by_key(&name, |s| s.name) {
        return match def.statics[i].value {
            AttrValue::Expr(e) => Some(e),
            AttrValue::Slot(s) => def.frame.map(|f| f[s as usize]),
        };
    }
    def.dynamics.iter().find_map(|d| match d.name {
        Expr::Str(s) if s.s == ev.name(name).as_bytes() => Some(d.value),
        _ => None,
    })
}

impl<'a> Field<'a> {
    /// The value, or `None` where Nix would see a thunk.
    fn peek(self, ev: &Eval<'a>) -> R<'a, Option<Value<'a>>> {
        match self.expr {
            Some(e) if eager(e) => Ok(Some(ev.force(self.value)?)),
            _ => Ok(None),
        }
    }

    fn expect<T>(
        self,
        ev: &Eval<'a>,
        expected: &str,
        what: &str,
        f: impl FnOnce(Value<'a>) -> Option<T>,
    ) -> R<'a, T> {
        let v = self.peek(ev)?;
        let got = match v {
            Some(v) => v.show_type(),
            None => "a thunk",
        };
        match v.and_then(f) {
            Some(t) => Ok(t),
            None => eval_err(format!("expected {expected} but got {got} {what}")),
        }
    }

    fn string(self, ev: &Eval<'a>, what: &str) -> R<'a, String> {
        self.expect(ev, "a string", what, |v| match v {
            Value::Str(s) => Some(s.as_str_lossy().into_owned()),
            _ => None,
        })
    }

    /// The attributes of the set this holds, in name order.
    fn fields(self, ev: &Eval<'a>, what: &str) -> R<'a, Vec<(&'a str, Field<'a>)>> {
        let attrs = self.expect(ev, "a set", what, |v| match v {
            Value::Attrs(a) => Some(a),
            _ => None,
        })?;
        let def = match self.expr {
            Some(Expr::Attrs(def)) => Some(*def),
            _ => None,
        };
        Ok(attrs
            .sorted(ev.ctx)
            .into_iter()
            .map(|e| {
                let expr = def.and_then(|d| attr_expr(ev, d, e.name));
                (
                    ev.name(e.name),
                    Field {
                        value: e.value,
                        expr,
                    },
                )
            })
            .collect())
    }
}

/// The input `name` declared by `field`, an attribute of `inputs`, or of
/// an input's `inputs` if not `top`.
fn input_spec<'a>(ev: &Eval<'a>, name: &str, field: Field<'a>, top: bool) -> R<'a, InputSpec> {
    let mut spec = InputSpec::default();
    let mut url = None;
    let mut extra = Attrs::new();
    for (attr, f) in field.fields(ev, &format!("for input '{name}'"))? {
        let at = format!("for attribute '{attr}' of input '{name}'");
        match attr {
            "url" => url = Some(f.string(ev, &at)?),
            "follows" => {
                let follows = f.string(ev, &at)?;
                spec.follows = Some(
                    follows
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                );
            }
            "flake" => {
                f.expect(ev, "a Boolean", &at, |v| {
                    matches!(v, Value::Bool(_)).then_some(())
                })?;
            }
            "inputs" => {
                for (child, c) in f.fields(ev, &at)? {
                    spec.overrides
                        .insert(child.to_owned(), input_spec(ev, child, c, false)?);
                }
            }
            _ => {
                let value = f.expect(ev, "a string, integer or Boolean", &at, Attr::from_value)?;
                extra.insert(attr.to_owned(), value);
            }
        }
    }
    spec.attrs = match url {
        // Beside a type, the URL is an attribute like any other rather
        // than a flake reference.
        Some(url) if extra.contains_key("type") => {
            extra.insert("url".to_owned(), Attr::Str(url));
            Some(extra)
        }
        Some(url) => {
            let mut attrs = flakeref::parse(&url, None).map_err(anyhow_err)?;
            attrs.extend(extra);
            Some(attrs)
        }
        None if !extra.is_empty() => Some(extra),
        None if top && spec.follows.is_none() => Some(indirect(name)),
        None => None,
    };
    Ok(spec)
}

fn indirect(id: &str) -> Attrs {
    Attrs::from([
        ("type".to_owned(), Attr::Str("indirect".to_owned())),
        ("id".to_owned(), Attr::Str(id.to_owned())),
    ])
}

/// Load the `flake.nix` in the logical directory `dir` of the flake shown
/// as `shown`.
pub fn load<'a>(ev: &Eval<'a>, dir: &str, shown: &str) -> R<'a, Flake<'a>> {
    let file = format!("{}/flake.nix", dir.trim_end_matches('/'));
    if !ev.ctx.io.exists(&file).map_err(anyhow_err)? {
        return eval_err(format!(
            "source tree referenced by '{shown}' does not contain a '/flake.nix' file"
        ));
    }
    let expr = ev.ctx.compile_file(&file).map_err(anyhow_err)?;
    if !matches!(expr, Expr::Attrs(_)) {
        return eval_err(format!("file '{file}' must be an attribute set"));
    }
    let top = Field {
        value: ev.eval(expr, ev.root_env())?,
        expr: Some(expr),
    };
    let mut outputs = None;
    let mut unsupported = None;
    let mut inputs = BTreeMap::new();
    for (name, f) in top.fields(ev, "")? {
        match name {
            "description" => {
                f.string(ev, "for attribute 'description'")?;
            }
            "nixConfig" => {}
            "inputs" => {
                for (input, i) in f.fields(ev, "for attribute 'inputs'")? {
                    inputs.insert(input.to_owned(), input_spec(ev, input, i, true)?);
                }
            }
            "outputs" => outputs = Some(f),
            other => {
                unsupported.get_or_insert(other);
            }
        }
    }
    let Some(outputs) = outputs else {
        return eval_err(format!("flake '{shown}' lacks attribute 'outputs'"));
    };
    let outputs = outputs.expect(ev, "a function", "for attribute 'outputs'", |v| {
        v.is_function().then_some(v)
    })?;
    if let Some(other) = unsupported {
        return eval_err(format!(
            "flake '{shown}' has an unsupported attribute '{other}'"
        ));
    }
    // The arguments of `outputs` that are not declared inputs are inputs
    // too, from the registry.
    if let Value::Lambda(c) = outputs
        && let Param::Pattern { formals, .. } = &c.def.param
    {
        for f in formals.iter() {
            let name = ev.name(f.name);
            if name != "self" && !inputs.contains_key(name) {
                inputs.insert(
                    name.to_owned(),
                    InputSpec {
                        attrs: Some(indirect(name)),
                        ..InputSpec::default()
                    },
                );
            }
        }
    }
    Ok(Flake { inputs, outputs })
}
