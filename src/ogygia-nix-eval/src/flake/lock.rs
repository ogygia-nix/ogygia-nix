//! Lock files: the graph of a flake's inputs, each pinned to a source.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::eval::Eval;
use crate::flake::file::InputSpec;
use crate::flake::flakeref::Attr;
use crate::flake::flakeref::Attrs;
use crate::value::EvalError;
use crate::value::R;
use crate::value::Value;
use crate::value::eval_err;

/// What an input of a node is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edge {
    /// The node with this key.
    Node(String),
    /// Whatever the input at this path from the root is.
    Follows(Vec<String>),
}

pub struct Node {
    pub inputs: BTreeMap<String, Edge>,
    pub locked: Option<Attrs>,
    pub original: Option<Attrs>,
    pub flake: bool,
    /// For an input whose path is relative, the path from the root to the
    /// input it is relative to.
    pub parent: Option<Vec<String>>,
}

pub struct Lock {
    pub root: String,
    pub nodes: HashMap<String, Node>,
}

impl Lock {
    /// The lock file of a flake with no inputs and no lock file.
    pub fn empty() -> Lock {
        let root = Node {
            inputs: BTreeMap::new(),
            locked: None,
            original: None,
            flake: true,
            parent: None,
        };
        Lock {
            root: "root".to_owned(),
            nodes: HashMap::from([("root".to_owned(), root)]),
        }
    }

    /// The node with key `key`, and the key as the lock file holds it.
    pub fn node(&self, key: &str) -> Result<(&str, &Node), Box<EvalError<'static>>> {
        match self.nodes.get_key_value(key) {
            Some((k, n)) => Ok((k, n)),
            None => eval_err(format!("lock file references missing node '{key}'")),
        }
    }

    /// The key of the node at `path` from the root, following `follows`.
    pub fn resolve(&self, path: &[String]) -> Result<&str, Box<EvalError<'static>>> {
        self.resolve_depth(path, 0)
    }

    /// The key of the node `edge` leads to, following `follows`.
    pub fn target(&self, edge: &Edge) -> Result<&str, Box<EvalError<'static>>> {
        self.target_depth(edge, 0)
    }

    fn target_depth(&self, edge: &Edge, depth: usize) -> Result<&str, Box<EvalError<'static>>> {
        match edge {
            Edge::Node(k) => Ok(self.node(k)?.0),
            Edge::Follows(p) => self.resolve_depth(p, depth),
        }
    }

    fn resolve_depth(
        &self,
        path: &[String],
        depth: usize,
    ) -> Result<&str, Box<EvalError<'static>>> {
        if depth > self.nodes.len() {
            return eval_err(format!("input '{}' follows itself", path.join("/")));
        }
        let (mut key, _) = self.node(&self.root)?;
        for (i, name) in path.iter().enumerate() {
            let (_, node) = self.node(key)?;
            key = match node.inputs.get(name) {
                Some(edge) => self.target_depth(edge, depth + 1)?,
                None => {
                    return eval_err(format!("input '{}' does not exist", path[..=i].join("/")));
                }
            };
        }
        Ok(key)
    }

    /// Whether this already locks `inputs`, the inputs of its root as
    /// `flake.nix` declares them, so that evaluating needs no change to it.
    /// An input whose reference is unchanged keeps its locked inputs, so
    /// only overrides of them are compared.
    pub fn locks(&self, inputs: &BTreeMap<String, InputSpec>) -> bool {
        self.node(&self.root).is_ok_and(|(_, root)| {
            inputs.len() == root.inputs.len()
                && inputs
                    .iter()
                    .all(|(name, spec)| match root.inputs.get(name) {
                        Some(edge) => self.locks_input(spec, edge),
                        None => false,
                    })
        })
    }

    fn locks_input(&self, spec: &InputSpec, edge: &Edge) -> bool {
        match (&spec.follows, edge) {
            (Some(want), Edge::Follows(path)) => want == path,
            (None, Edge::Node(key)) => {
                let Ok((_, node)) = self.node(key) else {
                    return false;
                };
                let same = spec
                    .attrs
                    .as_ref()
                    .is_none_or(|a| node.original.as_ref() == Some(a));
                // Overrides of inputs the node does not have change nothing.
                same && spec
                    .overrides
                    .iter()
                    .all(|(name, s)| node.inputs.get(name).is_none_or(|e| self.locks_input(s, e)))
            }
            _ => false,
        }
    }
}

fn get<'a>(ev: &Eval<'a>, a: &crate::value::Attrs<'a>, name: &str) -> Option<Value<'a>> {
    a.get(ev.sym(name))
}

fn string<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a, String> {
    Ok(String::from_utf8_lossy(ev.force_str_no_ctx(v)?).into_owned())
}

fn path<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a, Vec<String>> {
    ev.force_list(v)?.iter().map(|&s| string(ev, s)).collect()
}

/// An input's attributes, from a lock file.
fn attrs_from_value<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a, Attrs> {
    let mut attrs = Attrs::new();
    for e in ev.force_attrs(v)?.entries {
        let v = ev.force(e.value)?;
        let Some(attr) = Attr::from_value(v) else {
            return eval_err(format!(
                "unsupported input attribute type in lock file: {}",
                v.show_type()
            ));
        };
        attrs.insert(ev.name(e.name).to_owned(), attr);
    }
    Ok(attrs)
}

/// The lock file whose `fromJSON` value is `v`.
pub fn from_value<'a>(ev: &Eval<'a>, v: Value<'a>) -> R<'a, Lock> {
    let top = ev.force_attrs(v)?;
    let version = match get(ev, top, "version") {
        Some(v) => ev.force_int(v)?,
        None => return eval_err("lock file has no version"),
    };
    if !(5..=7).contains(&version) {
        return eval_err(format!("lock file has unsupported version {version}"));
    }
    let root = match get(ev, top, "root") {
        Some(r) => string(ev, r)?,
        None => return eval_err("lock file has no root node"),
    };
    let Some(nodes_v) = get(ev, top, "nodes") else {
        return eval_err("lock file has no nodes");
    };
    let mut nodes = HashMap::new();
    for e in ev.force_attrs(nodes_v)?.entries {
        let n = ev.force_attrs(e.value)?;
        let mut inputs = BTreeMap::new();
        if let Some(i) = get(ev, n, "inputs") {
            for input in ev.force_attrs(i)?.entries {
                let edge = match ev.force(input.value)? {
                    Value::List(_) => Edge::Follows(path(ev, input.value)?),
                    _ => Edge::Node(string(ev, input.value)?),
                };
                inputs.insert(ev.name(input.name).to_owned(), edge);
            }
        }
        let locked = get(ev, n, "locked")
            .map(|v| attrs_from_value(ev, v))
            .transpose()?;
        let original = get(ev, n, "original")
            .map(|v| attrs_from_value(ev, v))
            .transpose()?;
        let flake = match get(ev, n, "flake") {
            Some(f) => ev.force_bool(f)?,
            None => true,
        };
        let parent = get(ev, n, "parent").map(|p| path(ev, p)).transpose()?;
        nodes.insert(
            ev.name(e.name).to_owned(),
            Node {
                inputs,
                locked,
                original,
                flake,
                parent,
            },
        );
    }
    if !nodes.contains_key(&root) {
        return eval_err(format!("lock file references missing node '{root}'"));
    }
    Ok(Lock { root, nodes })
}
