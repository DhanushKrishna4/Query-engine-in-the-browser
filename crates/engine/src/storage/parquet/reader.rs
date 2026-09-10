//! Turning Parquet column chunks into this engine's columns.
//!
//! The unit of work is one column chunk of one row group -- [`decode_chunk`] --
//! because that is the unit a scan can decide to skip. Everything above it,
//! including the lazy loading in `storage::table`, is arranged so that a chunk
//! whose row group was pruned is never passed to this code at all.
//!
//! ## Nulls arrive as definition levels, not as gaps
//!
//! Parquet stores only the values that exist. Which rows those are is carried
//! separately, as *definition levels*: for a flat schema a level equal to the
//! column's maximum means "present" and anything less means NULL. So decoding
//! is two streams that have to be walked together -- a dense run of values and
//! a per-row level -- and the assembly step below is where they meet.
//!
//! ## The physical type is not the type
//!
//! `INT32` might be an integer, a date, or a decimal; `INT64` might be a
//! timestamp in one of three units; `BYTE_ARRAY` might be a string or a
//! decimal. The annotation says which, and it comes in two generations
//! (`LogicalType`, and the older `ConvertedType`), so both are consulted with
//! the newer winning.


use crate::error::Result;
use crate::storage::bitmap::Bitmap;
use crate::storage::column::{Column, ColumnData, StringColumn};
use crate::storage::schema::{Field, Schema};
use crate::types::{DataType, MICROS_PER_DAY};

use super::encoding::*;
use super::metadata::*;
use super::snappy;
use super::thrift::err;
use super::read_page_header;

/// Julian day number of 1970-01-01, for the deprecated INT96 timestamps.
const UNIX_EPOCH_JULIAN_DAY: i64 = 2_440_588;

/// One column of a flat Parquet schema.
#[derive(Debug, Clone)]
pub struct LeafColumn {
    pub field: Field,
    pub physical_type: PhysicalType,
    /// Bytes per value for FIXED_LEN_BYTE_ARRAY.
    pub type_length: Option<i32>,
    /// 1 for an optional column in a flat schema, 0 for a required one.
    pub max_def_level: i16,
    /// How to turn the physical values into the field's type.
    pub conversion: Conversion,
}

/// What has to happen to the physical values to produce the declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversion {
    /// The physical representation is already the target.
    Direct,
    /// Multiply by this to reach microseconds.
    TimestampScale(i64),
    /// Divide by this to reach microseconds, truncating.
    TimestampDivide(i64),
    /// Twelve bytes of Julian day and nanoseconds-of-day.
    Int96Timestamp,
    Decimal { precision: u8, scale: i8 },
    /// FLOAT widened to this engine's only floating type.
    FloatToDouble,
}

/// Read the flattened schema tree and produce one leaf per column.
///
/// Refuses nesting rather than guessing at it: a repeated or grouped field
/// needs repetition levels and a list-assembly step, and pretending otherwise
/// would silently flatten a list into its elements.
pub fn schema_leaves(meta: &FileMetaData) -> Result<Vec<LeafColumn>> {
    let root = &meta.schema[0];
    let children = root.num_children.unwrap_or(0);
    if meta.schema.len() as i32 != children + 1 {
        // Anything other than root-plus-leaves means a group is in there.
        let group = meta.schema[1..]
            .iter()
            .find(|e| e.num_children.unwrap_or(0) > 0)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "a group".into());
        return Err(err(format!(
            "nested Parquet schemas are not supported: `{group}` is a group, not a column"
        )));
    }

    let mut leaves = Vec::with_capacity(children as usize);
    for element in &meta.schema[1..] {
        if element.repetition == Some(Repetition::Repeated) {
            return Err(err(format!(
                "repeated columns are not supported: `{}` is a list",
                element.name
            )));
        }
        leaves.push(leaf_from(element)?);
    }
    Ok(leaves)
}

fn leaf_from(e: &SchemaElement) -> Result<LeafColumn> {
    let physical = e.physical_type.ok_or_else(|| {
        err(format!(
            "column `{}` declares no physical type, so it is a group",
            e.name
        ))
    })?;
    let nullable = e.repetition != Some(Repetition::Required);
    let (data_type, conversion) = map_type(e, physical)?;

    Ok(LeafColumn {
        field: Field::new(e.name.clone(), data_type, nullable),
        physical_type: physical,
        type_length: e.type_length,
        max_def_level: if nullable { 1 } else { 0 },
        conversion,
    })
}

/// The annotation decides; the physical type only constrains what annotations
/// are legal.
fn map_type(e: &SchemaElement, physical: PhysicalType) -> Result<(DataType, Conversion)> {
    let refuse = |what: String| -> Result<(DataType, Conversion)> {
        Err(err(format!(
            "column `{}`: {what} is not supported yet",
            e.name
        )))
    };

    // The modern annotation wins where both are present -- it is strictly more
    // expressive, and a writer emitting both makes them agree.
    if let Some(logical) = e.logical_type {
        match logical {
            LogicalType::String | LogicalType::Json | LogicalType::Enum => {
                return Ok((DataType::Utf8, Conversion::Direct))
            }
            LogicalType::Date => return Ok((DataType::Date32, Conversion::Direct)),
            LogicalType::Timestamp { unit, .. } => {
                return Ok((DataType::Timestamp, timestamp_conversion(unit)))
            }
            LogicalType::Decimal { precision, scale } => {
                return Ok(decimal(precision, scale));
            }
            LogicalType::Integer { bits, signed } => {
                if !signed && bits >= 64 {
                    return refuse("UINT_64".into());
                }
                let dt = if bits <= 32 && signed || bits < 32 {
                    DataType::Int32
                } else {
                    DataType::Int64
                };
                return Ok((dt, Conversion::Direct));
            }
            LogicalType::Uuid => return refuse("the UUID logical type".into()),
            LogicalType::Float16 => return refuse("FLOAT16".into()),
            LogicalType::Map | LogicalType::List => {
                return refuse("nested data".into());
            }
            // TIME and UNKNOWN fall through to the physical type; BSON is bytes.
            _ => {}
        }
    }

    if let Some(converted) = e.converted_type {
        match converted {
            ConvertedType::Utf8 | ConvertedType::Json | ConvertedType::Enum => {
                return Ok((DataType::Utf8, Conversion::Direct))
            }
            ConvertedType::Date => return Ok((DataType::Date32, Conversion::Direct)),
            ConvertedType::TimestampMillis => {
                return Ok((DataType::Timestamp, Conversion::TimestampScale(1_000)))
            }
            ConvertedType::TimestampMicros => {
                return Ok((DataType::Timestamp, Conversion::Direct))
            }
            ConvertedType::Decimal => {
                return Ok(decimal(
                    e.precision.unwrap_or(38),
                    e.scale.unwrap_or(0),
                ));
            }
            ConvertedType::Int8 | ConvertedType::Int16 | ConvertedType::Int32 => {
                return Ok((DataType::Int32, Conversion::Direct))
            }
            ConvertedType::Int64 => return Ok((DataType::Int64, Conversion::Direct)),
            ConvertedType::Uint8 | ConvertedType::Uint16 | ConvertedType::Uint32 => {
                // These fit in an i64 without wrapping; UINT_64 does not.
                return Ok((DataType::Int64, Conversion::Direct));
            }
            ConvertedType::Uint64 => return refuse("UINT_64".into()),
            ConvertedType::List | ConvertedType::Map | ConvertedType::MapKeyValue => {
                return refuse("nested data".into())
            }
            other => return refuse(format!("the {} annotation", other.name())),
        }
    }

    Ok(match physical {
        PhysicalType::Boolean => (DataType::Boolean, Conversion::Direct),
        PhysicalType::Int32 => (DataType::Int32, Conversion::Direct),
        PhysicalType::Int64 => (DataType::Int64, Conversion::Direct),
        PhysicalType::Float => (DataType::Float64, Conversion::FloatToDouble),
        PhysicalType::Double => (DataType::Float64, Conversion::Direct),
        PhysicalType::Int96 => (DataType::Timestamp, Conversion::Int96Timestamp),
        // An unannotated BYTE_ARRAY is opaque binary. Reading it as text would
        // be a guess, and a wrong one for anything that is not valid UTF-8.
        PhysicalType::ByteArray | PhysicalType::FixedLenByteArray => {
            return refuse("unannotated binary".into())
        }
    })
}

fn timestamp_conversion(unit: TimeUnit) -> Conversion {
    match unit {
        TimeUnit::Millis => Conversion::TimestampScale(1_000),
        TimeUnit::Micros => Conversion::Direct,
        // This engine's timestamps are microseconds, so nanoseconds lose their
        // last three digits. Truncating is what every engine with a
        // microsecond type does; refusing the file would be worse.
        TimeUnit::Nanos => Conversion::TimestampDivide(1_000),
    }
}

fn decimal(precision: i32, scale: i32) -> (DataType, Conversion) {
    let p = precision.clamp(1, 38) as u8;
    let s = scale.clamp(0, 38) as i8;
    (
        DataType::Decimal128 {
            precision: p,
            scale: s,
        },
        Conversion::Decimal {
            precision: p,
            scale: s,
        },
    )
}

pub fn arrow_schema(leaves: &[LeafColumn]) -> Schema {
    Schema::new(leaves.iter().map(|l| l.field.clone()).collect())
}

// ---------------------------------------------------------------------------
// Physical values
// ---------------------------------------------------------------------------

/// Variable-length values, accumulated without one allocation per value.
#[derive(Default)]
struct Bytes {
    offsets: Vec<u32>,
    data: Vec<u8>,
}

impl Bytes {
    fn new() -> Bytes {
        Bytes {
            offsets: vec![0],
            data: Vec::new(),
        }
    }
    fn push(&mut self, b: &[u8]) {
        self.data.extend_from_slice(b);
        self.offsets.push(self.data.len() as u32);
    }
    fn len(&self) -> usize {
        self.offsets.len() - 1
    }
    fn get(&self, i: usize) -> &[u8] {
        &self.data[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }
}

/// Decoded values, still in their physical form and still dense -- nulls are
/// not represented here, only in the definition levels.
enum Values {
    Bool(Vec<bool>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Bytes(Bytes),
}

impl Values {
    fn empty(t: PhysicalType) -> Values {
        match t {
            PhysicalType::Boolean => Values::Bool(Vec::new()),
            PhysicalType::Int32 => Values::I32(Vec::new()),
            PhysicalType::Int64 | PhysicalType::Int96 => Values::I64(Vec::new()),
            PhysicalType::Float => Values::F32(Vec::new()),
            PhysicalType::Double => Values::F64(Vec::new()),
            PhysicalType::ByteArray | PhysicalType::FixedLenByteArray => {
                Values::Bytes(Bytes::new())
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Values::Bool(v) => v.len(),
            Values::I32(v) => v.len(),
            Values::I64(v) => v.len(),
            Values::F32(v) => v.len(),
            Values::F64(v) => v.len(),
            Values::Bytes(b) => b.len(),
        }
    }

    /// Append `dict[index]` for each index, for a dictionary-encoded page.
    fn gather(&mut self, dict: &Values, indices: &[u64]) -> Result<()> {
        macro_rules! gather {
            ($dst:expr, $src:expr) => {{
                for i in indices {
                    let v = *$src
                        .get(*i as usize)
                        .ok_or_else(|| err("dictionary index is out of range"))?;
                    $dst.push(v);
                }
            }};
        }
        match (self, dict) {
            (Values::Bool(d), Values::Bool(s)) => gather!(d, s),
            (Values::I32(d), Values::I32(s)) => gather!(d, s),
            (Values::I64(d), Values::I64(s)) => gather!(d, s),
            (Values::F32(d), Values::F32(s)) => gather!(d, s),
            (Values::F64(d), Values::F64(s)) => gather!(d, s),
            (Values::Bytes(d), Values::Bytes(s)) => {
                for i in indices {
                    let i = *i as usize;
                    if i >= s.len() {
                        return Err(err("dictionary index is out of range"));
                    }
                    d.push(s.get(i));
                }
            }
            _ => return Err(err("dictionary page type does not match the data pages")),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Chunk decoding
// ---------------------------------------------------------------------------

/// Check that a chunk can be decoded, from its metadata alone.
///
/// Worth doing at load time rather than at first read: a lazily loaded table
/// would otherwise accept a file it cannot decode and fail in the middle of a
/// query, which is a much worse place to learn that the codec is unsupported.
/// Everything consulted here is in the footer, so it costs nothing.
pub fn check_supported(meta: &ColumnMetaData, leaf: &LeafColumn) -> Result<()> {
    if !matches!(
        meta.codec,
        Compression::Uncompressed | Compression::Snappy
    ) {
        return Err(err(format!(
            "column `{}` uses {} compression, which is not supported yet \
             (UNCOMPRESSED and SNAPPY are)",
            leaf.field.name,
            meta.codec.name()
        )));
    }
    for encoding in &meta.encodings {
        let ok = matches!(
            encoding,
            Encoding::Plain
                | Encoding::PlainDictionary
                | Encoding::Rle
                | Encoding::RleDictionary
                | Encoding::DeltaBinaryPacked
                | Encoding::DeltaLengthByteArray
                | Encoding::DeltaByteArray
                | Encoding::ByteStreamSplit
        );
        if !ok {
            return Err(err(format!(
                "column `{}` uses the {} encoding, which is not supported yet",
                leaf.field.name,
                encoding.name()
            )));
        }
    }
    Ok(())
}

/// Decode one column chunk of one row group into a column.
///
/// `bytes` is the whole file; only the chunk's own byte range is touched.
pub fn decode_chunk(
    bytes: &[u8],
    meta: &ColumnMetaData,
    leaf: &LeafColumn,
    num_rows: usize,
) -> Result<Column> {
    if let Compression::Uncompressed | Compression::Snappy = meta.codec {
    } else {
        return Err(err(format!(
            "column `{}` uses {} compression, which is not supported yet \
             (UNCOMPRESSED and SNAPPY are)",
            leaf.field.name,
            meta.codec.name()
        )));
    }

    let mut values = Values::empty(leaf.physical_type);
    let mut dictionary: Option<Values> = None;
    // One entry per row: true where the row holds a value.
    let mut present: Vec<bool> = Vec::with_capacity(num_rows);

    let mut offset = meta.start_offset() as usize;
    let end = offset + meta.total_compressed_size as usize;
    if end > bytes.len() {
        return Err(err(format!(
            "column `{}` claims bytes {offset}..{end} of a {}-byte file",
            leaf.field.name,
            bytes.len()
        )));
    }

    while offset < end && present.len() < num_rows {
        let (header, body) = read_page_header(bytes, offset)?;
        let compressed = bytes
            .get(body..body + header.compressed_page_size as usize)
            .ok_or_else(|| err("page body runs past the end of the file"))?;
        offset = body + header.compressed_page_size as usize;

        match header.page_type {
            PageType::DictionaryPage => {
                let h = header
                    .dictionary_page
                    .as_ref()
                    .ok_or_else(|| err("dictionary page has no dictionary header"))?;
                let data = decompress(meta.codec, compressed, header.uncompressed_page_size)?;
                let mut dict = Values::empty(leaf.physical_type);
                decode_plain(&data, leaf, h.num_values as usize, &mut dict)?;
                dictionary = Some(dict);
            }
            PageType::DataPage => {
                let h = header
                    .data_page
                    .as_ref()
                    .ok_or_else(|| err("data page has no v1 header"))?;
                // In a v1 page the levels are compressed together with the
                // values, so the whole page is decompressed first.
                let data = decompress(meta.codec, compressed, header.uncompressed_page_size)?;
                let count = h.num_values as usize;
                let rest = read_v1_levels(&data, leaf, count, h.definition_level_encoding, &mut present)?;
                let non_null = present[present.len() - count..].iter().filter(|p| **p).count();
                decode_values(rest, leaf, h.encoding, non_null, &dictionary, &mut values)?;
            }
            PageType::DataPageV2 => {
                let h = header
                    .data_page_v2
                    .as_ref()
                    .ok_or_else(|| err("data page has no v2 header"))?;
                if h.repetition_levels_byte_length > 0 {
                    return Err(err(format!(
                        "column `{}` carries repetition levels, so it is nested",
                        leaf.field.name
                    )));
                }
                // v2 keeps the levels *outside* the compressed region, which is
                // what lets a reader skip a page on its statistics without
                // decompressing it.
                let def_len = h.definition_levels_byte_length as usize;
                let levels = compressed
                    .get(..def_len)
                    .ok_or_else(|| err("v2 page is shorter than its declared levels"))?;
                let body = &compressed[def_len..];
                let count = h.num_values as usize;
                read_v2_levels(levels, leaf, count, &mut present)?;

                let uncompressed_values = header.uncompressed_page_size as usize - def_len;
                let data = if h.is_compressed {
                    decompress(meta.codec, body, uncompressed_values as i32)?
                } else {
                    body.to_vec()
                };
                let non_null = count - h.num_nulls as usize;
                decode_values(&data, leaf, h.encoding, non_null, &dictionary, &mut values)?;
            }
            PageType::IndexPage => {}
            PageType::Other(v) => {
                return Err(err(format!("unknown Parquet page type {v}")));
            }
        }
    }

    if present.len() != num_rows {
        return Err(err(format!(
            "column `{}` decoded {} rows, the row group declares {num_rows}",
            leaf.field.name,
            present.len()
        )));
    }
    assemble(values, present, leaf)
}

fn decompress(codec: Compression, data: &[u8], uncompressed_size: i32) -> Result<Vec<u8>> {
    match codec {
        Compression::Uncompressed => Ok(data.to_vec()),
        Compression::Snappy => {
            let out = snappy::decompress(data)?;
            if out.len() != uncompressed_size as usize {
                return Err(err(format!(
                    "page declared {uncompressed_size} uncompressed bytes and produced {}",
                    out.len()
                )));
            }
            Ok(out)
        }
        other => Err(err(format!("{} compression is not supported", other.name()))),
    }
}

/// Read a v1 page's definition levels and return the value bytes after them.
fn read_v1_levels<'a>(
    data: &'a [u8],
    leaf: &LeafColumn,
    count: usize,
    encoding: Encoding,
    present: &mut Vec<bool>,
) -> Result<&'a [u8]> {
    if leaf.max_def_level == 0 {
        // A required column has no levels at all.
        present.extend(std::iter::repeat_n(true, count));
        return Ok(data);
    }
    match encoding {
        Encoding::Rle => {
            // In a v1 page the RLE level section is prefixed with its own
            // four-byte length, because the values that follow start
            // immediately after it and nothing else says where.
            let len = u32::from_le_bytes(
                data.get(..4)
                    .ok_or_else(|| err("v1 page is too short for its level length"))?
                    .try_into()
                    .unwrap(),
            ) as usize;
            let levels = data
                .get(4..4 + len)
                .ok_or_else(|| err("v1 page level section runs past the page"))?;
            push_levels(levels, count, present)?;
            Ok(&data[4 + len..])
        }
        other => Err(err(format!(
            "definition levels encoded as {} are not supported (RLE is)",
            other.name()
        ))),
    }
}

fn read_v2_levels(
    levels: &[u8],
    leaf: &LeafColumn,
    count: usize,
    present: &mut Vec<bool>,
) -> Result<()> {
    if leaf.max_def_level == 0 {
        present.extend(std::iter::repeat_n(true, count));
        return Ok(());
    }
    // v2 levels are always RLE and carry no length prefix: the header said how
    // many bytes they occupy.
    push_levels(levels, count, present)
}

fn push_levels(levels: &[u8], count: usize, present: &mut Vec<bool>) -> Result<()> {
    // A flat optional column has a maximum level of 1, so one bit per row.
    let decoded = decode_rle_hybrid(levels, 1, count)?;
    present.extend(decoded.into_iter().map(|d| d == 1));
    Ok(())
}

fn decode_values(
    data: &[u8],
    leaf: &LeafColumn,
    encoding: Encoding,
    count: usize,
    dictionary: &Option<Values>,
    out: &mut Values,
) -> Result<()> {
    match encoding {
        Encoding::Plain => decode_plain(data, leaf, count, out),
        Encoding::PlainDictionary | Encoding::RleDictionary => {
            let dict = dictionary
                .as_ref()
                .ok_or_else(|| err("a dictionary-encoded page arrived before its dictionary"))?;
            // The first byte is the index width; the rest is an RLE hybrid
            // stream with no length prefix, running to the end of the page.
            let (width, rest) = data
                .split_first()
                .ok_or_else(|| err("dictionary-encoded page is empty"))?;
            let indices = decode_rle_hybrid(rest, *width as u32, count)?;
            out.gather(dict, &indices)
        }
        Encoding::Rle => {
            // Only booleans use RLE for values, at one bit each.
            let Values::Bool(v) = out else {
                return Err(err("RLE value encoding is only defined for booleans"));
            };
            // RLE-encoded *values* carry a four-byte length prefix, in v1 and
            // v2 alike -- unlike v2's definition levels, whose length is in the
            // page header and which therefore have none. The asymmetry is easy
            // to get wrong and costs no error when you do: the prefix decodes
            // as a plausible run header and the values come out shifted.
            let len = u32::from_le_bytes(
                data.get(..4)
                    .ok_or_else(|| err("boolean page is too short for its length prefix"))?
                    .try_into()
                    .unwrap(),
            ) as usize;
            let stream = data
                .get(4..4 + len)
                .ok_or_else(|| err("boolean page declares more RLE bytes than it holds"))?;
            for d in decode_rle_hybrid(stream, 1, count)? {
                v.push(d == 1);
            }
            Ok(())
        }
        Encoding::DeltaBinaryPacked => {
            let (decoded, _) = decode_delta_binary_packed(data)?;
            match out {
                Values::I32(v) => {
                    for d in decoded.into_iter().take(count) {
                        v.push(d as i32);
                    }
                }
                Values::I64(v) => v.extend(decoded.into_iter().take(count)),
                _ => return Err(err("DELTA_BINARY_PACKED is only defined for integers")),
            }
            Ok(())
        }
        Encoding::DeltaLengthByteArray => {
            let Values::Bytes(b) = out else {
                return Err(err("DELTA_LENGTH_BYTE_ARRAY is only defined for byte arrays"));
            };
            for v in decode_delta_length_byte_array(data)?.into_iter().take(count) {
                b.push(v);
            }
            Ok(())
        }
        Encoding::DeltaByteArray => {
            let Values::Bytes(b) = out else {
                return Err(err("DELTA_BYTE_ARRAY is only defined for byte arrays"));
            };
            for v in decode_delta_byte_array(data)?.into_iter().take(count) {
                b.push(&v);
            }
            Ok(())
        }
        Encoding::ByteStreamSplit => {
            let width = leaf
                .physical_type
                .plain_width()
                .ok_or_else(|| err("BYTE_STREAM_SPLIT needs a fixed-width type"))?;
            let flat = decode_byte_stream_split(data, width, count)?;
            decode_plain(&flat, leaf, count, out)
        }
        other => Err(err(format!(
            "the {} encoding is not supported yet",
            other.name()
        ))),
    }
}

fn decode_plain(data: &[u8], leaf: &LeafColumn, count: usize, out: &mut Values) -> Result<()> {
    match out {
        Values::Bool(v) => {
            // One bit per value, least significant first.
            let mut r = BitReader::new(data);
            for _ in 0..count {
                v.push(r.get(1)? == 1);
            }
        }
        Values::I32(v) => {
            let need = count * 4;
            let raw = data.get(..need).ok_or_else(short(count, need, data.len()))?;
            v.extend(raw.as_chunks::<4>().0.iter().map(|c| i32::from_le_bytes(*c)));
        }
        Values::I64(v) if leaf.physical_type == PhysicalType::Int96 => {
            let need = count * 12;
            let raw = data.get(..need).ok_or_else(short(count, need, data.len()))?;
            for c in raw.as_chunks::<12>().0 {
                v.push(int96_to_micros(c));
            }
        }
        Values::I64(v) => {
            let need = count * 8;
            let raw = data.get(..need).ok_or_else(short(count, need, data.len()))?;
            v.extend(raw.as_chunks::<8>().0.iter().map(|c| i64::from_le_bytes(*c)));
        }
        Values::F32(v) => {
            let need = count * 4;
            let raw = data.get(..need).ok_or_else(short(count, need, data.len()))?;
            v.extend(raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
        }
        Values::F64(v) => {
            let need = count * 8;
            let raw = data.get(..need).ok_or_else(short(count, need, data.len()))?;
            v.extend(raw.as_chunks::<8>().0.iter().map(|c| f64::from_le_bytes(*c)));
        }
        Values::Bytes(b) => {
            if leaf.physical_type == PhysicalType::FixedLenByteArray {
                let width = leaf.type_length.unwrap_or(0).max(0) as usize;
                if width == 0 {
                    return Err(err("a fixed-length column declares no width"));
                }
                let need = count * width;
                let raw = data.get(..need).ok_or_else(short(count, need, data.len()))?;
                for c in raw.chunks_exact(width) {
                    b.push(c);
                }
            } else {
                // Each value is a four-byte little-endian length and its bytes.
                let mut pos = 0;
                for _ in 0..count {
                    let len = u32::from_le_bytes(
                        data.get(pos..pos + 4)
                            .ok_or_else(|| err("byte-array page ended mid-length"))?
                            .try_into()
                            .unwrap(),
                    ) as usize;
                    pos += 4;
                    let value = data
                        .get(pos..pos + len)
                        .ok_or_else(|| err("byte-array page ended mid-value"))?;
                    b.push(value);
                    pos += len;
                }
            }
        }
    }
    Ok(())
}

fn short(count: usize, need: usize, have: usize) -> impl Fn() -> crate::error::Diagnostic {
    move || err(format!("page holds {have} bytes for {count} values needing {need}"))
}

/// INT96: eight bytes of nanoseconds within the day, then four of Julian day.
///
/// Deprecated for a decade and still written by older Hive and Spark, so a
/// reader that refuses it cannot open a large part of the world's Parquet.
fn int96_to_micros(c: &[u8]) -> i64 {
    let nanos = i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
    let julian_day = u32::from_le_bytes([c[8], c[9], c[10], c[11]]) as i64;
    (julian_day - UNIX_EPOCH_JULIAN_DAY)
        .wrapping_mul(MICROS_PER_DAY)
        .wrapping_add(nanos / 1_000)
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// Interleave the dense values with the nulls the definition levels describe.
fn assemble(values: Values, present: Vec<bool>, leaf: &LeafColumn) -> Result<Column> {
    let rows = present.len();
    let all_present = leaf.max_def_level == 0 || present.iter().all(|p| *p);
    if values.len() != present.iter().filter(|p| **p).count() {
        return Err(err(format!(
            "column `{}` decoded {} values for {} non-null rows",
            leaf.field.name,
            values.len(),
            present.iter().filter(|p| **p).count()
        )));
    }

    let validity = if all_present {
        None
    } else {
        let mut b = Bitmap::all_unset(rows);
        for (i, p) in present.iter().enumerate() {
            if *p {
                b.set(i, true);
            }
        }
        Some(b)
    };

    // A closure per type rather than one generic pass, because the placeholder
    // for a null differs by representation and the conversion does too.
    macro_rules! expand {
        ($src:expr, $default:expr, $convert:expr) => {{
            let mut out = Vec::with_capacity(rows);
            let mut cursor = 0usize;
            for p in &present {
                if *p {
                    let v = $src[cursor];
                    cursor += 1;
                    out.push($convert(v));
                } else {
                    out.push($default);
                }
            }
            out
        }};
    }

    let data = match (values, &leaf.field.data_type, leaf.conversion) {
        (Values::Bool(v), DataType::Boolean, _) => {
            let mut b = Bitmap::all_unset(rows);
            let mut cursor = 0;
            for (i, p) in present.iter().enumerate() {
                if *p {
                    if v[cursor] {
                        b.set(i, true);
                    }
                    cursor += 1;
                }
            }
            ColumnData::Boolean(b)
        }
        (Values::I32(v), DataType::Int32, _) => ColumnData::Int32(expand!(v, 0, |x| x)),
        (Values::I32(v), DataType::Date32, _) => ColumnData::Date32(expand!(v, 0, |x| x)),
        (Values::I32(v), DataType::Int64, _) => {
            ColumnData::Int64(expand!(v, 0i64, |x| x as i64))
        }
        (Values::I32(v), DataType::Decimal128 { .. }, Conversion::Decimal { precision, scale }) => {
            ColumnData::Decimal128 {
                values: expand!(v, 0i128, |x| x as i128),
                precision,
                scale,
            }
        }
        (Values::I64(v), DataType::Int64, _) => ColumnData::Int64(expand!(v, 0, |x| x)),
        (Values::I64(v), DataType::Timestamp, Conversion::TimestampScale(k)) => {
            ColumnData::Timestamp(expand!(v, 0i64, |x: i64| x.wrapping_mul(k)))
        }
        (Values::I64(v), DataType::Timestamp, Conversion::TimestampDivide(k)) => {
            ColumnData::Timestamp(expand!(v, 0i64, |x: i64| x.div_euclid(k)))
        }
        // INT96 and micros are both already microseconds by the time they land
        // here -- the conversion happened during plain decoding.
        (Values::I64(v), DataType::Timestamp, _) => ColumnData::Timestamp(expand!(v, 0, |x| x)),
        (Values::I64(v), DataType::Decimal128 { .. }, Conversion::Decimal { precision, scale }) => {
            ColumnData::Decimal128 {
                values: expand!(v, 0i128, |x| x as i128),
                precision,
                scale,
            }
        }
        (Values::F32(v), DataType::Float64, _) => {
            ColumnData::Float64(expand!(v, 0.0f64, |x| x as f64))
        }
        (Values::F64(v), DataType::Float64, _) => ColumnData::Float64(expand!(v, 0.0, |x| x)),
        (Values::Bytes(b), DataType::Utf8, _) => {
            let mut s = StringColumn::with_capacity(rows, b.data.len());
            let mut cursor = 0;
            for p in &present {
                if *p {
                    let raw = b.get(cursor);
                    cursor += 1;
                    // Lossy rather than fatal: one bad byte in a million-row
                    // column should not cost the whole file, and the engine
                    // has no binary type to fall back to.
                    s.push(&String::from_utf8_lossy(raw));
                } else {
                    s.push("");
                }
            }
            ColumnData::Utf8(s)
        }
        (Values::Bytes(b), DataType::Decimal128 { .. }, Conversion::Decimal { precision, scale }) => {
            let mut out = Vec::with_capacity(rows);
            let mut cursor = 0;
            for p in &present {
                if *p {
                    let raw = b.get(cursor);
                    cursor += 1;
                    out.push(be_decimal(raw)?);
                } else {
                    out.push(0);
                }
            }
            ColumnData::Decimal128 {
                values: out,
                precision,
                scale,
            }
        }
        (_, dt, conv) => {
            return Err(err(format!(
                "column `{}`: no conversion from its physical type to {dt} ({conv:?})",
                leaf.field.name
            )))
        }
    };

    Ok(Column::new(data, validity))
}

/// Parquet stores decimals big-endian, two's complement, in the fewest bytes
/// the precision needs -- so sign extension is on the reader.
fn be_decimal(raw: &[u8]) -> Result<i128> {
    if raw.is_empty() || raw.len() > 16 {
        return Err(err(format!(
            "a decimal value occupies {} bytes, which does not fit an i128",
            raw.len()
        )));
    }
    let negative = raw[0] & 0x80 != 0;
    let mut buf = if negative { [0xffu8; 16] } else { [0u8; 16] };
    buf[16 - raw.len()..].copy_from_slice(raw);
    Ok(i128::from_be_bytes(buf))
}

// ---------------------------------------------------------------------------
// Statistics from the footer
// ---------------------------------------------------------------------------

/// Decode a single PLAIN-encoded value from a `Statistics` bound.
///
/// The bounds are stored the way a value is stored inside a page, except that a
/// byte array carries no length prefix -- the field's own length is the value's.
/// The same conversion the column uses applies here too, or a timestamp bound
/// would be compared in milliseconds against microsecond data.
pub fn stat_value(raw: &[u8], leaf: &LeafColumn) -> Option<crate::types::ScalarValue> {
    use crate::types::ScalarValue;

    let i32_of = |r: &[u8]| r.get(..4).map(|c| i32::from_le_bytes(c.try_into().unwrap()));
    let i64_of = |r: &[u8]| r.get(..8).map(|c| i64::from_le_bytes(c.try_into().unwrap()));

    Some(match (&leaf.field.data_type, leaf.physical_type) {
        (DataType::Boolean, _) => ScalarValue::Boolean(*raw.first()? != 0),
        (DataType::Int32, _) => ScalarValue::Int32(i32_of(raw)?),
        (DataType::Date32, _) => ScalarValue::Date32(i32_of(raw)?),
        (DataType::Int64, PhysicalType::Int32) => ScalarValue::Int64(i32_of(raw)? as i64),
        (DataType::Int64, _) => ScalarValue::Int64(i64_of(raw)?),
        (DataType::Float64, PhysicalType::Float) => {
            ScalarValue::Float64(f32::from_le_bytes(raw.get(..4)?.try_into().ok()?) as f64)
        }
        (DataType::Float64, _) => {
            ScalarValue::Float64(f64::from_le_bytes(raw.get(..8)?.try_into().ok()?))
        }
        (DataType::Timestamp, PhysicalType::Int96) => {
            ScalarValue::Timestamp(int96_to_micros(raw.get(..12)?))
        }
        (DataType::Timestamp, _) => {
            let v = i64_of(raw)?;
            ScalarValue::Timestamp(match leaf.conversion {
                Conversion::TimestampScale(k) => v.wrapping_mul(k),
                Conversion::TimestampDivide(k) => v.div_euclid(k),
                _ => v,
            })
        }
        (DataType::Utf8, _) => ScalarValue::Utf8(String::from_utf8_lossy(raw).into_owned()),
        (DataType::Decimal128 { precision, scale }, PhysicalType::Int32) => {
            ScalarValue::Decimal128 {
                value: i32_of(raw)? as i128,
                precision: *precision,
                scale: *scale,
            }
        }
        (DataType::Decimal128 { precision, scale }, PhysicalType::Int64) => {
            ScalarValue::Decimal128 {
                value: i64_of(raw)? as i128,
                precision: *precision,
                scale: *scale,
            }
        }
        (DataType::Decimal128 { precision, scale }, _) => ScalarValue::Decimal128 {
            value: be_decimal(raw).ok()?,
            precision: *precision,
            scale: *scale,
        },
        _ => return None,
    })
}

/// Build this engine's zone map for one column chunk out of the footer.
///
/// This is the payoff of the whole step: the bounds a scan prunes on are read
/// from metadata, so a row group that cannot match is skipped without a byte of
/// its data being decompressed. With CSV the same numbers had to be computed by
/// reading everything first.
pub fn chunk_stats(
    meta: &ColumnMetaData,
    leaf: &LeafColumn,
    num_rows: usize,
) -> crate::storage::rowgroup::ColumnStats {
    use crate::storage::rowgroup::ColumnStats;

    let Some(s) = &meta.statistics else {
        // A writer that skipped statistics. Unknown bounds mean the zone map
        // rules nothing out, which is the safe direction to be wrong in.
        return ColumnStats {
            min: None,
            max: None,
            null_count: None,
            distinct_count_estimate: None,
        };
    };
    let mut min = s.min_value.as_deref().and_then(|r| stat_value(r, leaf));
    let mut max = s.max_value.as_deref().and_then(|r| stat_value(r, leaf));

    // Statistics that contradict themselves are not statistics.
    //
    // Real files in the wild carry them: `parquet-rs` before 5.0 wrote byte
    // array `min_value`/`max_value` in the order it happened to meet the
    // values rather than in sorted order, so a column whose first two
    // dictionary entries were `BUILDING` and `AUTOMOBILE` claims exactly that
    // as its range. Trusting it makes `WHERE c_mktsegment = 'BUILDING'` return
    // nothing, because the value sorts above a maximum that is not one --
    // silent wrong answers, which is the worst failure a reader has.
    //
    // Sniffing `created_by` for known-bad writers is what some readers do; a
    // bound that is impossible is a better test, because it needs no list and
    // catches writers nobody has heard of yet. Both are dropped rather than
    // swapped: a writer this confused has not earned the assumption that the
    // pair is merely reversed.
    if let (Some(lo), Some(hi)) = (&min, &max) {
        if crate::types::compare(lo, hi) == Some(std::cmp::Ordering::Greater) {
            min = None;
            max = None;
        }
    }

    ColumnStats {
        min,
        max,
        null_count: s.null_count.map(|n| n.clamp(0, num_rows as i64) as usize),
        distinct_count_estimate: s.distinct_count.filter(|d| *d >= 0).map(|d| d as usize),
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;
    use crate::storage::parquet::metadata::{Compression, Encoding, Statistics};
    use crate::storage::Field;
    use crate::types::{DataType, ScalarValue};

    fn column(stats: Statistics) -> (ColumnMetaData, LeafColumn) {
        let meta = ColumnMetaData {
            physical_type: PhysicalType::ByteArray,
            encodings: vec![Encoding::Plain],
            path_in_schema: vec!["s".into()],
            codec: Compression::Uncompressed,
            num_values: 4,
            total_uncompressed_size: 0,
            total_compressed_size: 0,
            data_page_offset: 4,
            dictionary_page_offset: None,
            statistics: Some(stats),
        };
        let leaf = LeafColumn {
            field: Field::new("s", DataType::Utf8, true),
            physical_type: PhysicalType::ByteArray,
            type_length: None,
            max_def_level: 1,
            conversion: Conversion::Direct,
        };
        (meta, leaf)
    }

    #[test]
    fn bounds_in_the_right_order_are_kept() {
        let (meta, leaf) = column(Statistics {
            min_value: Some(b"AUTOMOBILE".to_vec()),
            max_value: Some(b"MACHINERY".to_vec()),
            null_count: Some(1),
            distinct_count: None,
        });
        let s = chunk_stats(&meta, &leaf, 4);
        assert_eq!(s.min, Some(ScalarValue::Utf8("AUTOMOBILE".into())));
        assert_eq!(s.max, Some(ScalarValue::Utf8("MACHINERY".into())));
        assert_eq!(s.null_count, Some(1));
    }

    /// The shape `parquet-rs` 4.3.0 wrote for a dictionary-encoded string
    /// column: whichever value it met first, not the smallest. A zone map built
    /// from it prunes away rows that match.
    #[test]
    fn bounds_that_cross_are_discarded() {
        let (meta, leaf) = column(Statistics {
            min_value: Some(b"BUILDING".to_vec()),
            max_value: Some(b"AUTOMOBILE".to_vec()),
            null_count: Some(0),
            distinct_count: None,
        });
        let s = chunk_stats(&meta, &leaf, 4);
        assert_eq!(s.min, None, "an impossible minimum is not a minimum");
        assert_eq!(s.max, None);
        // The null count is independent and survives.
        assert_eq!(s.null_count, Some(0));
    }

    #[test]
    fn a_missing_null_count_stays_unknown() {
        let (meta, leaf) = column(Statistics {
            min_value: Some(b"a".to_vec()),
            max_value: Some(b"z".to_vec()),
            null_count: None,
            distinct_count: None,
        });
        assert_eq!(chunk_stats(&meta, &leaf, 4).null_count, None);
    }

    #[test]
    fn no_statistics_at_all_means_no_bounds_and_no_count() {
        let (mut meta, leaf) = column(Statistics::default());
        meta.statistics = None;
        let s = chunk_stats(&meta, &leaf, 4);
        assert_eq!((s.min, s.max, s.null_count), (None, None, None));
    }
}
