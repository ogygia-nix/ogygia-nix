//! The dictionary libFuzzer inserts words from: the words of `nix.dict`,
//! every builtin as `builtins.NAME` and as the global scope spells it, and
//! the identifier-like string literals of ogygia-nix-eval's source.

use std::collections::BTreeSet;

use ogygia_nix_eval::Builtin;
use strum::IntoEnumIterator;

const WORDS: &str = include_str!("../nix.dict");

const LITERALS: &str = include_str!(concat!(env!("OUT_DIR"), "/literals"));

/// The dictionary, in libFuzzer's format.
pub fn dictionary() -> String {
    let mut generated = BTreeSet::new();
    for builtin in Builtin::iter() {
        let name = builtin.name();
        generated.insert(format!("builtins.{name}"));
        if Builtin::in_scope(name, false, true).is_some() {
            generated.insert(name.to_owned());
        } else {
            generated.insert(format!("__{name}"));
        }
    }
    // Builtins are spelt as above, so a bare name the global scope lacks is
    // not added as an undefined variable.
    generated.extend(
        LITERALS
            .lines()
            .filter(|l| l.parse::<Builtin>().is_err())
            .map(str::to_owned),
    );
    let mut dict = WORDS.to_owned();
    for word in generated {
        let entry = format!("\"{word}\"");
        if !WORDS.lines().any(|l| l == entry) {
            dict.push_str(&entry);
            dict.push('\n');
        }
    }
    dict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words() -> Vec<String> {
        dictionary()
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                let inner = l
                    .strip_prefix('"')
                    .and_then(|l| l.strip_suffix('"'))
                    .unwrap_or_else(|| panic!("not a quoted word: {l:?}"));
                unescape(inner).unwrap_or_else(|| panic!("libFuzzer cannot parse {l:?}"))
            })
            .collect()
    }

    /// `word` as libFuzzer reads it: printable ASCII, with `\\`, `\"` and
    /// `\xHH` escapes.
    fn unescape(word: &str) -> Option<String> {
        let mut bytes = Vec::new();
        let mut chars = word.chars();
        while let Some(c) = chars.next() {
            if !(c.is_ascii_graphic() || c == ' ') {
                return None;
            }
            if c != '\\' {
                bytes.push(c as u8);
                continue;
            }
            match chars.next()? {
                'x' => {
                    let hex: String = chars.by_ref().take(2).collect();
                    bytes.push(u8::from_str_radix(&hex, 16).ok()?);
                }
                c @ ('\\' | '"') => bytes.push(c as u8),
                _ => return None,
            }
        }
        String::from_utf8(bytes).ok()
    }

    #[test]
    fn every_word_parses() {
        assert!(words().len() > 600);
    }

    #[test]
    fn names_builtins_as_nix_scopes_them() {
        let words = words();
        let has = |w: &str| words.iter().any(|x| x == w);
        for w in [
            "builtins.foldl'",
            "builtins.add",
            "builtins.map",
            "__add",
            "__foldl'",
            "map",
            "toString",
            "derivation",
        ] {
            assert!(has(w), "missing {w}");
        }
        for w in ["add", "foldl'", "attrNames", "__map"] {
            assert!(!has(w), "has {w}");
        }
    }

    #[test]
    fn has_documented_names_and_source_literals() {
        let words = words();
        let has = |w: &str| words.iter().any(|x| x == w);
        for w in [
            "__overrides",
            "__overrides = ",
            "__functor = ",
            "__findFile",
            "__nixPath = ",
            "outputHashMode = ",
            "__structuredAttrs",
            "type = \"derivation\"",
            "?narHash=",
            "\u{1F600}",
            "aarch64-linux",
        ] {
            assert!(has(w), "missing {w:?}");
        }
        assert!(!words.iter().any(|w| w == "|>" || w == "<|"));
    }

    #[test]
    fn has_no_duplicates() {
        let words = words();
        let unique: BTreeSet<_> = words.iter().collect();
        assert_eq!(unique.len(), words.len());
    }
}
