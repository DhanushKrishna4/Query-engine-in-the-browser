//! Column encodings, and predicates evaluated without decoding them.
//!
//! A columnar engine's advantage is that a column is a contiguous run of one
//! type, which means it compresses in ways a row does not. Four encodings, each
//! for a different shape of data:
//!
//! | | for | |
//! | --- | --- | --- |
//! | [`Dictionary`] | low-cardinality text | codes into a sorted dictionary |
//! | [`RunLength`] | sorted or low-entropy columns | one entry per run |
//! | [`BitPacked`] | small integer ranges | `value - min` in as few bits as fit |
//! | [`FrameOfReference`] | clustered numerics that drift | per-block minimum, then bit-packed |
//!
//! ## The point is not the bytes
//!
//! Compression is worth something on its own -- less memory, fewer cache
//! misses -- but the reason these are here is that a predicate can often be
//! answered *on the encoded form*, which is a different kind of saving
//! altogether:
//!
//!   * **Dictionary.** `city = 'London'` becomes one string comparison against
//!     the dictionary and then a scan of `u32` codes. A million string
//!     comparisons become a million integer ones, and the dictionary is
//!     usually a handful of entries. Because the dictionary is sorted, `<` and
//!     `>` work on codes too.
//!   * **Run-length.** One comparison per *run* rather than per row. A column
//!     with a thousand runs over a million rows costs a thousand comparisons.
//!   * **Bit-packed and frame-of-reference.** Each block carries its own
//!     minimum and width, so a block whose whole range falls outside the
//!     predicate is skipped without unpacking -- a zone map at block
//!     granularity, underneath the row-group one.
//!
//! ## Order-preserving codes
//!
//! The dictionary is sorted, which is what lets a range predicate be answered
//! by comparing codes. It costs a sort at encode time and it is the difference
//! between a dictionary that accelerates one operator and one that accelerates
//! all of them.
//!
//! NULLs are never encoded. They live in the column's validity bitmap, exactly
//! as they do for a plain column, and every decoder here produces values for
//! them that the bitmap then masks -- so a NULL never has to be representable
//! in the encoded domain.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::parser::ast::BinaryOperator;
use crate::storage::bitmap::Bitmap;
use crate::storage::column::{Column, ColumnData, StringColumn};
use crate::types::{self, DataType, ScalarValue};

/// Values per block for the block-wise encodings. A block is the unit that
/// carries its own reference and width, and the unit a predicate can skip.
///
/// 1024 is small enough that a block's range is tight on clustered data and
/// large enough that the per-block header is a rounding error.
pub const BLOCK: usize = 1024;

/// How much smaller an encoding must be before it is worth using.
///
/// Encoded columns cost a decode step whenever something needs the plain
/// values, so a 5% saving is not worth having. This is the same kind of
/// threshold as the index scan's selectivity cap: a rewrite that barely pays
/// is a rewrite that costs more than it looks.
const MIN_COMPRESSION: f64 = 1.2;

#[derive(Debug, Clone)]
pub enum Encoded {
    /// Codes into a sorted dictionary of distinct values.
    Dictionary {
        codes: Vec<u32>,
        values: StringColumn,
    },
    /// One entry per run of equal values, as (value index, run end).
    ///
    /// Run *ends* rather than lengths: a binary search over ends answers "which
    /// run holds row n" directly, which is what random access needs.
    ///
    /// A whole `Column` rather than bare `ColumnData`, because a run of NULLs
    /// is a run like any other and its validity has to survive -- storing only
    /// the data decodes a run of NULLs as a run of placeholder zeros.
    RunLength {
        values: Box<Column>,
        ends: Vec<u32>,
    },
    /// `value - reference` packed into `bit_width` bits, one reference for the
    /// whole column.
    BitPacked {
        reference: i64,
        bit_width: u32,
        packed: Vec<u64>,
        len: usize,
        data_type: DataType,
    },
    /// The same, but with a reference and width per block, so a column that
    /// drifts across a wide range still packs tightly.
    FrameOfReference {
        blocks: Vec<Block>,
        packed: Vec<u64>,
        len: usize,
        data_type: DataType,
    },
}

/// One block's header: where its values start, what they are relative to, and
/// how wide they are.
#[derive(Debug, Clone, Copy)]
pub struct Block {
    pub reference: i64,
    pub bit_width: u32,
    /// Offset into the packed bit stream, in bits.
    pub bit_offset: usize,
}

impl Encoded {
    pub fn name(&self) -> &'static str {
        match self {
            Encoded::Dictionary { .. } => "dictionary",
            Encoded::RunLength { .. } => "run-length",
            Encoded::BitPacked { .. } => "bit-packed",
            Encoded::FrameOfReference { .. } => "frame-of-reference",
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Encoded::Dictionary { codes, .. } => codes.len(),
            Encoded::RunLength { ends, .. } => ends.last().copied().unwrap_or(0) as usize,
            Encoded::BitPacked { len, .. } | Encoded::FrameOfReference { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn byte_size(&self) -> usize {
        match self {
            Encoded::Dictionary { codes, values } => {
                codes.len() * 4 + values.total_bytes() + values.len() * 4
            }
            Encoded::RunLength { values, ends } => values.byte_size() + ends.len() * 4,
            Encoded::BitPacked { packed, .. } => packed.len() * 8 + 16,
            Encoded::FrameOfReference { blocks, packed, .. } => {
                packed.len() * 8 + blocks.len() * 24
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Choose and apply an encoding, or return `None` if none of them pays.
///
/// The choice is made on the data's own shape rather than on a declaration:
/// count the distinct values and the runs, measure what each candidate would
/// cost, and take the smallest that beats [`MIN_COMPRESSION`].
pub fn encode(column: &Column) -> Option<Encoded> {
    let plain = column.data.byte_size();
    if column.is_empty() || plain == 0 {
        return None;
    }

    let candidates: Vec<Encoded> = [
        dictionary(column),
        run_length(column),
        bit_packed(column),
        frame_of_reference(column),
    ]
    .into_iter()
    .flatten()
    .collect();

    candidates
        .into_iter()
        .filter(|e| plain as f64 / e.byte_size().max(1) as f64 >= MIN_COMPRESSION)
        .min_by_key(|e| e.byte_size())
}

/// Dictionary-encode a text column, if it has few enough distinct values.
///
/// The dictionary is sorted, so codes preserve order and a range predicate can
/// be answered by comparing them.
fn dictionary(column: &Column) -> Option<Encoded> {
    let ColumnData::Utf8(s) = &column.data else {
        return None;
    };
    // Above this share of distinct values the codes cost as much as the strings
    // they replace.
    let mut distinct: Vec<&str> = Vec::new();
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for i in 0..s.len() {
        if !column.is_valid(i) {
            continue;
        }
        let v = s.get(i);
        if seen.insert(v, ()).is_none() {
            distinct.push(v);
            if distinct.len() * 2 > s.len() {
                return None;
            }
        }
    }
    distinct.sort_unstable();

    let code_of: HashMap<&str, u32> = distinct
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, i as u32))
        .collect();
    let mut values = StringColumn::with_capacity(distinct.len(), distinct.iter().map(|v| v.len()).sum());
    for v in &distinct {
        values.push(v);
    }
    let codes = (0..s.len())
        .map(|i| {
            if column.is_valid(i) {
                code_of[s.get(i)]
            } else {
                // A NULL's code is never read -- the validity bitmap masks it --
                // but it has to be *something*, and zero keeps the codes dense.
                0
            }
        })
        .collect();
    Some(Encoded::Dictionary { codes, values })
}

/// Run-length encode, if the column has enough runs of equal values.
fn run_length(column: &Column) -> Option<Encoded> {
    let n = column.len();
    if n == 0 {
        return None;
    }
    // Find the run boundaries first, cheaply, and give up early if there are
    // too many for this to be worth anything.
    let mut ends: Vec<u32> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut previous: Option<ScalarValue> = None;
    for i in 0..n {
        let v = if column.is_valid(i) {
            Some(column.value(i))
        } else {
            None
        };
        let same = match (&previous, &v) {
            (Some(a), Some(b)) => types::compare(a, b) == Some(Ordering::Equal),
            // A run of NULLs is a run.
            (None, None) => !starts.is_empty(),
            _ => false,
        };
        if !same {
            if let Some(end) = ends.last_mut() {
                *end = i as u32;
            }
            starts.push(i);
            ends.push(n as u32);
            if starts.len() * 4 > n {
                return None;
            }
        }
        previous = v;
    }
    if let Some(end) = ends.last_mut() {
        *end = n as u32;
    }

    // Materialize one value per run, in the column's own representation.
    let mut builder = crate::storage::column::ColumnBuilder::new(&column.data_type());
    for start in &starts {
        if column.is_valid(*start) {
            builder.append(&column.value(*start)).ok()?;
        } else {
            builder.append_null();
        }
    }
    Some(Encoded::RunLength {
        values: Box::new(builder.finish()),
        ends,
    })
}

/// The integer view of a column, for the two bit-packing encodings.
///
/// Floats and strings have no such view; decimals do but their `i128` range
/// does not fit the `i64` reference this uses.
fn integers(column: &Column) -> Option<(Vec<i64>, DataType)> {
    let t = column.data_type();
    let values: Vec<i64> = match &column.data {
        ColumnData::Int32(v) | ColumnData::Date32(v) => v.iter().map(|x| *x as i64).collect(),
        ColumnData::Int64(v) | ColumnData::Timestamp(v) => v.clone(),
        _ => return None,
    };
    Some((values, t))
}

/// How many bits it takes to hold `0..=span`.
fn width_for(span: u64) -> u32 {
    if span == 0 {
        0
    } else {
        64 - span.leading_zeros()
    }
}

fn bit_packed(column: &Column) -> Option<Encoded> {
    let (values, data_type) = integers(column)?;
    let (min, max) = (*values.iter().min()?, *values.iter().max()?);
    let span = (max as i128 - min as i128) as u128;
    if span >= u64::MAX as u128 {
        return None;
    }
    let bit_width = width_for(span as u64);
    // Wider than the type it replaces is not compression.
    if bit_width >= 64 {
        return None;
    }
    let mut writer = BitWriter::new(values.len() * bit_width as usize);
    for v in &values {
        writer.put((*v as i128 - min as i128) as u64, bit_width);
    }
    Some(Encoded::BitPacked {
        reference: min,
        bit_width,
        packed: writer.finish(),
        len: values.len(),
        data_type,
    })
}

/// Frame-of-reference: a minimum and a width per block.
///
/// The encoding for a column that climbs -- an id, a timestamp -- where one
/// global minimum leaves the span as wide as the column but each block's own
/// span is tiny.
fn frame_of_reference(column: &Column) -> Option<Encoded> {
    let (values, data_type) = integers(column)?;
    if values.len() <= BLOCK {
        // One block is just bit-packing with extra bookkeeping.
        return None;
    }
    let mut blocks = Vec::with_capacity(values.len().div_ceil(BLOCK));
    let mut writer = BitWriter::new(values.len() * 8);
    for chunk in values.chunks(BLOCK) {
        let (min, max) = (*chunk.iter().min()?, *chunk.iter().max()?);
        let span = (max as i128 - min as i128) as u128;
        if span >= u64::MAX as u128 {
            return None;
        }
        let bit_width = width_for(span as u64);
        if bit_width >= 64 {
            return None;
        }
        blocks.push(Block {
            reference: min,
            bit_width,
            bit_offset: writer.bits(),
        });
        for v in chunk {
            writer.put((*v as i128 - min as i128) as u64, bit_width);
        }
    }
    Some(Encoded::FrameOfReference {
        blocks,
        packed: writer.finish(),
        len: values.len(),
        data_type,
    })
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Rebuild the plain column. `validity` is carried through untouched -- the
/// encodings never represent NULLs, only values.
pub fn decode(e: &Encoded, validity: Option<Bitmap>) -> Column {
    let data = match e {
        Encoded::Dictionary { codes, values } => {
            let mut s = StringColumn::with_capacity(codes.len(), values.total_bytes());
            for c in codes {
                s.push(values.get(*c as usize));
            }
            ColumnData::Utf8(s)
        }
        Encoded::RunLength { values, ends } => {
            let mut builder = crate::storage::column::ColumnBuilder::new(&values.data_type());
            let mut start = 0u32;
            for (run, end) in ends.iter().enumerate() {
                for _ in start..*end {
                    if values.is_valid(run) {
                        // `append` cannot fail: the value came out of a column
                        // of this very type.
                        let _ = builder.append(&values.value(run));
                    } else {
                        builder.append_null();
                    }
                }
                start = *end;
            }
            return Column::new(builder.finish().data, validity);
        }
        Encoded::BitPacked { reference, bit_width, packed, len, data_type } => {
            let reader = BitReader::new(packed);
            let values: Vec<i64> = (0..*len)
                .map(|i| reference + reader.get(i * *bit_width as usize, *bit_width) as i64)
                .collect();
            from_integers(values, *data_type)
        }
        Encoded::FrameOfReference { blocks, packed, len, data_type } => {
            let reader = BitReader::new(packed);
            let mut values = Vec::with_capacity(*len);
            for (b, block) in blocks.iter().enumerate() {
                let start = b * BLOCK;
                let end = ((b + 1) * BLOCK).min(*len);
                for i in start..end {
                    let bit = block.bit_offset + (i - start) * block.bit_width as usize;
                    values.push(block.reference + reader.get(bit, block.bit_width) as i64);
                }
            }
            from_integers(values, *data_type)
        }
    };
    Column::new(data, validity)
}

fn from_integers(values: Vec<i64>, t: DataType) -> ColumnData {
    match t {
        DataType::Int32 => ColumnData::Int32(values.into_iter().map(|v| v as i32).collect()),
        DataType::Date32 => ColumnData::Date32(values.into_iter().map(|v| v as i32).collect()),
        DataType::Timestamp => ColumnData::Timestamp(values),
        _ => ColumnData::Int64(values),
    }
}

// ---------------------------------------------------------------------------
// Predicates, evaluated on the encoded form
// ---------------------------------------------------------------------------

/// Evaluate `column <op> literal` without decoding, if this encoding can.
///
/// Returns a bitmap with a bit set for every row where the comparison is TRUE.
///
/// `validity` is required rather than optional, because the encodings do not
/// represent NULLs and every one of them puts *something* in a NULL row's
/// place: a dictionary code of zero, which is a real value's code, or a packed
/// zero, which decodes to the block's minimum. Either can match the predicate.
/// Masking here is what makes "a comparison against NULL is UNKNOWN, never
/// TRUE" true of the fast path as well as the slow one.
///
/// `None` means "decode and do it the ordinary way" -- the encoding cannot
/// answer this predicate, which is a missed optimization and never a wrong
/// answer.
pub fn evaluate(
    e: &Encoded,
    op: BinaryOperator,
    literal: &ScalarValue,
    validity: Option<&Bitmap>,
) -> Option<Bitmap> {
    if literal.is_null() || !op.is_comparison() {
        return None;
    }
    let hits = evaluate_values(e, op, literal)?;
    Some(match validity {
        Some(v) => hits.and(v),
        None => hits,
    })
}

/// Whether *any* row could satisfy the predicate, without touching the rows.
///
/// This is the cheap question, and the only one worth asking during a scan.
/// Fully evaluating a predicate on the encoded form was implemented first and
/// measured 3x slower than decoding: the filter above re-evaluates regardless,
/// so the encoded pass is redundant, and a scalar bitmap loop is no match for a
/// vectorized comparison over a dense array.
///
/// Answering "could anything match" costs a pass over the *metadata* instead:
///
///   * a dictionary has a handful of distinct values, so this is O(distinct)
///     however many rows there are -- and it is exact, where a bloom filter is
///     probabilistic and a zone map is a range;
///   * a run-length column has one entry per run;
///   * frame-of-reference blocks each carry their own range, which is finer
///     than the row group's single min/max.
///
/// Bit-packing is deliberately absent: its range is the column's min and max,
/// which is exactly what the zone map already checked.
///
/// `false` means no row can match. `true` means some might, which is also what
/// an encoding that cannot answer returns.
pub fn can_match(e: &Encoded, op: BinaryOperator, literal: &ScalarValue) -> bool {
    if literal.is_null() || !op.is_comparison() {
        return true;
    }
    match e {
        Encoded::Dictionary { values, .. } => {
            let ScalarValue::Utf8(needle) = literal else {
                return true;
            };
            (0..values.len()).any(|i| compare_op(values.get(i).cmp(needle.as_str()), op))
        }
        Encoded::RunLength { values, .. } => (0..values.len()).any(|run| {
            values.is_valid(run)
                && types::compare(&values.value(run), literal).is_some_and(|o| compare_op(o, op))
        }),
        Encoded::FrameOfReference { blocks, data_type, .. } => {
            let Some(target) = as_integer(literal, *data_type) else {
                return true;
            };
            blocks.iter().any(|b| {
                !matches!(
                    block_verdict(b.reference, b.reference + mask(b.bit_width) as i64, target, op),
                    Verdict::None
                )
            })
        }
        // The zone map already knows this column's range.
        Encoded::BitPacked { .. } => true,
    }
}

/// The comparison itself, over whatever the encoding stores. NULL rows are
/// meaningless here and are removed by the caller above.
fn evaluate_values(e: &Encoded, op: BinaryOperator, literal: &ScalarValue) -> Option<Bitmap> {
    match e {
        Encoded::Dictionary { codes, values } => {
            // One pass over the dictionary translates the predicate into code
            // space; after that it is an integer scan. This is the whole reason
            // dictionary encoding earns its place.
            let ScalarValue::Utf8(needle) = literal else {
                return None;
            };
            let mut matching = Bitmap::all_unset(values.len());
            for i in 0..values.len() {
                if compare_op(values.get(i).cmp(needle.as_str()), op) {
                    matching.set(i, true);
                }
            }
            let mut out = Bitmap::all_unset(codes.len());
            for (row, code) in codes.iter().enumerate() {
                if matching.get(*code as usize) {
                    out.set(row, true);
                }
            }
            Some(out)
        }

        Encoded::RunLength { values, ends } => {
            // One comparison per run, then fill.
            let mut out = Bitmap::all_unset(e.len());
            let mut start = 0u32;
            for (run, end) in ends.iter().enumerate() {
                let value = values.value(run);
                if values.is_valid(run) && !value.is_null() {
                    if let Some(ordering) = types::compare(&value, literal) {
                        if compare_op(ordering, op) {
                            for row in start..*end {
                                out.set(row as usize, true);
                            }
                        }
                    }
                }
                start = *end;
            }
            Some(out)
        }

        Encoded::BitPacked { reference, bit_width, packed, len, data_type } => {
            let target = as_integer(literal, *data_type)?;
            let reader = BitReader::new(packed);
            let mut out = Bitmap::all_unset(*len);
            for i in 0..*len {
                let v = reference + reader.get(i * *bit_width as usize, *bit_width) as i64;
                if compare_op(v.cmp(&target), op) {
                    out.set(i, true);
                }
            }
            Some(out)
        }

        Encoded::FrameOfReference { blocks, packed, len, data_type } => {
            let target = as_integer(literal, *data_type)?;
            let reader = BitReader::new(packed);
            let mut out = Bitmap::all_unset(*len);
            for (b, block) in blocks.iter().enumerate() {
                let start = b * BLOCK;
                let end = ((b + 1) * BLOCK).min(*len);
                // The block's own range is a zone map. If nothing in it can
                // match, its bits stay unset and it is never unpacked.
                let low = block.reference;
                let high = block.reference + mask(block.bit_width) as i64;
                match block_verdict(low, high, target, op) {
                    Verdict::None => continue,
                    Verdict::All => {
                        for i in start..end {
                            out.set(i, true);
                        }
                        continue;
                    }
                    Verdict::Some => {}
                }
                for i in start..end {
                    let bit = block.bit_offset + (i - start) * block.bit_width as usize;
                    let v = block.reference + reader.get(bit, block.bit_width) as i64;
                    if compare_op(v.cmp(&target), op) {
                        out.set(i, true);
                    }
                }
            }
            Some(out)
        }
    }
}

enum Verdict {
    /// No value in the block can satisfy the predicate.
    None,
    /// Every value does.
    All,
    /// Some might; unpack and check.
    Some,
}

/// What a block's `[low, high]` range says about a predicate, before unpacking.
fn block_verdict(low: i64, high: i64, target: i64, op: BinaryOperator) -> Verdict {
    let all = |b: bool| if b { Verdict::All } else { Verdict::None };
    match op {
        BinaryOperator::Eq => {
            if target < low || target > high {
                Verdict::None
            } else if low == high {
                Verdict::All
            } else {
                Verdict::Some
            }
        }
        BinaryOperator::Lt => {
            if high < target {
                all(true)
            } else if low >= target {
                Verdict::None
            } else {
                Verdict::Some
            }
        }
        BinaryOperator::LtEq => {
            if high <= target {
                all(true)
            } else if low > target {
                Verdict::None
            } else {
                Verdict::Some
            }
        }
        BinaryOperator::Gt => {
            if low > target {
                all(true)
            } else if high <= target {
                Verdict::None
            } else {
                Verdict::Some
            }
        }
        BinaryOperator::GtEq => {
            if low >= target {
                all(true)
            } else if high < target {
                Verdict::None
            } else {
                Verdict::Some
            }
        }
        _ => Verdict::Some,
    }
}

fn as_integer(v: &ScalarValue, t: DataType) -> Option<i64> {
    // The literal must be of the column's own kind, or the comparison this
    // performs is not the comparison SQL asked for.
    match (v, t) {
        (ScalarValue::Int32(x), DataType::Int32 | DataType::Int64) => Some(*x as i64),
        (ScalarValue::Int64(x), DataType::Int32 | DataType::Int64) => Some(*x),
        (ScalarValue::Date32(x), DataType::Date32) => Some(*x as i64),
        (ScalarValue::Timestamp(x), DataType::Timestamp) => Some(*x),
        _ => None,
    }
}

fn compare_op(ordering: Ordering, op: BinaryOperator) -> bool {
    match op {
        BinaryOperator::Eq => ordering == Ordering::Equal,
        BinaryOperator::NotEq => ordering != Ordering::Equal,
        BinaryOperator::Lt => ordering == Ordering::Less,
        BinaryOperator::LtEq => ordering != Ordering::Greater,
        BinaryOperator::Gt => ordering == Ordering::Greater,
        BinaryOperator::GtEq => ordering != Ordering::Less,
        _ => false,
    }
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

// ---------------------------------------------------------------------------
// Bit packing
// ---------------------------------------------------------------------------

struct BitWriter {
    words: Vec<u64>,
    bits: usize,
}

impl BitWriter {
    fn new(capacity_bits: usize) -> BitWriter {
        BitWriter {
            words: Vec::with_capacity(capacity_bits.div_ceil(64) + 1),
            bits: 0,
        }
    }

    fn bits(&self) -> usize {
        self.bits
    }

    /// Append `width` bits, least significant first, straddling words freely.
    fn put(&mut self, value: u64, width: u32) {
        if width == 0 {
            return;
        }
        let value = value & mask(width);
        let word = self.bits / 64;
        let offset = (self.bits % 64) as u32;
        while self.words.len() <= word + 1 {
            self.words.push(0);
        }
        self.words[word] |= value << offset;
        if offset + width > 64 {
            self.words[word + 1] |= value >> (64 - offset);
        }
        self.bits += width as usize;
    }

    fn finish(mut self) -> Vec<u64> {
        let needed = self.bits.div_ceil(64);
        self.words.truncate(needed.max(1));
        self.words
    }
}

struct BitReader<'a> {
    words: &'a [u64],
}

impl<'a> BitReader<'a> {
    fn new(words: &'a [u64]) -> BitReader<'a> {
        BitReader { words }
    }

    /// Read `width` bits starting at bit `at`.
    fn get(&self, at: usize, width: u32) -> u64 {
        if width == 0 {
            return 0;
        }
        let word = at / 64;
        let offset = (at % 64) as u32;
        let low = self.words.get(word).copied().unwrap_or(0) >> offset;
        let value = if offset + width > 64 {
            let high = self.words.get(word + 1).copied().unwrap_or(0);
            low | (high << (64 - offset))
        } else {
            low
        };
        value & mask(width)
    }
}

#[cfg(test)]
mod tests;
