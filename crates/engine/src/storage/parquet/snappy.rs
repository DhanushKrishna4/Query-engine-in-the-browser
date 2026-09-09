//! Snappy decompression, raw format.
//!
//! Parquet compresses each page independently and stores the result *without*
//! Snappy's stream framing -- no stream identifier, no per-chunk CRC, just the
//! raw block format: a varint uncompressed length followed by a sequence of
//! literals and back-references. Reaching for a framed decoder here would fail
//! on the very first byte.
//!
//! Only decompression is implemented. Nothing in this engine writes Parquet.
//!
//! ## The overlap case
//!
//! A back-reference may name an offset smaller than its own length, which means
//! it reads bytes the same copy is still writing. That is not a corruption to
//! guard against -- it is how Snappy encodes runs, and `x` repeated 300 times
//! compresses to a literal `x` followed by copies of length 64 at offset 1. So
//! the overlapping case must be copied one byte at a time, in order, and only
//! the non-overlapping case may use a block move.

use crate::error::Result;

use super::thrift::err;

/// Decompress a raw Snappy block.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    let (expected, mut pos) = read_length(input)?;
    let mut out: Vec<u8> = Vec::with_capacity(expected);

    while pos < input.len() {
        let tag = input[pos];
        pos += 1;
        match tag & 0b11 {
            0 => {
                // Literal. The top six bits hold length-1, except that 60..=63
                // mean "that many minus 59 bytes of little-endian length
                // follow" -- the escape that lets a literal exceed 60 bytes.
                let n = (tag >> 2) as usize;
                let len = if n < 60 {
                    n + 1
                } else {
                    let extra = n - 59;
                    let raw = take(input, pos, extra)?;
                    pos += extra;
                    let mut v = 0usize;
                    for (i, b) in raw.iter().enumerate() {
                        v |= (*b as usize) << (8 * i);
                    }
                    v + 1
                };
                out.extend_from_slice(take(input, pos, len)?);
                pos += len;
            }
            1 => {
                // One-byte offset: three bits of length in the tag, and the
                // offset's top three bits too, which is why this form covers
                // the short nearby repeats that dominate real data.
                let len = 4 + ((tag >> 2) & 0b111) as usize;
                let hi = (tag >> 5) as usize;
                let lo = *input.get(pos).ok_or_else(truncated)? as usize;
                pos += 1;
                copy(&mut out, (hi << 8) | lo, len)?;
            }
            2 => {
                let len = (tag >> 2) as usize + 1;
                let b = take(input, pos, 2)?;
                pos += 2;
                copy(&mut out, u16::from_le_bytes([b[0], b[1]]) as usize, len)?;
            }
            _ => {
                let len = (tag >> 2) as usize + 1;
                let b = take(input, pos, 4)?;
                pos += 4;
                copy(
                    &mut out,
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize,
                    len,
                )?;
            }
        }
    }

    if out.len() != expected {
        return Err(err(format!(
            "snappy block declared {expected} bytes and produced {}",
            out.len()
        )));
    }
    Ok(out)
}

/// The uncompressed length, and where the compressed data starts.
fn read_length(input: &[u8]) -> Result<(usize, usize)> {
    let mut value = 0usize;
    let mut shift = 0;
    for (i, b) in input.iter().enumerate() {
        if shift > 31 {
            return Err(err("snappy length prefix is malformed"));
        }
        value |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            return Ok((value, i + 1));
        }
        shift += 7;
    }
    Err(err("snappy block is empty or has no length prefix"))
}

fn take(input: &[u8], pos: usize, n: usize) -> Result<&[u8]> {
    input
        .get(pos..pos + n)
        .ok_or_else(|| err(format!("snappy block ends mid-element, wanted {n} bytes")))
}

fn truncated() -> crate::error::Diagnostic {
    err("snappy block ends mid-element")
}

fn copy(out: &mut Vec<u8>, offset: usize, len: usize) -> Result<()> {
    if offset == 0 || offset > out.len() {
        return Err(err(format!(
            "snappy back-reference reaches {offset} bytes back with {} written",
            out.len()
        )));
    }
    let start = out.len() - offset;
    if offset >= len {
        // Source and destination do not overlap, so a block move is safe.
        out.extend_from_within(start..start + len);
    } else {
        // They do overlap, and that is deliberate: this is how a run is
        // encoded. Copying byte by byte is what repeats the pattern.
        for i in 0..len {
            let b = out[start + i];
            out.push(b);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Vectors produced by `pyarrow.compress(raw, codec='snappy')`, so the
    /// decoder is checked against a real encoder rather than against my own
    /// reading of the specification.
    #[test]
    fn matches_a_real_encoder() {
        let cases: &[(&str, &str)] = &[
            // empty
            ("", "00"),
            // a bare literal
            ("68656c6c6f", "051068656c6c6f"),
            // "ab" forty times: a literal then an overlapping copy at offset 2
            (
                "6162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162",
                "50046162fe0200360200",
            ),
            // 300 x's: overlapping copies at offset 1, the run encoding
            (
                &"78".repeat(300),
                "ac020078fe0100fe0100fe0100fe0100aa0100",
            ),
            // incompressible: one long literal using the 60+ escape
            (
                "0b30557a9fc4e913385d82a7ccf11b40658aafd4f923486d92b7dc062b50759abfe40e33587da2c7ec163b6085aacff41e43688db2d701264b7095badf092e53789dc2e711365b80a5caef193e6388add2f721466b90b5da04294e7398bde20c31567ba0c5ea14395e83a8cdf21c41668bb0d5fa24496e93b8dd072c51769bc0e50f34597ea3c8ed173c6186abd0f51f44698eb3d802274c7196bbe00a2f54799ec3e812375c81a6cbf01a3f6489aed3f822476c91b6db052a4f7499bee30d32577ca1c6eb153a5f",
                "c801f0c70b30557a9fc4e913385d82a7ccf11b40658aafd4f923486d92b7dc062b50759abfe40e33587da2c7ec163b6085aacff41e43688db2d701264b7095badf092e53789dc2e711365b80a5caef193e6388add2f721466b90b5da04294e7398bde20c31567ba0c5ea14395e83a8cdf21c41668bb0d5fa24496e93b8dd072c51769bc0e50f34597ea3c8ed173c6186abd0f51f44698eb3d802274c7196bbe00a2f54799ec3e812375c81a6cbf01a3f6489aed3f822476c91b6db052a4f7499bee30d32577ca1c6eb153a5f",
            ),
        ];
        for (raw, compressed) in cases {
            let want = hex(raw);
            let got = decompress(&hex(compressed)).unwrap();
            assert_eq!(got, want, "mismatch for {} bytes", want.len());
        }
    }

    #[test]
    fn a_long_repeated_phrase_uses_two_byte_offsets() {
        // "the quick brown fox..." eight times, which encodes as a literal and
        // then copies further back than a one-byte offset can reach.
        let phrase = b"the quick brown fox jumps over the lazy dog. ";
        let want: Vec<u8> = phrase.iter().cycle().take(phrase.len() * 8).copied().collect();
        let compressed = hex("e8027874686520717569636b2062726f776e20666f78206a756d7073206f76657220011f206c617a7920646f672e050efe2d00fe2d00fe2d00fe2d00da2d00");
        assert_eq!(decompress(&compressed).unwrap(), want);
    }

    #[test]
    fn a_declared_length_that_does_not_match_is_an_error() {
        // Claims 99 bytes, supplies a five-byte literal.
        let mut b = vec![99];
        b.extend(hex("1068656c6c6f"));
        assert!(decompress(&b).is_err());
    }

    #[test]
    fn a_back_reference_before_the_start_is_an_error() {
        // Copy at offset 8 with nothing written yet.
        let b = vec![0x08, 0x02, 0x08, 0x00];
        assert!(decompress(&b).is_err());
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        // A literal claiming twenty bytes with three supplied.
        let b = vec![20, (19 << 2), b'a', b'b', b'c'];
        assert!(decompress(&b).is_err());
        assert!(decompress(&[]).is_err());
        // A length prefix with the continuation bit set and nothing after it.
        assert!(decompress(&[0x80]).is_err());
    }

    #[test]
    fn an_empty_block_decompresses_to_nothing() {
        assert!(decompress(&[0x00]).unwrap().is_empty());
    }
}
