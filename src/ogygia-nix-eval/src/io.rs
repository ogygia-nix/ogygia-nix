//! Filesystem and store access for the evaluator.
//!
//! Paths the evaluator sees are logical: a flake input is `/nix/store/…-source`
//! whether or not it has been realised there. [`Io`] reads store paths from
//! its store or its store directory, and other logical paths from the
//! filesystem.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use sha2::Digest;
use sha2::Sha256;

use crate::accessor::Accessor;
use crate::accessor::FileType;
use crate::accessor::SymlinkLoop;
use crate::flake::source::Dir;
use crate::nar;
use crate::path::join;
use crate::store;
use crate::store::MemoryStore;
use crate::store::Store;
use crate::store::StorePath;

// The directory holding the files Nix ships with itself, which `<nix/…>`
// paths resolve within, as `nix/…`. Set at build time; without it they do
// not exist.
const DEFAULT_INCLUDE_PATH: Option<&str> = option_env!("OGYGIA_NIX_EVAL_DEFAULT_INCLUDE_PATH");

/// A `builtins.path` filter: whether to keep a path of the given type.
pub type KeepFn<'f> = &'f dyn Fn(&str, FileType) -> Result<bool>;

/// `logical` relative to `root`, if it is `root` or inside it.
fn relative<'p>(logical: &'p str, root: &str) -> Option<&'p str> {
    if logical == root {
        return Some("");
    }
    logical
        .strip_prefix(root.trim_end_matches('/'))?
        .strip_prefix('/')
}

/// The filesystem, addressed by logical path.
struct Fs;

impl Fs {
    /// The physical path behind `logical`.
    fn physical(&self, logical: &str) -> std::io::Result<PathBuf> {
        if let Some(rest) = logical.strip_prefix(crate::path::COREPKGS) {
            let Some(include) = DEFAULT_INCLUDE_PATH else {
                return Err(ErrorKind::NotFound.into());
            };
            return Ok(Path::new(include)
                .join("nix")
                .join(rest.trim_start_matches('/')));
        }
        Ok(PathBuf::from(logical))
    }
}

impl Accessor for Fs {
    fn file_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        os_type(self.physical(path).and_then(std::fs::symlink_metadata))
    }

    fn resolved_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        os_type(self.physical(path).and_then(std::fs::metadata))
    }

    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.physical(path)?)
    }

    fn read_dir(&self, path: &str) -> std::io::Result<Vec<(String, FileType)>> {
        os_read_dir(&self.physical(path)?, |_| true)
    }

    fn read_link(&self, path: &str) -> std::io::Result<Vec<u8>> {
        os_read_link(&self.physical(path)?)
    }

    fn is_executable(&self, path: &str) -> std::io::Result<bool> {
        os_is_executable(&self.physical(path)?)
    }
}

/// The entries of the directory `path` whose names `keep` accepts, sorted
/// by name.
pub(crate) fn os_read_dir(
    path: &Path,
    keep: impl Fn(&OsStr) -> bool,
) -> std::io::Result<Vec<(String, FileType)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if keep(&name) {
            let ty = FileType::of(entry.file_type()?);
            out.push((name.to_string_lossy().into_owned(), ty));
        }
    }
    out.sort();
    Ok(out)
}

/// The target of the symlink `path`.
pub(crate) fn os_read_link(path: &Path) -> std::io::Result<Vec<u8>> {
    Ok(std::fs::read_link(path)?.into_os_string().into_vec())
}

/// Whether `path`, not followed if it is a symlink, is executable.
pub(crate) fn os_is_executable(path: &Path) -> std::io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(meta.permissions().mode() & 0o100 != 0)
}

/// The type of a file from its metadata: `None` if it does not exist, and
/// [`SymlinkLoop`] if it is reached through too many symlinks.
fn os_type(meta: std::io::Result<std::fs::Metadata>) -> std::io::Result<Option<FileType>> {
    match meta {
        Ok(m) => Ok(Some(FileType::of(m.file_type()))),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Err(SymlinkLoop::error()),
        Err(_) => Ok(None),
    }
}

/// The tree at `root` in `base`, holding only the entries in `kept`
/// (relative to `root`) if that is set.
struct Subtree {
    base: Rc<dyn Accessor>,
    root: String,
    kept: Option<HashSet<String>>,
}

impl Subtree {
    /// `path` in `base`, or `NotFound` if it is not kept.
    fn path(&self, path: &str) -> std::io::Result<String> {
        match &self.kept {
            Some(kept) if !path.is_empty() && !kept.contains(path) => {
                Err(ErrorKind::NotFound.into())
            }
            _ => Ok(join(&self.root, path)),
        }
    }
}

impl Accessor for Subtree {
    fn file_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        self.path(path)
            .map_or(Ok(None), |p| self.base.file_type(&p))
    }

    fn resolved_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        self.path(path)
            .map_or(Ok(None), |p| self.base.resolved_type(&p))
    }

    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.base.read_file(&self.path(path)?)
    }

    fn read_dir(&self, path: &str) -> std::io::Result<Vec<(String, FileType)>> {
        let mut entries = self.base.read_dir(&self.path(path)?)?;
        entries.retain(|(name, _)| self.path(&join(path, name)).is_ok());
        Ok(entries)
    }

    fn read_link(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.base.read_link(&self.path(path)?)
    }

    fn is_executable(&self, path: &str) -> std::io::Result<bool> {
        self.base.is_executable(&self.path(path)?)
    }

    fn real_path(&self, path: &str) -> std::io::Result<String> {
        let real = self.base.real_path(&self.path(path)?)?;
        if self.root.is_empty() {
            return Ok(real);
        }
        relative(&real, &self.root)
            .map(str::to_owned)
            .ok_or(ErrorKind::NotFound.into())
    }
}

pub struct Io {
    pub(crate) pure: bool,
    store: Box<dyn Store>,
    /// A directory of store objects that are valid as well as those added.
    store_dir: Option<PathBuf>,
    /// The objects of the store directory that have been read from it.
    stored: RefCell<HashMap<StorePath, Rc<dyn Accessor>>>,
    /// Store paths pure evaluation may read.
    allowed: RefCell<HashSet<StorePath>>,
    store_paths: RefCell<HashMap<String, String>>,
}

impl Default for Io {
    fn default() -> Io {
        Io {
            pure: false,
            store: Box::new(MemoryStore::default()),
            store_dir: None,
            stored: RefCell::default(),
            allowed: RefCell::default(),
            store_paths: RefCell::default(),
        }
    }
}

impl Io {
    /// An [`Io`] that, like Nix in pure evaluation mode, forbids access to
    /// any path outside a store path it did not add or read from its store
    /// directory.
    pub fn pure() -> Io {
        Io {
            pure: true,
            ..Io::default()
        }
    }

    /// This [`Io`] with the store objects in `dir`, such as `/nix/store`,
    /// valid too, as if they had been added.
    pub(crate) fn with_store_dir(self, dir: Option<PathBuf>) -> Io {
        Io {
            store_dir: dir,
            ..self
        }
    }

    /// Fail if `logical` may not be accessed at all.
    pub fn check_access(&self, logical: &str) -> Result<()> {
        if self.pure
            && !logical.starts_with(crate::path::COREPKGS)
            && !StorePath::split(logical).is_some_and(|(sp, _)| {
                self.allowed.borrow().contains(&sp) || self.stored.borrow().contains_key(&sp)
            })
        {
            bail!(
                "access to absolute path '{logical}' is forbidden in pure evaluation mode (use '--impure' to override)"
            );
        }
        Ok(())
    }

    /// The tree holding `logical`: the store object it is in, if the store
    /// has it or it was read from the store directory, else the filesystem.
    fn locate<'p>(&self, logical: &'p str) -> Result<(Rc<dyn Accessor>, Cow<'p, str>)> {
        if let Some((sp, rest)) = StorePath::split(logical) {
            if let Some(acc) = self.store.accessor(&sp)? {
                return Ok((acc, Cow::Borrowed(rest)));
            }
            if let Some(acc) = self.stored.borrow().get(&sp) {
                return Ok((acc.clone(), Cow::Borrowed(rest)));
            }
        }
        Ok((Rc::new(Fs), Cow::Borrowed(logical)))
    }

    /// The contents of the regular file `logical` resolves to.
    pub fn read(&self, logical: &str) -> Result<Vec<u8>> {
        self.check_access(logical)?;
        match self.type_of(logical, true)? {
            None => bail!("path '{logical}' does not exist"),
            Some(FileType::Regular) => {}
            Some(_) => bail!("file '{logical}' is not a regular file"),
        }
        let (acc, path) = self.locate(logical)?;
        acc.read_file(&path)
            .with_context(|| format!("reading file '{logical}'"))
    }

    pub fn read_to_string(&self, logical: &str) -> Result<String> {
        let bytes = self.read(logical)?;
        String::from_utf8(bytes).with_context(|| format!("file '{logical}' is not UTF-8"))
    }

    /// The type of `logical`, following a final symlink if `follow` is set,
    /// or `None` if it does not exist. Fails if resolving `logical` loops.
    fn type_of(&self, logical: &str, follow: bool) -> Result<Option<FileType>> {
        if self.check_access(logical).is_err() {
            return Ok(None);
        }
        let Ok((acc, path)) = self.locate(logical) else {
            return Ok(None);
        };
        let ty = if follow {
            acc.resolved_type(&path)
        } else {
            acc.file_type(&path)
        };
        ty.with_context(|| format!("resolving path '{logical}'"))
    }

    /// The type of `logical` without following a final symlink, or `None` if
    /// it does not exist. Fails if resolving `logical` loops.
    pub fn file_type(&self, logical: &str) -> Result<Option<FileType>> {
        self.type_of(logical, false)
    }

    /// Whether `logical` exists, without following a final symlink. Fails if
    /// resolving `logical` loops.
    pub fn exists(&self, logical: &str) -> Result<bool> {
        Ok(self.type_of(logical, false)?.is_some())
    }

    /// Whether `logical` exists, following symlinks. Fails if resolving
    /// `logical` loops.
    pub fn resolves(&self, logical: &str) -> Result<bool> {
        Ok(self.type_of(logical, true)?.is_some())
    }

    /// Whether `logical` is a directory, following symlinks. Fails if
    /// resolving `logical` loops.
    pub fn is_dir(&self, logical: &str) -> Result<bool> {
        Ok(self.type_of(logical, true)? == Some(FileType::Directory))
    }

    /// The entries of a directory, sorted by name.
    pub fn read_dir(&self, logical: &str) -> Result<Vec<(String, FileType)>> {
        self.check_access(logical)?;
        let (acc, path) = self.locate(logical)?;
        acc.read_dir(&path)
            .with_context(|| format!("reading directory '{logical}'"))
    }

    /// The file `import` reads for `logical`: the path itself, or its
    /// `default.nix` if it is a directory.
    pub fn resolve_import(&self, logical: &str) -> Result<String> {
        self.check_access(logical)?;
        if !self.exists(logical)? {
            bail!("path '{logical}' does not exist");
        }
        if self.is_dir(logical)? {
            return Ok(format!("{}/default.nix", logical.trim_end_matches('/')));
        }
        Ok(logical.to_owned())
    }

    /// Copy `logical` to the store, as `"${path}"` does, and return its store
    /// path.
    pub fn add_path_to_store(&self, logical: &str) -> Result<String> {
        if let Some(p) = self.store_paths.borrow().get(logical) {
            return Ok(p.clone());
        }
        let name = crate::path::store_name(logical);
        let sp = self.add_filtered_to_store(logical, &name, None, &[])?;
        self.store_paths
            .borrow_mut()
            .insert(logical.to_owned(), sp.clone());
        Ok(sp)
    }

    /// Copy `logical` to the store under `name`, keeping only entries
    /// `keep(path, type)` accepts (`builtins.path`) and referring to the store
    /// paths `refs`, and return its store path.
    pub fn add_filtered_to_store(
        &self,
        logical: &str,
        name: &str,
        keep: Option<KeepFn<'_>>,
        refs: &[&str],
    ) -> Result<String> {
        store::check_name(name)?;
        self.check_access(logical)?;
        if !self.exists(logical)? {
            bail!("path '{logical}' does not exist");
        }
        let (base, rel) = self.locate(logical)?;
        // What `logical` resolves to is copied, and `keep` sees paths below it.
        let root = base
            .real_path(&rel)
            .with_context(|| format!("path '{logical}' does not exist"))?;
        let logical = &join(
            logical[..logical.len() - rel.len()].trim_end_matches('/'),
            &root,
        );
        let kept = match keep {
            None => None,
            Some(keep) => {
                let mut kept = HashSet::new();
                select(&*base, &root, logical, "", keep, &mut kept)?;
                Some(kept)
            }
        };
        let tree = Rc::new(Subtree { base, root, kept });
        let (sp, _) = self.store.add_source(name, tree, refs)?;
        self.allow(sp)
    }

    /// Copy the regular file `logical` resolves to to the store under `name`
    /// by the hash of its contents alone (`builtins.path` with `recursive =
    /// false`), and return its store path.
    pub fn add_flat_to_store(&self, logical: &str, name: &str) -> Result<String> {
        store::check_name(name)?;
        let sp = self.store.add_flat(name, &self.read(logical)?)?;
        self.allow(sp)
    }

    /// Add `tree` to the store under `name`, returning its store path and the
    /// SHA-256 of its NAR serialisation. Pure evaluation may read it.
    pub(crate) fn add_tree(
        &self,
        name: &str,
        tree: Rc<dyn Accessor>,
    ) -> Result<(String, [u8; 32])> {
        let (sp, hash) = self.store.add_source(name, tree, &[])?;
        Ok((self.allow(sp)?, hash))
    }

    /// Add a file holding `contents` to the store, as `builtins.toFile` does,
    /// and return its store path.
    pub fn add_text(&self, name: &str, contents: &[u8], refs: &[&str]) -> Result<String> {
        let sp = self.store.add_text(name, contents, refs)?;
        self.allow(sp)
    }

    /// Write a derivation's `.drv` file to the store and return its store
    /// path. Unlike other additions, pure evaluation may not read it.
    pub fn add_derivation(&self, name: &str, contents: &[u8], refs: &[&str]) -> Result<String> {
        let sp = self.store.add_text(name, contents, refs)?;
        Ok(sp.as_str().to_owned())
    }

    /// Whether the store or the store directory has `path`.
    pub fn is_valid(&self, path: &StorePath) -> Result<bool> {
        Ok(self.store.accessor(path)?.is_some() || self.in_store_dir(path).is_some())
    }

    /// Where `path` is in the store directory, if it is there.
    fn in_store_dir(&self, path: &StorePath) -> Option<PathBuf> {
        let base = path.as_str().rsplit('/').next()?;
        let physical = self.store_dir.as_ref()?.join(base);
        std::fs::symlink_metadata(&physical)
            .is_ok()
            .then_some(physical)
    }

    /// Read `path` from the store directory, its symlinks resolved within
    /// it, returning whether it is there.
    pub(crate) fn read_stored(&self, path: &StorePath) -> bool {
        let Some(root) = self.in_store_dir(path) else {
            return false;
        };
        let tree = Rc::new(Dir { root, keep: None });
        self.stored.borrow_mut().insert(path.clone(), tree);
        true
    }

    /// Let pure evaluation read the valid store path `path`.
    pub fn allow_valid(&self, path: &StorePath) {
        if !self.read_stored(path) {
            self.allowed.borrow_mut().insert(path.clone());
        }
    }

    /// The store paths `path` refers to, or none if the store does not have
    /// it.
    pub fn references(&self, path: &StorePath) -> Result<Vec<String>> {
        self.store.references(path)
    }

    /// `path` and every store path it refers to, directly or indirectly.
    pub fn closure(&self, path: &str) -> Result<BTreeSet<String>> {
        let mut closure = BTreeSet::new();
        let mut todo = vec![path.to_owned()];
        while let Some(p) = todo.pop() {
            if closure.contains(&p) {
                continue;
            }
            if let Some(sp) = StorePath::parse(&p) {
                todo.extend(self.store.references(&sp)?);
            }
            closure.insert(p);
        }
        Ok(closure)
    }

    fn allow(&self, sp: StorePath) -> Result<String> {
        let path = sp.as_str().to_owned();
        self.allowed.borrow_mut().insert(sp);
        Ok(path)
    }

    /// SHA-256 of the NAR serialisation of `logical`.
    pub fn nar_hash(&self, logical: &str) -> Result<[u8; 32]> {
        self.check_access(logical)?;
        if !self.exists(logical)? {
            bail!("path '{logical}' does not exist");
        }
        let (acc, path) = self.locate(logical)?;
        let mut h = Sha256::new();
        nar::dump(&mut h, &*acc, &path)?;
        Ok(h.finalize().into())
    }
}

/// Record in `kept` each entry below `rel` in the directory `root` of `base`
/// that `keep` accepts, in NAR order, not descending into rejected
/// directories. `keep` sees entries by their logical path below `logical`.
fn select(
    base: &dyn Accessor,
    root: &str,
    logical: &str,
    rel: &str,
    keep: KeepFn<'_>,
    kept: &mut HashSet<String>,
) -> Result<()> {
    let path = join(root, rel);
    if base.file_type(&path)? != Some(FileType::Directory) {
        return Ok(());
    }
    let entries = base
        .read_dir(&path)
        .with_context(|| format!("reading directory '{}'", join(logical, rel)))?;
    for (name, ty) in entries {
        let child = join(rel, &name);
        if keep(&join(logical, &child), ty)? {
            select(base, root, logical, &child, keep, kept)?;
            kept.insert(child);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Expected results are those of `nix-instantiate --eval --strict
    // --read-write-mode`.

    fn eval(expr: &str, pure: bool) -> Result<String, crate::Error> {
        crate::eval_to_string(expr, "/", pure, crate::Settings::default())
    }

    #[test]
    fn added_files_are_readable() {
        let expr =
            r#"builtins.readFile (builtins.unsafeDiscardStringContext (builtins.toFile "x" "y"))"#;
        assert_eq!(eval(expr, true).unwrap(), r#""y""#);
    }

    #[test]
    fn derivation_files_are_readable_only_when_impure() {
        let expr = r#"builtins.readFile (derivation { name = "d"; system = "x"; builder = "/b"; }).drvPath"#;
        assert!(eval(expr, false).unwrap().starts_with(r#""Derive("#));
        assert!(eval(expr, true).is_err());
    }
}
