//! Flake bundles: a flake, its inputs and its lock file as one text, so
//! that libFuzzer can treat them as a single input.
//!
//! A bundle is a first line `#~flake REF`, naming the flake reference to
//! evaluate, followed by directives, each a line of its own:
//!
//! - `#~lock`: lock the flake with `nix flake lock` before evaluating it.
//! - `#~tree NAME KIND`: start a source tree, a directory of files, where
//!   `KIND` is `path` (a plain directory), `git` (a Git repository) or
//!   `store` (a directory added to the store, standing in for an input
//!   fetched from elsewhere, such as GitHub).
//! - `#~file PATH FLAG...`: start a file of the current tree, holding every
//!   line up to the next directive. Its flags are `x` (executable), `link`
//!   (a symlink to its contents, without a final newline), and in a Git
//!   tree `staged` (added to the index after the commit) or `unstaged`
//!   (written after the commit and not added, so untracked or modified).
//!
//! In the reference and every file, `@dir(NAME)` stands for the absolute
//! path of tree `NAME`, and `@narHash(NAME)` for the NAR hash of store
//! tree `NAME`, so that a lock file can refer to them. Store trees are
//! written first, in order, so in a store tree it stands only for a store
//! tree declared before it.

use std::collections::HashSet;

/// The first line of every bundle starts with this.
pub const MAGIC: &str = "#~flake ";

pub struct Bundle<'s> {
    pub reference: &'s str,
    pub lock: bool,
    pub trees: Vec<Tree<'s>>,
}

pub struct Tree<'s> {
    pub name: &'s str,
    pub kind: Kind,
    pub files: Vec<File<'s>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Path,
    Git,
    Store,
}

pub struct File<'s> {
    pub path: &'s str,
    pub executable: bool,
    pub symlink: bool,
    pub state: State,
    pub contents: &'s str,
}

/// When a file of a Git tree is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Committed,
    Staged,
    Unstaged,
}

/// Whether `src` is a bundle rather than a Nix expression.
pub fn is_bundle(src: &str) -> bool {
    src.starts_with(MAGIC)
}

/// Every line of `src` with its byte offset.
fn lines(src: &str) -> impl Iterator<Item = (usize, &str)> {
    src.split_inclusive('\n').scan(0, |at, line| {
        let start = *at;
        *at += line.len();
        Some((start, line))
    })
}

/// The bundle `src` holds, or `None` if it is not a well-formed one.
pub fn parse(src: &str) -> Option<Bundle<'_>> {
    let rest = src.strip_prefix(MAGIC)?;
    let (reference, _) = rest.split_once('\n').unwrap_or((rest, ""));
    let mut bundle = Bundle {
        reference,
        lock: false,
        trees: Vec::new(),
    };
    // The file being read, and where its contents start.
    let mut open: Option<(File<'_>, usize)> = None;
    let mut names = HashSet::new();
    for (start, line) in lines(src).skip(1) {
        let Some(directive) = line.strip_prefix("#~") else {
            continue;
        };
        if let Some((file, from)) = open.take() {
            bundle.trees.last_mut()?.files.push(File {
                contents: &src[from..start],
                ..file
            });
        }
        let mut words = directive.split_ascii_whitespace();
        match (words.next()?, words.next(), words.next()) {
            ("lock", None, _) => bundle.lock = true,
            ("tree", Some(name), Some(kind)) if words.next().is_none() => {
                let kind = match kind {
                    "path" => Kind::Path,
                    "git" => Kind::Git,
                    "store" => Kind::Store,
                    _ => return None,
                };
                if !valid_name(name) || !names.insert(name) {
                    return None;
                }
                bundle.trees.push(Tree {
                    name,
                    kind,
                    files: Vec::new(),
                });
            }
            ("file", Some(path), flag) => {
                let tree = bundle.trees.last()?;
                if !valid_path(path) {
                    return None;
                }
                let mut file = File {
                    path,
                    executable: false,
                    symlink: false,
                    state: State::Committed,
                    contents: "",
                };
                for flag in flag.into_iter().chain(words) {
                    match flag {
                        "x" => file.executable = true,
                        "link" => file.symlink = true,
                        "staged" if tree.kind == Kind::Git => file.state = State::Staged,
                        "unstaged" if tree.kind == Kind::Git => file.state = State::Unstaged,
                        _ => return None,
                    }
                }
                open = Some((file, start + line.len()));
            }
            _ => return None,
        }
    }
    if let Some((file, from)) = open {
        bundle.trees.last_mut()?.files.push(File {
            contents: &src[from..],
            ..file
        });
    }
    (!bundle.trees.is_empty()).then_some(bundle)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A relative path of plain names, none of them Git's own directory.
fn valid_path(path: &str) -> bool {
    path.len() <= 200
        && path
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".." && s != ".git")
}

/// The byte ranges of `src` that hold Nix source: the contents of every
/// `.nix` file of a bundle, but for the newline ending the line before the
/// next directive, or all of anything else.
pub fn nix_ranges(src: &str) -> Vec<std::ops::Range<usize>> {
    if !is_bundle(src) {
        return std::iter::once(0..src.len()).collect();
    }
    let mut ranges = Vec::new();
    let mut open: Option<usize> = None;
    for (start, line) in lines(src).skip(1) {
        if !line.starts_with("#~") {
            continue;
        }
        if let Some(from) = open.take() {
            ranges.push(from..(start - 1).max(from));
        }
        let mut words = line[2..].split_ascii_whitespace();
        if words.next() == Some("file") && words.next().is_some_and(|p| p.ends_with(".nix")) {
            open = Some(start + line.len());
        }
    }
    if let Some(from) = open {
        ranges.push(from..src.len());
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE: &str = "#~flake path:@dir(root)#x
#~lock
#~tree root git
#~file flake.nix
{ outputs = _: { x = 1; }; }
#~file a/b x unstaged
b
#~tree dep store
#~file l link
target";

    #[test]
    fn parses_trees_and_files() {
        let b = parse(BUNDLE).unwrap();
        assert_eq!(b.reference, "path:@dir(root)#x");
        assert!(b.lock);
        assert_eq!(b.trees.len(), 2);
        let root = &b.trees[0];
        assert_eq!((root.name, root.kind), ("root", Kind::Git));
        assert_eq!(root.files[0].path, "flake.nix");
        assert_eq!(root.files[0].contents, "{ outputs = _: { x = 1; }; }\n");
        assert!(root.files[1].executable);
        assert_eq!(root.files[1].state, State::Unstaged);
        assert_eq!(root.files[1].contents, "b\n");
        let dep = &b.trees[1];
        assert!(dep.files[0].symlink);
        assert_eq!(dep.files[0].contents, "target");
    }

    #[test]
    fn rejects_malformed_bundles() {
        for src in [
            "#~flake x\n#~file a\n",
            "#~flake x\n#~tree a path\n#~file ../a\n",
            "#~flake x\n#~tree a path\n#~file .git/config\n",
            "#~flake x\n#~tree a path\n#~tree a path\n",
            "#~flake x\n#~tree a/b path\n",
            "#~flake x\n#~tree a zip\n",
            "#~flake x\n#~tree a path\n#~file f staged\n",
            "#~flake x\n#~bogus\n",
            "#~flake x\n",
        ] {
            assert!(parse(src).is_none(), "{src:?}");
        }
    }

    #[test]
    fn finds_nix_source() {
        let ranges = nix_ranges(BUNDLE);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&BUNDLE[ranges[0].clone()], "{ outputs = _: { x = 1; }; }");
        assert_eq!(
            nix_ranges("1 + 1"),
            std::iter::once(0..5).collect::<Vec<_>>()
        );
    }
}
