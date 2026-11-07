//! Mutations of a flake bundle's structure rather than of its Nix source:
//! its trees and their kinds, its files' flags, whether it is locked, its
//! flake reference, its lock file, and the inputs and outputs its flake
//! declares. Every output is a well-formed bundle whose trees can be
//! written out.

use rnix::SyntaxKind::*;

use super::Rng;
use super::run_with_stack;
use crate::bundle;
use crate::bundle::Kind;
use crate::bundle::State;

struct Bundle {
    reference: String,
    lock: bool,
    trees: Vec<Tree>,
}

struct Tree {
    name: String,
    kind: Kind,
    files: Vec<File>,
}

struct File {
    path: String,
    executable: bool,
    symlink: bool,
    state: State,
    contents: String,
}

impl Bundle {
    fn new(b: &bundle::Bundle<'_>) -> Bundle {
        Bundle {
            reference: b.reference.to_owned(),
            lock: b.lock,
            trees: b
                .trees
                .iter()
                .map(|t| Tree {
                    name: t.name.to_owned(),
                    kind: t.kind,
                    files: t
                        .files
                        .iter()
                        .map(|f| File {
                            path: f.path.to_owned(),
                            executable: f.executable,
                            symlink: f.symlink,
                            state: f.state,
                            contents: f.contents.to_owned(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// The bundle as text, which [`bundle::parse`] reads back as it is.
    fn render(&self) -> String {
        let mut out = format!("{}{}\n", bundle::MAGIC, self.reference);
        if self.lock {
            out.push_str("#~lock\n");
        }
        for tree in &self.trees {
            let kind = match tree.kind {
                Kind::Path => "path",
                Kind::Git => "git",
                Kind::Store => "store",
            };
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("#~tree {} {kind}\n", tree.name));
            for file in &tree.files {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("#~file ");
                out.push_str(&file.path);
                for (set, flag) in [
                    (file.executable, " x"),
                    (file.symlink, " link"),
                    (file.state == State::Staged, " staged"),
                    (file.state == State::Unstaged, " unstaged"),
                ] {
                    if set {
                        out.push_str(flag);
                    }
                }
                out.push('\n');
                out.push_str(&file.contents);
            }
        }
        out
    }

    /// The tree the reference names, else the first.
    fn root(&self) -> usize {
        self.trees
            .iter()
            .position(|t| self.reference.contains(&format!("@dir({})", t.name)))
            .unwrap_or(0)
    }

    /// The names of the trees but the root, and of an input none of them
    /// provides.
    fn input_names(&self) -> Vec<String> {
        let root = self.root();
        let mut names: Vec<String> = self
            .trees
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != root)
            .map(|(_, t)| t.name.clone())
            .collect();
        names.push("nixpkgs".to_owned());
        names
    }

    /// The directory within the root of the flake the reference names: its
    /// path within the root, then its `dir` parameter. The root's top level
    /// when the reference does not say, or names a directory that cannot
    /// hold a flake.
    fn flake_dir(&self) -> String {
        let root = &self.trees[self.root()];
        let flake = self
            .reference
            .split_once('#')
            .map_or(&*self.reference, |(f, _)| f);
        let (path, query) = flake.split_once('?').unwrap_or((flake, ""));
        let path = match path.split_once(&format!("@dir({})", root.name)) {
            Some((_, p)) => p,
            None => {
                let p = path.rsplit_once(':').map_or(path, |(_, p)| p);
                let p = p.strip_prefix("//").unwrap_or(p);
                if p.starts_with('/') { "" } else { p }
            }
        };
        let param = query
            .split('&')
            .find_map(|p| p.strip_prefix("dir="))
            .unwrap_or("");
        let segments: Vec<&str> = path
            .split('/')
            .chain(param.split('/'))
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        let dir = segments.join("/");
        if segments.contains(&"..") || clashes(root, &within(&dir, "flake.nix")) {
            return String::new();
        }
        dir
    }

    /// The `flake.nix` of the flake the reference names, made a regular
    /// file holding a flake when it is missing or a symlink.
    fn flake_nix(&mut self) -> &mut String {
        let path = within(&self.flake_dir(), "flake.nix");
        let root = self.root();
        let files = &mut self.trees[root].files;
        let at = match files.iter().rposition(|f| f.path == path) {
            Some(at) => at,
            None => {
                files.push(file(&path, "{ outputs = _: { }; }\n"));
                files.len() - 1
            }
        };
        let file = &mut files[at];
        if file.symlink {
            file.symlink = false;
            file.contents = "{ outputs = _: { }; }\n".to_owned();
        }
        &mut file.contents
    }

    /// Set the file `path` beside the reference's `flake.nix` to
    /// `contents`, committed.
    fn set_flake_file(&mut self, path: &str, contents: String) {
        let path = within(&self.flake_dir(), path);
        let root = self.root();
        let files = &mut self.trees[root].files;
        files.retain(|f| f.path != path);
        files.push(file(&path, &contents));
    }
}

/// `path` within directory `dir` of a tree, `""` being its top level.
fn within(dir: &str, path: &str) -> String {
    if dir.is_empty() {
        path.to_owned()
    } else {
        format!("{dir}/{path}")
    }
}

fn file(path: &str, contents: &str) -> File {
    File {
        path: path.to_owned(),
        executable: false,
        symlink: false,
        state: State::Committed,
        contents: contents.to_owned(),
    }
}

fn pick<'a, T>(rng: &mut Rng, xs: &'a [T]) -> &'a T {
    &xs[rng.below(xs.len())]
}

/// Whether a file at `path` in `tree` would clash with one already there,
/// as a directory where a file is or a file where a directory is.
fn clashes(tree: &Tree, path: &str) -> bool {
    tree.files.iter().any(|f| {
        let (a, b) = (f.path.as_str(), path);
        a.strip_prefix(b).is_some_and(|r| r.starts_with('/'))
            || b.strip_prefix(a).is_some_and(|r| r.starts_with('/'))
    })
}

/// A name no tree has and the reference's flake has no file or directory
/// of.
fn fresh_name(b: &Bundle, rng: &mut Rng) -> String {
    let base = pick(rng, &["dep", "a", "b", "sub", "nf", "in"]);
    let root = &b.trees[b.root()];
    let dir = b.flake_dir();
    (0..)
        .map(|n| match n {
            0 => (*base).to_owned(),
            n => format!("{base}{n}"),
        })
        .find(|name| {
            b.trees.iter().all(|t| &t.name != name)
                && !clashes(root, &within(&dir, &format!("{name}/flake.nix")))
                && root.files.iter().all(|f| f.path != within(&dir, name))
        })
        .expect("some name is free")
}

/// `nix` with `binding` added to its outermost attribute set, after
/// removing that set's bindings of `replace`; a set of `binding` and empty
/// outputs when it has none.
fn with_binding(nix: &str, binding: &str, replace: Option<&str>) -> String {
    let set = run_with_stack(|| {
        let root = rnix::Root::parse(nix);
        let set = root
            .syntax()
            .descendants()
            .find(|n| n.kind() == NODE_ATTR_SET)?;
        let open = set
            .children_with_tokens()
            .find(|e| e.kind() == TOKEN_L_BRACE)?
            .text_range()
            .end();
        let removed: Vec<_> = set
            .children()
            .filter(|n| n.kind() == NODE_ATTRPATH_VALUE)
            .filter(|n| {
                n.children()
                    .find(|c| c.kind() == NODE_ATTRPATH)
                    .is_some_and(|p| Some(p.text().to_string().as_str()) == replace)
            })
            .map(|n| n.text_range())
            .collect();
        Some((usize::from(open), removed))
    });
    let Some((open, removed)) = set else {
        return format!("{{ {binding} outputs = _: {{ }}; }}\n");
    };
    let mut out = nix.to_owned();
    for range in removed.iter().rev() {
        out.replace_range(usize::from(range.start())..usize::from(range.end()), "");
    }
    out.insert_str(open, &format!(" {binding}"));
    out
}

/// Flakes, or with no `flake.nix` other trees, for an added input.
const INPUT_FILES: &[(&str, &str)] = &[
    ("flake.nix", "{ outputs = _: { v = 1; }; }\n"),
    (
        "flake.nix",
        "{ outputs = { self }: { v = self.outPath; }; }\n",
    ),
    (
        "flake.nix",
        "{ outputs = { self, ... }@inputs: { v = builtins.attrNames inputs; }; }\n",
    ),
    (
        "flake.nix",
        "{ inputs.x.url = \"path:../nf\"; outputs = { self, x }: { v = x; }; }\n",
    ),
    ("default.nix", "{ v = 1; }\n"),
];

/// Add an input to the reference's flake, with a tree or a directory beside
/// the flake to provide it, locking the bundle half the time.
fn add_input(b: &mut Bundle, rng: &mut Rng) {
    let name = fresh_name(b, rng);
    let dir = b.flake_dir();
    let (path, contents) = *pick(rng, INPUT_FILES);
    let how = rng.below(4);
    let url = match how {
        0 => format!("path:@dir({name})"),
        1 => format!("git+file://@dir({name})"),
        2 => {
            let up = "../".repeat(dir.split_terminator('/').count() + 1);
            format!("path:{up}{name}")
        }
        _ => format!("path:./{name}"),
    };
    if how == 3 {
        let root = b.root();
        b.trees[root]
            .files
            .push(file(&within(&dir, &format!("{name}/{path}")), contents));
    } else {
        let kind = match how {
            1 => Kind::Git,
            _ => *pick(rng, &[Kind::Path, Kind::Git, Kind::Store]),
        };
        b.trees.push(Tree {
            name: name.clone(),
            kind,
            files: vec![file(path, contents)],
        });
    }
    let flake = if path == "flake.nix" {
        ""
    } else {
        " flake = false;"
    };
    let nix = b.flake_nix();
    *nix = with_binding(
        nix,
        &format!("inputs.{name} = {{ url = \"{url}\";{flake} }};"),
        None,
    );
    if rng.below(2) == 0 {
        b.lock = true;
    }
}

/// Give a tree another kind, committing all of its files when it is no
/// longer a Git tree.
fn change_kind(b: &mut Bundle, rng: &mut Rng) {
    let at = rng.below(b.trees.len());
    let tree = &mut b.trees[at];
    let others: Vec<Kind> = [Kind::Path, Kind::Git, Kind::Store]
        .into_iter()
        .filter(|&k| k != tree.kind)
        .collect();
    tree.kind = *pick(rng, &others);
    if tree.kind != Kind::Git {
        for f in &mut tree.files {
            f.state = State::Committed;
        }
    }
}

/// Targets of a symlink, with `{t}` standing for a tree's name and `{self}`
/// for the link's own name.
const LINK_TARGETS: &[&str] = &[
    "flake.nix",
    "default.nix",
    ".",
    "..",
    "../{t}",
    "../{t}/flake.nix",
    "@dir({t})",
    "@dir({t})/flake.nix",
    "/nix/store",
    "missing",
    "{self}",
];

/// Toggle a flag of a file, or add a file when there is none.
fn toggle_flag(b: &mut Bundle, rng: &mut Rng) {
    let files: Vec<(usize, usize)> = b
        .trees
        .iter()
        .enumerate()
        .flat_map(|(t, tree)| (0..tree.files.len()).map(move |f| (t, f)))
        .collect();
    if files.is_empty() {
        let root = b.root();
        b.trees[root].files.push(file("f", "f\n"));
        return;
    }
    let (t, f) = *pick(rng, &files);
    let target = pick(rng, LINK_TARGETS).replace("{t}", &pick(rng, &b.trees).name);
    let git = b.trees[t].kind == Kind::Git;
    let file = &mut b.trees[t].files[f];
    match rng.below(if git { 4 } else { 2 }) {
        0 => file.executable = !file.executable,
        1 => {
            file.symlink = !file.symlink;
            if file.symlink {
                let name = file.path.rsplit('/').next().expect("split is never empty");
                file.contents = format!("{}\n", target.replace("{self}", name));
            }
        }
        flag => {
            let state = if flag == 2 {
                State::Staged
            } else {
                State::Unstaged
            };
            file.state = if file.state == state {
                State::Committed
            } else {
                state
            };
        }
    }
}

const SCHEMES: &[&str] = &[
    "path:",
    "path:",
    "git+file://",
    "git+file://",
    "",
    "git+file:",
    "file://",
    "tarball+file://",
];

/// Parameters of a reference, with `{d}` standing for a directory of a
/// tree's and `{t}` for a tree's name.
const PARAMS: &[&str] = &[
    "dir={d}",
    "dir=",
    "ref=main",
    "ref=refs/heads/main",
    "rev=0123456789abcdef0123456789abcdef01234567",
    "narHash=sha256-pQpattmS9VmO3ZIQUFn66az8GSmB4IvYhTTCFn6SUmo=",
    "narHash=@narHash({t})",
    "lastModified=1700000000",
    "shallow=1",
    "submodules=1",
    "allRefs=1",
    "revCount=1",
];

/// Segments of an attribute path, plain, quoted or percent-encoded.
const SEGMENTS: &[&str] = &[
    "x",
    "x",
    "v",
    "default",
    "packages",
    "\"x\"",
    "\"a.b\"",
    "%22a.b%22",
    "%78",
    "a%20b",
    "\"\"",
    "%",
];

/// A new reference: a scheme, a tree's directory or the current one and
/// perhaps a directory within it, parameters, and an attribute path.
fn reference(b: &Bundle, rng: &mut Rng) -> String {
    let tree = pick(rng, &b.trees);
    let dirs: Vec<&str> = tree
        .files
        .iter()
        .filter_map(|f| f.path.rsplit_once('/').map(|(d, _)| d))
        .chain(["sub"])
        .collect();
    let dir = pick(rng, &dirs);
    let mut out = (*pick(rng, SCHEMES)).to_owned();
    if rng.below(4) == 0 {
        out.push('.');
    } else {
        out.push_str(&format!("@dir({})", tree.name));
    }
    if rng.below(4) == 0 {
        out.push_str(&format!("/{dir}"));
    }
    for i in 0..rng.below(3) {
        let t = &pick(rng, &b.trees).name;
        out.push(if i == 0 { '?' } else { '&' });
        out.push_str(&pick(rng, PARAMS).replace("{d}", dir).replace("{t}", t));
    }
    if rng.below(5) != 0 {
        let segments: Vec<&str> = (0..=rng.below(3)).map(|_| *pick(rng, SEGMENTS)).collect();
        out.push('#');
        out.push_str(&segments.join("."));
    }
    out
}

/// A NAR hash for a lock file: of a store tree, or of an empty directory,
/// or of nothing in the store.
fn nar_hash(b: &Bundle, rng: &mut Rng) -> String {
    let stored: Vec<&Tree> = b.trees.iter().filter(|t| t.kind == Kind::Store).collect();
    match rng.below(4) {
        0 => "sha256-pQpattmS9VmO3ZIQUFn66az8GSmB4IvYhTTCFn6SUmo=".to_owned(),
        1 => "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
        _ if stored.is_empty() => "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
        _ => format!("@narHash({})", pick(rng, &stored).name),
    }
}

const REV: &str = "0123456789abcdef0123456789abcdef01234567";

/// A lock file node of input `name`, of a type picked at random.
fn lock_node(b: &Bundle, name: &str, rng: &mut Rng) -> String {
    let tree = b
        .trees
        .iter()
        .find(|t| t.name == name)
        .map_or("missing", |t| &t.name);
    let hash = nar_hash(b, rng);
    let time = "\"lastModified\": 1700000000";
    let (locked, original) = match rng.below(6) {
        0 => (
            format!(
                "\"type\": \"path\", \"path\": \"@dir({tree})\", {time}, \"narHash\": \"{hash}\""
            ),
            format!("\"type\": \"path\", \"path\": \"@dir({tree})\""),
        ),
        1 => (
            format!(
                "\"type\": \"git\", \"url\": \"file://@dir({tree})\", \"ref\": \"main\", \"rev\": \"{REV}\", \"revCount\": 1, {time}, \"narHash\": \"{hash}\""
            ),
            format!("\"type\": \"git\", \"url\": \"file://@dir({tree})\""),
        ),
        2 => (
            format!(
                "\"type\": \"github\", \"owner\": \"o\", \"repo\": \"{name}\", \"rev\": \"{REV}\", {time}, \"narHash\": \"{hash}\""
            ),
            format!("\"type\": \"github\", \"owner\": \"o\", \"repo\": \"{name}\""),
        ),
        3 => (
            format!(
                "\"type\": \"tarball\", \"url\": \"https://example.com/{name}.tar.gz\", {time}, \"narHash\": \"{hash}\""
            ),
            format!("\"type\": \"tarball\", \"url\": \"https://example.com/{name}.tar.gz\""),
        ),
        4 => (
            format!(
                "\"type\": \"file\", \"url\": \"https://example.com/{name}\", \"narHash\": \"{hash}\""
            ),
            format!("\"type\": \"file\", \"url\": \"https://example.com/{name}\""),
        ),
        _ => (
            "\"type\": \"indirect\", \"id\": \"nixpkgs\"".to_owned(),
            "\"type\": \"indirect\", \"id\": \"nixpkgs\"".to_owned(),
        ),
    };
    let flake = if rng.below(4) == 0 {
        "\"flake\": false, "
    } else {
        ""
    };
    format!("\"{name}\": {{ {flake}\"locked\": {{ {locked} }}, \"original\": {{ {original} }} }}")
}

/// How a lock file's node refers to its input `name`, one of `names`: by
/// node, or following a path that may lead to another input, through one,
/// to none, to the root, or back to itself.
fn lock_input(name: &str, names: &[String], rng: &mut Rng) -> String {
    let other = pick(rng, names);
    let target = match rng.below(6) {
        0 | 1 => return format!("\"{name}\": \"{name}\""),
        2 => format!("[ \"{other}\" ]"),
        3 => format!("[ \"{other}\", \"{name}\" ]"),
        4 => "[ \"missing\" ]".to_owned(),
        _ => format!("[ \"{name}\" ]"),
    };
    format!("\"{name}\": {target}")
}

/// Write a lock file for the reference's flake, with a node of each of a
/// few inputs, adding a store tree for its NAR hashes half the time there
/// is none.
fn lock_file(b: &mut Bundle, rng: &mut Rng) {
    if rng.below(2) == 0 && !b.trees.iter().any(|t| t.kind == Kind::Store) {
        let name = fresh_name(b, rng);
        b.trees.push(Tree {
            name,
            kind: Kind::Store,
            files: vec![file("flake.nix", "{ outputs = { self }: { v = 1; }; }\n")],
        });
    }
    let all = b.input_names();
    let names: Vec<String> = (0..=rng.below(all.len().min(3)))
        .map(|_| pick(rng, &all).clone())
        .collect();
    let mut names = names;
    names.sort();
    names.dedup();
    let mut nodes: Vec<String> = names.iter().map(|n| lock_node(b, n, rng)).collect();
    if rng.below(3) == 0 {
        let at = rng.below(nodes.len());
        let inputs = lock_input(pick(rng, &names), &names, rng);
        let node = &mut nodes[at];
        node.truncate(node.len() - 1);
        node.push_str(&format!(", \"inputs\": {{ {inputs} }} }}"));
    }
    let inputs: Vec<String> = names.iter().map(|n| lock_input(n, &names, rng)).collect();
    nodes.push(format!(
        "\"root\": {{ \"inputs\": {{ {} }} }}",
        inputs.join(", ")
    ));
    let version = pick(rng, &[7, 7, 7, 5, 6, 8]);
    let lock = format!(
        "{{\n  \"nodes\": {{\n    {}\n  }},\n  \"root\": \"root\",\n  \"version\": {version}\n}}\n",
        nodes.join(",\n    ")
    );
    b.set_flake_file("flake.lock", lock);
}

/// Bindings of a flake's attributes, with `{x}`, `{y}` and `{z}` standing
/// for names of inputs.
const FRAGMENTS: &[&str] = &[
    "inputs.{x}.flake = false;",
    "inputs.{x}.follows = \"{y}/{z}\";",
    "inputs.{x}.follows = \"{y}\";",
    "inputs.{x}.follows = \"\";",
    "inputs.{x}.inputs.{y}.follows = \"\";",
    "inputs.{x}.inputs.{y}.follows = \"{z}\";",
    "inputs.self.submodules = true;",
    "inputs.self.flake = false;",
    "nixConfig = { bash-prompt = \"x\"; };",
    "nixConfig.allow-import-from-derivation = false;",
    "nixConfig.extra-substituters = [ \"https://example.com\" ];",
    "description = \"d\";",
];

/// What an output may be, with `{x}` standing for the name of an input.
const OUTPUTS: &[&str] = &[
    "self.rev",
    "self.dirtyRev",
    "self.shortRev",
    "self.dirtyShortRev",
    "self.lastModified",
    "self.lastModifiedDate",
    "self.revCount",
    "self.narHash",
    "self.sourceInfo",
    "self.outPath",
    "self.submodules",
    "self ? rev",
    "toString self",
    "removeAttrs self [ \"outputs\" ]",
    "builtins.attrNames inputs",
    "removeAttrs inputs.{x} [ \"outputs\" ]",
    "inputs.{x}.sourceInfo or null",
];

/// Add one of [`FRAGMENTS`] to the reference's flake, or replace its outputs
/// with one of [`OUTPUTS`], named as the reference's attribute path starts.
fn flake_fragment(b: &mut Bundle, rng: &mut Rng) {
    let names = b.input_names();
    let mut name = || pick(rng, &names).clone();
    let (x, y, z) = (name(), name(), name());
    let fill = |s: &str| s.replace("{x}", &x).replace("{y}", &y).replace("{z}", &z);
    let (binding, replace) = if rng.below(2) == 0 {
        (fill(pick(rng, FRAGMENTS)), None)
    } else {
        let attr = b
            .reference
            .split_once('#')
            .map(|(_, a)| a.split('.').next().unwrap_or(""))
            .filter(|a| {
                a.starts_with(|c: char| c.is_ascii_alphabetic())
                    && a.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .unwrap_or("x")
            .to_owned();
        let output = fill(pick(rng, OUTPUTS));
        (
            format!("outputs = {{ self, ... }}@inputs: {{ {attr} = {output}; }};"),
            Some("outputs"),
        )
    };
    let nix = b.flake_nix();
    *nix = with_binding(nix, &binding, replace);
}

/// Mutate the structure of `bundle`, keeping it well formed, in one of the
/// ways above.
pub(super) fn mutate_bundle(bundle: &bundle::Bundle<'_>, rng: &mut Rng) -> String {
    let mut b = Bundle::new(bundle);
    match rng.below(7) {
        0 => add_input(&mut b, rng),
        1 => change_kind(&mut b, rng),
        2 => b.lock = !b.lock,
        3 => toggle_flag(&mut b, rng),
        4 => b.reference = reference(&b, rng),
        5 => lock_file(&mut b, rng),
        _ => flake_fragment(&mut b, rng),
    }
    b.render()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn seeds() -> Vec<String> {
        let dir =
            PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
                .join("flakes");
        let mut seeds: Vec<String> = std::fs::read_dir(&dir)
            .expect("read flakes")
            .map(|e| std::fs::read_to_string(e.expect("read flakes").path()).expect("read bundle"))
            .collect();
        seeds.sort();
        seeds
    }

    /// Whether `b`'s trees can be written out: no file where another needs
    /// a directory, and every symlink with a target.
    fn writable(b: &bundle::Bundle<'_>) -> bool {
        b.trees.iter().all(|t| {
            t.files.iter().all(|f| {
                let others = t.files.iter().filter(|g| g.path != f.path);
                let target = f.contents.strip_suffix('\n').unwrap_or(f.contents);
                others
                    .map(|g| g.path)
                    .all(|g| !g.starts_with(&format!("{}/", f.path)))
                    && (!f.symlink || !target.is_empty())
            })
        })
    }

    #[test]
    fn renders_what_it_parses() {
        for seed in seeds() {
            let b = Bundle::new(&bundle::parse(&seed).unwrap());
            let rendered = b.render();
            let again = Bundle::new(&bundle::parse(&rendered).unwrap()).render();
            assert_eq!(rendered, again);
        }
    }

    #[test]
    fn mutated_bundles_parse() {
        let mut outs = Vec::new();
        for seed in seeds() {
            let mut text = seed;
            for step in 0..15u32 {
                let b = bundle::parse(&text).unwrap();
                let mut rng = Rng::new(step.wrapping_mul(0x9e37_79b9) ^ text.len() as u32);
                let out = mutate_bundle(&b, &mut rng);
                let parsed = bundle::parse(&out).unwrap_or_else(|| panic!("{text}\n->\n{out}"));
                assert!(writable(&parsed), "{text}\n->\n{out}");
                drop(parsed);
                outs.push(out.clone());
                // Long chains only grow; start over from the seed's mutations.
                if out.len() < 4096 {
                    text = out;
                }
            }
        }
        for wanted in [
            "path:./",
            "path:../",
            "git+file://@dir(",
            "#~lock",
            " store\n",
            " x\n",
            " link\n",
            " staged\n",
            " unstaged\n",
            "?dir=",
            "?ref=",
            "?rev=",
            "?narHash=",
            "lastModified=",
            "shallow=1",
            "%22a.b%22",
            "\"type\": \"path\"",
            "\"type\": \"git\"",
            "\"type\": \"github\"",
            "\"type\": \"tarball\"",
            "\"type\": \"file\"",
            "\"type\": \"indirect\"",
            "\"narHash\": \"@narHash(",
            "[ \"missing\" ]",
            "flake = false;",
            ".follows = \"\";",
            "nixConfig",
            "self.dirtyRev",
            "self.lastModifiedDate",
            "self.submodules",
        ] {
            assert!(outs.iter().any(|o| o.contains(wanted)), "never {wanted:?}");
        }
    }

    #[test]
    fn edits_the_flake_the_reference_names() {
        for (reference, path) in [
            ("@dir(root)/sub#x", "sub/flake.nix"),
            ("@dir(root)?dir=a#x", "a/flake.nix"),
            ("@dir(root)/a?dir=b#x", "a/b/flake.nix"),
        ] {
            let seed = format!(
                "#~flake {reference}\n#~tree root git\n#~file flake.nix\n{{ outputs = _: {{ }}; }}\n#~file {path}\n{{ outputs = _: {{ x = 1; }}; }}\n"
            );
            for mutation in [add_input, flake_fragment] {
                for step in 0..8u32 {
                    let parsed = bundle::parse(&seed).unwrap();
                    let mut b = Bundle::new(&parsed);
                    mutation(&mut b, &mut Rng::new(step));
                    let out = b.render();
                    let again = bundle::parse(&out).unwrap_or_else(|| panic!("{out}"));
                    let file = |b: &bundle::Bundle<'_>, path: &str| {
                        b.trees[0]
                            .files
                            .iter()
                            .find(|f| f.path == path)
                            .map(|f| f.contents.to_owned())
                    };
                    assert_ne!(file(&again, path), file(&parsed, path), "{out}");
                    assert_eq!(file(&again, "flake.nix"), file(&parsed, "flake.nix"));
                }
            }
        }
    }

    #[test]
    fn adds_store_trees_beside_the_root() {
        for reference in [".#x", "path:.#x", "path:@dir(root)#x"] {
            let seed = format!(
                "#~flake {reference}\n#~tree root path\n#~file flake.nix\n{{ outputs = _: {{ }}; }}\n"
            );
            let added = (0..32u32).find_map(|step| {
                let mut b = Bundle::new(&bundle::parse(&seed).unwrap());
                lock_file(&mut b, &mut Rng::new(step));
                let store = &b.trees.last()?.name;
                let hashed = b.render().contains(&format!("@narHash({store})"));
                (b.trees.len() > 1 && hashed).then_some(b)
            });
            let b = added.unwrap_or_else(|| panic!("never hashed a store tree for {reference}"));
            assert_eq!(b.trees[0].name, "root", "{}", b.render());
            assert_eq!(b.root(), 0, "{}", b.render());
            assert!(
                b.trees[0].files.iter().any(|f| f.path == "flake.lock"),
                "{}",
                b.render()
            );
        }
    }

    #[test]
    fn adds_bindings_to_the_outer_set() {
        assert_eq!(
            with_binding("{ outputs = _: { }; }", "a = 1;", None),
            "{ a = 1; outputs = _: { }; }"
        );
        assert_eq!(
            with_binding(
                "{ x = 2; outputs = _: { }; }",
                "outputs = 1;",
                Some("outputs")
            ),
            "{ outputs = 1; x = 2;  }"
        );
        assert_eq!(
            with_binding("1", "a = 1;", None),
            "{ a = 1; outputs = _: { }; }\n"
        );
    }
}
