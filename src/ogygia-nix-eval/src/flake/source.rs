//! The sources of flakes: copying a flake's files to the store, and what
//! the flake learns about where they came from.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use gix::objs::tree::EntryKind;
use gix::objs::tree::EntryMode;

use crate::accessor::Accessor;
use crate::accessor::FileType;
use crate::accessor::SymlinkLoop;
use crate::archive;
use crate::archive::MemTree;
use crate::flake::FlakeEnv;
use crate::flake::flakeref;
use crate::flake::flakeref::Attrs;
use crate::io;
use crate::io::Io;
use crate::store;
use crate::store::HashAlgo;
use crate::store::HashFormat;
use crate::store::StorePath;

/// What a flake knows about its source, besides where it is.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub nar_hash: Option<String>,
    pub last_modified: Option<u64>,
    pub rev: Option<String>,
    pub rev_count: Option<u64>,
    pub dirty_rev: Option<String>,
    pub submodules: Option<bool>,
}

/// A source in the store.
#[derive(Clone, Debug)]
pub struct Source {
    /// Its store path.
    pub path: String,
    pub info: Info,
}

/// A tree whose paths [`resolve`] can follow symlinks in.
trait Resolve {
    /// An entry of the tree.
    type Node: Clone;

    fn root(&self) -> Self::Node;

    /// The entry `name` of the directory `dir`, or `NotFound`.
    fn child(&self, dir: &Self::Node, name: &[u8]) -> std::io::Result<(FileType, Self::Node)>;

    /// The target of the symlink `link`.
    fn target(&self, link: &Self::Node) -> std::io::Result<Vec<u8>>;
}

/// How many symlinks resolving one path may follow.
const MAX_SYMLINKS: usize = 1024;

/// An entry [`resolve`] reached.
struct Resolved<N> {
    node: N,
    /// The names leading to it from the root, through no symlinks.
    names: Vec<Vec<u8>>,
}

/// The entry at `path` in `tree`, following symlinks in its ancestors, and
/// in `path` itself if `follow` is set. A symlink leading out of the tree
/// does not resolve, and following too many fails with [`SymlinkLoop`].
fn resolve<T: Resolve>(tree: &T, path: &str, follow: bool) -> std::io::Result<Resolved<T::Node>> {
    let mut pending: VecDeque<Vec<u8>> = path
        .as_bytes()
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    // The directories from the root to the one reached so far, and the
    // names of all but the root.
    let mut dirs = vec![tree.root()];
    let mut names = Vec::new();
    let mut links = 0;
    while let Some(name) = pending.pop_front() {
        match name.as_slice() {
            b"." => continue,
            b".." => {
                if dirs.len() == 1 {
                    return Err(std::io::ErrorKind::NotFound.into());
                }
                dirs.pop();
                names.pop();
                continue;
            }
            _ => {}
        }
        let dir = dirs.last().expect("the root is never left");
        let (ty, node) = tree.child(dir, &name)?;
        if ty == FileType::Symlink && (follow || !pending.is_empty()) {
            if links == MAX_SYMLINKS {
                return Err(SymlinkLoop::error());
            }
            let target = tree.target(&node)?;
            if target.starts_with(b"/") {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            links += 1;
            for c in target.split(|&b| b == b'/').rev().filter(|c| !c.is_empty()) {
                pending.push_front(c.to_vec());
            }
            continue;
        }
        names.push(name);
        if ty == FileType::Directory {
            dirs.push(node);
        } else if pending.is_empty() {
            return Ok(Resolved { node, names });
        } else {
            return Err(std::io::ErrorKind::NotADirectory.into());
        }
    }
    let node = dirs.pop().expect("the root is never left");
    Ok(Resolved { node, names })
}

/// What resolving a path found: `None` if it does not resolve, and the
/// error if it fails for following too many symlinks.
fn found<T>(resolved: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match resolved {
        Ok(t) => Ok(Some(t)),
        Err(e) if SymlinkLoop::is(&e) => Err(e),
        Err(_) => Ok(None),
    }
}

/// `path` in `tree` with every symlink in it followed, as by [`resolve`].
fn real_path<T: Resolve>(tree: &T, path: &str) -> std::io::Result<String> {
    String::from_utf8(resolve(tree, path, true)?.names.join(&b'/'))
        .map_err(|_| std::io::ErrorKind::InvalidData.into())
}

/// The files under a directory, or only those in `keep` (relative paths,
/// every ancestor directory included), if set.
pub(crate) struct Dir {
    pub(crate) root: PathBuf,
    pub(crate) keep: Option<HashSet<String>>,
}

impl Resolve for Dir {
    /// A path relative to the root, through no symlinks.
    type Node = PathBuf;

    fn root(&self) -> PathBuf {
        PathBuf::new()
    }

    fn child(&self, dir: &PathBuf, name: &[u8]) -> std::io::Result<(FileType, PathBuf)> {
        let path = dir.join(OsStr::from_bytes(name));
        if !self.kept(&path) {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        let ty = std::fs::symlink_metadata(self.root.join(&path))?.file_type();
        Ok((FileType::of(ty), path))
    }

    fn target(&self, link: &PathBuf) -> std::io::Result<Vec<u8>> {
        io::os_read_link(&self.root.join(link))
    }
}

impl Dir {
    /// Whether the entry at `path`, relative to the root, is kept.
    fn kept(&self, path: &Path) -> bool {
        self.keep
            .as_ref()
            .is_none_or(|keep| path.to_str().is_some_and(|p| keep.contains(p)))
    }

    /// The physical path of the entry at `path`, resolved as by [`resolve`].
    fn physical(&self, path: &str, follow: bool) -> std::io::Result<PathBuf> {
        let rel = resolve(self, path, follow)?.node;
        // Joining an empty path would add a `/`, which a file at the root
        // does not resolve with.
        if rel.as_os_str().is_empty() {
            return Ok(self.root.clone());
        }
        Ok(self.root.join(rel))
    }

    /// The type of the entry at `path`, resolved as by [`resolve`].
    fn entry_type(&self, path: &str, follow: bool) -> std::io::Result<Option<FileType>> {
        Ok(found(self.physical(path, follow))?.and_then(|p| {
            std::fs::symlink_metadata(p)
                .ok()
                .map(|m| FileType::of(m.file_type()))
        }))
    }
}

impl Accessor for Dir {
    fn file_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        self.entry_type(path, false)
    }

    fn resolved_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        self.entry_type(path, true)
    }

    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.physical(path, true)?)
    }

    fn read_dir(&self, path: &str) -> std::io::Result<Vec<(String, FileType)>> {
        let rel = resolve(self, path, true)?.node;
        io::os_read_dir(&self.root.join(&rel), |name| self.kept(&rel.join(name)))
    }

    fn read_link(&self, path: &str) -> std::io::Result<Vec<u8>> {
        io::os_read_link(&self.physical(path, false)?)
    }

    fn is_executable(&self, path: &str) -> std::io::Result<bool> {
        io::os_is_executable(&self.physical(path, false)?)
    }

    fn real_path(&self, path: &str) -> std::io::Result<String> {
        real_path(self, path)
    }
}

/// The tree of a Git commit.
struct GitTree {
    repo: gix::Repository,
    tree: gix::ObjectId,
}

/// The type of a Git tree entry, or `None` for a submodule.
fn git_file_type(mode: EntryMode) -> Option<FileType> {
    match mode.kind() {
        EntryKind::Tree => Some(FileType::Directory),
        EntryKind::Link => Some(FileType::Symlink),
        EntryKind::Blob | EntryKind::BlobExecutable => Some(FileType::Regular),
        EntryKind::Commit => None,
    }
}

impl Resolve for GitTree {
    type Node = (EntryMode, gix::ObjectId);

    fn root(&self) -> Self::Node {
        (EntryKind::Tree.into(), self.tree)
    }

    fn child(&self, dir: &Self::Node, name: &[u8]) -> std::io::Result<(FileType, Self::Node)> {
        let tree = self.repo.find_tree(dir.1).map_err(std::io::Error::other)?;
        let entry = tree.find_entry(name).ok_or(std::io::ErrorKind::NotFound)?;
        let mode = entry.mode();
        let ty = git_file_type(mode).unwrap_or(FileType::Unknown);
        Ok((ty, (mode, entry.object_id())))
    }

    fn target(&self, link: &Self::Node) -> std::io::Result<Vec<u8>> {
        self.read_blob(link.1)
    }
}

impl GitTree {
    /// The entry at `path`, resolved as by [`resolve`].
    fn entry(&self, path: &str, follow: bool) -> std::io::Result<(EntryMode, gix::ObjectId)> {
        resolve(self, path, follow).map(|r| r.node)
    }

    /// The type of the entry at `path`, resolved as by [`resolve`].
    fn entry_type(&self, path: &str, follow: bool) -> std::io::Result<Option<FileType>> {
        Ok(found(self.entry(path, follow))?.and_then(|(mode, _)| git_file_type(mode)))
    }

    fn read_blob(&self, id: gix::ObjectId) -> std::io::Result<Vec<u8>> {
        self.repo
            .find_blob(id)
            .map(|mut b| b.take_data())
            .map_err(std::io::Error::other)
    }
}

impl Accessor for GitTree {
    fn file_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        self.entry_type(path, false)
    }

    fn resolved_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        self.entry_type(path, true)
    }

    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        match self.entry(path, true)? {
            (mode, id) if mode.is_blob() => self.read_blob(id),
            _ => Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }

    fn read_dir(&self, path: &str) -> std::io::Result<Vec<(String, FileType)>> {
        let (mode, id) = self.entry(path, true)?;
        if !mode.is_tree() {
            return Err(std::io::ErrorKind::NotADirectory.into());
        }
        let tree = self.repo.find_tree(id).map_err(std::io::Error::other)?;
        let mut out = Vec::new();
        for entry in tree.iter() {
            let entry = entry.map_err(std::io::Error::other)?;
            // Submodules are not fetched.
            let Some(ty) = git_file_type(entry.mode()) else {
                continue;
            };
            out.push((String::from_utf8_lossy(entry.filename()).into_owned(), ty));
        }
        out.sort();
        Ok(out)
    }

    fn read_link(&self, path: &str) -> std::io::Result<Vec<u8>> {
        match self.entry(path, false)? {
            (mode, id) if mode.is_link() => self.read_blob(id),
            _ => Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }

    fn is_executable(&self, path: &str) -> std::io::Result<bool> {
        let (mode, _) = self.entry(path, false)?;
        Ok(mode.kind() == EntryKind::BlobExecutable)
    }

    fn real_path(&self, path: &str) -> std::io::Result<String> {
        real_path(self, path)
    }
}

/// Add `tree` to the store as a source, failing if its NAR hash is not
/// `expected`.
fn add(
    io: &Io,
    tree: Rc<dyn Accessor>,
    expected: Option<&str>,
    what: &str,
) -> Result<(String, String)> {
    let (path, digest) = io.add_tree("source", tree)?;
    let hash = store::format_hash(HashAlgo::Sha256, &digest, HashFormat::Sri);
    if let Some(expected) = expected {
        let (_, want) = store::parse_hash(expected, Some(HashAlgo::Sha256))?;
        if want != digest {
            bail!("NAR hash mismatch in input '{what}', expected '{expected}' but got '{hash}'");
        }
    }
    Ok((path, hash))
}

/// The latest modification time of anything under `path`, itself included.
fn latest_mtime(path: &Path) -> Result<u64> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("getting status of '{}'", path.display()))?;
    let mut latest = meta.mtime().max(0) as u64;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            latest = latest.max(latest_mtime(&entry?.path())?);
        }
    }
    Ok(latest)
}

/// Copy the directory `dir` to the store, as a `path:` flake.
fn fetch_path(io: &Io, dir: &Path, locked: &Attrs, what: &str) -> Result<Source> {
    if !dir.exists() {
        bail!("path '{}' does not exist", dir.display());
    }
    let tree = Rc::new(Dir {
        root: dir.to_owned(),
        keep: None,
    });
    let (path, hash) = add(io, tree, flakeref::get_str(locked, "narHash"), what)?;
    let last_modified = match flakeref::get_int(locked, "lastModified") {
        Some(t) => t,
        None => latest_mtime(dir)?,
    };
    Ok(Source {
        path,
        info: Info {
            nar_hash: Some(hash),
            last_modified: Some(last_modified),
            ..Info::default()
        },
    })
}

fn open_repo(dir: &Path) -> Result<gix::Repository> {
    // gix resolves `..` lexically, failing above the root, where Nix leaves
    // it to the file system: after following a symbolic link, say.
    let real = std::fs::canonicalize(dir);
    gix::open(real.as_deref().unwrap_or(dir))
        .with_context(|| format!("opening Git repository '{}'", dir.display()))
}

fn commit_count(repo: &gix::Repository, commit: gix::ObjectId) -> Result<u64> {
    let mut count = 0;
    for info in repo.rev_walk([commit]).all()? {
        info?;
        count += 1;
    }
    Ok(count)
}

/// Whether the tracked files of the repository's working tree, or its
/// index, differ from `HEAD`. Submodules are not considered.
fn is_dirty(repo: &gix::Repository) -> Result<bool> {
    use gix::status::Item;
    use gix::status::index_worktree;
    let submodule = gix::index::entry::Mode::COMMIT;
    let status = repo
        .status(gix::progress::Discard)?
        .untracked_files(gix::status::UntrackedFiles::None)
        .index_worktree_submodules(None)
        .into_iter(None)?;
    for item in status {
        match item? {
            Item::TreeIndex(change) if change.entry_mode() == submodule => {}
            Item::IndexWorktree(index_worktree::Item::Modification { entry, .. })
                if entry.mode == submodule => {}
            Item::IndexWorktree(item) if item.summary().is_none() => {}
            _ => return Ok(true),
        }
    }
    Ok(false)
}

/// Copy the files Git tracks in the working tree of the repository at
/// `dir` to the store, as an unlocked `git+file:` flake.
fn fetch_git_worktree(io: &Io, dir: &Path, what: &str) -> Result<Source> {
    let repo = open_repo(dir)?;
    let mut keep = HashSet::new();
    let index = repo.index_or_empty()?;
    for entry in index.entries() {
        // Submodules are not fetched.
        if entry.mode == gix::index::entry::Mode::COMMIT {
            continue;
        }
        let path = String::from_utf8_lossy(entry.path(&index)).into_owned();
        let mut at = path.as_str();
        keep.insert(at.to_owned());
        while let Some((parent, _)) = at.rsplit_once('/') {
            keep.insert(parent.to_owned());
            at = parent;
        }
    }
    let dirty = is_dirty(&repo)?;
    let head = repo.head_commit().ok();
    let tree = Rc::new(Dir {
        root: repo
            .workdir()
            .context("a Git flake needs a working tree")?
            .to_owned(),
        keep: Some(keep),
    });
    let (path, hash) = add(io, tree, None, what)?;
    let mut info = Info {
        nar_hash: Some(hash),
        last_modified: Some(match &head {
            Some(c) => c.time()?.seconds.max(0) as u64,
            None => 0,
        }),
        submodules: Some(false),
        ..Info::default()
    };
    if let Some(head) = head {
        let rev = head.id.to_string();
        if dirty {
            info.dirty_rev = Some(format!("{rev}-dirty"));
        } else {
            info.rev_count = Some(commit_count(&repo, head.id)?);
            info.rev = Some(rev);
        }
    }
    Ok(Source { path, info })
}

/// Copy the tree of commit `rev` of the repository at `dir` to the store.
fn fetch_git_rev(io: &Io, dir: &Path, rev: &str, locked: &Attrs, what: &str) -> Result<Source> {
    let repo = open_repo(dir)?;
    let (id, last_modified, tree_id) = {
        let commit = repo
            .find_commit(gix::ObjectId::from_hex(rev.as_bytes())?)
            .with_context(|| format!("Git repository '{}' has no commit {rev}", dir.display()))?;
        let time = commit.time()?.seconds.max(0) as u64;
        (commit.id, time, commit.tree_id()?.detach())
    };
    let rev_count = match flakeref::get_int(locked, "revCount") {
        Some(n) => n,
        None => commit_count(&repo, id)?,
    };
    let tree = Rc::new(GitTree {
        repo,
        tree: tree_id,
    });
    let (path, hash) = add(io, tree, flakeref::get_str(locked, "narHash"), what)?;
    Ok(Source {
        path,
        info: Info {
            nar_hash: Some(hash),
            last_modified: Some(flakeref::get_int(locked, "lastModified").unwrap_or(last_modified)),
            rev: Some(id.to_string()),
            rev_count: Some(rev_count),
            submodules: Some(false),
            ..Info::default()
        },
    })
}

/// The source `locked` names, read from the store directory, if it is there.
fn from_store(io: &Io, locked: &Attrs) -> Result<Option<Source>> {
    let Some(nar_hash) = flakeref::get_str(locked, "narHash") else {
        return Ok(None);
    };
    let (_, hash) = store::parse_hash(nar_hash, Some(HashAlgo::Sha256))?;
    let path = store::source_path(&hash, "source");
    let sp = StorePath::parse(&path).expect("a source path is a store path");
    if !io.read_stored(&sp) {
        return Ok(None);
    }
    Ok(Some(Source {
        path,
        info: Info {
            nar_hash: Some(nar_hash.to_owned()),
            last_modified: flakeref::get_int(locked, "lastModified"),
            rev: flakeref::get_str(locked, "rev").map(str::to_owned),
            rev_count: flakeref::get_int(locked, "revCount"),
            ..Info::default()
        },
    }))
}

/// The absolute path a `file:` URL with no host names, such as `/r` for
/// `file:/r` or `file:///r?q`, decoded.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file:")?;
    let path = rest.strip_prefix("//").unwrap_or(rest);
    let path = path.split_once('?').map_or(path, |(p, _)| p);
    let path = crate::builtins::url::decode(path.as_bytes());
    path.starts_with(b"/")
        .then(|| PathBuf::from(OsStr::from_bytes(&path)))
}

/// The contents of the file at `url`: read directly for a local `file:`
/// URL, and downloaded by `env`'s fetcher otherwise.
fn download(env: &FlakeEnv, url: &str) -> Result<Vec<u8>> {
    if let Some(path) = file_url_path(url) {
        return std::fs::read(&path).with_context(|| format!("reading '{}'", path.display()));
    }
    match &env.fetcher {
        Some(fetcher) => fetcher
            .get(url)
            .with_context(|| format!("downloading '{url}'")),
        None => bail!("cannot download '{url}': this evaluator has no fetcher"),
    }
}

/// Unpack the archive at `url` into the store. Its last modification time
/// is the one `attrs` gives, or, if they are not locked, the archive's.
fn fetch_archive(io: &Io, env: &FlakeEnv, url: &str, attrs: &Attrs, what: &str) -> Result<Source> {
    let unpacked =
        archive::unpack(&download(env, url)?).with_context(|| format!("unpacking '{url}'"))?;
    let nar_hash = flakeref::get_str(attrs, "narHash");
    let (path, hash) = add(io, Rc::new(unpacked.tree), nar_hash, what)?;
    let last_modified = match flakeref::get_int(attrs, "lastModified") {
        Some(t) => Some(t),
        None if nar_hash.is_none() => unpacked.last_modified,
        None => None,
    };
    Ok(Source {
        path,
        info: Info {
            nar_hash: Some(hash),
            last_modified,
            rev: flakeref::get_str(attrs, "rev").map(str::to_owned),
            ..Info::default()
        },
    })
}

/// Add the file at `url` to the store, as it is.
fn fetch_file(io: &Io, env: &FlakeEnv, url: &str, attrs: &Attrs, what: &str) -> Result<Source> {
    let tree = Rc::new(MemTree::file(download(env, url)?));
    let (path, hash) = add(io, tree, flakeref::get_str(attrs, "narHash"), what)?;
    Ok(Source {
        path,
        info: Info {
            nar_hash: Some(hash),
            last_modified: flakeref::get_int(attrs, "lastModified"),
            ..Info::default()
        },
    })
}

/// The URL of the archive of revision `rev` of the GitHub, GitLab or
/// SourceHut repository `attrs` names.
fn forge_archive_url(ty: &str, attrs: &Attrs, rev: &str) -> String {
    let owner = flakeref::get_str(attrs, "owner").unwrap_or("");
    let repo = flakeref::get_str(attrs, "repo").unwrap_or("");
    let host = flakeref::get_str(attrs, "host");
    match ty {
        "github" => format!(
            "https://{}/{owner}/{repo}/archive/{rev}.tar.gz",
            host.unwrap_or("github.com")
        ),
        "gitlab" => format!(
            "https://{}/api/v4/projects/{}%2F{repo}/repository/archive.tar.gz?sha={rev}",
            host.unwrap_or("gitlab.com"),
            owner.replace('/', "%2F")
        ),
        _ => format!(
            "https://{}/{owner}/{repo}/archive/{rev}.tar.gz",
            host.unwrap_or("git.sr.ht")
        ),
    }
}

/// Fetch the source of the input `attrs`, locked or not.
pub fn fetch(io: &Io, env: &FlakeEnv, attrs: &Attrs, what: &str) -> Result<Source> {
    let ty = flakeref::get_str(attrs, "type").unwrap_or("");
    let url = flakeref::get_str(attrs, "url").unwrap_or("");
    match (ty, file_url_path(url)) {
        ("path", _) => {
            let path = flakeref::get_str(attrs, "path").unwrap_or("");
            return fetch_path(io, Path::new(path), attrs, what);
        }
        ("git", Some(dir)) => {
            return match flakeref::get_str(attrs, "rev") {
                Some(rev) => fetch_git_rev(io, &dir, rev, attrs, what),
                None => fetch_git_worktree(io, &dir, what),
            };
        }
        ("indirect", _) => bail!(
            "cannot find flake 'flake:{}' in the flake registries",
            flakeref::get_str(attrs, "id").unwrap_or("")
        ),
        _ => {}
    }
    if let Some(source) = from_store(io, attrs)? {
        return Ok(source);
    }
    match ty {
        "tarball" => fetch_archive(io, env, url, attrs, what),
        "file" => fetch_file(io, env, url, attrs, what),
        "github" | "gitlab" | "sourcehut" => {
            let Some(rev) = flakeref::get_str(attrs, "rev") else {
                bail!(
                    "cannot fetch input '{what}': it has no revision, and this evaluator does not look them up"
                );
            };
            fetch_archive(io, env, &forge_archive_url(ty, attrs, rev), attrs, what)
        }
        "git" => bail!(
            "cannot fetch input '{what}': this evaluator only reads remote Git repositories from the store"
        ),
        _ => bail!("input type '{ty}' is unsupported"),
    }
}

/// `secs` since the epoch as `YYYYMMDDHHMMSS` in UTC.
pub fn date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The first seven characters of `rev`, as `shortRev` holds them.
pub fn short_rev(rev: &str) -> &str {
    &rev[..rev.len().min(7)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_as_nix_does() {
        assert_eq!(date(1_600_000_000), "20200913122640");
        assert_eq!(date(1000), "19700101001640");
        assert_eq!(date(1_500_000_100), "20170714024140");
    }

    // The flakes below evaluate as `nix eval --json
    // --no-update-lock-file` evaluates them, with lock files written by
    // `nix flake lock` and the expected output printed by Nix 2.36.
    const TARBALL_HASH: &str = "sha256-nnyd4JnnpMA0Lp5sw3G+jnPNNLfc1rJRGYNiHL6ab9c=";
    const FILE_HASH: &str = "sha256-RCJ37DJg+G+SI5m0zPshU3sGVDGrhCq/TeBVi/rLRVY=";
    const REV: &str = "5edf11c44bc78a0d334f6334cdaf7d60d732daab";

    /// A gzipped tarball of `top/`, whose NAR hash is `TARBALL_HASH`.
    fn tarball() -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        let mut add = |path: &str, ty: tar::EntryType, mode: u32, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(ty);
            h.set_mode(mode);
            h.set_mtime(1000);
            if ty.is_symlink() {
                h.set_size(0);
                b.append_link(&mut h, path, std::str::from_utf8(data).unwrap())
                    .unwrap();
            } else {
                h.set_size(data.len() as u64);
                b.append_data(&mut h, path, data).unwrap();
            }
        };
        use tar::EntryType::*;
        add("top/", Directory, 0o755, b"");
        add("top/a", Regular, 0o644, b"hello\n");
        add("top/empty/", Directory, 0o755, b"");
        add("top/l", Symlink, 0o777, b"a");
        add("top/sub/s", Regular, 0o644, b"s\n");
        add("top/x", Regular, 0o755, b"#!/bin/sh\n");
        b.into_inner().unwrap().finish().unwrap()
    }

    /// A directory holding `tarball()`, a file and a flake with `inputs`,
    /// `outputs` and lock file nodes `nodes`, in which `@DIR@` is the
    /// directory.
    fn flake(inputs: &str, outputs: &str, nodes: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        std::fs::write(dir.path().join("t.tar.gz"), tarball()).unwrap();
        std::fs::write(dir.path().join("f.txt"), "data\n").unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let text = format!("{{ inputs = {{ {inputs} }}; outputs = {outputs}; }}");
        std::fs::write(root.join("flake.nix"), text.replace("@DIR@", &d)).unwrap();
        let lock = format!(r#"{{ "nodes": {{ {nodes} }}, "root": "root", "version": 7 }}"#);
        std::fs::write(root.join("flake.lock"), lock.replace("@DIR@", &d)).unwrap();
        dir
    }

    fn eval(dir: &tempfile::TempDir, fetcher: Option<Box<dyn crate::Fetcher>>) -> String {
        let env = FlakeEnv {
            cwd: dir.path().to_owned(),
            store: None,
            fetcher,
        };
        let installable = format!("{}/root#x", dir.path().display());
        crate::eval_flake_json(&installable, "x: x", &env, crate::Settings::default())
            .unwrap_or_else(|e| format!("error: {e}"))
    }

    #[test]
    fn evaluates_tarball_and_file_inputs() {
        let dir = flake(
            r#"t = { url = "tarball+file://@DIR@/t.tar.gz"; flake = false; };
               f = { url = "file+file://@DIR@/f.txt"; flake = false; };"#,
            r#"{ self, t, f }: {
                 x = {
                   t = builtins.removeAttrs t [ "outPath" "sourceInfo" ] // { path = t.outPath; };
                   f = builtins.removeAttrs f [ "outPath" "sourceInfo" ] // { path = f.outPath; };
                   files = builtins.readDir t;
                   a = builtins.readFile "${t}/sub/s";
                   c = builtins.readFile f;
                 };
               }"#,
            &format!(
                r#""root": {{ "inputs": {{ "f": "f", "t": "t" }} }},
                   "f": {{ "flake": false,
                     "locked": {{ "narHash": "{FILE_HASH}", "type": "file", "url": "file://@DIR@/f.txt" }},
                     "original": {{ "type": "file", "url": "file://@DIR@/f.txt" }} }},
                   "t": {{ "flake": false,
                     "locked": {{ "lastModified": 1000, "narHash": "{TARBALL_HASH}", "type": "tarball", "url": "file://@DIR@/t.tar.gz" }},
                     "original": {{ "type": "tarball", "url": "file://@DIR@/t.tar.gz" }} }}"#
            ),
        );
        assert_eq!(
            eval(&dir, None),
            concat!(
                r#"{"a":"s\n","c":"data\n","#,
                r#""f":{"narHash":"sha256-RCJ37DJg+G+SI5m0zPshU3sGVDGrhCq/TeBVi/rLRVY=","path":"/nix/store/1bkk5g2aa0vdvxnnbl86xw22j78sfp8p-source"},"#,
                r#""files":{"a":"regular","empty":"directory","l":"symlink","sub":"directory","x":"regular"},"#,
                r#""t":{"lastModified":1000,"lastModifiedDate":"19700101001640","narHash":"sha256-nnyd4JnnpMA0Lp5sw3G+jnPNNLfc1rJRGYNiHL6ab9c=","path":"/nix/store/g9j8h21lmddykkr8h9qjsiym1vrj9gr2-source"}}"#
            )
        );
    }

    /// Serves `tarball()` at every URL, recording the URLs it is asked for.
    struct Fake(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl crate::Fetcher for Fake {
        fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            self.0.lock().unwrap().push(url.to_owned());
            Ok(tarball())
        }
    }

    fn github_flake(nar_hash: &str) -> tempfile::TempDir {
        flake(
            r#"g = { url = "github:o/r"; flake = false; };"#,
            r#"{ self, g }: { x = builtins.removeAttrs g.sourceInfo [ "outPath" ]; }"#,
            &format!(
                r#""root": {{ "inputs": {{ "g": "g" }} }},
                   "g": {{ "flake": false,
                     "locked": {{ "lastModified": 1000, "narHash": "{nar_hash}", "owner": "o", "repo": "r", "rev": "{REV}", "type": "github" }},
                     "original": {{ "owner": "o", "repo": "r", "type": "github" }} }}"#
            ),
        )
    }

    #[test]
    fn downloads_forge_inputs_through_the_fetcher() {
        let urls = std::sync::Arc::default();
        let dir = github_flake(TARBALL_HASH);
        assert_eq!(
            eval(&dir, Some(Box::new(Fake(std::sync::Arc::clone(&urls))))),
            format!(
                r#"{{"lastModified":1000,"lastModifiedDate":"19700101001640","narHash":"{TARBALL_HASH}","rev":"{REV}","shortRev":"5edf11c"}}"#
            )
        );
        assert_eq!(
            *urls.lock().unwrap(),
            [format!("https://github.com/o/r/archive/{REV}.tar.gz")]
        );
    }

    #[test]
    fn rejects_a_download_with_the_wrong_nar_hash() {
        let dir = github_flake(FILE_HASH);
        let out = eval(&dir, Some(Box::new(Fake(Default::default()))));
        assert!(out.contains("NAR hash mismatch"), "{out}");
    }

    #[test]
    fn cannot_download_without_a_fetcher() {
        let out = eval(&github_flake(TARBALL_HASH), None);
        assert!(out.contains("has no fetcher"), "{out}");
    }

    #[test]
    fn finds_forge_archives() {
        let attrs = |owner: &str, host: Option<&str>| {
            let mut a = Attrs::new();
            a.insert("owner".into(), flakeref::Attr::Str(owner.into()));
            a.insert("repo".into(), flakeref::Attr::Str("r".into()));
            if let Some(h) = host {
                a.insert("host".into(), flakeref::Attr::Str(h.into()));
            }
            a
        };
        assert_eq!(
            forge_archive_url("github", &attrs("o", Some("ghe.example")), "c"),
            "https://ghe.example/o/r/archive/c.tar.gz"
        );
        assert_eq!(
            forge_archive_url("gitlab", &attrs("g/s", None), "c"),
            "https://gitlab.com/api/v4/projects/g%2Fs%2Fr/repository/archive.tar.gz?sha=c"
        );
        assert_eq!(
            forge_archive_url("sourcehut", &attrs("~o", None), "c"),
            "https://git.sr.ht/~o/r/archive/c.tar.gz"
        );
    }
}
