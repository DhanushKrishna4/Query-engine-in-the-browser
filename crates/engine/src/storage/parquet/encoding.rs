//! Parquet's value encodings.
//!
//! A Parquet column is not a buffer of values; it is a buffer of values in
//! whichever encoding the writer thought cheapest, and the writer's choice
//! varies by library, by version and by column. So a reader that handles only
//! PLAIN works on files it wrote itself and fails on everyone else's.
//!
//! Everything here is bit-oriented and **least-significant-bit first**: the
//! first value occupies the low bits of the first byte. That is the opposite of
//! the deprecated `BIT_PACKED` encoding for levels, which is MSB-first and is
//! not implemented -- no current writer emits it, and supporting it would mean
//! two bit orders in one file for no gain.

use crate::error::Result;

use super::thrift::err;

/// Reads bit-packed values, and byte-aligned varints between them.
///
/// The two interleave: the delta encodings write varint headers, then packed
/// data, then more varints. Every packed section happens to end on a byte
/// boundary (miniblocks hold a multiple of 32 values), but `align` makes that
/// explicit rather than assumed.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Bits consumed so far.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, pos: 0 }
    }

    pub fn byte_pos(&self) -> usize {
        self.pos.div_ceil(8)
    }

    pub fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }

    /// Read `n` bits, LSB first.
    pub fn get(&mut self, n: u32) -> Result<u64> {
        if n == 0 {
            return Ok(0);
        }
        // A byte offset of up to 7 plus 57 bits still fits one u64 load; wider
        // reads are split so the shift never drops bits off the top.
        if n > 57 {
            let lo = self.get(32)?;
            let hi = self.get(n - 32)?;
            return Ok(lo | (hi << 32));
        }
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(err("bit-packed data ended early"));
        }
        let byte = self.pos / 8;
        let shift = (self.pos % 8) as u32;
        let mut buf = [0u8; 8];
        let end = (byte + 8).min(self.data.len());
        buf[..end - byte].copy_from_slice(&self.data[byte..end]);
        let word = u64::from_le_bytes(buf);
        self.pos += n as usize;
        Ok((word >> shift) & mask(n))
    }

    pub fn read_varint(&mut self) -> Result<u64> {
        self.align();
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let b = *self
                .data
                .get(self.pos / 8)
                .ok_or_else(|| err("varint ended early"))?;
            self.pos += 8;
            if shift > 63 {
                return Err(err("varint is too long for a 64-bit value"));
            }
            value |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    pub fn read_zigzag(&mut self) -> Result<i64> {
        let n = self.read_varint()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    fn read_byte(&mut self) -> Result<u8> {
        self.align();
        let b = *self
            .data
            .get(self.pos / 8)
            .ok_or_else(|| err("bit reader ended early"))?;
        self.pos += 8;
        Ok(b)
    }
}

fn mask(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// Decode an RLE / bit-packed hybrid stream.
///
/// This carries both definition levels and dictionary indices, which is why it
/// is the most-used decoder in the file. The stream is a sequence of runs, each
/// introduced by a varint whose low bit picks the kind:
///
/// * **bit-packed** -- the header holds the number of *groups of eight* values,
///   each `bit_width` bits;
/// * **RLE** -- the header holds a repeat count, followed by the repeated value
///   in `ceil(bit_width / 8)` bytes.
///
/// A `bit_width` of zero is legal and means every value is zero: a column with
/// a single distinct value needs no index bits at all. That run consumes no
/// value bytes, so a decoder that assumes at least one would desynchronize.
pub fn decode_rle_hybrid(data: &[u8], bit_width: u32, count: usize) -> Result<Vec<u64>> {
    let mut out = Vec::with_capacity(count);
    let mut r = BitReader::new(data);
    let value_bytes = bit_width.div_ceil(8) as usize;

    while out.len() < count {
        let header = r.read_varint()?;
        let is_packed = header & 1 == 1;
        let len = (header >> 1) as usize;
        if len == 0 {
            return Err(err("RLE stream contains a zero-length run"));
        }
        if is_packed {
            // Groups of eight, so the run always ends byte-aligned.
            let values = len.saturating_mul(8);
            for _ in 0..values {
                if out.len() == count {
                    // The final group is padded to eight; the padding is not
                    // data, but its bits have still been consumed.
                    break;
                }
                out.push(r.get(bit_width)?);
            }
            // Skip whatever of the last group was padding.
            let consumed = out.len().min(values);
            let remaining = values - consumed;
            for _ in 0..remaining {
                r.get(bit_width)?;
            }
        } else {
            let mut value = 0u64;
            for i in 0..value_bytes {
                value |= (r.read_byte()? as u64) << (8 * i);
            }
            let take = len.min(count - out.len());
            out.extend(std::iter::repeat_n(value, take));
        }
    }
    Ok(out)
}

/// `DELTA_BINARY_PACKED`, the default integer encoding of Parquet v2.
///
/// A first value, then blocks of miniblocks. Each block subtracts its own
/// minimum delta before packing, so a strictly ascending column -- an id, a
/// timestamp -- packs to nearly nothing: every delta equals the minimum and the
/// residuals are all zero, giving a bit width of zero.
///
/// Returns the values and the byte offset just past the encoded section, which
/// the byte-array encodings need because their data follows the lengths.
pub fn decode_delta_binary_packed(data: &[u8]) -> Result<(Vec<i64>, usize)> {
    let mut r = BitReader::new(data);
    let block_size = r.read_varint()? as usize;
    let miniblocks = r.read_varint()? as usize;
    let total = r.read_varint()? as usize;
    let first = r.read_zigzag()?;

    if miniblocks == 0 || block_size == 0 || !block_size.is_multiple_of(miniblocks) {
        return Err(err(format!(
            "delta encoding declares {block_size} values in {miniblocks} miniblocks"
        )));
    }
    let per_miniblock = block_size / miniblocks;

    let mut out = Vec::with_capacity(total);
    if total > 0 {
        out.push(first);
    }
    let mut prev = first;

    while out.len() < total {
        let min_delta = r.read_zigzag()?;
        let mut widths = Vec::with_capacity(miniblocks);
        for _ in 0..miniblocks {
            widths.push(r.read_byte()? as u32);
        }
        for width in widths {
            if out.len() >= total {
                // A miniblock that holds no values may be omitted entirely --
                // only its bit width is written. Stopping here rather than
                // trusting the count keeps the reader inside the buffer.
                break;
            }
            // A miniblock that holds *any* value is padded to its full width,
            // so the padding has to be consumed even though it is discarded.
            // Stopping at the last real value would leave the reader mid-block
            // and put every following section at the wrong offset -- which for
            // DELTA_BYTE_ARRAY means reading the suffix header out of the
            // middle of the prefix data.
            for _ in 0..per_miniblock {
                if out.len() < total {
                    let residual = r.get(width)? as i64;
                    prev = prev.wrapping_add(min_delta).wrapping_add(residual);
                    out.push(prev);
                } else if r.get(width).is_err() {
                    // A writer that truncated the padding rather than writing
                    // it. Nothing follows that we still need.
                    break;
                }
            }
        }
    }
    r.align();
    Ok((out, r.byte_pos()))
}

/// `DELTA_LENGTH_BYTE_ARRAY`: delta-packed lengths, then the bytes end to end.
pub fn decode_delta_length_byte_array(data: &[u8]) -> Result<Vec<&[u8]>> {
    let (lengths, mut pos) = decode_delta_binary_packed(data)?;
    let mut out = Vec::with_capacity(lengths.len());
    for len in lengths {
        let len = usize::try_from(len).map_err(|_| err("negative string length in Parquet data"))?;
        let end = pos
            .checked_add(len)
            .filter(|e| *e <= data.len())
            .ok_or_else(|| err("string data ended early"))?;
        out.push(&data[pos..end]);
        pos = end;
    }
    Ok(out)
}

/// `DELTA_BYTE_ARRAY`: each value shares a prefix with the one before it.
///
/// Sorted or near-sorted strings -- paths, ids, timestamps as text -- compress
/// well because consecutive values differ only in their tails.
pub fn decode_delta_byte_array(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let (prefixes, after_prefixes) = decode_delta_binary_packed(data)?;
    let (suffixes, after_suffixes) = decode_delta_binary_packed(&data[after_prefixes..])?;
    if prefixes.len() != suffixes.len() {
        return Err(err(format!(
            "delta byte array has {} prefixes and {} suffixes",
            prefixes.len(),
            suffixes.len()
        )));
    }

    let mut pos = after_prefixes + after_suffixes;
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(prefixes.len());
    let mut previous: Vec<u8> = Vec::new();
    for (prefix_len, suffix_len) in prefixes.into_iter().zip(suffixes) {
        let prefix_len = usize::try_from(prefix_len).map_err(|_| err("negative prefix length"))?;
        let suffix_len = usize::try_from(suffix_len).map_err(|_| err("negative suffix length"))?;
        if prefix_len > previous.len() {
            return Err(err(format!(
                "delta byte array shares {prefix_len} bytes with a {}-byte predecessor",
                previous.len()
            )));
        }
        let end = pos
            .checked_add(suffix_len)
            .filter(|e| *e <= data.len())
            .ok_or_else(|| err("delta byte array data ended early"))?;
        let mut value = Vec::with_capacity(prefix_len + suffix_len);
        value.extend_from_slice(&previous[..prefix_len]);
        value.extend_from_slice(&data[pos..end]);
        pos = end;
        previous = value.clone();
        out.push(value);
    }
    Ok(out)
}

/// `BYTE_STREAM_SPLIT`: transposed bytes.
///
/// All the first bytes of every value, then all the second bytes, and so on.
/// Floating-point columns benefit because the exponent bytes of neighbouring
/// values are usually similar while their mantissa bytes are noise -- grouping
/// like with like gives the block compressor something to work with.
pub fn decode_byte_stream_split(data: &[u8], width: usize, count: usize) -> Result<Vec<u8>> {
    if width == 0 || data.len() < width * count {
        return Err(err(format!(
            "byte-stream-split page holds {} bytes for {count} values of {width} bytes",
            data.len()
        )));
    }
    let mut out = vec![0u8; width * count];
    for plane in 0..width {
        let base = plane * count;
        for i in 0..count {
            out[i * width + plane] = data[base + i];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    #[test]
    fn bits_come_out_least_significant_first() {
        // 0b1011_0010 read three bits at a time is 2, 4, 5.
        let data = [0b1011_0010u8];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get(3).unwrap(), 0b010);
        assert_eq!(r.get(3).unwrap(), 0b110);
        assert_eq!(r.get(2).unwrap(), 0b10);
    }

    #[test]
    fn values_may_straddle_byte_boundaries() {
        // Two 12-bit values. The first is all of byte 0 plus the low nibble of
        // byte 1; the second is byte 1's high nibble plus all of byte 2.
        let data = [0x34, 0x12, 0x78, 0x56];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get(12).unwrap(), 0x234);
        assert_eq!(r.get(12).unwrap(), 0x781);
    }

    #[test]
    fn wide_reads_do_not_lose_the_top_bits() {
        let data = u64::MAX.to_le_bytes();
        let mut r = BitReader::new(&data);
        assert_eq!(r.get(64).unwrap(), u64::MAX);

        // A full 64-bit value starting at bit 5, which is past the point where
        // one u64 load can serve the read and the split path takes over.
        for value in [u64::MAX, 1, 0x0123_4567_89ab_cdef, 1 << 63] {
            let staged = (value as u128) << 5;
            let bytes = staged.to_le_bytes();
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.get(5).unwrap(), 0);
            assert_eq!(r.get(64).unwrap(), value, "value {value:#x} at bit offset 5");
        }

        // And a 57-bit read at an offset, the widest the single-load path takes.
        let staged = ((0x01ff_ffff_ffff_ffffu64 & mask(57)) as u128) << 7;
        let bytes = staged.to_le_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.get(7).unwrap(), 0);
        assert_eq!(r.get(57).unwrap(), mask(57));
    }

    #[test]
    fn an_rle_run_repeats_one_value() {
        // Header 20 = (10 << 1) | 0: ten repeats of a one-byte value.
        let mut b = varint(20);
        b.push(7);
        assert_eq!(decode_rle_hybrid(&b, 8, 10).unwrap(), vec![7u64; 10]);
    }

    #[test]
    fn a_bit_packed_run_holds_groups_of_eight() {
        // Header 3 = (1 << 1) | 1: one group of eight, three bits each.
        let values: [u64; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
        let mut packed = 0u64;
        for (i, v) in values.iter().enumerate() {
            packed |= v << (3 * i);
        }
        let mut b = varint(3);
        b.extend(&packed.to_le_bytes()[..3]);
        assert_eq!(decode_rle_hybrid(&b, 3, 8).unwrap(), values.to_vec());
    }

    #[test]
    fn a_zero_bit_width_costs_no_bytes_at_all() {
        // A column with one distinct value: every index is zero, and the RLE
        // run carries no value bytes. A decoder that insisted on reading one
        // would read the next run's header instead.
        let b = varint(2000); // 1000 repeats, RLE
        assert_eq!(decode_rle_hybrid(&b, 0, 1000).unwrap(), vec![0u64; 1000]);
    }

    #[test]
    fn runs_of_both_kinds_mix_in_one_stream() {
        let mut b = varint(8); // RLE: four repeats
        b.push(1);
        b.extend(varint(3)); // packed: one group of eight
        let mut packed = 0u64;
        for i in 0..8u64 {
            packed |= (i % 2) << i;
        }
        b.extend(&packed.to_le_bytes()[..1]);
        let got = decode_rle_hybrid(&b, 1, 12).unwrap();
        assert_eq!(got, vec![1, 1, 1, 1, 0, 1, 0, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn a_truncated_stream_is_an_error() {
        let b = varint(2001); // claims a packed run with no data behind it
        assert!(decode_rle_hybrid(&b, 8, 100).is_err());
        assert!(decode_rle_hybrid(&[], 8, 1).is_err());
        // A zero-length run would loop forever if it were not rejected.
        assert!(decode_rle_hybrid(&varint(0), 8, 1).is_err());
    }

    /// Build a DELTA_BINARY_PACKED stream the way a writer would, so the
    /// decoder is checked against the format rather than against itself.
    fn encode_delta(values: &[i64], block_size: usize, miniblocks: usize) -> Vec<u8> {
        let per = block_size / miniblocks;
        let mut out = Vec::new();
        out.extend(varint(block_size as u64));
        out.extend(varint(miniblocks as u64));
        out.extend(varint(values.len() as u64));
        let first = values[0];
        out.extend(varint(((first << 1) ^ (first >> 63)) as u64));

        let deltas: Vec<i64> = values.windows(2).map(|w| w[1] - w[0]).collect();
        for block in deltas.chunks(block_size) {
            let min = *block.iter().min().unwrap();
            out.extend(varint(((min << 1) ^ (min >> 63)) as u64));
            let mut widths = Vec::new();
            let mut bodies: Vec<(u32, Vec<i64>)> = Vec::new();
            for mb in block.chunks(per) {
                let residuals: Vec<i64> = mb.iter().map(|d| d - min).collect();
                let max = residuals.iter().copied().max().unwrap_or(0) as u64;
                let width = if max == 0 { 0 } else { 64 - max.leading_zeros() };
                widths.push(width as u8);
                bodies.push((width, residuals));
            }
            while widths.len() < miniblocks {
                widths.push(0);
                bodies.push((0, Vec::new()));
            }
            out.extend(&widths);
            // Pack each miniblock at its own width, padded to `per` values.
            for (width, residuals) in bodies {
                if width == 0 {
                    continue;
                }
                let mut bits: Vec<bool> = Vec::new();
                for i in 0..per {
                    let v = residuals.get(i).copied().unwrap_or(0) as u64;
                    for b in 0..width {
                        bits.push((v >> b) & 1 == 1);
                    }
                }
                for chunk in bits.chunks(8) {
                    let mut byte = 0u8;
                    for (i, bit) in chunk.iter().enumerate() {
                        if *bit {
                            byte |= 1 << i;
                        }
                    }
                    out.push(byte);
                }
            }
        }
        out
    }

    #[test]
    fn delta_round_trips_through_a_hand_written_encoder() {
        let cases: Vec<Vec<i64>> = vec![
            vec![42],
            (0..128).collect(),
            (0..1000).map(|i| i * 7).collect(),
            (0..300).map(|i| 1_000_000 - i * 3).collect(),
            vec![5; 256],
            (0..200).map(|i| if i % 2 == 0 { i } else { -i }).collect(),
            vec![i64::MIN / 2, 0, i64::MAX / 2],
        ];
        for values in cases {
            let encoded = encode_delta(&values, 128, 4);
            let (got, _) = decode_delta_binary_packed(&encoded).unwrap();
            assert_eq!(got, values, "round trip failed for {} values", values.len());
        }
    }

    #[test]
    fn an_ascending_column_packs_to_nothing() {
        // Every delta equals the minimum, so every residual is zero and the
        // bit width is zero. This is the case the encoding exists for.
        let values: Vec<i64> = (0..1024).collect();
        let encoded = encode_delta(&values, 128, 4);
        assert!(encoded.len() < 80, "1024 values took {} bytes", encoded.len());
        assert_eq!(decode_delta_binary_packed(&encoded).unwrap().0, values);
    }

    #[test]
    fn byte_stream_split_transposes_back() {
        // Two f32-width values, 0x04030201 and 0x08070605, split into planes.
        let planes = [1u8, 5, 2, 6, 3, 7, 4, 8];
        let got = decode_byte_stream_split(&planes, 4, 2).unwrap();
        assert_eq!(got, vec![1, 2, 3, 4, 5, 6, 7, 8]);

        let doubles = [1.5f64, -2.25, 1e300];
        let mut planes = vec![0u8; 24];
        for (i, d) in doubles.iter().enumerate() {
            for (k, b) in d.to_le_bytes().iter().enumerate() {
                planes[k * 3 + i] = *b;
            }
        }
        let flat = decode_byte_stream_split(&planes, 8, 3).unwrap();
        for (i, want) in doubles.iter().enumerate() {
            let got = f64::from_le_bytes(flat[i * 8..i * 8 + 8].try_into().unwrap());
            assert_eq!(got, *want);
        }
        assert!(decode_byte_stream_split(&planes, 8, 99).is_err());
    }
}
