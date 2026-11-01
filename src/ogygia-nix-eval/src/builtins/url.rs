//! URL syntax checking for fetchers, following RFC 3986 as Nix applies it.

/// The parts of a URI that `parse` returns, still percent-encoded.
pub struct Uri<'s> {
    pub scheme: &'s [u8],
    pub authority: Option<Authority<'s>>,
    pub path: &'s [u8],
}

impl Uri<'_> {
    /// Whether the scheme is `file`, perhaps after a `+`, and there is a host.
    pub fn file_with_host(&self) -> bool {
        let scheme = split_once(self.scheme, b'+').map_or(self.scheme, |(_, s)| s);
        scheme == b"file" && self.authority.as_ref().is_some_and(|a| !a.host.is_empty())
    }
}

pub struct Authority<'s> {
    pub userinfo: Option<&'s [u8]>,
    /// A registered name, or an IP literal with its brackets.
    pub host: &'s [u8],
    pub port: Option<u16>,
}

/// Check that `url` is a URI, and that its port, if any, is in range.
pub fn check(url: &[u8]) -> Result<(), String> {
    parse(url).map(drop)
}

/// Check `url` as [`check`] does, returning the parts before its query.
pub fn parse(url: &[u8]) -> Result<Uri<'_>, String> {
    let invalid = || format!("'{}' is not a valid URL", String::from_utf8_lossy(url));
    let (scheme, rest) = url.split_at(url.iter().position(|&c| c == b':').ok_or_else(invalid)?);
    let rest = &rest[1..];
    if !scheme.first().is_some_and(u8::is_ascii_alphabetic)
        || !scheme
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
    {
        return Err(invalid());
    }
    let (rest, fragment) = split_once(rest, b'#').unwrap_or((rest, b""));
    // Nix also accepts '#' in the fragment.
    if !chars(fragment, |c| pchar(c) || b"/?#".contains(&c)) {
        return Err(invalid());
    }
    let (rest, query) = split_once(rest, b'?').unwrap_or((rest, b""));
    // Nix also accepts '[' and ']' in the query.
    if !chars(query, |c| pchar(c) || b"/?[]".contains(&c)) {
        return Err(invalid());
    }
    let (authority, path) = match rest.strip_prefix(b"//") {
        Some(rest) => {
            let end = rest.iter().position(|&c| c == b'/').unwrap_or(rest.len());
            let a = authority(&rest[..end]).map_err(|e| e.unwrap_or_else(invalid))?;
            (Some(a), &rest[end..])
        }
        None => (None, rest),
    };
    if !chars(path, |c| pchar(c) || c == b'/') {
        return Err(invalid());
    }
    Ok(Uri {
        scheme,
        authority,
        path,
    })
}

/// Split `s` at the first `sep`, if any.
pub fn split_once(s: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let i = s.iter().position(|&c| c == sep)?;
    Some((&s[..i], &s[i + 1..]))
}

/// Fails with `Some` message for a bad port, or `None` for bad syntax.
fn authority(a: &[u8]) -> Result<Authority<'_>, Option<String>> {
    let (userinfo, host_port) = match split_once(a, b'@') {
        Some((userinfo, host_port)) => (Some(userinfo), host_port),
        None => (None, a),
    };
    if !chars(userinfo.unwrap_or_default(), |c| {
        unreserved(c) || sub_delim(c) || c == b':'
    }) {
        return Err(None);
    }
    let (host, host_ok, port) = match host_port.strip_prefix(b"[") {
        Some(rest) => {
            let end = rest.iter().position(|&c| c == b']').ok_or(None)?;
            let port = match &rest[end + 1..] {
                [] => None,
                [b':', port @ ..] => Some(port),
                _ => return Err(None),
            };
            (&host_port[..end + 2], ip_literal(&rest[..end]), port)
        }
        None => {
            let (host, port) = match split_once(host_port, b':') {
                Some((host, port)) => (host, Some(port)),
                None => (host_port, None),
            };
            (host, chars(host, |c| unreserved(c) || sub_delim(c)), port)
        }
    };
    if !host_ok {
        return Err(None);
    }
    let port = match port {
        Some(p) if !p.iter().all(u8::is_ascii_digit) => return Err(None),
        Some(p @ [_, ..]) => {
            let digits = String::from_utf8_lossy(p);
            match digits.trim_start_matches('0').parse::<u16>() {
                Ok(port @ 1..) => Some(port),
                _ => return Err(Some(format!("port '{digits}' is invalid"))),
            }
        }
        _ => None,
    };
    Ok(Authority {
        userinfo,
        host,
        port,
    })
}

fn ip_literal(s: &[u8]) -> bool {
    // Nix accepts only a lowercase 'v'.
    if let Some(future) = s.strip_prefix(b"v") {
        let dot = future.iter().position(|&c| c == b'.');
        return dot.is_some_and(|i| {
            i > 0
                && future[..i].iter().all(u8::is_ascii_hexdigit)
                && future.len() > i + 1
                && future[i + 1..]
                    .iter()
                    .all(|&c| unreserved(c) || sub_delim(c) || c == b':')
        });
    }
    let (addr, zone) = match s.iter().position(|&c| c == b'%') {
        Some(i) => (&s[..i], Some(&s[i..])),
        None => (s, None),
    };
    let zone_ok = zone.is_none_or(|z| {
        z.strip_prefix(b"%25")
            .is_some_and(|id| !id.is_empty() && chars(id, unreserved))
    });
    zone_ok && ipv6(addr)
}

pub fn ipv6(s: &[u8]) -> bool {
    match s.windows(2).position(|w| w == b"::") {
        Some(i) => match (groups(&s[..i], false), groups(&s[i + 2..], true)) {
            (Some(head), Some(tail)) => head + tail <= 7,
            _ => false,
        },
        None => groups(s, true) == Some(8),
    }
}

/// The number of 16-bit groups in `:`-separated `s`, counting a trailing
/// IPv4 address, allowed if `v4_tail`, as two.
fn groups(s: &[u8], v4_tail: bool) -> Option<usize> {
    if s.is_empty() {
        return Some(0);
    }
    let pieces: Vec<&[u8]> = s.split(|&c| c == b':').collect();
    let mut n = 0;
    for (i, p) in pieces.iter().enumerate() {
        if v4_tail && i == pieces.len() - 1 && p.contains(&b'.') {
            ipv4(p).then_some(())?;
            n += 2;
        } else if (1..=4).contains(&p.len()) && p.iter().all(u8::is_ascii_hexdigit) {
            n += 1;
        } else {
            return None;
        }
    }
    Some(n)
}

fn ipv4(s: &[u8]) -> bool {
    let octets: Vec<&[u8]> = s.split(|&c| c == b'.').collect();
    octets.len() == 4
        && octets.iter().all(|o| {
            matches!(o.len(), 1..=3)
                && o.iter().all(u8::is_ascii_digit)
                && (o.len() == 1 || o[0] != b'0')
                && std::str::from_utf8(o)
                    .ok()
                    .and_then(|o| o.parse::<u8>().ok())
                    .is_some()
        })
}

/// Whether every character of `s` is a percent-encoded byte or satisfies
/// `ok`.
pub fn chars(s: &[u8], ok: impl Fn(u8) -> bool) -> bool {
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            if !s
                .get(i + 1..i + 3)
                .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
            {
                return false;
            }
            i += 3;
        } else if ok(s[i]) {
            i += 1;
        } else {
            return false;
        }
    }
    true
}

pub fn unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-._~".contains(&c)
}

fn sub_delim(c: u8) -> bool {
    b"!$&'()*+,;=".contains(&c)
}

pub fn pchar(c: u8) -> bool {
    unreserved(c) || sub_delim(c) || c == b':' || c == b'@'
}

/// `s` with its percent-encoded bytes decoded, where [`chars`] accepts it.
pub fn decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let byte = s
            .get(i + 1..i + 3)
            .filter(|h| s[i] == b'%' && h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match byte {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(s[i]);
                i += 1;
            }
        }
    }
    out
}

/// `s` with every byte that is neither unreserved nor in `keep`
/// percent-encoded.
pub fn encode(s: &[u8], keep: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        if unreserved(c) || keep.contains(&c) {
            out.push(c);
        } else {
            out.extend(format!("%{c:02X}").bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid() {
        for url in [
            "https://u:p:q@h!$&'()*+,;=~_-.:080/a;b=c:@/%41%aB?[]/?#a#b?/:@",
            "https://:65535",
            "https://h:/",
            "https://",
            "https:",
            "file:a",
            "a+b-c.d://h/x",
            "https://h%20b/",
            "https://[::1%25a%41]/",
            "https://[v1.x:y]:80/",
            "https://[::]/",
            "https://[1::]/",
            "https://[1:2:3:4:5:6:7:8]/",
            "https://[1:2:3:4:5:6:7::]/",
            "https://[::1:2:3:4:5:6:7]/",
            "https://[1:2:3:4:5:6:1.2.3.4]/",
            "https://[::ffff:255.0.10.4]/",
            "https://u@[::1]/",
        ] {
            assert_eq!(check(url.as_bytes()), Ok(()), "{url}");
        }
    }

    #[test]
    fn invalid() {
        for url in [
            "file:a =",
            "git:é",
            "a_b://h/x",
            "1a://h/x",
            "https://h/%4",
            "https://h/a%",
            "https://h/%G1",
            "https://h/[",
            "https://h/a]b",
            "https://h#[",
            "https://h#%zz",
            "https://h?%zz",
            "https://h?a b",
            "https://h?{",
            "https://h b/",
            "https://h:x/",
            "https://h:8 0/",
            "https://u@v@h/",
            "https://u[@h/",
            "https://[zz]/",
            "https://[]/",
            "https://[::1/",
            "https://[::1]x/",
            "https://[::1]@h/",
            "https://[V1.x]/",
            "https://[vz.x]/",
            "https://[v1.]/",
            "https://[::1%eth0]/",
            "https://[::1%25]/",
            "https://[::1%25a!]/",
            "https://[1:2:3:4:5:6:7:8:9]/",
            "https://[1:2:3:4:5:6::7:8]/",
            "https://[1:2:3:4:5:6:7:1.2.3.4]/",
            "https://[1::2::3]/",
            "https://[12345::]/",
            "https://[:1]/",
            "https://[1:]/",
            "https://[1.2.3.4]/",
            "https://[::1.2.3]/",
            "https://[::1.2.3.256]/",
            "https://[::01.2.3.4]/",
            "https://[::1.2.3.4:5]/",
            "https://h/\x7f",
        ] {
            assert!(check(url.as_bytes()).is_err(), "{url}");
        }
    }

    #[test]
    fn port_range() {
        for (url, ok) in [
            ("https://h:1/", true),
            ("https://h:00000000000065535/", true),
            ("https://h:0/", false),
            ("https://h:65536/", false),
            ("https://h:4294967296/", false),
        ] {
            assert_eq!(check(url.as_bytes()).is_ok(), ok, "{url}");
        }
    }
}
