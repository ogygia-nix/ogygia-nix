//! Store paths, computed as specified in the Nix manual's "Store Path"
//! section, and the stores holding the objects they name.

mod memory;

use std::rc::Rc;

use anyhow::Result;
use anyhow::bail;
use md5::Md5;
use sha1::Sha1;
use sha2::Digest;
use sha2::Sha256;
use sha2::Sha512;

pub use self::memory::MemoryStore;
use crate::accessor::Accessor;

pub const STORE_DIR: &str = "/nix/store";

const BASE32_CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Nix's base-32 encoding (least significant bits first, reversed).
pub fn base32_encode(bytes: &[u8]) -> String {
    let len = (bytes.len() * 8).div_ceil(5);
    let mut out = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let mut c = (bytes[i] as u16) >> j;
        if i + 1 < bytes.len() {
            c |= (bytes[i + 1] as u16) << (8 - j);
        }
        out.push(BASE32_CHARS[(c & 0x1f) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let len = s.len() * 5 / 8;
    let mut out = vec![0u8; len];
    for (n, ch) in s.bytes().rev().enumerate() {
        let digit = BASE32_CHARS.iter().position(|&c| c == ch)? as u16;
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        if i < len {
            out[i] |= (digit << j) as u8;
        }
        let carry = digit >> (8 - j);
        if i + 1 < len {
            out[i + 1] |= carry as u8;
        } else if carry != 0 {
            return None;
        }
    }
    Some(out)
}

/// XOR-fold a hash down to `size` bytes.
fn compress(hash: &[u8], size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    for (i, b) in hash.iter().enumerate() {
        out[i % size] ^= b;
    }
    out
}

/// `makeStorePath`: the store path for a fingerprint `ty:sha256:<hex>:<dir>:<name>`.
pub fn make_store_path(ty: &str, inner_sha256: &[u8], name: &str) -> String {
    let fingerprint = format!(
        "{ty}:sha256:{}:{STORE_DIR}:{name}",
        hex::encode(inner_sha256)
    );
    let h = Sha256::digest(fingerprint.as_bytes());
    format!("{STORE_DIR}/{}-{name}", base32_encode(&compress(&h, 20)))
}

/// Type prefix with references appended, as used for `source` and `text`.
fn type_with_refs(ty: &str, refs: &[&str], self_ref: bool) -> String {
    let mut s = ty.to_owned();
    let mut refs: Vec<&str> = refs.to_vec();
    refs.sort();
    for r in refs {
        s.push(':');
        s.push_str(r);
    }
    if self_ref {
        s.push_str(":self");
    }
    s
}

/// The path of a NAR-hashed (recursive SHA-256) source that refers to the
/// store paths `refs`, such as a flake input or a copied path.
pub fn source_path(nar_sha256: &[u8], name: &str, refs: &[&str]) -> String {
    make_store_path(&type_with_refs("source", refs, false), nar_sha256, name)
}

/// The path `builtins.toFile` writes `contents` to.
pub fn text_path(contents: &[u8], name: &str, refs: &[&str]) -> String {
    let h = Sha256::digest(contents);
    make_store_path(&type_with_refs("text", refs, false), &h, name)
}

/// The path of a content-addressed store object, such as a fixed-output
/// derivation's output.
pub fn fixed_output_path(recursive: bool, algo: HashAlgo, hash: &[u8], name: &str) -> String {
    if recursive && algo == HashAlgo::Sha256 {
        return make_store_path("source", hash, name);
    }
    let inner = format!(
        "fixed:out:{}{algo}:{}:",
        if recursive { "r:" } else { "" },
        hex::encode(hash)
    );
    let h = Sha256::digest(inner.as_bytes());
    make_store_path("output:out", &h, name)
}

/// Check a store path name for characters Nix rejects.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("store path name is empty");
    }
    if name.len() > 211 {
        bail!("store path name '{name}' is longer than 211 characters");
    }
    if name == "." || name == ".." {
        bail!("store path name '{name}' is not valid");
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || "+-._?=".contains(*c)))
    {
        bail!("store path name '{name}' contains illegal character '{c}'");
    }
    Ok(())
}

/// The path of a store object, `{STORE_DIR}/{hash}-{name}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StorePath(String);

impl StorePath {
    /// `parseStorePath`: `path`, made canonical, if it names a store object
    /// rather than a path inside one.
    pub fn parse(path: &str) -> Option<StorePath> {
        if !path.starts_with('/') {
            return None;
        }
        match StorePath::split(&crate::path::canon_path(path))? {
            (sp, "") => Some(sp),
            _ => None,
        }
    }

    /// The store object the canonical path `path` is in, and the path within
    /// that object, which is empty for the object itself.
    pub fn split(path: &str) -> Option<(StorePath, &str)> {
        let base = path.strip_prefix(STORE_DIR)?.strip_prefix('/')?;
        let (base, rest) = base.split_once('/').unwrap_or((base, ""));
        let (hash, name) = (base.as_bytes().get(..32)?, base.get(33..)?);
        if base.as_bytes()[32] != b'-' || !hash.iter().all(|c| BASE32_CHARS.contains(c)) {
            return None;
        }
        check_name(name).ok()?;
        Some((StorePath(format!("{STORE_DIR}/{base}")), rest))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where store objects are read from and written to.
pub trait Store {
    /// The contents of the store object `path`, rooted at the empty path, or
    /// `None` if the store does not have it.
    fn accessor(&self, path: &StorePath) -> Result<Option<Rc<dyn Accessor>>>;

    /// The store paths `path` refers to, or none if the store does not have
    /// it.
    fn references(&self, path: &StorePath) -> Result<Vec<String>>;

    /// Add a regular file holding `contents` that refers to the store paths
    /// `refs`.
    fn add_text(&self, name: &str, contents: &[u8], refs: &[&str]) -> Result<StorePath>;

    /// Add a regular, non-executable file holding `contents`, addressed by
    /// their SHA-256 as a fixed-output path.
    fn add_flat(&self, name: &str, contents: &[u8]) -> Result<StorePath>;

    /// Add the tree rooted at the empty path in `tree`, referring to the
    /// store paths `refs` and addressed by the hash of its NAR serialisation,
    /// which is returned too.
    fn add_source(
        &self,
        name: &str,
        tree: Rc<dyn Accessor>,
        refs: &[&str],
    ) -> Result<(StorePath, [u8; 32])>;
}

/// A hash algorithm Nix supports.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl HashAlgo {
    /// Digest size in bytes.
    pub fn size(self) -> usize {
        match self {
            Self::Md5 => 16,
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha512 => 64,
        }
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Md5 => Md5::digest(data).to_vec(),
            Self::Sha1 => Sha1::digest(data).to_vec(),
            Self::Sha256 => Sha256::digest(data).to_vec(),
            Self::Sha512 => Sha512::digest(data).to_vec(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }
}

impl std::str::FromStr for HashAlgo {
    type Err = anyhow::Error;

    fn from_str(algo: &str) -> Result<Self> {
        Ok(match algo {
            "md5" => Self::Md5,
            "sha1" => Self::Sha1,
            "sha256" => Self::Sha256,
            "sha512" => Self::Sha512,
            _ => bail!("unknown hash algorithm '{algo}'"),
        })
    }
}

impl std::fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Parse a hash in any format Nix accepts: `<algo>:<hash>`, SRI
/// (`<algo>-<base64>`), or base16, Nix base-32 or base64 of a digest of
/// `algo`. A string containing `:` or `-` is always taken to name its
/// algorithm before the first one. Returns the algorithm and the digest.
pub fn parse_hash(s: &str, algo: Option<HashAlgo>) -> Result<(HashAlgo, Vec<u8>)> {
    let prefix = match s.split_once(':') {
        Some((a, rest)) => Some((a, rest, false)),
        None => s.split_once('-').map(|(a, rest)| (a, rest, true)),
    };
    let (algo, s, sri) = match (prefix, algo) {
        (Some((a, rest, sri)), expected) => {
            let a: HashAlgo = a.parse()?;
            if let Some(expected) = expected
                && a != expected
            {
                bail!("hash '{s}' should have type '{expected}'");
            }
            (a, rest, sri)
        }
        (None, Some(a)) => (a, s, false),
        (None, None) => bail!("hash '{s}' does not include a type"),
    };
    let size = algo.size();
    if sri {
        return match base64_decode(s) {
            Some(bytes) if bytes.len() == size => Ok((algo, bytes)),
            _ => bail!("invalid SRI hash '{s}'"),
        };
    }
    let bytes = if s.len() == size * 2 {
        hex::decode(s).ok()
    } else if s.len() == (size * 8).div_ceil(5) {
        base32_decode(s)
    } else if s.len() == size.div_ceil(3) * 4 {
        base64_decode(s)
    } else {
        None
    };
    match bytes {
        Some(b) if b.len() == size => Ok((algo, b)),
        _ => bail!("invalid hash '{s}' for algorithm '{algo}'"),
    }
}

/// Decode base64 as Nix does: decoding stops at the first `=`, newlines
/// are skipped, and the padding and trailing bits are not checked.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let digit = match c {
            b'=' => break,
            b'\n' => continue,
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6 | u32::from(digit)) & 0xfff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// One of Nix's hash formats.
#[derive(Clone, Copy)]
pub enum HashFormat {
    Base16,
    Nix32,
    Base64,
    Sri,
}

impl std::str::FromStr for HashFormat {
    type Err = anyhow::Error;

    fn from_str(format: &str) -> Result<Self> {
        Ok(match format {
            "base16" => Self::Base16,
            "nix32" | "base32" => Self::Nix32,
            "base64" => Self::Base64,
            "sri" => Self::Sri,
            _ => bail!("unknown hash format '{format}'"),
        })
    }
}

/// Render a digest of `algo` in `format`.
pub fn format_hash(algo: HashAlgo, bytes: &[u8], format: HashFormat) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    match format {
        HashFormat::Base16 => hex::encode(bytes),
        HashFormat::Nix32 => base32_encode(bytes),
        HashFormat::Base64 => b64.encode(bytes),
        HashFormat::Sri => format!("{algo}-{}", b64.encode(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_round_trip() {
        let bytes: Vec<u8> = (0..32).collect();
        let s = base32_encode(&bytes);
        assert_eq!(s.len(), 52);
        assert_eq!(base32_decode(&s).unwrap(), bytes);
    }

    #[test]
    fn text_path_matches_nix() {
        // `builtins.toFile "x" "y"` in Nix.
        assert_eq!(
            text_path(b"y", "x", &[]),
            "/nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x"
        );
    }

    #[test]
    fn flake_input_path_from_nar_hash() {
        // nixpkgs as locked by the NixOS configuration this crate targets.
        let (_, h) =
            parse_hash("sha256-62XMQD4WLdMAfdT0/8gJmPH2dKeTw908ryNloAplTr8=", None).unwrap();
        assert_eq!(
            source_path(&h, "source", &[]),
            "/nix/store/0c7qm6wkgdg9dd3ws34vvg9xacdwa0dq-source"
        );
    }
}
