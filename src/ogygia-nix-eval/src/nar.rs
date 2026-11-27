//! Nix Archive serialisation, as specified in the Nix manual's "Nix Archive"
//! section.

use std::io::Write;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use sha2::Digest;
use sha2::Sha256;

use crate::accessor::Accessor;
use crate::accessor::FileType;

/// Write one NAR string: its length, the bytes, and padding to 8 bytes.
fn write_str(w: &mut impl Write, s: &[u8]) -> std::io::Result<()> {
    w.write_all(&(s.len() as u64).to_le_bytes())?;
    w.write_all(s)?;
    let pad = (8 - s.len() % 8) % 8;
    w.write_all(&[0u8; 8][..pad])
}

/// Write the NAR serialisation of `path` in `acc`.
pub fn dump(w: &mut impl Write, acc: &dyn Accessor, path: &str) -> Result<()> {
    write_str(w, b"nix-archive-1")?;
    dump_node(w, acc, path)
}

/// SHA-256 of the NAR serialisation of `path` in `acc`.
pub fn sha256(acc: &dyn Accessor, path: &str) -> Result<[u8; 32]> {
    struct Hasher(Sha256);
    impl Write for Hasher {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut h = Hasher(Sha256::new());
    dump(&mut h, acc, path)?;
    Ok(h.0.finalize().into())
}

fn dump_node(w: &mut impl Write, acc: &dyn Accessor, path: &str) -> Result<()> {
    let Some(ft) = acc
        .file_type(path)
        .with_context(|| format!("reading '{path}'"))?
    else {
        bail!("path '{path}' does not exist");
    };
    write_str(w, b"(")?;
    write_str(w, b"type")?;
    match ft {
        FileType::Symlink => {
            write_str(w, b"symlink")?;
            write_str(w, b"target")?;
            let target = acc
                .read_link(path)
                .with_context(|| format!("reading '{path}'"))?;
            write_str(w, &target)?;
        }
        FileType::Directory => {
            write_str(w, b"directory")?;
            let entries = acc
                .read_dir(path)
                .with_context(|| format!("reading directory '{path}'"))?;
            for (name, _) in entries {
                let child = crate::path::join(path, &name);
                write_str(w, b"entry")?;
                write_str(w, b"(")?;
                write_str(w, b"name")?;
                write_str(w, name.as_bytes())?;
                write_str(w, b"node")?;
                dump_node(w, acc, &child)?;
                write_str(w, b")")?;
            }
        }
        FileType::Regular => {
            write_str(w, b"regular")?;
            if acc
                .is_executable(path)
                .with_context(|| format!("reading '{path}'"))?
            {
                write_str(w, b"executable")?;
                write_str(w, b"")?;
            }
            write_str(w, b"contents")?;
            let contents = acc
                .read_file(path)
                .with_context(|| format!("reading file '{path}'"))?;
            write_str(w, &contents)?;
        }
        FileType::Unknown => bail!("file '{path}' has an unsupported type"),
    }
    write_str(w, b")")?;
    Ok(())
}
