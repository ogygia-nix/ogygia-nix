//! `flake.nix`: a flake's description, inputs and outputs.
//!
//! Nix reads a flake's attributes without evaluating them: an attribute it
//! holds as a thunk is rejected wherever a value is expected.

use std::collections::BTreeMap;

use crate::eval::Coerce;
use crate::eval::Eval;
use crate::flake::flakeref;
use crate::flake::flakeref::Attr;
use crate::flake::flakeref::Attrs;
use crate::ir::Expr;
use crate::ir::Param;
use crate::value::R;
use crate::value::ThunkState;
use crate::value::Value;
use crate::value::anyhow_err;
use crate::value::eval_err;

/// An input as a flake's `flake.nix` declares it.
#[derive(Clone, Debug, Default)]
pub struct InputSpec {
    /// Its reference; `None` for an override that leaves it as it is.
    pub attrs: Option<Attrs>,
    pub follows: Option<Vec<String>>,
    /// Whether it is a flake, if `flake.nix` says.
    pub flake: Option<bool>,
    /// Its own inputs, overridden.
    pub overrides: BTreeMap<String, InputSpec>,
}

/// A loaded `flake.nix`.
pub struct Flake<'a> {
    pub inputs: BTreeMap<String, InputSpec>,
    pub outputs: Value<'a>,
    /// The attributes `inputs.self` adds to the flake's own reference.
    pub self_attrs: Attrs,
}

/// An attribute of a flake's literal sets, not yet evaluated.
#[derive(Clone, Copy)]
struct Field<'a>(Value<'a>);

impl<'a> Field<'a> {
    /// The value, or `None` where Nix holds a thunk. Nix holds a literal
    /// list as a value, a literal set whose names are all static, and a
    /// builtin.
    fn peek(self, ev: &Eval<'a>) -> R<'a, Option<Value<'a>>> {
        let held = match self.0 {
            Value::Thunk(t) => match t.0.get() {
                ThunkState::Done(_) | ThunkState::Expr(Expr::List(_), _) => true,
                ThunkState::Expr(Expr::Attrs(def), _) if def.dynamics.is_empty() => true,
                _ => ev.is_builtin(self.0),
            },
            _ => true,
        };
        held.then(|| ev.force(self.0)).transpose()
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
        Ok(attrs
            .sorted(ev.ctx)
            .into_iter()
            .map(|e| (ev.name(e.name), Field(e.value)))
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
                spec.flake = Some(f.expect(ev, "a Boolean", &at, |v| match v {
                    Value::Bool(b) => Some(b),
                    _ => None,
                })?);
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

/// The attributes `inputs.self` adds to the flake's own reference, which it
/// declares instead of an input: `submodules` and `lfs` alone.
fn self_input<'a>(ev: &Eval<'a>, field: Field<'a>) -> R<'a, Attrs> {
    let mut attrs = Attrs::new();
    for (attr, f) in field.fields(ev, "for input 'self'")? {
        let at = format!("for attribute '{attr}' of input 'self'");
        let value = f.expect(ev, "a string, integer or Boolean", &at, Attr::from_value)?;
        if !matches!(attr, "submodules" | "lfs") {
            return eval_err(format!("flake 'self' attribute '{attr}' is not supported"));
        }
        attrs.insert(attr.to_owned(), value);
    }
    Ok(attrs)
}

/// Check `nixConfig`, whose settings Nix does not apply but requires to
/// be strings, integers, Booleans, paths it can copy to the store, or lists
/// of strings.
fn check_config<'a>(ev: &Eval<'a>, field: Field<'a>) -> R<'a, ()> {
    for (name, f) in field.fields(ev, "for attribute 'nixConfig'")? {
        let got = match f.peek(ev)? {
            None => "a thunk",
            Some(Value::Str(_) | Value::Int(_) | Value::Bool(_)) => continue,
            Some(v @ Value::Path(_)) => {
                ev.coerce_to_string(v, Coerce::INTERP)?;
                continue;
            }
            Some(Value::List(l)) => {
                for item in l.items {
                    if !matches!(Field(*item).peek(ev)?, Some(Value::Str(_))) {
                        return eval_err(format!(
                            "list element in flake configuration setting '{name}' is a list while a string is expected"
                        ));
                    }
                }
                continue;
            }
            Some(v) => v.show_type(),
        };
        return eval_err(format!("flake configuration setting '{name}' is {got}"));
    }
    Ok(())
}

fn indirect(id: &str) -> Attrs {
    Attrs::from([
        ("type".to_owned(), Attr::Str("indirect".to_owned())),
        ("id".to_owned(), Attr::Str(id.to_owned())),
    ])
}

/// Load the `flake.nix` in the logical directory `dir` of the flake shown
/// as `shown`. Any `.` and `..` in `dir` are resolved without consulting
/// the filesystem.
pub fn load<'a>(ev: &Eval<'a>, dir: &str, shown: &str) -> R<'a, Flake<'a>> {
    read(ev, dir, shown, true)
}

/// [`load`] for a locked input, whose `nixConfig` Nix does not check.
pub fn load_locked<'a>(ev: &Eval<'a>, dir: &str, shown: &str) -> R<'a, Flake<'a>> {
    read(ev, dir, shown, false)
}

fn read<'a>(ev: &Eval<'a>, dir: &str, shown: &str, config: bool) -> R<'a, Flake<'a>> {
    let file = format!("{}/flake.nix", crate::path::canon_path(dir));
    if !ev.ctx.io.exists(&file).map_err(anyhow_err)? {
        return eval_err(format!(
            "source tree referenced by '{shown}' does not contain a '/flake.nix' file"
        ));
    }
    let expr = ev.ctx.compile_file(&file)?;
    if !matches!(expr, Expr::Attrs(_)) {
        return eval_err(format!("file '{file}' must be an attribute set"));
    }
    let top = Field(ev.eval(expr, ev.root_env())?);
    let mut outputs = None;
    let mut unsupported = None;
    let mut inputs = BTreeMap::new();
    let mut self_attrs = Attrs::new();
    for (name, f) in top.fields(ev, "")? {
        match name {
            "description" => {
                f.string(ev, "for attribute 'description'")?;
            }
            "nixConfig" if config => check_config(ev, f)?,
            "nixConfig" => {}
            "inputs" => {
                for (input, i) in f.fields(ev, "for attribute 'inputs'")? {
                    if input == "self" {
                        self_attrs = self_input(ev, i)?;
                    } else {
                        inputs.insert(input.to_owned(), input_spec(ev, input, i, true)?);
                    }
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
    Ok(Flake {
        inputs,
        outputs,
        self_attrs,
    })
}
