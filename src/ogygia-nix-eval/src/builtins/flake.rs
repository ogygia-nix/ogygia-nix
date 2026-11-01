//! The flake builtins, which Nix provides but refuses to run while the
//! `flakes` experimental feature is disabled, as it is outside flake
//! evaluation, except for the references of input types that do not need
//! it.

use std::collections::BTreeMap;

use super::url;
use super::url::split_once;
use crate::eval::Eval;
use crate::value::R;
use crate::value::Value;
use crate::value::eval_err;
use crate::value::unimplemented;

const FLAKES_DISABLED: &str = "experimental Nix feature 'flakes' is disabled; add '--extra-experimental-features flakes' to enable it";

/// The input types Nix shows only with the `flakes` experimental feature
/// enabled.
const FLAKE_TYPES: &[&[u8]] = &[b"indirect", b"path", b"github", b"gitlab", b"sourcehut"];

fn flakes_disabled<'a>() -> R<'a> {
    eval_err(FLAKES_DISABLED)
}

pub fn get_flake<'a>(ev: &Eval<'a>, flake_ref: Value<'a>) -> R<'a> {
    ev.force_str_no_ctx(flake_ref)?;
    if ev.ctx.flakes {
        return unimplemented("builtins.getFlake");
    }
    flakes_disabled()
}

pub fn parse_flake_ref<'a>(ev: &Eval<'a>, flake_ref: Value<'a>) -> R<'a> {
    let attrs = url_ref(ev.force_str_no_ctx(flake_ref)?, ev.ctx.flakes).or_else(eval_err)?;
    Ok(ev.attrs(
        attrs
            .into_iter()
            .map(|(name, attr)| {
                let value = match attr {
                    Attr::Str(s) => ev.str_val(&s, &[]),
                    Attr::Int(i) => Value::Int(i),
                    Attr::Bool(b) => Value::Bool(b),
                };
                ev.entry(name, value)
            })
            .collect(),
    ))
}

pub(crate) enum Attr {
    Str(Vec<u8>),
    Int(i64),
    Bool(bool),
}

pub(crate) type Attrs = Vec<(&'static str, Attr)>;

pub(crate) type Params = BTreeMap<Vec<u8>, Vec<u8>>;

/// The attributes of the flake reference `r`. Without `flakes`, it must be
/// the URL of a Git, Mercurial, tarball or file input.
pub(crate) fn url_ref(r: &[u8], flakes: bool) -> Result<Attrs, String> {
    let shown = String::from_utf8_lossy(r);
    let (r, fragment) = split_once(r, b'#').unwrap_or((r, b""));
    if !fragment.is_empty() {
        return Err(format!(
            "unexpected fragment '{}' in flake reference '{shown}'",
            String::from_utf8_lossy(fragment)
        ));
    }
    if flakes && bare_id(r) {
        let shown = format!("flake:{}", String::from_utf8_lossy(r));
        return indirect_ref(r, Params::new(), &shown);
    }
    let not_absolute = || format!("flake reference '{shown}' is not an absolute path");
    let (base, query) = split_once(r, b'?').unwrap_or((r, b""));
    // Nix reads what is not a URL as a path or a registry name, which need
    // the flakes feature, and rejects a relative path first.
    let Ok(uri) = url::parse(base) else {
        if flakes && r.starts_with(b"/") {
            return path_ref(base, false, params(query)?, &shown);
        }
        return Err(if flakes || r.starts_with(b".") || r.contains(&b':') {
            not_absolute()
        } else {
            FLAKES_DISABLED.to_owned()
        });
    };
    let mut params = params(query)?;
    if file_with_host(&uri) {
        return Err(not_absolute());
    }

    let (ty, scheme) = match uri.scheme {
        b"path" | b"github" | b"gitlab" | b"sourcehut" | b"flake" if !flakes => {
            return Err(FLAKES_DISABLED.to_owned());
        }
        b"path" => {
            if uri.authority.as_ref().is_some_and(|a| !a.host.is_empty()) {
                return Err(format!("path URL '{shown}' should not have an authority"));
            }
            return path_ref(uri.path, true, params, &shown);
        }
        b"flake" => {
            if uri.authority.is_some() {
                return Err(format!("URL '{shown}' is invalid"));
            }
            return indirect_ref(uri.path, params, &shown);
        }
        ty @ (b"github" | b"gitlab" | b"sourcehut") => {
            if uri.authority.is_some() {
                return Err(format!("URL '{shown}' is invalid"));
            }
            return forge_ref(ty, uri.path, params, &shown);
        }
        s @ b"git" => ("git", s),
        s @ (b"http" | b"https" | b"file") => ("tarball", s),
        s => match split_once(s, b'+') {
            Some((b"git", s)) => ("git", s),
            Some((b"hg", s @ (b"http" | b"https" | b"ssh" | b"file"))) => ("hg", s),
            Some((b"tarball", s @ (b"http" | b"https" | b"file"))) => ("tarball", s),
            Some((b"file", s @ (b"http" | b"https" | b"file"))) => ("file", s),
            _ => return Err(format!("input '{shown}' is unsupported")),
        },
    };

    let mut attrs = vec![("type", Attr::Str(ty.into()))];
    dir_attr(&mut attrs, &mut params);
    let mut str_attr = |params: &mut Params, name: &'static str| {
        if let Some(v) = params.remove(name.as_bytes()) {
            attrs.push((name, Attr::Str(v)));
        }
    };
    match ty {
        "git" | "hg" => {
            if let Some(r) = params.get(&b"ref"[..]) {
                let (valid, kind) = match ty {
                    "git" => (git_ref(r), "Git"),
                    _ => (hg_ref(r), "Mercurial"),
                };
                if !valid {
                    return Err(format!(
                        "invalid {kind} branch/tag name '{}'",
                        String::from_utf8_lossy(r)
                    ));
                }
            }
            str_attr(&mut params, "ref");
            str_attr(&mut params, "rev");
            if ty == "git" {
                const VERIFIED: [&str; 4] = ["verifyCommit", "keytype", "publicKey", "publicKeys"];
                if VERIFIED.iter().any(|n| params.contains_key(n.as_bytes())) {
                    return Err("experimental Nix feature 'verified-fetches' is disabled; add '--extra-experimental-features verified-fetches' to enable it".to_owned());
                }
                for name in ["shallow", "submodules", "lfs", "exportIgnore", "allRefs"] {
                    if let Some(v) = params.remove(name.as_bytes()) {
                        attrs.push((name, Attr::Bool(v == b"1")));
                    }
                }
            }
        }
        _ => {
            str_attr(&mut params, "narHash");
            str_attr(&mut params, "rev");
            for name in ["revCount", "lastModified"] {
                // An integer that does not parse is dropped.
                if let Some(i) = params.remove(name.as_bytes()).and_then(|v| int(&v)) {
                    attrs.push((name, Attr::Int(i)));
                }
            }
            for name in ["unpack", "name", "url"] {
                params.remove(name.as_bytes());
            }
        }
    }

    let mut out = [scheme, &b":"[..]].concat();
    if let Some(a) = &uri.authority {
        out.extend(b"//");
        authority(&mut out, a);
    }
    if uri.path.is_empty() && scheme == b"file" {
        out.push(b'/');
    } else {
        let segments: Vec<Vec<u8>> = uri
            .path
            .split(|&c| c == b'/')
            .map(|s| url::encode(&url::decode(s), b":@"))
            .collect();
        out.extend(segments.join(&b'/'));
    }
    if !params.is_empty() {
        let query: Vec<Vec<u8>> = params
            .iter()
            .map(|(k, v)| [url::encode(k, b":@/?"), url::encode(v, b":@/?")].join(&b'='))
            .collect();
        out.push(b'?');
        out.extend(query.join(&b'&'));
    }
    attrs.push(("url", Attr::Str(out)));
    Ok(attrs)
}

/// Whether Nix reads the flake reference `r` as a path, which in a flake
/// input or installable may be relative: when it is neither a registry name
/// nor a URL, or is a URL with a host whose scheme is `file`, perhaps after
/// a `+`.
pub(crate) fn is_path(r: &[u8]) -> bool {
    let base = split_once(r, b'?').map_or(r, |(b, _)| b);
    match url::parse(base) {
        Ok(uri) => file_with_host(&uri),
        Err(_) => !bare_id(r),
    }
}

fn file_with_host(uri: &url::Uri<'_>) -> bool {
    let scheme = split_once(uri.scheme, b'+').map_or(uri.scheme, |(_, s)| s);
    scheme == b"file" && uri.authority.as_ref().is_some_and(|a| !a.host.is_empty())
}

/// The parameters of the URL query `query`, decoded. The first of
/// parameters with the same name wins, and one without `=` is ignored.
pub(crate) fn params(query: &[u8]) -> Result<Params, String> {
    if !url::chars(query, |c| url::pchar(c) || b"/? \"[]".contains(&c)) {
        return Err(format!(
            "invalid URI query '{}'",
            String::from_utf8_lossy(query)
        ));
    }
    let mut params = Params::new();
    for (k, v) in query
        .split(|&c| c == b'&')
        .filter_map(|p| split_once(p, b'='))
    {
        params
            .entry(url::decode(k))
            .or_insert_with(|| url::decode(v));
    }
    Ok(params)
}

/// An integer parameter. One that overflows a signed integer wraps.
fn int(v: &[u8]) -> Option<i64> {
    Some(std::str::from_utf8(v).ok()?.parse::<u64>().ok()? as i64)
}

/// Move the `dir` parameter, unless empty, to `attrs`.
fn dir_attr(attrs: &mut Attrs, params: &mut Params) {
    if let Some(dir) = params.remove(&b"dir"[..]).filter(|d| !d.is_empty()) {
        attrs.push(("dir", Attr::Str(dir)));
    }
}

fn unknown_param(shown: &str, params: &Params) -> Result<(), String> {
    match params.keys().next() {
        Some(k) => Err(format!(
            "URL '{shown}' contains unknown parameter '{}'",
            String::from_utf8_lossy(k)
        )),
        None => Ok(()),
    }
}

/// The non-empty segments of the URL path `path`, decoded.
fn segments(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|&c| c == b'/')
        .filter(|s| !s.is_empty())
        .map(url::decode)
        .collect()
}

fn is_rev(s: &[u8]) -> bool {
    s.len() == 40 && s.iter().all(u8::is_ascii_hexdigit)
}

/// A `path:` reference to `path`, or a bare absolute path unless `decode`.
/// Runs of slashes become one, and an empty path is the root.
fn path_ref(path: &[u8], decode: bool, params: Params, shown: &str) -> Result<Attrs, String> {
    let mut p = Vec::new();
    for (i, segment) in path.split(|&c| c == b'/').enumerate() {
        let segment = if decode {
            let s = url::decode(segment);
            let shown = String::from_utf8_lossy(&s);
            if s.contains(&b'/') {
                return Err(format!(
                    "URL path component '{shown}' contains '/', which is not allowed in file names"
                ));
            }
            if s.contains(&0) {
                return Err(format!(
                    "URL path component '{shown}' contains NUL byte which is not allowed"
                ));
            }
            s
        } else {
            segment.to_vec()
        };
        if i > 0 && p.last() != Some(&b'/') {
            p.push(b'/');
        }
        p.extend(segment);
    }
    if p.is_empty() {
        p.push(b'/');
    }
    path_attrs(p, params, shown)
}

/// A `path:` reference to `path` as given, with the parameters `params` of
/// the URL shown as `shown`, all of which a path input must support.
pub(crate) fn path_attrs(path: Vec<u8>, mut params: Params, shown: &str) -> Result<Attrs, String> {
    let mut attrs = vec![
        ("type", Attr::Str(b"path".into())),
        ("path", Attr::Str(path)),
    ];
    dir_attr(&mut attrs, &mut params);
    for (k, v) in params {
        let attr = match &k[..] {
            b"narHash" => ("narHash", Attr::Str(v)),
            b"rev" => ("rev", Attr::Str(v)),
            b"lastModified" | b"revCount" => {
                let name = if k == b"revCount" {
                    "revCount"
                } else {
                    "lastModified"
                };
                let Some(i) = int(&v) else {
                    return Err(format!("path URL '{shown}' has invalid parameter '{name}'"));
                };
                (name, Attr::Int(i))
            }
            _ => {
                return Err(format!(
                    "path URL '{shown}' has unsupported parameter '{}'",
                    String::from_utf8_lossy(&k)
                ));
            }
        };
        attrs.push(attr);
    }
    Ok(attrs)
}

/// A GitHub, GitLab or SourceHut reference: `owner/repo`, then a commit
/// hash or a branch or tag name, which may contain slashes.
fn forge_ref(ty: &[u8], path: &[u8], mut params: Params, shown: &str) -> Result<Attrs, String> {
    let segments = segments(path);
    let [owner, repo, rest @ ..] = &segments[..] else {
        return Err(format!("URL '{shown}' is invalid"));
    };
    let (mut ref_, mut rev) = match rest {
        [] => (None, None),
        [s] if is_rev(s) => (None, Some(s.clone())),
        rest => (Some(rest.join(&b'/')), None),
    };
    if let Some(r) = params.remove(&b"ref"[..]) {
        if ref_.is_some() {
            return Err(format!("URL '{shown}' contains multiple branch/tag names"));
        }
        ref_ = Some(r);
    }
    if let Some(r) = params.remove(&b"rev"[..]) {
        let s = String::from_utf8_lossy(&r);
        crate::store::parse_hash(&s, Some(crate::store::HashAlgo::Sha1))
            .map_err(|e| e.to_string())?;
        if rev.is_some() {
            return Err(format!("URL '{shown}' contains multiple commit hashes"));
        }
        rev = Some(r);
    }
    let mut attrs = vec![
        ("type", Attr::Str(ty.into())),
        ("owner", Attr::Str(owner.clone())),
        ("repo", Attr::Str(repo.clone())),
    ];
    dir_attr(&mut attrs, &mut params);
    if let Some(host) = params.remove(&b"host"[..]) {
        if !host
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
        {
            return Err(format!("input '{shown}' contains an invalid instance host"));
        }
        attrs.push(("host", Attr::Str(host)));
    }
    if let Some(h) = params.remove(&b"narHash"[..]) {
        attrs.push(("narHash", Attr::Str(h)));
    }
    unknown_param(shown, &params)?;
    match (ref_, rev) {
        (Some(_), Some(_)) => {
            return Err(format!(
                "input '{shown}' contains both a commit hash and a branch/tag name"
            ));
        }
        (Some(r), None) if !git_ref(&r) => {
            return Err(format!(
                "input '{shown}' contains an invalid branch/tag name"
            ));
        }
        (Some(r), None) => attrs.push(("ref", Attr::Str(r))),
        (None, Some(r)) => attrs.push(("rev", Attr::Str(r))),
        (None, None) => {}
    }
    Ok(attrs)
}

/// Whether `r` is a registry name as written without `flake:`: an ID,
/// then perhaps a branch or tag name and a commit hash.
fn bare_id(r: &[u8]) -> bool {
    let (id, rest) = split_once(r, b'/').map_or((r, None), |(i, r)| (i, Some(r)));
    flake_id(id)
        && rest.is_none_or(|rest| {
            rest.first()
                .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'@')
                && rest
                    .iter()
                    .all(|&c| c.is_ascii_alphanumeric() || b"_.@+/-".contains(&c))
        })
}

fn flake_id(id: &[u8]) -> bool {
    id.first().is_some_and(u8::is_ascii_alphabetic)
        && id
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// A registry reference: an ID, then a commit hash, a branch or tag name,
/// or both. Only the `dir` parameter is kept.
fn indirect_ref(path: &[u8], mut params: Params, shown: &str) -> Result<Attrs, String> {
    let segments = segments(path);
    let [id, rest @ ..] = &segments[..] else {
        return Err(format!("URL '{shown}' is invalid"));
    };
    if !flake_id(id) {
        return Err(format!(
            "'{}' is not a valid flake ID",
            String::from_utf8_lossy(id)
        ));
    }
    let mut attrs = vec![
        ("type", Attr::Str(b"indirect".into())),
        ("id", Attr::Str(id.clone())),
    ];
    let rev = |s: &[u8]| ("rev", Attr::Str(s.to_ascii_lowercase()));
    let not = |s: &[u8], what: &str| {
        format!(
            "in flake URL '{shown}', '{}' is not {what}",
            String::from_utf8_lossy(s)
        )
    };
    match rest {
        [] => {}
        [s] if is_rev(s) => attrs.push(rev(s)),
        [s] if git_ref(s) => attrs.push(("ref", Attr::Str(s.clone()))),
        [s] => return Err(not(s, "a commit hash or branch/tag name")),
        [r, _] if !git_ref(r) => return Err(not(r, "a branch/tag name")),
        [_, s] if !is_rev(s) => return Err(not(s, "a commit hash")),
        [r, s] => {
            attrs.push(("ref", Attr::Str(r.clone())));
            attrs.push(rev(s));
        }
        _ => return Err(format!("URL '{shown}' is invalid")),
    }
    dir_attr(&mut attrs, &mut params);
    Ok(attrs)
}

/// Write `a` with each part percent-encoded afresh, and the port without
/// leading zeros.
fn authority(out: &mut Vec<u8>, a: &url::Authority<'_>) {
    let fresh = |s: &[u8]| url::encode(&url::decode(s), b"");
    if let Some(userinfo) = a.userinfo {
        match split_once(userinfo, b':') {
            Some((user, password)) => {
                out.extend(fresh(user));
                out.push(b':');
                out.extend(fresh(password));
            }
            None => out.extend(fresh(userinfo)),
        }
        out.push(b'@');
    }
    match a.host.strip_prefix(b"[").and_then(|h| h.strip_suffix(b"]")) {
        Some(future) if future.starts_with(b"v") => {
            out.push(b'[');
            out.extend(url::encode(future, b":"));
            out.push(b']');
        }
        Some(ip) => {
            out.push(b'[');
            match ip.windows(3).position(|w| w == b"%25") {
                Some(i) => {
                    out.extend(&ip[..i + 3]);
                    out.extend(fresh(&ip[i + 3..]));
                }
                None => out.extend(ip),
            }
            out.push(b']');
        }
        None => out.extend(fresh(a.host)),
    }
    if let Some(port) = a.port {
        out.extend(format!(":{port}").bytes());
    }
}

/// Whether `r` is a valid Git branch or tag name. Nix stops reading it at
/// the first NUL byte.
fn git_ref(r: &[u8]) -> bool {
    let r = &r[..r.iter().position(|&c| c == 0).unwrap_or(r.len())];
    let has = |s: &[u8]| r.windows(s.len()).any(|w| w == s);
    !r.is_empty()
        && r != b"@"
        && !r.starts_with(b"-")
        && !r.starts_with(b"/")
        && !r.ends_with(b"/")
        && !r.ends_with(b".")
        && !has(b"..")
        && !has(b"//")
        && !has(b"@{")
        && r.iter()
            .all(|&c| c >= 0x20 && c != 0x7f && !b" ~^:?*[\\".contains(&c))
        && r.split(|&c| c == b'/')
            .all(|c| !c.starts_with(b".") && !c.ends_with(b".lock"))
}

/// Whether `r` is a valid Mercurial branch or tag name.
fn hg_ref(r: &[u8]) -> bool {
    r.first()
        .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'@')
        && r.iter()
            .all(|&c| c.is_ascii_alphanumeric() || b"_./@+-".contains(&c))
}

pub fn flake_ref_to_string<'a>(ev: &Eval<'a>, set: Value<'a>) -> R<'a> {
    let set = ev.force_attrs(set)?;
    for e in set.entries {
        match ev.force(e.value)? {
            Value::Str(_) | Value::Int(_) | Value::Bool(_) => {}
            v => {
                return eval_err(format!(
                    "flake reference attribute sets may only contain integers, Booleans, and strings, but attribute '{}' is {}",
                    ev.ctx.name(e.name),
                    v.show_type()
                ));
            }
        }
    }
    match set
        .get(ev.ctx.syms.type_)
        .map(|v| ev.force(v))
        .transpose()?
    {
        None => eval_err("'type' attribute to specify input scheme is required but not provided"),
        Some(Value::Str(ty)) if !ev.ctx.flakes && FLAKE_TYPES.contains(&ty.s) => flakes_disabled(),
        Some(Value::Str(_)) => unimplemented("builtins.flakeRefToString"),
        Some(_) => eval_err("input attribute 'type' is not a string"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects() {
        for r in [
            "https://:0",
            "https://:00",
            "https://:65536",
            "https://:-1",
            "https://h:1:2",
            "https://u@h@h",
            "https://a b",
            "https://h/a b",
            "https://h/é",
            "https://h/a%zz",
            "https://[zz]",
            "https://[V1.x]",
            "https://h#f",
            "https://h?a=%zz",
            "https://h?a=é",
            "https://h?a=b^c",
            "HTTPS://h",
            "ftp://h",
            "hg://h/x",
            "hg+git://h/x",
            "tarball+ftp://h/x",
            "file+git://h/x",
            "file://h/x",
            "git+file://localhost/x",
            "path:/x",
            "github:o/r",
            "flake:nixpkgs",
            "nixpkgs",
            "/x",
            "./x",
            "git+https://h?verifyCommit=0",
            "git+https://h?ref=",
            "git+https://h?ref=%00a",
            "git+https://h?ref=-a",
            "git+https://h?ref=@",
            "git+https://h?ref=a..b",
            "git+https://h?ref=a/.b",
            "git+https://h?ref=a.lock/b",
            "git+https://h?ref=a/",
            "git+https://h?ref=a.",
            "git+https://h?ref=a@%7Bb",
            "git+https://h?ref=a%5Cb",
            "git+https://h?ref=a%01",
            "hg+https://h?ref=",
            "hg+https://h?ref=-a",
            "hg+https://h?ref=a%00",
            "hg+https://h?ref=a%C3%A9",
        ] {
            assert!(url_ref(r.as_bytes(), false).is_err(), "{r}");
        }
    }

    #[test]
    fn git_ref_stops_at_nul() {
        let attrs = url_ref(b"git+https://h?ref=a%00..&ref=b", false).unwrap();
        assert!(
            attrs
                .iter()
                .any(|(n, a)| *n == "ref" && matches!(a, Attr::Str(s) if s == b"a\0.."))
        );
    }

    #[test]
    fn rejects_with_flakes() {
        for r in [
            "github:a",
            "github:a/",
            "github://a/b",
            "github:a b/c",
            "github:a/b#x",
            "github:a/b?foo=1",
            "github:a/b?lastModified=1",
            "github:a/b?rev=xyz",
            "github:a/b?rev=",
            "github:a/b?rev=g123456789abcdef0123456789abcdef01234567",
            "github:a/b?rev=sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "github:a/b/c?ref=d",
            "github:a/b/c?rev=0123456789abcdef0123456789abcdef01234567",
            "github:a/b/0123456789abcdef0123456789abcdef01234567?ref=x",
            "github:a/b/0123456789abcdef0123456789abcdef01234567?rev=0123456789abcdef0123456789abcdef01234567",
            "github:a/b?ref=",
            "github:a/b?ref=a..b",
            "github:a/b/a..b",
            "github:a/b/..",
            "github:a/b?ref=a%7Fb",
            "github:a/b?host=a_b",
            "github:a/b?host=a:1",
            "github:a/b?host=a%00b",
            "gitlab:a",
            "sourcehut:a/b?ref=a..b",
            "path://x",
            "path:/x y",
            "path:/x[y",
            "path:/x?ref=r",
            "path:/x?foo=bar",
            "path:/x?lastModified=",
            "path:/x?lastModified=-1",
            "path:/x?revCount=1x",
            "path:/x?lastModified=99999999999999999999",
            "path:/x%2Fy",
            "path:/x%00y",
            "path:/x#f",
            "/x?foo=1",
            "/x?a=%zz",
            "./x",
            ".",
            "../x",
            "flake:",
            "flake://a",
            "flake:1a",
            "flake:a/.a",
            "flake:a/a%01",
            "flake:a/a%20b",
            "flake:a/a/b",
            "flake:a/a..b/0123456789abcdef0123456789abcdef01234567",
            "flake:a/b/c/d",
            "flake:a?dir=%zz",
            "nixpkgs?ref=r",
            "nixpkgs/",
            "nixpkgs//r",
            "nixpkgs/_a",
            "nixpkgs/a,b",
            "nixpkgs/a%2Fb",
            "nixpkgs/r/s",
            "_a",
            "a.b",
            "nixpkgs#x",
        ] {
            assert!(url_ref(r.as_bytes(), true).is_err(), "{r}");
        }
    }
}
