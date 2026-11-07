//! Mutations that edit whole nodes and tokens of rnix's syntax tree, or
//! insert Nix's keywords and operators between them, so changes to an input
//! stay meaningful Nix more often than byte edits do. They only make some
//! inputs likelier: libFuzzer's byte-level mutations, which can turn any
//! input into any other, run alongside them. In a flake bundle they edit the
//! Nix files it holds, one at a time.

use std::ops::Range;

use ogygia_nix_eval::run_with_stack;
use rnix::TextRange;

struct Rng(u32);

impl Rng {
    fn new(seed: u32) -> Rng {
        Rng(seed | 1)
    }

    fn below(&mut self, n: usize) -> usize {
        // xorshift32
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as usize % n
    }
}

/// The ranges of every node and token in rnix's syntax tree of `text`,
/// which is never empty: the root spans all of it.
fn elements(text: &str) -> Vec<TextRange> {
    run_with_stack(|| {
        rnix::Root::parse(text)
            .syntax()
            .descendants_with_tokens()
            .map(|e| e.text_range())
            .collect()
    })
}

/// One of the parts of `text` holding Nix source, picked at random.
fn nix_part(text: &str, rng: &mut Rng) -> Option<Range<usize>> {
    let parts = crate::bundle::nix_ranges(text);
    (!parts.is_empty()).then(|| parts[rng.below(parts.len())].clone())
}

/// The keywords and operators of Nix, but for `|>` and `<|`, which Nix
/// rejects without an experimental feature the comparison leaves off.
const TOKENS: &[&str] = &[
    "assert", "else", "if", "in", "inherit", "let", "or", "rec", "then", "with", "->", "||", "&&",
    "==", "!=", "<", "<=", ">", ">=", "//", "++", "+", "-", "*", "/", "!", "?", ".", ":", "@",
    "...", "=", ";", ",", "{", "}", "[", "]", "(", ")",
];

fn splice(text: &str, range: TextRange, with: &str) -> String {
    let (start, end) = (usize::from(range.start()), usize::from(range.end()));
    [&text[..start], with, &text[end..]].concat()
}

/// Replace, delete, copy or swap whole nodes and tokens of `text`, or
/// insert a keyword or operator after one.
fn mutate_tree(text: &str, rng: &mut Rng) -> String {
    let ranges = elements(text);
    let a = ranges[rng.below(ranges.len())];
    let b = ranges[rng.below(ranges.len())];
    let piece = &text[b];
    match rng.below(5) {
        0 => splice(text, a, piece),
        1 => splice(text, a, ""),
        2 => splice(text, TextRange::empty(a.end()), &format!(" {piece}")),
        3 => {
            let token = TOKENS[rng.below(TOKENS.len())];
            splice(text, TextRange::empty(a.end()), &format!(" {token} "))
        }
        _ => {
            let (first, second) = if a.start() <= b.start() {
                (a, b)
            } else {
                (b, a)
            };
            if first.end() > second.start() {
                return splice(text, a, piece);
            }
            let between = TextRange::new(first.end(), second.start());
            [
                &text[..usize::from(first.start())],
                &text[second],
                &text[between],
                &text[first],
                &text[usize::from(second.end())..],
            ]
            .concat()
        }
    }
}

/// libFuzzer's custom mutator: mutate the input `data[..size]` in place
/// into at most `max_size` bytes, returning the new size. Half the time,
/// and whenever the input is not UTF-8 or the result would not fit, this is
/// `fallback`, libFuzzer's own mutation, with the same arguments.
pub fn mutate(
    data: &mut [u8],
    size: usize,
    max_size: usize,
    seed: u32,
    fallback: impl FnOnce(&mut [u8], usize, usize) -> usize,
) -> usize {
    let mut rng = Rng::new(seed);
    if rng.below(2) == 0
        && let Ok(text) = std::str::from_utf8(&data[..size])
        && let Some(part) = nix_part(text, &mut rng)
    {
        let out = [
            &text[..part.start],
            mutate_tree(&text[part.clone()], &mut rng).as_str(),
            &text[part.end..],
        ]
        .concat();
        if out.len() <= max_size {
            data[..out.len()].copy_from_slice(out.as_bytes());
            return out.len();
        }
    }
    fallback(data, size, max_size)
}

/// libFuzzer's custom crossover: write a combination of `data1` and
/// `data2` to `out`, truncated to fit, returning its length. When both are
/// UTF-8 it is `data1` with one of its nodes or tokens replaced by one of
/// `data2`'s; otherwise a prefix of one joined to a suffix of the other.
pub fn crossover(data1: &[u8], data2: &[u8], out: &mut [u8], seed: u32) -> usize {
    let mut rng = Rng::new(seed);
    let texts = std::str::from_utf8(data1)
        .ok()
        .zip(std::str::from_utf8(data2).ok());
    let parts = texts.and_then(|(a, b)| {
        let to = nix_part(a, &mut rng)?;
        let from = nix_part(b, &mut rng)?;
        Some((a, to, &b[from]))
    });
    let spliced = match parts {
        Some((a, to, b)) => {
            let into = elements(&a[to.clone()]);
            let from = elements(b);
            let piece = &b[from[rng.below(from.len())]];
            let at = into[rng.below(into.len())] + rnix::TextSize::from(to.start as u32);
            splice(a, at, piece).into_bytes()
        }
        None => {
            let cut1 = rng.below(data1.len() + 1);
            let cut2 = rng.below(data2.len() + 1);
            [&data1[..cut1], &data2[cut2..]].concat()
        }
    };
    let len = spliced.len().min(out.len());
    out[..len].copy_from_slice(&spliced[..len]);
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUTS: &[&str] = &[
        "",
        "1",
        "let a = { b = [ 1 2.5 ]; c = \"x${y}z\"; }; in a.b ++ [ (a.c or null) ]",
        "{ x, ... }@args: ''\n  é ${toString x} ''$\n''",
        "(null} ,; ''",
        "日本語 /* unterminated",
    ];

    fn no_fallback(_: &mut [u8], _: usize, _: usize) -> usize {
        panic!("fell back to libFuzzer's mutation")
    }

    #[test]
    fn tree_mutations_keep_text_valid() {
        for input in INPUTS {
            for seed in 0..500 {
                let mut data = input.as_bytes().to_vec();
                data.resize(4096, 0);
                // Seeds whose first draw picks the fallback leave the input alone.
                let size = mutate(&mut data, input.len(), 4096, seed, |_, size, _| size);
                assert!(size <= 4096);
                std::str::from_utf8(&data[..size])
                    .unwrap_or_else(|e| panic!("{input:?} with seed {seed}: {e}"));
            }
        }
    }

    #[test]
    fn tree_mutations_insert_every_token() {
        let mut missing = TOKENS.to_vec();
        for seed in 0..20000 {
            let out = mutate_tree("1", &mut Rng::new(seed));
            missing.retain(|token| out != format!("1 {token} "));
        }
        assert!(missing.is_empty(), "never inserted: {missing:?}");
    }

    #[test]
    fn mutation_respects_max_size() {
        let input = INPUTS[2];
        for seed in 0..500 {
            let mut data = input.as_bytes().to_vec();
            let max_size = input.len();
            let size = mutate(&mut data, input.len(), max_size, seed, |_, size, _| size);
            assert!(size <= max_size, "seed {seed}");
        }
    }

    #[test]
    fn non_utf8_falls_back() {
        let mut data = vec![0xff, 0xfe, 0, 0];
        for seed in 0..100 {
            assert_eq!(mutate(&mut data, 2, 4, seed, |_, _, _| 3), 3);
        }
        // A UTF-8 input whose first draw picks a tree mutation never falls back.
        let mut data = b"1 + 2".to_vec();
        data.resize(4096, 0);
        let seed = (0..)
            .find(|&s| Rng::new(s).below(2) == 0)
            .expect("some seed picks a tree mutation");
        mutate(&mut data, 5, 4096, seed, no_fallback);
    }

    #[test]
    fn tree_mutations_keep_bundle_directives() {
        let bundle = "#~flake path:@dir(r)#x\n#~tree r path\n#~file flake.nix\n{ outputs = _: { x = [ 1 2 ]; }; }\n#~file flake.lock\n{ }\n";
        let directives = |s: &str| -> Vec<String> {
            s.lines()
                .filter(|l| l.starts_with("#~"))
                .map(str::to_owned)
                .collect()
        };
        for seed in 0..500 {
            let mut data = bundle.as_bytes().to_vec();
            data.resize(4096, 0);
            let size = mutate(&mut data, bundle.len(), 4096, seed, |_, size, _| size);
            let out = std::str::from_utf8(&data[..size]).unwrap();
            assert_eq!(directives(out), directives(bundle), "seed {seed}");
            assert!(out.ends_with("{ }\n"), "seed {seed}: {out:?}");
        }
    }

    #[test]
    fn crossover_fits_and_splices_text() {
        for a in INPUTS {
            for b in INPUTS {
                for seed in 0..50 {
                    let mut out = vec![0; 4096];
                    let len = crossover(a.as_bytes(), b.as_bytes(), &mut out, seed);
                    std::str::from_utf8(&out[..len])
                        .unwrap_or_else(|e| panic!("{a:?} x {b:?} with seed {seed}: {e}"));
                    let mut small = vec![0; 3];
                    assert!(crossover(a.as_bytes(), b.as_bytes(), &mut small, seed) <= 3);
                }
            }
        }
        let mut out = vec![0; 16];
        let len = crossover(&[0xff, 1], &[2, 0xfe], &mut out, 7);
        assert!(len <= 4);
    }
}
