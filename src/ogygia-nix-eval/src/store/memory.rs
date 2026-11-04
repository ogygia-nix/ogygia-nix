//! A store held in memory for the life of the evaluator.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::rc::Rc;

use anyhow::Result;
use sha2::Digest;
use sha2::Sha256;

use super::Store;
use super::StorePath;
use crate::accessor::Accessor;
use crate::accessor::FileType;
use crate::nar;

/// Holds every object added to it, and nothing else. Copied sources are not
/// copied: they read through to the tree they were added from.
#[derive(Default)]
pub struct MemoryStore {
    objects: RefCell<HashMap<StorePath, Object>>,
}

struct Object {
    contents: Rc<dyn Accessor>,
    refs: Vec<String>,
}

impl MemoryStore {
    fn insert(&self, path: StorePath, object: impl FnOnce() -> Object) -> StorePath {
        self.objects
            .borrow_mut()
            .entry(path.clone())
            .or_insert_with(object);
        path
    }
}

impl Store for MemoryStore {
    fn accessor(&self, path: &StorePath) -> Result<Option<Rc<dyn Accessor>>> {
        Ok(self.objects.borrow().get(path).map(|o| o.contents.clone()))
    }

    fn references(&self, path: &StorePath) -> Result<Vec<String>> {
        Ok(self
            .objects
            .borrow()
            .get(path)
            .map(|o| o.refs.clone())
            .unwrap_or_default())
    }

    fn add_text(&self, name: &str, contents: &[u8], refs: &[&str]) -> Result<StorePath> {
        super::check_name(name)?;
        let path = StorePath(super::text_path(contents, name, refs));
        Ok(self.insert(path, || Object {
            contents: Rc::new(File(contents.to_vec())),
            refs: refs.iter().map(|&r| r.to_owned()).collect(),
        }))
    }

    fn add_flat(&self, name: &str, contents: &[u8]) -> Result<StorePath> {
        super::check_name(name)?;
        let hash = Sha256::digest(contents);
        let path = StorePath(super::fixed_output_path(
            false,
            super::HashAlgo::Sha256,
            &hash,
            name,
        ));
        Ok(self.insert(path, || Object {
            contents: Rc::new(File(contents.to_vec())),
            refs: Vec::new(),
        }))
    }

    fn add_source(
        &self,
        name: &str,
        tree: Rc<dyn Accessor>,
        refs: &[&str],
    ) -> Result<(StorePath, [u8; 32])> {
        super::check_name(name)?;
        let mut h = Sha256::new();
        nar::dump(&mut h, &*tree, "")?;
        let hash: [u8; 32] = h.finalize().into();
        let path = StorePath(super::source_path(&hash, name, refs));
        let path = self.insert(path, || Object {
            contents: tree,
            refs: refs.iter().map(|&r| r.to_owned()).collect(),
        });
        Ok((path, hash))
    }
}

/// A store object that is a single, non-executable file.
struct File(Vec<u8>);

impl File {
    fn root(path: &str) -> std::io::Result<()> {
        if path.is_empty() {
            Ok(())
        } else {
            Err(ErrorKind::NotFound.into())
        }
    }
}

impl Accessor for File {
    fn file_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        Ok(path.is_empty().then_some(FileType::Regular))
    }

    fn resolved_type(&self, path: &str) -> std::io::Result<Option<FileType>> {
        self.file_type(path)
    }

    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        File::root(path)?;
        Ok(self.0.clone())
    }

    fn read_dir(&self, path: &str) -> std::io::Result<Vec<(String, FileType)>> {
        File::root(path)?;
        Err(ErrorKind::NotADirectory.into())
    }

    fn read_link(&self, path: &str) -> std::io::Result<Vec<u8>> {
        File::root(path)?;
        Err(ErrorKind::InvalidInput.into())
    }

    fn is_executable(&self, path: &str) -> std::io::Result<bool> {
        File::root(path)?;
        Ok(false)
    }
}
