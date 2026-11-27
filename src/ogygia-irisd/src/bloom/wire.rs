//! Shared wire format for bloom filter serialization.
//!
//! Wire layout:
//! ```text
//! [8 bytes]    SipHash of the BloomHeader (BE u64)
//! [4 bytes]    BloomHeader::num_hashes (LE u32)
//! [8 bytes]    BloomHeader::num_bits (LE u64)
//! [N×8 bytes]  BE u64 words of the bit vector
//! ```
//!
//! The header hash provides naive versioning: `BloomHeader` derives
//! `Hash`, so if the struct definition changes the receiver's hash of
//! the deserialized header won't match the wire hash.

use std::hash::Hash;
use std::hash::Hasher;

use anyhow::Context;
use anyhow::Result;
use siphasher::sip::SipHasher;

const HEADER_LEN: usize = 12;

/// Bloom filter wire format header.
#[derive(Debug, Hash, PartialEq)]
pub struct BloomHeader {
    pub num_hashes: u32,
    pub num_bits: u64,
}

impl BloomHeader {
    /// Compute a deterministic hash via the `Hash` impl (used for wire versioning).
    fn sip_hash(&self) -> u64 {
        let mut hasher = SipHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0; HEADER_LEN];
        bytes[..4].copy_from_slice(&self.num_hashes.to_le_bytes());
        bytes[4..].copy_from_slice(&self.num_bits.to_le_bytes());
        bytes
    }

    fn from_bytes(bytes: &[u8; HEADER_LEN]) -> Self {
        Self {
            num_hashes: u32::from_le_bytes(*bytes.first_chunk().unwrap()),
            num_bits: u64::from_le_bytes(*bytes.last_chunk().unwrap()),
        }
    }
}

/// Serialize a bloom header and bit-vector body into the wire format.
///
/// Consumes the word iterator directly — no intermediate collection.
pub fn serialize(header: &BloomHeader, words: impl Iterator<Item = u64>) -> Vec<u8> {
    let hash = header.sip_hash();
    let word_count = (header.num_bits as usize).div_ceil(64);

    let mut buf = Vec::with_capacity(8 + HEADER_LEN + word_count * 8);
    buf.extend_from_slice(&hash.to_be_bytes());
    buf.extend_from_slice(&header.to_bytes());
    for word in words {
        buf.extend_from_slice(&word.to_be_bytes());
    }
    buf
}

/// Deserialize a bloom from the wire format.
///
/// Validates the header hash (format versioning) and internal
/// consistency (body length matches `header.num_bits`). Returns the
/// parsed header and u64 words for the caller to validate against
/// local expectations.
pub fn deserialize(data: &[u8]) -> Result<(BloomHeader, Vec<u64>)> {
    anyhow::ensure!(data.len() >= 8, "bloom data too short for header hash");

    let wire_hash = u64::from_be_bytes(
        data[..8]
            .try_into()
            .context("bloom data too short for header hash")?,
    );

    let (header_bytes, body) = data[8..]
        .split_first_chunk::<HEADER_LEN>()
        .context("failed to deserialize bloom header: data too short")?;
    let header = BloomHeader::from_bytes(header_bytes);

    let computed_hash = header.sip_hash();
    anyhow::ensure!(
        computed_hash == wire_hash,
        "bloom header hash mismatch (incompatible wire format version): \
         wire {:#018x}, local {:#018x}",
        wire_hash,
        computed_hash,
    );

    anyhow::ensure!(
        body.len().is_multiple_of(8),
        "bloom body not aligned to u64"
    );

    let words: Vec<u64> = body
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_be_bytes(*c))
        .collect();

    let expected_words = (header.num_bits as usize).div_ceil(64);
    anyhow::ensure!(
        words.len() == expected_words,
        "bloom body length mismatch: {} words but header declares {} bits ({} words)",
        words.len(),
        header.num_bits,
        expected_words,
    );

    Ok((header, words))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_hash_detects_mismatch() {
        let header = BloomHeader {
            num_hashes: 7,
            num_bits: 128,
        };
        let data = serialize(&header, [0u64, 0].into_iter());

        // Corrupt the header hash.
        let mut bad = data.clone();
        bad[0] ^= 0xFF;
        let err = deserialize(&bad).unwrap_err();
        assert!(
            err.to_string().contains("header hash mismatch"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn wire_bytes_pinned() {
        let cases: [(BloomHeader, [u8; 12]); 3] = [
            (
                BloomHeader {
                    num_hashes: 0,
                    num_bits: 0,
                },
                [0; 12],
            ),
            (
                BloomHeader {
                    num_hashes: 0x0102_0304,
                    num_bits: 0x0001_0203,
                },
                [4, 3, 2, 1, 3, 2, 1, 0, 0, 0, 0, 0],
            ),
            (
                BloomHeader {
                    num_hashes: u32::MAX,
                    num_bits: 0,
                },
                [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
        ];
        for (header, header_bytes) in cases {
            let data = serialize(&header, std::iter::empty());
            assert_eq!(data[8..], header_bytes, "{header:?}");
        }

        // Too large to serialize with a body; decode it and stop at the
        // body length check, whose message echoes the decoded num_bits.
        let header = BloomHeader {
            num_hashes: 3,
            num_bits: 0x0506_0708_090A_0B0C,
        };
        let mut data = header.sip_hash().to_be_bytes().to_vec();
        data.extend_from_slice(&[3, 0, 0, 0, 0x0C, 0x0B, 0x0A, 9, 8, 7, 6, 5]);
        let err = deserialize(&data).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("declares {} bits", header.num_bits)),
            "unexpected error: {}",
            err
        );

        let header = BloomHeader {
            num_hashes: 7,
            num_bits: 64,
        };
        let data = serialize(&header, [0x0102_0304_0506_0708u64].into_iter());
        let expected = [
            0x20, 0x27, 0x86, 0x54, 0x04, 0xB6, 0xA4, 0x94, //
            7, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, //
            1, 2, 3, 4, 5, 6, 7, 8,
        ];
        assert_eq!(data, expected);
        let (parsed_header, parsed_words) = deserialize(&expected).unwrap();
        assert_eq!(parsed_header, header);
        assert_eq!(parsed_words, [0x0102_0304_0506_0708]);
    }

    #[test]
    fn short_header() {
        let header = BloomHeader {
            num_hashes: 7,
            num_bits: 0,
        };
        let data = serialize(&header, std::iter::empty());
        for len in 8..data.len() {
            let err = deserialize(&data[..len]).unwrap_err();
            assert!(
                err.to_string()
                    .contains("failed to deserialize bloom header"),
                "unexpected error: {}",
                err
            );
        }
    }

    #[test]
    fn roundtrip() {
        let header = BloomHeader {
            num_hashes: 7,
            num_bits: 192,
        };
        let words = vec![0xDEAD_BEEF_CAFE_BABEu64, 0x1234_5678_9ABC_DEF0, 0];
        let data = serialize(&header, words.iter().copied());
        let (parsed_header, parsed_words) = deserialize(&data).unwrap();
        assert_eq!(parsed_header, header);
        assert_eq!(parsed_words, words);
    }

    #[test]
    fn body_length_mismatch() {
        let header = BloomHeader {
            num_hashes: 7,
            num_bits: 64, // expects 1 word
        };

        // Craft a payload where the header declares 64 bits (1 word)
        // but the body contains 2 words.
        let hash = header.sip_hash();
        let header_bytes = header.to_bytes();
        let mut bad = Vec::new();
        bad.extend_from_slice(&hash.to_be_bytes());
        bad.extend_from_slice(&header_bytes);
        bad.extend_from_slice(&1u64.to_be_bytes());
        bad.extend_from_slice(&2u64.to_be_bytes());

        let err = deserialize(&bad).unwrap_err();
        assert!(
            err.to_string().contains("body length mismatch"),
            "unexpected error: {}",
            err
        );
    }
}
