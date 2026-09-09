//! The Thrift compact protocol, enough of it to read Parquet metadata.
//!
//! Parquet's footer is a Thrift-serialized `FileMetaData`, so a Parquet reader
//! is a Thrift reader first. Only the decoding half is here, and only the
//! compact protocol -- Parquet has used it exclusively since the format was
//! specified, so the binary protocol would be dead code.
//!
//! ## Why the skip path matters more than the read path
//!
//! The format gains fields over time: `min_value`/`max_value` replaced
//! `min`/`max`, column indexes and bloom-filter offsets arrived later, and
//! writers stamp their own extensions in. A reader that does not know a field
//! must step over exactly its bytes and carry on, or every future writer breaks
//! it. That is what [`ThriftReader::skip`] is for, and it is the reason every
//! struct here loops over whatever fields it is given rather than expecting a
//! fixed layout.
//!
//! ## Field ids are deltas, and the delta is per struct
//!
//! A field header carries the *difference* from the previous field id in the
//! same struct, in four bits, which is why most headers cost one byte. Nested
//! structs therefore need a stack: descending into one resets the running id to
//! zero, and returning must restore the outer struct's. Getting that wrong
//! produces field ids that look plausible and are silently wrong, which is a
//! good deal worse than a parse error.

use crate::error::{Diagnostic, Result, Stage};

// Compact-protocol type codes. `BooleanTrue`/`BooleanFalse` are types rather
// than values because a boolean *field* stores its value in the type nibble
// and occupies no bytes of its own.
pub const T_STOP: u8 = 0x00;
pub const T_BOOLEAN_TRUE: u8 = 0x01;
pub const T_BOOLEAN_FALSE: u8 = 0x02;
pub const T_BYTE: u8 = 0x03;
pub const T_I16: u8 = 0x04;
pub const T_I32: u8 = 0x05;
pub const T_I64: u8 = 0x06;
pub const T_DOUBLE: u8 = 0x07;
pub const T_BINARY: u8 = 0x08;
pub const T_LIST: u8 = 0x09;
pub const T_SET: u8 = 0x0a;
pub const T_MAP: u8 = 0x0b;
pub const T_STRUCT: u8 = 0x0c;

/// Guards against a corrupt length driving an enormous allocation or a runaway
/// loop. Nothing in a Parquet footer legitimately nests this deep.
const MAX_DEPTH: usize = 64;

pub struct ThriftReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// Running field id within the current struct.
    last_field_id: i16,
    /// Saved ids of enclosing structs.
    stack: Vec<i16>,
}

impl<'a> ThriftReader<'a> {
    pub fn new(data: &'a [u8]) -> ThriftReader<'a> {
        ThriftReader {
            data,
            pos: 0,
            last_field_id: 0,
            stack: Vec::new(),
        }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn begin_struct(&mut self) {
        self.stack.push(self.last_field_id);
        self.last_field_id = 0;
    }

    pub fn end_struct(&mut self) {
        self.last_field_id = self.stack.pop().unwrap_or(0);
    }

    /// The next field's id and type, or `None` at the struct's STOP byte.
    pub fn next_field(&mut self) -> Result<Option<(i16, u8)>> {
        let header = self.byte()?;
        if header == T_STOP {
            return Ok(None);
        }
        let ty = header & 0x0f;
        let delta = (header & 0xf0) >> 4;
        let id = if delta == 0 {
            // Long form: the id does not fit in four bits, so it follows as a
            // zigzag varint and is absolute rather than relative.
            self.read_i16()?
        } else {
            self.last_field_id + delta as i16
        };
        self.last_field_id = id;
        Ok(Some((id, ty)))
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| err("thrift data ended mid-value"))?;
        self.pos += 1;
        Ok(b)
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.data.len())
            .ok_or_else(|| err(format!("thrift value claims {n} bytes, past the end")))?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// Unsigned LEB128.
    pub fn read_varint(&mut self) -> Result<u64> {
        let mut value: u64 = 0;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            // Ten groups of seven bits is the most a u64 can hold.
            if shift > 63 {
                return Err(err("thrift varint is too long for a 64-bit value"));
            }
            value |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    /// Zigzag: the sign bit moves to the bottom so that small negatives stay
    /// short.
    pub fn read_i64(&mut self) -> Result<i64> {
        let n = self.read_varint()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        let v = self.read_i64()?;
        i32::try_from(v).map_err(|_| err(format!("thrift i32 field holds {v}")))
    }

    pub fn read_i16(&mut self) -> Result<i16> {
        let v = self.read_i64()?;
        i16::try_from(v).map_err(|_| err(format!("thrift i16 field holds {v}")))
    }

    pub fn read_byte(&mut self) -> Result<i8> {
        Ok(self.byte()? as i8)
    }

    pub fn read_double(&mut self) -> Result<f64> {
        let b = self.bytes(8)?;
        Ok(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Borrowed, not copied: statistics and dictionary values are read straight
    /// out of the footer buffer.
    pub fn read_binary(&mut self) -> Result<&'a [u8]> {
        let len = self.read_varint()? as usize;
        self.bytes(len)
    }

    pub fn read_string(&mut self) -> Result<String> {
        let raw = self.read_binary()?;
        Ok(String::from_utf8_lossy(raw).into_owned())
    }

    /// A boolean *field*, whose value lives in the type nibble of its header.
    pub fn read_bool_field(&mut self, ty: u8) -> Result<bool> {
        match ty {
            T_BOOLEAN_TRUE => Ok(true),
            T_BOOLEAN_FALSE => Ok(false),
            // Inside a list there is no header to carry the value, so it comes
            // as its own byte. Writers disagree about whether that byte is
            // 1/2 (the type codes) or 0/1; both readings agree that zero is
            // false and one is true.
            _ => Ok(self.byte()? != 0),
        }
    }

    /// List or set header: element count and element type.
    pub fn read_list_header(&mut self) -> Result<(usize, u8)> {
        let header = self.byte()?;
        let ty = header & 0x0f;
        let short = (header & 0xf0) >> 4;
        // Fifteen is the escape: the real count follows as a varint.
        let len = if short == 15 {
            self.read_varint()? as usize
        } else {
            short as usize
        };
        if len > self.data.len() - self.pos + 1 && ty != T_BOOLEAN_TRUE && ty != T_BOOLEAN_FALSE {
            // Every element costs at least one byte, so a count larger than the
            // bytes remaining is corrupt. Catching it here turns a huge
            // allocation into a diagnostic.
            return Err(err(format!(
                "thrift list claims {len} elements with {} bytes left",
                self.data.len() - self.pos
            )));
        }
        Ok((len, ty))
    }

    /// Step over a value of the given type without interpreting it.
    ///
    /// This is what keeps the reader working against writers newer than it is.
    pub fn skip(&mut self, ty: u8) -> Result<()> {
        self.skip_at(ty, 0)
    }

    fn skip_at(&mut self, ty: u8, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(err("thrift structure nests too deeply"));
        }
        match ty {
            T_BOOLEAN_TRUE | T_BOOLEAN_FALSE => {}
            T_BYTE => {
                self.byte()?;
            }
            T_I16 | T_I32 | T_I64 => {
                self.read_varint()?;
            }
            T_DOUBLE => {
                self.bytes(8)?;
            }
            T_BINARY => {
                self.read_binary()?;
            }
            T_LIST | T_SET => {
                let (len, elem) = self.read_list_header()?;
                for _ in 0..len {
                    self.skip_at(elem, depth + 1)?;
                }
            }
            T_MAP => {
                let len = self.read_varint()? as usize;
                if len > 0 {
                    let kinds = self.byte()?;
                    let (k, v) = ((kinds & 0xf0) >> 4, kinds & 0x0f);
                    for _ in 0..len {
                        self.skip_at(k, depth + 1)?;
                        self.skip_at(v, depth + 1)?;
                    }
                }
            }
            T_STRUCT => {
                self.begin_struct();
                while let Some((_, field_ty)) = self.next_field()? {
                    self.skip_at(field_ty, depth + 1)?;
                }
                self.end_struct();
            }
            other => return Err(err(format!("unknown thrift type code {other}"))),
        }
        Ok(())
    }
}

pub(crate) fn err(msg: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Stage::Execute, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode an unsigned LEB128, so tests can build byte sequences readably.
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

    fn zigzag(v: i64) -> Vec<u8> {
        varint(((v << 1) ^ (v >> 63)) as u64)
    }

    #[test]
    fn varints_round_trip() {
        for v in [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            let bytes = varint(v);
            assert_eq!(ThriftReader::new(&bytes).read_varint().unwrap(), v, "{v}");
        }
    }

    #[test]
    fn zigzag_keeps_small_negatives_short() {
        for v in [0i64, -1, 1, -2, 2, 63, -64, i32::MIN as i64, i64::MIN, i64::MAX] {
            let bytes = zigzag(v);
            assert_eq!(ThriftReader::new(&bytes).read_i64().unwrap(), v, "{v}");
        }
        // The point of zigzag: -1 is one byte, not ten.
        assert_eq!(zigzag(-1).len(), 1);
        assert_eq!(varint(u64::MAX).len(), 10);
    }

    #[test]
    fn field_ids_accumulate_by_delta() {
        // Three fields at ids 1, 2 and 5, each an i32.
        let mut b = vec![0x15];
        b.extend(zigzag(10));
        b.push(0x15);
        b.extend(zigzag(20));
        b.push(0x35);
        b.extend(zigzag(30));
        b.push(T_STOP);

        let mut r = ThriftReader::new(&b);
        r.begin_struct();
        let mut seen = Vec::new();
        while let Some((id, ty)) = r.next_field().unwrap() {
            assert_eq!(ty, T_I32);
            seen.push((id, r.read_i32().unwrap()));
        }
        r.end_struct();
        assert_eq!(seen, vec![(1, 10), (2, 20), (5, 30)]);
    }

    #[test]
    fn a_delta_of_zero_means_the_id_follows() {
        // A jump of more than 15 cannot fit in the nibble.
        let mut b = vec![0x05];
        b.extend(zigzag(400));
        b.extend(zigzag(7));
        b.push(T_STOP);

        let mut r = ThriftReader::new(&b);
        r.begin_struct();
        assert_eq!(r.next_field().unwrap(), Some((400, T_I32)));
        assert_eq!(r.read_i32().unwrap(), 7);
        assert_eq!(r.next_field().unwrap(), None);
    }

    #[test]
    fn nested_structs_restore_the_outer_field_id() {
        // Outer field 3 is a struct containing field 1; the field after it is
        // outer field 4. Without a stack the delta would resume from 1 and the
        // trailing field would be misread as id 2.
        let mut b = vec![0x3c]; // delta 3, struct
        b.push(0x15); // inner field 1, i32
        b.extend(zigzag(99));
        b.push(T_STOP); // end inner
        b.push(0x15); // delta 1 from 3 -> field 4, i32
        b.extend(zigzag(42));
        b.push(T_STOP);

        let mut r = ThriftReader::new(&b);
        r.begin_struct();
        let (id, ty) = r.next_field().unwrap().unwrap();
        assert_eq!((id, ty), (3, T_STRUCT));
        r.skip(ty).unwrap();
        assert_eq!(r.next_field().unwrap(), Some((4, T_I32)));
        assert_eq!(r.read_i32().unwrap(), 42);
    }

    #[test]
    fn booleans_ride_in_the_type_nibble() {
        let b = vec![0x11, 0x22, T_STOP];
        let mut r = ThriftReader::new(&b);
        r.begin_struct();
        let (id, ty) = r.next_field().unwrap().unwrap();
        assert_eq!(id, 1);
        assert!(r.read_bool_field(ty).unwrap());
        let (id, ty) = r.next_field().unwrap().unwrap();
        assert_eq!(id, 3);
        assert!(!r.read_bool_field(ty).unwrap());
    }

    #[test]
    fn short_and_long_list_headers() {
        // Three i32s, count in the nibble.
        let mut b = vec![0x35];
        for v in [1, 2, 3] {
            b.extend(zigzag(v));
        }
        let mut r = ThriftReader::new(&b);
        assert_eq!(r.read_list_header().unwrap(), (3, T_I32));

        // Twenty i32s: the nibble escapes to 15 and the count follows.
        let mut b = vec![0xf5];
        b.extend(varint(20));
        for v in 0..20 {
            b.extend(zigzag(v));
        }
        let mut r = ThriftReader::new(&b);
        assert_eq!(r.read_list_header().unwrap(), (20, T_I32));
        for v in 0..20 {
            assert_eq!(r.read_i32().unwrap(), v);
        }
    }

    #[test]
    fn skipping_steps_over_exactly_the_right_bytes() {
        // A struct with a binary, a list of structs, and a double, followed by
        // a sentinel field that must still be readable.
        let mut b = vec![0x18]; // field 1, binary
        b.extend(varint(3));
        b.extend(b"abc");
        b.push(0x19); // field 2, list
        b.push(0x2c); // 2 structs
        for _ in 0..2 {
            b.push(0x15);
            b.extend(zigzag(1));
            b.push(T_STOP);
        }
        b.push(0x17); // field 3, double
        b.extend(1.5f64.to_le_bytes());
        b.push(0x15); // field 4, i32
        b.extend(zigzag(777));
        b.push(T_STOP);

        let mut r = ThriftReader::new(&b);
        r.begin_struct();
        for _ in 0..3 {
            let (_, ty) = r.next_field().unwrap().unwrap();
            r.skip(ty).unwrap();
        }
        assert_eq!(r.next_field().unwrap(), Some((4, T_I32)));
        assert_eq!(r.read_i32().unwrap(), 777, "skip consumed the wrong length");
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let mut b = vec![0x18];
        b.extend(varint(10));
        b.extend(b"abc"); // claims ten bytes, supplies three
        let mut r = ThriftReader::new(&b);
        r.begin_struct();
        let (_, ty) = r.next_field().unwrap().unwrap();
        assert!(r.skip(ty).is_err());

        // An empty buffer ends the same way.
        assert!(ThriftReader::new(&[]).next_field().is_err());
    }

    #[test]
    fn a_corrupt_list_count_does_not_allocate() {
        // Claims four billion elements in a nine-byte buffer.
        let mut b = vec![0xf5];
        b.extend(varint(4_000_000_000));
        assert!(ThriftReader::new(&b).read_list_header().is_err());
    }

    #[test]
    fn doubles_are_little_endian() {
        let b = 1234.5678f64.to_le_bytes();
        assert_eq!(ThriftReader::new(&b).read_double().unwrap(), 1234.5678);
    }
}
