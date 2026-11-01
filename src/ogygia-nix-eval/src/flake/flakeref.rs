//! Flake references: the URLs of flake inputs and installables, and the
//! attribute sets lock files record them as.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use anyhow::bail;

use crate::builtins::flake;
use crate::value::Value;

/// The value of an attribute of an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attr {
    Str(String),
    Int(u64),
    Bool(bool),
}

impl Attr {
    /// The attribute `v` holds, if it is a string, Boolean or non-negative
    /// integer.
    pub fn from_value(v: Value<'_>) -> Option<Attr> {
        match v {
            Value::Str(s) => Some(Attr::Str(s.as_str_lossy().into_owned())),
            Value::Bool(b) => Some(Attr::Bool(b)),
            Value::Int(i) if i >= 0 => Some(Attr::Int(i as u64)),
            _ => None,
        }
    }
}

/// An input as a set of attributes, as a lock file's `original` and
/// `locked` record it. `dir`, the flake's directory within the input, is
/// one of them.
pub type Attrs = BTreeMap<String, Attr>;

pub fn get_str<'a>(attrs: &'a Attrs, name: &str) -> Option<&'a str> {
    match attrs.get(name) {
        Some(Attr::Str(s)) => Some(s),
        _ => None,
    }
}

pub fn get_int(attrs: &Attrs, name: &str) -> Option<u64> {
    match attrs.get(name) {
        Some(Attr::Int(i)) => Some(*i),
        _ => None,
    }
}

pub(crate) fn percent_decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            let Some(b) = hex else {
                bail!("invalid URI parameter '{s}'");
            };
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| anyhow::anyhow!("invalid URI parameter '{s}'"))
}

/// Parse a flake reference as written in a flake's inputs or on the command
/// line, as `builtins.parseFlakeRef` does, except that a path may be
/// relative. Relative paths are kept as written when `cwd` is `None`, and
/// otherwise made absolute against it.
pub fn parse(url: &str, cwd: Option<&Path>) -> Result<Attrs> {
    let r = url.as_bytes();
    let parsed = if flake::is_path(r) {
        let (base, query) = url.split_once('?').unwrap_or((url, ""));
        let mut path = String::new();
        for c in base.chars() {
            if !(c == '/' && path.ends_with('/')) {
                path.push(c);
            }
        }
        flake::params(query.as_bytes()).and_then(|p| flake::path_attrs(path.into_bytes(), p, url))
    } else {
        flake::url_ref(r, true)
    };
    let mut attrs: Attrs = parsed
        .map_err(anyhow::Error::msg)?
        .into_iter()
        .map(|(name, attr)| {
            let attr = match attr {
                flake::Attr::Str(s) => Attr::Str(String::from_utf8_lossy(&s).into_owned()),
                flake::Attr::Int(i) => Attr::Int(i as u64),
                flake::Attr::Bool(b) => Attr::Bool(b),
            };
            (name.to_owned(), attr)
        })
        .collect();
    if let Some(Attr::Str(path)) = attrs.get_mut("path") {
        *path = resolve(path, cwd);
    }
    Ok(attrs)
}

/// `path` against `cwd`, if given and `path` is relative, with `.` and `..`
/// components resolved.
fn resolve(path: &str, cwd: Option<&Path>) -> String {
    match cwd {
        Some(cwd) if !path.starts_with('/') => {
            crate::path::canon_path(&format!("{}/{path}", cwd.display()))
        }
        _ => path.to_owned(),
    }
}

/// The reference `attrs` describes, as Nix shows it in messages.
pub fn to_string(attrs: &Attrs) -> String {
    let ty = get_str(attrs, "type").unwrap_or("");
    let mut skip = vec!["type"];
    let mut out = match ty {
        "path" => {
            skip.push("path");
            format!("path:{}", get_str(attrs, "path").unwrap_or(""))
        }
        "git" => {
            skip.push("url");
            format!("git+{}", get_str(attrs, "url").unwrap_or(""))
        }
        "tarball" | "file" => {
            skip.push("url");
            get_str(attrs, "url").unwrap_or("").to_owned()
        }
        "indirect" => {
            skip.push("id");
            format!("flake:{}", get_str(attrs, "id").unwrap_or(""))
        }
        _ => {
            skip.extend(["owner", "repo"]);
            format!(
                "{ty}:{}/{}",
                get_str(attrs, "owner").unwrap_or(""),
                get_str(attrs, "repo").unwrap_or("")
            )
        }
    };
    let query: Vec<String> = attrs
        .iter()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .map(|(k, v)| match v {
            Attr::Str(s) => format!("{k}={s}"),
            Attr::Int(i) => format!("{k}={i}"),
            Attr::Bool(b) => format!("{k}={}", u8::from(*b)),
        })
        .collect();
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query.join("&"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(attrs: &[(&str, &str)]) -> Attrs {
        attrs
            .iter()
            .map(|(k, v)| (k.to_string(), Attr::Str(v.to_string())))
            .collect()
    }

    #[test]
    fn parses_input_urls() {
        assert_eq!(
            parse("github:o/r", None).unwrap(),
            s(&[("type", "github"), ("owner", "o"), ("repo", "r")])
        );
        assert_eq!(
            parse("github:o/r/nixos-unstable", None).unwrap(),
            s(&[
                ("type", "github"),
                ("owner", "o"),
                ("repo", "r"),
                ("ref", "nixos-unstable")
            ])
        );
        assert_eq!(
            parse("path:./sub", None).unwrap(),
            s(&[("type", "path"), ("path", "./sub")])
        );
        assert_eq!(
            parse("git+file:///r?dir=d", None).unwrap(),
            s(&[("type", "git"), ("url", "file:///r"), ("dir", "d")])
        );
        assert_eq!(
            parse("https://x/n.tar.xz", None).unwrap(),
            s(&[("type", "tarball"), ("url", "https://x/n.tar.xz")])
        );
        assert_eq!(
            parse("nixpkgs", None).unwrap(),
            s(&[("type", "indirect"), ("id", "nixpkgs")])
        );
        assert_eq!(
            parse("git+file://h//x(y)?dir=d", None).unwrap(),
            s(&[("type", "path"), ("path", "git+file:/h/x(y)"), ("dir", "d")])
        );
        assert_eq!(
            parse("a b", None).unwrap(),
            s(&[("type", "path"), ("path", "a b")])
        );
    }

    #[test]
    fn resolves_relative_paths_against_cwd() {
        assert_eq!(
            parse("./a/../b", Some(Path::new("/c"))).unwrap(),
            s(&[("type", "path"), ("path", "/c/b")])
        );
    }

    #[test]
    fn checks_query_parameters_by_type() {
        assert_eq!(
            parse("?a&%64ir=d&dir=e", Some(Path::new("/c"))).unwrap(),
            s(&[("type", "path"), ("path", "/c"), ("dir", "d")])
        );
        assert!(parse("?a=b", Some(Path::new("/c"))).is_err());
        assert!(parse("path:./x?ref=r", None).is_err());
        assert!(parse("github:o/r?a=b", None).is_err());
        assert_eq!(
            parse("git+file:///r?a=b&ref=c", None).unwrap(),
            s(&[("type", "git"), ("url", "file:///r?a=b"), ("ref", "c")])
        );
    }
}
