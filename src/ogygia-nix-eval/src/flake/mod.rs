//! Flakes, evaluated as `nix eval` evaluates an installable: the flake's
//! inputs come from its lock file, which must not need changing.

mod file;
mod flakeref;
mod lock;
pub(crate) mod source;
mod update;

use std::cell::OnceCell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use crate::eval::Eval;
use crate::flake::flakeref::Attr;
use crate::flake::flakeref::Attrs;
use crate::flake::lock::Lock;
use crate::flake::source::Info;
use crate::flake::source::Source;
use crate::value::Ctx;
use crate::value::Entry;
use crate::value::ErrorKind;
use crate::value::R;
use crate::value::ThunkState;
use crate::value::Value;
use crate::value::anyhow_err;
use crate::value::eval_err;
use crate::value::unimplemented;

/// Where a flake is evaluated.
pub struct FlakeEnv {
    /// The directory relative flake references are relative to.
    pub cwd: PathBuf,
    /// A directory holding store objects, such as `/nix/store`, that are
    /// valid store paths: a locked input is read from it instead of being
    /// fetched, if it is there.
    pub store: Option<PathBuf>,
    /// What downloads inputs from anywhere but a local `file:` URL. Without
    /// one, such inputs cannot be fetched.
    pub fetcher: Option<Box<dyn crate::Fetcher>>,
}

/// The flakes of one evaluation: the root and the nodes of its lock file,
/// each fetched and evaluated at most once, by node key.
pub(crate) struct Flakes<'a> {
    env: &'a FlakeEnv,
    lock: Lock,
    /// How the root flake was referred to, for messages.
    root_ref: String,
    sources: RefCell<HashMap<&'a str, Located>>,
    values: RefCell<HashMap<&'a str, Value<'a>>>,
    outputs: RefCell<HashMap<&'a str, Value<'a>>>,
}

/// A source and where in it a flake is.
#[derive(Clone)]
struct Located {
    source: Source,
    /// The flake's directory, `source.path` or below it.
    dir: String,
}

impl Located {
    /// This location with `.` and `..` in its directory resolved, as Nix has
    /// it for a flake it fetches while locking.
    fn resolved(self) -> Located {
        Located {
            dir: crate::path::canon_path(&self.dir),
            ..self
        }
    }
}

fn r<'a, T>(e: anyhow::Result<T>) -> R<'a, T> {
    e.map_err(anyhow_err)
}

impl<'a> Flakes<'a> {
    /// The source of node `key`, fetched the first time it is needed.
    fn locate(&'a self, ev: &Eval<'a>, key: &'a str) -> R<'a, Located> {
        if let Some(l) = self.sources.borrow().get(key) {
            return Ok(l.clone());
        }
        let (_, node) = self.lock.node(key)?;
        let Some(locked) = &node.locked else {
            return eval_err(format!("lock file node '{key}' is not locked"));
        };
        let located = match (flakeref::relative_path(locked), &node.parent) {
            // Within the flake it is relative to, and from the same source.
            (Some(rel), Some(parent)) => {
                let parent = self.locate(ev, self.lock.resolve(parent)?)?;
                Located {
                    dir: flake_dir(&format!("{}/{rel}", parent.dir), locked),
                    source: parent.source,
                }
            }
            _ => fetch_flake(ev, self.env, locked)?,
        };
        self.sources.borrow_mut().insert(key, located.clone());
        Ok(located)
    }

    /// The value of node `key`, as the flakes it is an input of see it.
    fn value(&'a self, ev: &Eval<'a>, key: &'a str) -> Value<'a> {
        if let Some(v) = self.values.borrow().get(key) {
            return *v;
        }
        let build: &'a dyn Fn(&Eval<'a>) -> R<'a> =
            ev.bump.alloc(move |ev: &Eval<'a>| self.build(ev, key));
        let v = ev.thunk(ThunkState::Native(build));
        self.values.borrow_mut().insert(key, v);
        v
    }

    /// The outputs of node `key`: its `outputs` function applied to its
    /// inputs and itself.
    fn outputs(&'a self, ev: &Eval<'a>, key: &'a str) -> R<'a> {
        if let Some(v) = self.outputs.borrow().get(key) {
            return Ok(*v);
        }
        let located = self.locate(ev, key)?;
        let shown = if key == self.lock.root {
            self.root_ref.clone()
        } else {
            let (_, node) = self.lock.node(key)?;
            node.locked
                .as_ref()
                .map(flakeref::to_string)
                .unwrap_or_default()
        };
        let flake = file::load_locked(ev, &located.dir, &shown)?;
        let mut args = vec![ev.entry("self", self.value(ev, key))];
        args.extend(self.inputs(ev, key)?);
        let v = ev.lazy_app(flake.outputs, ev.attrs(args));
        self.outputs.borrow_mut().insert(key, v);
        Ok(v)
    }

    /// The outputs of node `key` as its value's `outputs` attribute: forcing
    /// them builds its value, so they fail unless they are a set.
    fn checked_outputs(&'a self, ev: &Eval<'a>, key: &'a str) -> Value<'a> {
        let build: &'a dyn Fn(&Eval<'a>) -> R<'a> = ev.bump.alloc(move |ev: &Eval<'a>| {
            ev.force(self.value(ev, key))?;
            ev.force(self.outputs(ev, key)?)
        });
        ev.thunk(ThunkState::Native(build))
    }

    /// The inputs of node `key`, each the value of the node it resolves to.
    fn inputs(&'a self, ev: &Eval<'a>, key: &'a str) -> R<'a, Vec<Entry<'a>>> {
        let (_, node) = self.lock.node(key)?;
        let mut out = Vec::new();
        for (name, edge) in &node.inputs {
            let input = self.lock.target(edge)?;
            out.push(ev.entry(name, self.value(ev, input)));
        }
        Ok(out)
    }

    /// Node `key`'s value: for a flake, its outputs and where it came from.
    fn build(&'a self, ev: &Eval<'a>, key: &'a str) -> R<'a> {
        let located = self.locate(ev, key)?;
        let info = info_entries(ev, &located);
        let source_info = ev.attrs(info.clone());
        let (_, node) = self.lock.node(key)?;
        let mut entries = Vec::new();
        if node.flake {
            let outputs = self.outputs(ev, key)?;
            entries.extend_from_slice(ev.force_attrs(outputs)?.entries);
            entries.extend(info);
            entries.push(ev.entry("_type", ev.string("flake")));
            entries.push(ev.entry("inputs", ev.attrs(self.inputs(ev, key)?)));
            entries.push(ev.entry("outputs", outputs));
        } else {
            entries.extend(info);
        }
        entries.push(ev.entry("sourceInfo", source_info));
        entries.push(ev.entry("outPath", store_string(ev, &located.dir, &located.source)));
        Ok(ev.attrs(entries))
    }
}

/// The lock file of the flake in `dir`, empty if it has none.
fn read_lock<'a>(ev: &Eval<'a>, dir: &str) -> R<'a, Lock> {
    let lock_file = format!("{dir}/flake.lock");
    if !r(ev.ctx.io.exists(&lock_file))? {
        return Ok(Lock::empty());
    }
    let text = r(ev.ctx.io.read(&lock_file))?;
    let json = crate::builtins::json::from_json(ev, ev.str_val(&text, &[]))?;
    lock::from_value(ev, json)
}

/// The flake `attrs` refers to, fetched.
fn fetch_flake<'a>(ev: &Eval<'a>, env: &FlakeEnv, attrs: &Attrs) -> R<'a, Located> {
    let what = flakeref::to_string(attrs);
    let source = r(source::fetch(&ev.ctx.io, env, attrs, &what))?;
    Ok(Located {
        dir: flake_dir(&source.path, attrs),
        source,
    })
}

/// Add `self_attrs`, from the `inputs.self` of the flake `attrs` refers to
/// and `located` holds, to its reference, if it is a Git input.
fn add_self_attrs(
    attrs: &mut Attrs,
    located: &mut Located,
    self_attrs: &Attrs,
) -> anyhow::Result<()> {
    if flakeref::get_str(attrs, "type") == Some("git") {
        attrs.extend(self_attrs.clone());
        located.source.info.submodules = source::submodules(attrs)?;
    }
    Ok(())
}

/// The directory of the flake `attrs` refers to, in `base`, the directory
/// it is relative to.
fn flake_dir(base: &str, attrs: &Attrs) -> String {
    match flakeref::get_str(attrs, "dir") {
        Some(d) if !d.is_empty() => format!("{base}/{d}"),
        _ => base.to_owned(),
    }
}

/// A string of the logical path `path` within `source`, referring to it.
fn store_string<'a>(ev: &Eval<'a>, path: &str, source: &Source) -> Value<'a> {
    let sp = ev.bump.alloc_str(&source.path);
    ev.str_val(path.as_bytes(), &[Ctx::Opaque(sp)])
}

/// The attributes of a flake's `sourceInfo`.
fn info_entries<'a>(ev: &Eval<'a>, located: &Located) -> Vec<Entry<'a>> {
    let Info {
        nar_hash,
        last_modified,
        rev,
        rev_count,
        dirty_rev,
        submodules,
    } = &located.source.info;
    let mut out = vec![ev.entry(
        "outPath",
        store_string(ev, &located.source.path, &located.source),
    )];
    if let Some(h) = nar_hash {
        out.push(ev.entry("narHash", ev.string(h)));
    }
    if let Some(t) = last_modified {
        out.push(ev.entry("lastModified", Value::Int(*t as i64)));
        out.push(ev.entry("lastModifiedDate", ev.string(&source::date(*t))));
    }
    if let Some(rev) = rev {
        out.push(ev.entry("rev", ev.string(rev)));
        out.push(ev.entry("shortRev", ev.string(source::short_rev(rev))));
    }
    if let Some(n) = rev_count {
        out.push(ev.entry("revCount", Value::Int(*n as i64)));
    }
    if let Some(rev) = dirty_rev {
        out.push(ev.entry("dirtyRev", ev.string(rev)));
        let short = format!("{}-dirty", source::short_rev(rev));
        out.push(ev.entry("dirtyShortRev", ev.string(&short)));
    }
    if let Some(s) = submodules {
        out.push(ev.entry("submodules", Value::Bool(*s)));
    }
    out
}

/// The innermost directory containing `dir` that is a Git repository's
/// working tree, if any.
fn git_root(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|d| d.join(".git").exists())
}

/// The directory of the flake that directory `dir` is part of: the nearest
/// of it and its ancestors with a `flake.nix`, not looking above the root of
/// a Git working tree.
fn find_flake(dir: &Path) -> anyhow::Result<&Path> {
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => anyhow::bail!(
            "path \"{}\" is not a flake (because it's not a directory)",
            dir.display()
        ),
        Err(e) => anyhow::bail!("getting status of \"{}\": {e}", dir.display()),
    }
    for d in dir.ancestors() {
        if d.join("flake.nix").symlink_metadata().is_ok() {
            return Ok(d);
        }
        if d.join(".git").exists() {
            anyhow::bail!(
                "path \"{}\" is not part of a flake (neither it nor its parent directories contain a 'flake.nix' file)",
                d.display()
            );
        }
    }
    anyhow::bail!("could not find a flake.nix file")
}

/// The flake reference of an installable, made absolute against `cwd`. A
/// path refers to the flake it is part of, and its query is that
/// reference's. Within a Git repository's working tree, that is a
/// `git+file:` reference to the repository, whose `dir` is the flake's
/// directory within it.
fn installable_ref(flake: &str, cwd: &Path) -> anyhow::Result<Attrs> {
    if !crate::builtins::flake::is_path(flake.as_bytes()) {
        return flakeref::parse(flake, Some(cwd));
    }
    let (base, query) = match flake.split_once('?') {
        Some((base, query)) => (base, format!("?{query}")),
        None => (flake, String::new()),
    };
    let path = PathBuf::from(flakeref::path(base, cwd));
    let dir = find_flake(&path)?;
    let Some(root) = git_root(dir) else {
        let mut attrs = flakeref::parse(flake, Some(cwd))?;
        attrs.insert("path".to_owned(), Attr::Str(dir.display().to_string()));
        return Ok(attrs);
    };
    let root_url = crate::builtins::url::encode(root.as_os_str().as_bytes(), b"/");
    let url = format!("git+file://{}{query}", String::from_utf8_lossy(&root_url));
    let mut attrs = flakeref::parse(&url, None)?;
    let sub = dir.strip_prefix(root).unwrap_or(Path::new(""));
    if !sub.as_os_str().is_empty() {
        if attrs.contains_key("dir") {
            anyhow::bail!("flake URL '{flake}' has an inconsistent 'dir' parameter");
        }
        attrs.insert("dir".to_owned(), Attr::Str(sub.display().to_string()));
    }
    Ok(attrs)
}

/// An attribute path as written after `#`: names separated by dots, any
/// of them quoted. Every unquoted dot ends a name, even an empty one; only
/// an empty name after the last dot is dropped.
fn attr_path(s: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            '.' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    if quoted {
        anyhow::bail!("missing closing quote in selection path '{s}'");
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// The value at `path` in `v`, or `None` if it has no such attribute. A
/// value that is not a set, or whose evaluation fails with a type error,
/// has none.
fn select<'a>(ev: &Eval<'a>, v: Value<'a>, path: &[String]) -> R<'a, Option<Value<'a>>> {
    let mut v = v;
    for name in path {
        let a = match ev.force(v) {
            Ok(Value::Attrs(a)) => a,
            Ok(_) => return Ok(None),
            Err(e) if e.kind == ErrorKind::Type => return Ok(None),
            Err(e) => return Err(e),
        };
        match a.get(ev.sym(name)) {
            Some(x) => v = x,
            None => return Ok(None),
        }
    }
    Ok(Some(v))
}

/// Evaluate `installable` as `nix eval --json --no-update-lock-file --apply
/// apply` does, returning what it prints, without the final newline.
pub(crate) fn eval_json<'a>(
    ev: &Eval<'a>,
    installable: &str,
    apply: &str,
    env: &'a FlakeEnv,
    flakes: &'a OnceCell<Flakes<'a>>,
) -> R<'a, Vec<u8>> {
    // Nix reads what follows the last `^` as the outputs to select.
    if installable.contains('^') {
        return unimplemented("installable outputs specifier '^'");
    }
    let (flake, fragment) = match installable.split_once('#') {
        Some((f, a)) => (f, Some(a)),
        None => (installable, None),
    };
    let mut attrs = r(installable_ref(flake, &env.cwd))?;
    let shown = flakeref::to_string(&attrs);
    let mut located = fetch_flake(ev, env, &attrs)?.resolved();
    let loaded = file::load(ev, &located.dir, &shown)?;
    r(add_self_attrs(&mut attrs, &mut located, &loaded.self_attrs))?;
    let lock = read_lock(ev, &located.dir)?;
    lock.check_locked()?;
    // Nix locks the inputs anew, and goes on without writing the lock file
    // if that changes nothing or leaves some unlocked.
    let (new, located) = update::lock(ev, env, &lock, located, &loaded.inputs)?;
    if !new.same(&lock) && !new.has_unlocked() {
        return eval_err(format!(
            "flake '{shown}' requires lock file changes but they're not allowed due to '--no-update-lock-file'"
        ));
    }
    let flakes = flakes.get_or_init(|| Flakes {
        env,
        lock: new,
        root_ref: shown.clone(),
        sources: RefCell::new(HashMap::new()),
        values: RefCell::new(HashMap::new()),
        outputs: RefCell::new(HashMap::new()),
    });
    for (key, located) in located {
        let (key, _) = flakes.lock.node(&key)?;
        flakes.sources.borrow_mut().insert(key, located);
    }
    let (root, _) = flakes.lock.node(&flakes.lock.root)?;
    let outputs = flakes.checked_outputs(ev, root);
    let system = &ev.settings.current_system;
    let candidates: Vec<Vec<String>> = match fragment {
        None | Some("") => vec![
            vec!["packages".into(), system.clone(), "default".into()],
            vec!["defaultPackage".into(), system.clone()],
        ],
        Some(f) => {
            let f = r(flakeref::percent_decode(f))?;
            match f.strip_prefix('.') {
                // A leading dot selects from the outputs alone.
                Some(f) => vec![r(attr_path(f))?],
                None => {
                    let p = r(attr_path(&f))?;
                    let with = |prefix: &[&str]| -> Vec<String> {
                        prefix
                            .iter()
                            .map(|s| s.to_string())
                            .chain(p.iter().cloned())
                            .collect()
                    };
                    vec![
                        with(&["packages", system]),
                        with(&["legacyPackages", system]),
                        p.clone(),
                    ]
                }
            }
        }
    };
    let mut found = None;
    for c in &candidates {
        if let Some(v) = select(ev, outputs, c)? {
            found = Some(v);
            break;
        }
    }
    let Some(v) = found else {
        let names: Vec<String> = candidates
            .iter()
            .map(|c| format!("'{}'", c.join(".")))
            .collect();
        let (last, rest) = names.split_last().expect("there are candidates");
        let list = if rest.is_empty() {
            last.clone()
        } else {
            format!("{} or {last}", rest.join(", "))
        };
        return eval_err(format!("flake '{shown}' does not provide attribute {list}"));
    };
    ev.force(v)?;
    let f = ev.eval_string(apply, &env.cwd.display().to_string())?;
    let result = ev.call(f, &[v], None)?;
    let mut out = Vec::new();
    let mut ctx = Vec::new();
    crate::builtins::json::write_json(ev, result, &mut out, &mut ctx)?;
    Ok(out)
}
