//! `derivation`, `derivationStrict` and `placeholder`.

use std::collections::BTreeSet;

use sha2::Digest;
use sha2::Sha256;

use crate::derivation::Derivation;
use crate::derivation::Output;
use crate::derivation::input_addressed_path;
use crate::derivation::output_path_name;
use crate::eval::Coerce;
use crate::eval::Eval;
use crate::store::HashAlgo;
use crate::value::Ctx;
use crate::value::R;
use crate::value::ThunkState;
use crate::value::Value;
use crate::value::anyhow_err;
use crate::value::eval_err;
use crate::value::type_err;

/// `derivation` is a thin Nix wrapper around `derivationStrict` that turns its
/// result into one attribute set per output.
const DERIVATION_NIX: &str = r#"
drvAttrs@{ outputs ? [ "out" ], ... }:
let
  strict = derivationStrict drvAttrs;
  commonAttrs = drvAttrs // (builtins.listToAttrs outputsList) // {
    all = map (x: x.value) outputsList;
    inherit drvAttrs;
  };
  outputToAttrListElement = outputName: {
    name = outputName;
    value = commonAttrs // {
      outPath = builtins.getAttr outputName strict;
      drvPath = strict.drvPath;
      type = "derivation";
      inherit outputName;
    };
  };
  outputsList = map outputToAttrListElement outputs;
in
(builtins.head outputsList).value
"#;

/// The value of `derivation`, which is evaluated when first used.
pub fn derivation<'a>(ev: &Eval<'a>) -> Value<'a> {
    let expr = ev
        .ctx
        .compile_internal("derivation-internal.nix", DERIVATION_NIX)
        .expect("the derivation wrapper compiles");
    ev.thunk(ThunkState::Expr(expr, ev.root_env()))
}

pub fn placeholder<'a>(ev: &Eval<'a>, output: Value<'a>) -> R<'a> {
    let output = ev.force_str_no_ctx(output)?;
    let mut data = b"nix-output:".to_vec();
    data.extend_from_slice(output);
    let h = Sha256::digest(&data);
    Ok(ev.string(&format!("/{}", crate::store::base32_encode(&h))))
}

fn to_str(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The algorithm an `outputHashAlgo` value names, or `None` for a value that
/// names no algorithm, which is ignored.
fn parse_hash_algo<'a>(algo: &str) -> R<'a, Option<HashAlgo>> {
    if algo == "blake3" {
        return eval_err(
            "experimental Nix feature 'blake3-hashes' is disabled; add '--extra-experimental-features blake3-hashes' to enable it",
        );
    }
    Ok(algo.parse().ok())
}

/// Whether an `outputHashMode` value hashes recursively (NAR) rather than flat.
fn parse_hash_mode<'a>(mode: &str, name: &str) -> R<'a, bool> {
    match mode {
        "flat" => Ok(false),
        "recursive" | "nar" => Ok(true),
        "text" => eval_err(format!(
            "experimental Nix feature 'dynamic-derivations' is disabled (text-hashed derivation '{name}', outputHashMode = \"text\"); add '--extra-experimental-features dynamic-derivations' to enable it"
        )),
        "git" => eval_err(
            "experimental Nix feature 'git-hashing' is disabled; add '--extra-experimental-features git-hashing' to enable it",
        ),
        other => eval_err(format!(
            "invalid value '{other}' for 'outputHashMode' attribute"
        )),
    }
}

pub fn derivation_strict<'a>(ev: &Eval<'a>, attrs: Value<'a>) -> R<'a> {
    let attrs = ev.force_attrs(attrs)?;
    let syms = &ev.ctx.syms;
    let Some(name_v) = attrs.get(syms.name) else {
        return type_err("required attribute 'name' missing");
    };
    let name = to_str(ev.force_str_no_ctx(name_v)?);
    if let Err(e) = crate::store::check_name(&name) {
        return eval_err(format!("invalid derivation name: {e:#}"));
    }
    if name.ends_with(".drv") {
        return eval_err(format!(
            "derivation names are not allowed to end in '.drv' ('{name}')"
        ));
    }
    let structured = match attrs.get(syms.structured_attrs) {
        Some(v) => ev.force_bool(v)?,
        None => false,
    };
    let ignore_nulls = match attrs.get(syms.ignore_nulls) {
        Some(v) => ev.force_bool(v)?,
        None => false,
    };

    let mut drv = Derivation {
        name: name.clone(),
        ..Default::default()
    };
    let mut ctx: Vec<Ctx<'a>> = Vec::new();
    let mut output_hash: Option<String> = None;
    let mut output_hash_algo: Option<HashAlgo> = None;
    let mut recursive = false;
    let mut outputs: Vec<String> = vec!["out".to_owned()];
    let mut json = Vec::new();
    let mut json_first = true;
    let more = Coerce {
        more: true,
        copy: true,
    };

    let mut set_outputs = |list: Vec<String>| -> R<'a, ()> {
        let mut seen = BTreeSet::new();
        for o in &list {
            if o == "drvPath" {
                return eval_err("invalid derivation output name 'drvPath'");
            }
            if !seen.insert(o.clone()) {
                return eval_err(format!("duplicate derivation output '{o}'"));
            }
        }
        if list.is_empty() {
            return eval_err("derivation cannot have an empty set of outputs");
        }
        outputs = list;
        Ok(())
    };

    for e in attrs.sorted(ev.ctx) {
        let key = ev.name(e.name);
        if key == "__ignoreNulls" {
            continue;
        }
        if ignore_nulls && matches!(ev.force(e.value)?, Value::Null) {
            continue;
        }
        if structured && key == "__structuredAttrs" {
            continue;
        }
        if key == "args" {
            for a in ev.force_list(e.value)? {
                let (s, c) = ev.coerce_to_string(*a, more)?;
                ctx.extend(c);
                drv.args.push(s);
            }
            continue;
        }
        let s = if structured {
            if !json_first {
                json.push(b',');
            }
            json_first = false;
            super::json::json_string(&mut json, key.as_bytes());
            json.push(b':');
            super::json::write_json(ev, e.value, &mut json, &mut ctx)?;
            match key {
                "builder" => ev.force_str(e.value)?.s.to_vec(),
                "system" | "outputHash" | "outputHashAlgo" | "outputHashMode" => {
                    ev.force_str_no_ctx(e.value)?.to_vec()
                }
                "outputs" => {
                    let list = ev
                        .force_list(e.value)?
                        .iter()
                        .map(|o| Ok(to_str(ev.force_str(*o)?.s)))
                        .collect::<R<'a, Vec<_>>>()?;
                    set_outputs(list)?;
                    continue;
                }
                _ => continue,
            }
        } else {
            let (s, c) = ev.coerce_to_string(e.value, more)?;
            ctx.extend(c);
            if key == "outputs" {
                set_outputs(
                    to_str(&s)
                        .split([' ', '\t', '\n', '\r'])
                        .filter(|o| !o.is_empty())
                        .map(str::to_owned)
                        .collect(),
                )?;
            }
            s
        };
        match key {
            "builder" => drv.builder = s.clone(),
            "system" => drv.platform = s.clone(),
            "outputHash" => output_hash = Some(to_str(&s)),
            "outputHashAlgo" => output_hash_algo = parse_hash_algo(&to_str(&s))?,
            "outputHashMode" => recursive = parse_hash_mode(&to_str(&s), &name)?,
            _ => {}
        }
        if !structured {
            drv.env.insert(key.to_owned(), s);
        }
    }
    if structured {
        json.insert(0, b'{');
        json.push(b'}');
        drv.env.insert("__json".to_owned(), json);
    }
    if drv.builder.is_empty() {
        return type_err("required attribute 'builder' missing");
    }
    if drv.platform.is_empty() {
        return type_err("required attribute 'system' missing");
    }

    for c in &ctx {
        match *c {
            Ctx::Opaque(p) => {
                drv.input_srcs.insert(p.to_owned());
            }
            Ctx::Built { drv: d, output } => {
                drv.input_drvs
                    .entry(d.to_owned())
                    .or_default()
                    .insert(output.to_owned());
            }
            Ctx::DrvDeep(d) => {
                let closure = ev.ctx.io.closure(d).map_err(anyhow_err)?;
                for p in closure {
                    if let Some((input, _)) = ev.ctx.drvs.borrow().get(&p) {
                        drv.input_drvs
                            .entry(p.clone())
                            .or_default()
                            .extend(input.outputs.keys().cloned());
                    }
                    drv.input_srcs.insert(p);
                }
            }
        }
    }

    let modulo_of = |p: &str| -> anyhow::Result<[u8; 32]> {
        match ev.ctx.drvs.borrow().get(p) {
            Some((_, h)) => Ok(*h),
            None => anyhow::bail!("derivation '{p}' is not known to this evaluation"),
        }
    };

    if let Some(hash) = output_hash {
        if outputs != ["out"] {
            return eval_err("multiple outputs are not supported in fixed-output derivations");
        }
        let (algo, bytes) = if hash.is_empty() {
            let Some(algo) = output_hash_algo else {
                return eval_err("empty hash requires explicit hash algorithm");
            };
            (algo, vec![0; algo.size()])
        } else {
            crate::store::parse_hash(&hash, output_hash_algo).map_err(anyhow_err)?
        };
        let path = crate::store::fixed_output_path(recursive, algo, &bytes, &name);
        drv.env.insert("out".to_owned(), path.clone().into_bytes());
        drv.outputs.insert(
            "out".to_owned(),
            Output {
                path,
                hash_algo: format!("{}{algo}", if recursive { "r:" } else { "" }),
                hash: hex::encode(&bytes),
            },
        );
    } else {
        for o in &outputs {
            drv.outputs.insert(o.clone(), Output::default());
            drv.env.insert(o.clone(), Vec::new());
        }
        let modulo = drv.hash_modulo(&modulo_of).map_err(anyhow_err)?;
        for o in &outputs {
            if let Err(e) = crate::store::check_name(&output_path_name(&name, o)) {
                return Err(anyhow_err(e));
            }
            let path = input_addressed_path(&modulo, &name, o);
            drv.env.insert(o.clone(), path.clone().into_bytes());
            drv.outputs.get_mut(o).expect("inserted above").path = path;
        }
    }

    let modulo = drv.hash_modulo(&modulo_of).map_err(anyhow_err)?;
    let drv_path = drv.write(&ev.ctx.io).map_err(anyhow_err)?;
    let drv_path: &'a str = ev.bump.alloc_str(&drv_path);
    let mut entries = vec![ev.entry(
        "drvPath",
        ev.str_val(drv_path.as_bytes(), &[Ctx::DrvDeep(drv_path)]),
    )];
    for (o, out) in &drv.outputs {
        let output: &'a str = ev.bump.alloc_str(o);
        entries.push(ev.entry(
            o,
            ev.str_val(
                out.path.as_bytes(),
                &[Ctx::Built {
                    drv: drv_path,
                    output,
                }],
            ),
        ));
    }
    ev.ctx
        .drvs
        .borrow_mut()
        .insert(drv_path.to_owned(), (drv, modulo));
    Ok(ev.attrs(entries))
}
