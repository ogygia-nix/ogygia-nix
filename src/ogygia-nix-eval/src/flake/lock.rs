//! Lock files: the graph of a flake's inputs, each pinned to a source.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use crate::eval::Eval;
use crate::flake::flakeref;
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

impl Node {
    /// The root node of a lock file, with inputs `inputs`.
    pub fn root(inputs: BTreeMap<String, Edge>) -> Node {
        Node {
            inputs,
            locked: None,
            original: None,
            flake: true,
            parent: None,
        }
    }

    /// The keys of the nodes its inputs lead to without following another.
    fn children(&self) -> impl Iterator<Item = &str> {
        self.inputs.values().filter_map(|e| match e {
            Edge::Node(k) => Some(k.as_str()),
            Edge::Follows(_) => None,
        })
    }
}

pub struct Lock {
    pub root: String,
    pub nodes: HashMap<String, Node>,
}

impl Lock {
    /// The lock file of a flake with no inputs and no lock file.
    pub fn empty() -> Lock {
        Lock {
            root: "root".to_owned(),
            nodes: HashMap::from([("root".to_owned(), Node::root(BTreeMap::new()))]),
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

    /// Whether this locks the same inputs to the same sources as `other`,
    /// whatever the keys of their nodes: inputs that lead to one node in
    /// one must lead to one node in the other. The root's own source is not
    /// compared.
    pub fn same(&self, other: &Lock) -> bool {
        let mut ours = HashMap::new();
        let mut theirs = HashMap::new();
        let mut pending = vec![(self.root.as_str(), other.root.as_str())];
        while let Some((a, b)) = pending.pop() {
            match (ours.insert(a, b), theirs.insert(b, a)) {
                (None, None) => {}
                (Some(x), Some(y)) if x == b && y == a => continue,
                _ => return false,
            }
            let (Some(n), Some(m)) = (self.nodes.get(a), other.nodes.get(b)) else {
                return false;
            };
            let same_source = a == self.root
                || (n.locked == m.locked
                    && n.original == m.original
                    && n.flake == m.flake
                    && n.parent == m.parent);
            if !same_source || n.inputs.len() != m.inputs.len() {
                return false;
            }
            for (name, e) in &n.inputs {
                match (e, m.inputs.get(name)) {
                    (Edge::Follows(p), Some(Edge::Follows(q))) if p == q => {}
                    (Edge::Node(x), Some(Edge::Node(y))) => pending.push((x, y)),
                    _ => return false,
                }
            }
        }
        true
    }

    /// Whether some input is not locked to a fixed source, as a local Git
    /// repository's dirty working tree is not, so that Nix would not write
    /// this as a lock file.
    pub fn has_unlocked(&self) -> bool {
        self.nodes.values().any(|n| {
            n.locked.as_ref().is_some_and(|l| {
                flakeref::get_str(l, "type") == Some("git") && !l.contains_key("rev")
            })
        })
    }

    /// Fails as Nix does reading a lock file if an input has neither a NAR
    /// hash nor a revision fixing its source and is not a path relative to
    /// another.
    pub fn check_locked(&self) -> Result<(), Box<EvalError<'static>>> {
        let mut seen = HashSet::new();
        let mut pending = vec![self.root.as_str()];
        while let Some(key) = pending.pop() {
            if !seen.insert(key) {
                continue;
            }
            let (_, node) = self.node(key)?;
            if let Some(l) = &node.locked {
                let rev = l.contains_key("rev")
                    && matches!(
                        flakeref::get_str(l, "type"),
                        Some("git" | "github" | "gitlab" | "sourcehut")
                    );
                let relative = flakeref::relative_path(l).is_some();
                if !(l.contains_key("narHash") || rev || relative) {
                    return eval_err(format!(
                        "Lock file contains unlocked input '{}'. Use '--allow-dirty-locks' to accept this lock file.",
                        flakeref::to_string(l)
                    ));
                }
            }
            pending.extend(node.children());
        }
        Ok(())
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
    let nodes_v = ev.force_attrs(nodes_v)?;
    // Nix reads only the nodes reachable from the root, and of the root
    // only its inputs, ignoring a root that is not an object.
    let mut nodes = HashMap::new();
    let mut pending = vec![root.clone()];
    while let Some(key) = pending.pop() {
        if nodes.contains_key(&key) {
            continue;
        }
        let Some(v) = get(ev, nodes_v, &key) else {
            return eval_err(format!("lock file references missing node '{key}'"));
        };
        let node = if key == root {
            match ev.force(v)? {
                Value::Attrs(n) => Node::root(inputs(ev, n)?),
                _ => Node::root(BTreeMap::new()),
            }
        } else {
            node(ev, ev.force_attrs(v)?)?
        };
        pending.extend(node.children().map(str::to_owned));
        nodes.insert(key, node);
    }
    Ok(Lock { root, nodes })
}

fn inputs<'a>(ev: &Eval<'a>, n: &crate::value::Attrs<'a>) -> R<'a, BTreeMap<String, Edge>> {
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
    Ok(inputs)
}

fn node<'a>(ev: &Eval<'a>, n: &crate::value::Attrs<'a>) -> R<'a, Node> {
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
    Ok(Node {
        inputs: inputs(ev, n)?,
        locked,
        original,
        flake,
        parent,
    })
}
