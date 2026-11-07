//! Locking a flake's inputs anew, as Nix does when its lock file does not
//! lock them: an input whose reference is unchanged keeps its node, and
//! every other is fetched, along with the inputs of its `flake.nix` as its
//! own lock file locks them.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::eval::Eval;
use crate::flake::FlakeEnv;
use crate::flake::Located;
use crate::flake::file;
use crate::flake::file::InputSpec;
use crate::flake::flake_dir;
use crate::flake::flakeref;
use crate::flake::flakeref::Attr;
use crate::flake::flakeref::Attrs;
use crate::flake::lock::Edge;
use crate::flake::lock::Lock;
use crate::flake::lock::Node;
use crate::flake::r;
use crate::flake::read_lock;
use crate::flake::source;
use crate::flake::source::Info;
use crate::flake::source::Source;
use crate::value::R;
use crate::value::eval_err;

/// What an input is, once overrides are applied.
enum Want {
    /// The input at this path from the root.
    Follows(Vec<String>),
    /// The source this reference names.
    Ref { attrs: Attrs, flake: bool },
}

/// A node of an old lock file: the root's, or that of an input fetched
/// anew.
#[derive(Clone, Copy)]
struct Old<'l> {
    lock: &'l Lock,
    key: &'l str,
    /// The path of the flake whose lock file it is.
    at: &'l [String],
}

struct Locker<'x, 'a> {
    ev: &'x Eval<'a>,
    env: &'a FlakeEnv,
    nodes: HashMap<String, Node>,
    /// Overrides by the path of the input they override, each with the
    /// path of the flake declaring it. The first declared, the outermost,
    /// wins.
    overrides: HashMap<Vec<String>, (InputSpec, Vec<String>)>,
    /// Where the inputs fetched so far are, by node key.
    located: HashMap<String, Located>,
    /// Every input that follows another, and the input it follows.
    follows: Vec<(Vec<String>, Vec<String>)>,
    /// The references of the flakes being fetched, and the keys of the
    /// nodes of the current old lock file being kept, outermost first.
    fetching: Vec<Attrs>,
    keeping: Vec<String>,
}

/// The key of the node at `path`.
fn key(path: &[String]) -> String {
    format!("/{}", path.join("/"))
}

fn child(path: &[String], name: &str) -> Vec<String> {
    let mut p = path.to_vec();
    p.push(name.to_owned());
    p
}

/// The path of a reference to a directory relative to the flake declaring
/// it.
fn relative(attrs: &Attrs) -> Option<&str> {
    match (
        flakeref::get_str(attrs, "type"),
        flakeref::get_str(attrs, "path"),
    ) {
        (Some("path"), Some(p)) if !p.starts_with('/') => Some(p),
        _ => None,
    }
}

impl<'a> Locker<'_, 'a> {
    fn register(&mut self, decl: &[String], path: Vec<String>, spec: &InputSpec) {
        for (name, o) in &spec.overrides {
            let p = child(&path, name);
            self.overrides
                .entry(p.clone())
                .or_insert_with(|| (o.clone(), decl.to_vec()));
            self.register(decl, p, o);
        }
    }

    /// The override of the input at `path` that replaces what it is, and
    /// the path of the flake declaring it.
    fn overriding(&self, path: &[String]) -> Option<&(InputSpec, Vec<String>)> {
        self.overrides
            .get(path)
            .filter(|(o, _)| o.follows.is_some() || o.attrs.is_some())
    }

    /// What the input at `path` is: as `own`, declared by the flake at
    /// `decl`, says, unless an override says otherwise.
    fn want(&self, path: &[String], own: &InputSpec, decl: &[String]) -> Option<Want> {
        let (spec, decl, flake) = match self.overriding(path) {
            Some((o, at)) => (o, at.as_slice(), o.flake.or(own.flake)),
            None => (own, decl, own.flake),
        };
        if let Some(f) = &spec.follows {
            return Some(Want::Follows(decl.iter().chain(f).cloned().collect()));
        }
        spec.attrs.clone().map(|attrs| Want::Ref {
            attrs,
            flake: flake.unwrap_or(true),
        })
    }

    /// Lock the inputs `inputs` of the flake at `path`, whose node is
    /// already added, as the flake at `decl` declares them, reusing what
    /// node `old` of an old lock file locks.
    fn lock_inputs(
        &mut self,
        path: &[String],
        inputs: &BTreeMap<String, InputSpec>,
        decl: &[String],
        old: Option<Old<'_>>,
    ) -> R<'a, ()> {
        for (name, spec) in inputs {
            self.register(decl, child(path, name), spec);
        }
        let mut edges = BTreeMap::new();
        for (name, spec) in inputs {
            let p = child(path, name);
            let old_child = old.and_then(|o| match o.lock.nodes.get(o.key)?.inputs.get(name) {
                Some(Edge::Node(k)) => Some(Old { key: k, ..o }),
                _ => None,
            });
            let Some(want) = self.want(&p, spec, decl) else {
                continue;
            };
            let edge = match want {
                Want::Follows(target) => {
                    self.follows.push((p, target.clone()));
                    Edge::Follows(target)
                }
                Want::Ref { attrs, flake } => {
                    // A relative node is kept only if its parent is the
                    // node whose input it is.
                    let parent = relative(&attrs).map(|_| path.to_vec());
                    match old_child.and_then(|o| Some((o, o.lock.nodes.get(o.key)?))) {
                        Some((o, n))
                            if n.original.as_ref() == Some(&attrs) && n.parent == parent =>
                        {
                            self.reuse(&p, o)?
                        }
                        _ => self.fetch(&p, attrs, flake, decl)?,
                    }
                    Edge::Node(key(&p))
                }
            };
            edges.insert(name.clone(), edge);
        }
        self.nodes
            .get_mut(&key(path))
            .expect("the node is added before its inputs")
            .inputs = edges;
        Ok(())
    }

    /// Keep node `old` of an old lock file as the input at `path`. Its
    /// inputs are kept as `old` locks them, unless one follows another
    /// without an override saying what it is, when they are locked anew
    /// from its `flake.nix`.
    fn reuse(&mut self, path: &[String], old: Old<'_>) -> R<'a, ()> {
        let (k, n) = old.lock.node(old.key)?;
        if self.keeping.iter().any(|x| x == k) {
            return eval_err(format!("lock file node '{k}' is its own input"));
        }
        self.nodes.insert(
            key(path),
            Node {
                inputs: BTreeMap::new(),
                locked: n.locked.clone(),
                original: n.original.clone(),
                flake: n.flake,
                parent: n.parent.clone(),
            },
        );
        let stale = n.inputs.iter().any(|(name, edge)| {
            matches!(edge, Edge::Follows(_)) && self.overriding(&child(path, name)).is_none()
        });
        let old = Old { key: k, ..old };
        self.keeping.push(k.to_owned());
        let locked = if !stale {
            self.keep_inputs(path, n, old)
        } else if n.flake {
            self.relock(path, old)
        } else {
            Ok(())
        };
        self.keeping.pop();
        locked
    }

    /// Lock the inputs of the input at `path` as its old node `n`, `old`,
    /// locks them, as if the flake whose lock file it is declared them.
    fn keep_inputs(&mut self, path: &[String], n: &Node, old: Old<'_>) -> R<'a, ()> {
        let mut inputs = BTreeMap::new();
        for (name, edge) in &n.inputs {
            let spec = match edge {
                Edge::Follows(p) => InputSpec {
                    follows: Some(p.clone()),
                    ..InputSpec::default()
                },
                Edge::Node(k) => {
                    let (_, c) = old.lock.node(k)?;
                    InputSpec {
                        attrs: c.original.clone(),
                        flake: Some(c.flake),
                        ..InputSpec::default()
                    }
                }
            };
            inputs.insert(name.clone(), spec);
        }
        self.lock_inputs(path, &inputs, old.at, Some(old))
    }

    /// Lock the inputs the `flake.nix` of the input at `path` declares,
    /// reusing what its old node `old` locks.
    fn relock(&mut self, path: &[String], old: Old<'_>) -> R<'a, ()> {
        let located = self.locate(path)?;
        let shown = self.nodes[&key(path)]
            .locked
            .as_ref()
            .map(flakeref::to_string)
            .unwrap_or_default();
        let loaded = file::load(self.ev, &located.dir, &shown)?;
        self.lock_inputs(path, &loaded.inputs, path, Some(old))
    }

    /// Where the input at `path`, whose node is added, is: fetched from
    /// what its node locks, unless it already has been.
    fn locate(&mut self, path: &[String]) -> R<'a, Located> {
        let k = key(path);
        if let Some(l) = self.located.get(&k) {
            return Ok(l.clone());
        }
        let Some(node) = self.nodes.get(&k) else {
            return eval_err(format!("input '{}' does not exist", path.join("/")));
        };
        let Some(locked) = node.locked.clone() else {
            return eval_err(format!("lock file node '{k}' is not locked"));
        };
        let located = match (relative(&locked), node.parent.clone()) {
            (Some(rel), Some(parent)) => {
                let base = self.locate(&parent)?;
                Located {
                    dir: format!("{}/{rel}", base.dir),
                    source: base.source,
                }
            }
            _ => {
                let what = flakeref::to_string(&locked);
                let source = r(source::fetch(&self.ev.ctx.io, self.env, &locked, &what))?;
                Located {
                    dir: source.path.clone(),
                    source,
                }
            }
        };
        let located = Located {
            dir: flake_dir(&located.dir, &locked),
            ..located
        };
        self.located.insert(k, located.clone());
        Ok(located)
    }

    /// Fetch the input at `path` that `attrs`, from the flake at `decl`,
    /// refers to, and lock its inputs if it is a flake, reusing what its
    /// own lock file locks.
    fn fetch(&mut self, path: &[String], attrs: Attrs, flake: bool, decl: &[String]) -> R<'a, ()> {
        if flake && self.fetching.contains(&attrs) {
            return eval_err(format!(
                "found circular import of flake '{}'",
                flakeref::to_string(&attrs)
            ));
        }
        let mut locked = attrs.clone();
        let (located, parent) = match relative(&attrs) {
            // Within the flake it is relative to, and from the same source,
            // of which it knows nothing more.
            Some(rel) => {
                let base = &self.located[&key(decl)];
                let located = Located {
                    dir: r(within(&base.source.path, &base.dir, rel))?,
                    source: Source {
                        path: base.source.path.clone(),
                        info: Info::default(),
                    },
                };
                (located, Some(decl.to_vec()))
            }
            None => {
                if let Some(rev) = r(source::locked_rev(&locked))? {
                    locked.insert("rev".to_owned(), Attr::Str(rev));
                }
                let what = flakeref::to_string(&locked);
                let source = r(source::fetch(&self.ev.ctx.io, self.env, &locked, &what))?;
                let located = Located {
                    dir: source.path.clone(),
                    source,
                };
                (located, None)
            }
        };
        let located = Located {
            dir: flake_dir(&located.dir, &locked),
            ..located
        };
        let shown = flakeref::to_string(&locked);
        self.nodes.insert(
            key(path),
            Node {
                inputs: BTreeMap::new(),
                locked: Some(locked),
                original: Some(attrs.clone()),
                flake,
                parent,
            },
        );
        self.located.insert(key(path), located.clone());
        if !flake {
            return Ok(());
        }
        let loaded = file::load(self.ev, &located.dir, &shown)?;
        let lock = read_lock(self.ev, &located.dir)?;
        lock.check_locked()?;
        let old = Old {
            lock: &lock,
            key: &lock.root,
            at: path,
        };
        self.fetching.push(attrs);
        let keeping = std::mem::take(&mut self.keeping);
        let locked = self.lock_inputs(path, &loaded.inputs, path, Some(old));
        self.keeping = keeping;
        self.fetching.pop();
        locked
    }
}

/// The directory `rel` leads to from `dir`, within the source at `root`.
fn within(root: &str, dir: &str, rel: &str) -> anyhow::Result<String> {
    let below = dir.strip_prefix(root).unwrap_or("");
    let mut names: Vec<&str> = below.split('/').filter(|n| !n.is_empty()).collect();
    for name in rel.split('/') {
        match name {
            "" | "." => {}
            ".." => {
                if names.pop().is_none() {
                    anyhow::bail!(
                        "path '{rel}' leads out of the source of the flake it is relative to"
                    );
                }
            }
            _ => names.push(name),
        }
    }
    Ok(names
        .iter()
        .fold(root.to_owned(), |p, n| format!("{p}/{n}")))
}

/// The lock file Nix makes for the root flake at `root`, whose `flake.nix`
/// declares `inputs`, from its lock file `old`, and where each node it
/// fetched, by key, is.
pub fn lock<'a>(
    ev: &Eval<'a>,
    env: &'a FlakeEnv,
    old: &Lock,
    root: Located,
    inputs: &BTreeMap<String, InputSpec>,
) -> R<'a, (Lock, HashMap<String, Located>)> {
    let mut locker = Locker {
        ev,
        env,
        nodes: HashMap::new(),
        overrides: HashMap::new(),
        located: HashMap::from([(key(&[]), root)]),
        follows: Vec::new(),
        fetching: Vec::new(),
        keeping: Vec::new(),
    };
    locker.nodes.insert(
        key(&[]),
        Node {
            inputs: BTreeMap::new(),
            locked: None,
            original: None,
            flake: true,
            parent: None,
        },
    );
    let old = Old {
        lock: old,
        key: &old.root,
        at: &[],
    };
    locker.lock_inputs(&[], inputs, &[], Some(old))?;
    let lock = Lock {
        root: key(&[]),
        nodes: locker.nodes,
    };
    for (input, target) in &locker.follows {
        if lock.resolve(target).is_err() {
            return eval_err(format!(
                "input '{}' follows a non-existent input '{}'",
                input.join("/"),
                target.join("/")
            ));
        }
    }
    Ok((lock, locker.located))
}
