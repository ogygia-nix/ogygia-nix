//! Builtins that read files or compute store paths of them.

use std::cell::Cell;

use crate::accessor::FileType;
use crate::eval::Coerce;
use crate::eval::Eval;
use crate::value::Ctx;
use crate::value::Entry;
use crate::value::Env;
use crate::value::ErrorKind;
use crate::value::R;
use crate::value::Value;
use crate::value::anyhow_err;
use crate::value::error;
use crate::value::eval_err;
use crate::value::type_err;
use crate::value::unimplemented;

pub fn import<'a>(ev: &Eval<'a>, path: Value<'a>) -> R<'a> {
    let path = ev.realise_path(path)?;
    ev.import(&path)
}

pub fn scoped_import<'a>(ev: &Eval<'a>, scope: Value<'a>, path: Value<'a>) -> R<'a> {
    let path = ev.realise_path(path)?;
    let scope = ev.force_attrs(scope)?;
    let file = ev.ctx.io.resolve_import(&path).map_err(anyhow_err)?;
    let names: Vec<_> = scope.entries.iter().map(|e| e.name).collect();
    let expr = ev
        .ctx
        .compile_file_scoped(&file, &names)
        .map_err(anyhow_err)?;
    let slots = ev
        .bump
        .alloc_slice_fill_iter(scope.entries.iter().map(|e| Cell::new(e.value)));
    let env = ev.alloc(Env {
        parent: None,
        slots,
    });
    ev.eval(expr, env)
}

pub fn read_file<'a>(ev: &Eval<'a>, path: Value<'a>) -> R<'a> {
    let path = ev.realise_path(path)?;
    let bytes = ev.ctx.io.read(&path).map_err(anyhow_err)?;
    Ok(ev.str_val(&bytes, &[]))
}

pub fn read_dir<'a>(ev: &Eval<'a>, path: Value<'a>) -> R<'a> {
    let path = ev.realise_path(path)?;
    let entries = ev.ctx.io.read_dir(&path).map_err(anyhow_err)?;
    Ok(ev.attrs(
        entries
            .into_iter()
            .map(|(name, ty)| Entry {
                name: ev.sym(&name),
                value: ev.string(ty.name()),
                pos: None,
            })
            .collect(),
    ))
}

pub fn read_file_type<'a>(ev: &Eval<'a>, path: Value<'a>) -> R<'a> {
    let path = ev.realise_path(path)?;
    // Unlike other builtins, readFileType follows no symlinks at all.
    let parent = crate::path::dir_of(&path);
    let root = parent.len() - crate::path::abs(parent).len() + 1;
    let ancestors = parent[root..]
        .match_indices('/')
        .map(|(i, _)| &parent[..root + i])
        .chain([parent]);
    for a in ancestors {
        if ev.ctx.io.file_type(a).map_err(anyhow_err)? == Some(FileType::Symlink) {
            return eval_err(format!("path '{parent}' (or its ancestor) is a symlink"));
        }
    }
    match ev.ctx.io.file_type(&path).map_err(anyhow_err)? {
        Some(t) => Ok(ev.string(t.name())),
        None => eval_err(format!("path '{path}' does not exist")),
    }
}

pub fn path_exists<'a>(ev: &Eval<'a>, path: Value<'a>) -> R<'a> {
    let v = ev.force(path)?;
    let (raw, _) = ev.coerce_to_string(v, Coerce::PLAIN)?;
    let path = ev.realise_path(v)?;
    let io = &ev.ctx.io;
    // A trailing `/` or `/.` requires a directory.
    let exists = if (raw.ends_with(b"/") || raw.ends_with(b"/.")) && path != "/" {
        io.is_dir(&path)
    } else {
        io.exists(&path)
    };
    Ok(Value::Bool(exists.map_err(anyhow_err)?))
}

pub fn hash_file<'a>(ev: &Eval<'a>, algo: Value<'a>, path: Value<'a>) -> R<'a> {
    let algo = super::strings::hash_algo(ev.force_str_no_ctx(algo)?)?;
    let path = ev.realise_path(path)?;
    let bytes = ev.ctx.io.read(&path).map_err(anyhow_err)?;
    Ok(ev.string(&hex::encode(algo.digest(&bytes))))
}

pub fn find_file<'a>(ev: &Eval<'a>, search_path: Value<'a>, name: Value<'a>) -> R<'a> {
    let syms = &ev.ctx.syms;
    let search = ev
        .force_list(search_path)?
        .iter()
        .map(|entry| {
            let entry = ev.force_attrs(*entry)?;
            let prefix = match entry.get(syms.prefix) {
                Some(p) => String::from_utf8_lossy(ev.force_str_no_ctx(p)?).into_owned(),
                None => String::new(),
            };
            let Some(path) = entry.get(syms.path) else {
                return type_err("attribute 'path' missing in a search path entry");
            };
            let (path, ctx) = ev.coerce_to_string(path, Coerce::PLAIN)?;
            ev.realise(&ctx)?;
            Ok((prefix, String::from_utf8_lossy(&path).into_owned()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let name = ev.force_str_no_ctx(name)?;
    let name = String::from_utf8_lossy(name).into_owned();
    for (prefix, path) in search {
        let rest = if prefix.is_empty() {
            Some(name.as_str())
        } else if name == prefix {
            Some("")
        } else {
            name.strip_prefix(&format!("{prefix}/"))
        };
        let Some(rest) = rest else { continue };
        ev.ctx
            .io
            .check_access(&crate::path::canon_path(&path))
            .map_err(anyhow_err)?;
        let candidate = if rest.is_empty() {
            path
        } else {
            format!("{path}/{rest}")
        };
        let candidate = crate::path::canon_path(&candidate);
        if ev.ctx.io.resolves(&candidate).map_err(anyhow_err)? {
            return Ok(ev.path_val(&candidate));
        }
    }
    if let Some(rest) = name.strip_prefix("nix/") {
        let path = format!("{}/{rest}", crate::path::COREPKGS);
        return Ok(ev.path_val(&crate::path::canon_path(&path)));
    }
    Err(error(
        ErrorKind::Throw,
        format!(
            "file '{name}' was not found in the Nix search path (add it using $NIX_PATH or -I)"
        ),
    ))
}

pub fn to_file<'a>(ev: &Eval<'a>, name: Value<'a>, contents: Value<'a>) -> R<'a> {
    let name = ev.force_str_no_ctx(name)?;
    let name = String::from_utf8_lossy(name).into_owned();
    let contents = ev.force_str(contents)?;
    let mut refs = Vec::new();
    for c in contents.ctx {
        match c {
            Ctx::Opaque(p) => refs.push(*p),
            _ => {
                return eval_err(format!(
                    "files created by builtins.toFile may not reference derivations, but {name} references one"
                ));
            }
        }
    }
    crate::store::check_name(&name).map_err(anyhow_err)?;
    let path = ev
        .ctx
        .io
        .add_text(&name, contents.s, &refs)
        .map_err(anyhow_err)?;
    Ok(store_path_val(ev, &path))
}

/// The store path `sp` as a string that refers to it.
fn store_path_val<'a>(ev: &Eval<'a>, sp: &str) -> Value<'a> {
    let sp: &'a str = ev.bump.alloc_str(sp);
    ev.str_val(sp.as_bytes(), &[Ctx::Opaque(sp)])
}

pub fn to_path<'a>(ev: &Eval<'a>, path: Value<'a>) -> R<'a> {
    let path = ev.coerce_to_path(path)?;
    Ok(ev.string(crate::path::abs(&path)))
}

pub fn store_path<'a>(ev: &Eval<'a>, path: Value<'a>) -> R<'a> {
    if ev.ctx.io.pure {
        return eval_err("'builtins.storePath' is not allowed in pure evaluation mode");
    }
    let path = ev.coerce_to_path(path)?;
    let prefix = format!("{}/", crate::store::STORE_DIR);
    let Some(rest) = path.strip_prefix(&prefix) else {
        return eval_err(format!("path '{path}' is not in the Nix store"));
    };
    let base = &path[..prefix.len() + rest.find('/').unwrap_or(rest.len())];
    let base: &'a str = ev.bump.alloc_str(base);
    Ok(ev.str_val(path.as_bytes(), &[Ctx::Opaque(base)]))
}

/// Shared by `builtins.path` and `filterSource`. Unless `recursive`, `path`
/// must be a regular file, copied unfiltered. With `sha256`, the hash the copy
/// must have (of its NAR serialisation if `recursive`, else of its contents),
/// a store path already valid for that hash is returned without reading
/// `path`.
fn add_path<'a>(
    ev: &Eval<'a>,
    path: &str,
    name: Option<String>,
    filter: Option<Value<'a>>,
    recursive: bool,
    sha256: Option<Vec<u8>>,
) -> R<'a> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => crate::path::store_name(path),
    };
    let io = &ev.ctx.io;
    let expected = sha256.map(|h| {
        crate::store::fixed_output_path(recursive, crate::store::HashAlgo::Sha256, &h, &name)
    });
    if let Some(sp) = expected.as_deref().and_then(crate::store::StorePath::parse)
        && io.is_valid(&sp).map_err(anyhow_err)?
    {
        io.allow_valid(&sp);
        return Ok(store_path_val(ev, sp.as_str()));
    }
    let sp = if recursive {
        let err = &Cell::new(None);
        let keep = filter.map(|f| {
            move |p: &str, ty: FileType| -> anyhow::Result<bool> {
                let args = [ev.string(crate::path::abs(p)), ev.string(ty.name())];
                ev.call(f, &args, None)
                    .and_then(|r| ev.force_bool(r))
                    .map_err(|e| {
                        err.set(Some(e));
                        anyhow::anyhow!("filter failed")
                    })
            }
        });
        let keep = keep.as_ref().map(|k| k as crate::io::KeepFn<'_>);
        io.add_filtered_to_store(path, &name, keep)
            .map_err(|e| err.take().unwrap_or_else(|| anyhow_err(e)))?
    } else {
        io.add_flat_to_store(path, &name).map_err(anyhow_err)?
    };
    if expected.is_some_and(|e| e != sp) {
        return eval_err(format!(
            "store path mismatch in (possibly filtered) path added from '{path}'"
        ));
    }
    Ok(store_path_val(ev, &sp))
}

pub fn path<'a>(ev: &Eval<'a>, set: Value<'a>) -> R<'a> {
    let set = ev.force_attrs(set)?;
    let (mut path, mut name, mut filter, mut recursive, mut sha256) =
        (None, None, None, true, None);
    for e in set.entries {
        match ev.ctx.name(e.name) {
            "path" => path = Some(ev.realise_path(e.value)?),
            "name" => {
                name = Some(String::from_utf8_lossy(ev.force_str_no_ctx(e.value)?).into_owned());
            }
            "filter" => filter = Some(ev.force_function(e.value)?),
            "recursive" => recursive = ev.force_bool(e.value)?,
            "sha256" => sha256 = Some(sha256_arg(ev, e.value)?.unwrap_or_else(|| vec![0; 32])),
            n => return eval_err(format!("unsupported argument '{n}' to 'builtins.path'")),
        }
    }
    let Some(path) = path else {
        return eval_err(
            "missing required 'path' attribute in the first argument to builtins.path",
        );
    };
    add_path(ev, &path, name, filter, recursive, sha256)
}

/// The hash a `sha256` argument gives, or `None` if it is empty.
fn sha256_arg<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a, Option<Vec<u8>>> {
    let hash = ev.force_str_no_ctx(v)?;
    if hash.is_empty() {
        return Ok(None);
    }
    let algo = Some(crate::store::HashAlgo::Sha256);
    let (_, hash) =
        crate::store::parse_hash(&String::from_utf8_lossy(hash), algo).map_err(anyhow_err)?;
    Ok(Some(hash))
}

pub fn filter_source<'a>(ev: &Eval<'a>, filter: Value<'a>, path: Value<'a>) -> R<'a> {
    let path = ev.realise_path(path)?;
    let filter = ev.force_function(filter)?;
    add_path(ev, &path, None, Some(filter), true, None)
}

pub fn fetch_git<'a>(ev: &Eval<'a>, arg: Value<'a>) -> R<'a> {
    match ev.force(arg)? {
        Value::Attrs(set) => {
            if set.get(ev.ctx.syms.type_).is_some() {
                return eval_err("unexpected argument 'type'");
            }
            for e in set.entries {
                let name = ev.ctx.name(e.name);
                match ev.force(e.value)? {
                    Value::Str(s) if name == "url" => check_git_url(s.s)?,
                    Value::Str(_) | Value::Path(_) | Value::Bool(_) => {}
                    Value::Int(i) if i >= 0 => {}
                    Value::Int(i) => {
                        return eval_err(format!(
                            "negative value given for 'fetchGit' argument '{name}': {i}"
                        ));
                    }
                    other => {
                        return eval_err(format!(
                            "argument '{name}' to 'fetchGit' is {} while a string, Boolean or integer is expected",
                            other.show_type()
                        ));
                    }
                }
            }
        }
        v => {
            ev.coerce_to_string(v, Coerce::PLAIN)?;
        }
    }
    unimplemented("builtins.fetchGit")
}

/// Rejects a `fetchGit` URL that is neither an absolute path, a valid
/// URL, nor SCP-style `[user@]host:path` with a non-empty path, where a
/// host in brackets must be an IPv6 address, or that is a URL with a host
/// whose scheme is `file`, perhaps after a `+`.
fn check_git_url<'a>(url: &[u8]) -> R<'a, ()> {
    const SCHEMES: &[&[u8]] = &[b"file", b"git", b"ssh", b"http", b"https", b"ftp", b"ftps"];
    if url.first() == Some(&b'/') {
        return Ok(());
    }
    let shown = String::from_utf8_lossy(url);
    let not_url = || {
        if super::url::check(&[b"x:", url].concat()).is_ok() {
            eval_err(format!("'{shown}' doesn't have a scheme"))
        } else {
            eval_err(format!("'{shown}' is not a valid URL"))
        }
    };
    let colon = url.iter().position(|&c| c == b':');
    let Some(scheme) = colon.map(|i| &url[..i]).filter(|s| !s.contains(&b'/')) else {
        return not_url();
    };
    if SCHEMES.contains(&scheme.strip_prefix(b"git+").unwrap_or(scheme))
        || url.windows(3).any(|w| w == b"://")
    {
        let uri = super::url::parse(url).or_else(eval_err)?;
        if uri.file_with_host() {
            let rest = &url[scheme.len() + 3..];
            let end = rest.iter().position(|c| b"/?#".contains(c));
            return eval_err(format!(
                "file:// URL '{shown}' has unexpected authority '{}'",
                String::from_utf8_lossy(&super::url::decode(&rest[..end.unwrap_or(rest.len())]))
            ));
        }
        return Ok(());
    }
    // The first "@[", or else a leading '[', opens a bracket. When a ']'
    // closes it, the path starts after that ']', which must be followed by
    // ':'; otherwise it starts after the first ':'.
    let open = url
        .windows(2)
        .position(|w| w == b"@[")
        .map(|i| i + 1)
        .or((url[0] == b'[').then_some(0));
    let colon = match open.and_then(|o| url[o..].iter().position(|&c| c == b']').map(|e| o + e + 1))
    {
        Some(end) if url.get(end) == Some(&b':') => end,
        Some(_) => return not_url(),
        None => scheme.len(),
    };
    let (prefix, path) = (&url[..colon], &url[colon + 1..]);
    let host = prefix
        .iter()
        .position(|&c| c == b'@')
        .map_or(prefix, |i| &prefix[i + 1..]);
    if let Some(host) = host.strip_prefix(b"[").and_then(|h| h.strip_suffix(b"]"))
        && !super::url::ipv6(host)
    {
        return eval_err(format!(
            "Git SCP bracketed URL is not valid: '{}' is not a valid IPv6 address",
            String::from_utf8_lossy(host)
        ));
    }
    if path.is_empty() {
        return eval_err(format!("SCP-style Git URL '{shown}' has an empty path"));
    }
    Ok(())
}

pub fn fetch_mercurial<'a>(ev: &Eval<'a>, arg: Value<'a>) -> R<'a> {
    match ev.force(arg)? {
        Value::Attrs(set) => {
            for e in set.entries {
                match ev.ctx.name(e.name) {
                    "url" => {
                        ev.coerce_to_string(e.value, Coerce::PLAIN)?;
                    }
                    "rev" | "name" => {
                        ev.force_str_no_ctx(e.value)?;
                    }
                    n => {
                        return eval_err(format!("unsupported argument '{n}' to 'fetchMercurial'"));
                    }
                }
            }
        }
        v => {
            ev.coerce_to_string(v, Coerce::PLAIN)?;
        }
    }
    unimplemented("builtins.fetchMercurial")
}

pub fn fetch_tarball<'a>(ev: &Eval<'a>, arg: Value<'a>) -> R<'a> {
    check_fetch(ev, arg, "fetchTarball")?;
    unimplemented("builtins.fetchTarball")
}

pub fn fetchurl<'a>(ev: &Eval<'a>, arg: Value<'a>) -> R<'a> {
    check_fetch(ev, arg, "fetchurl")?;
    unimplemented("builtins.fetchurl")
}

pub fn fetch_tree<'a>(ev: &Eval<'a>, arg: Value<'a>) -> R<'a> {
    ev.force(arg)?;
    unimplemented("builtins.fetchTree")
}

/// Fails on the arguments to `who` that Nix rejects before fetching.
fn check_fetch<'a>(ev: &Eval<'a>, arg: Value<'a>, who: &str) -> R<'a, ()> {
    match ev.force(arg)? {
        Value::Attrs(set) => {
            for e in set.entries {
                match ev.ctx.name(e.name) {
                    "url" | "name" => {
                        ev.force_str_no_ctx(e.value)?;
                    }
                    "sha256" => {
                        sha256_arg(ev, e.value)?;
                    }
                    n => return eval_err(format!("unsupported argument '{n}' to '{who}'")),
                }
            }
        }
        v => {
            ev.force_str_no_ctx(v)?;
        }
    }
    Ok(())
}
