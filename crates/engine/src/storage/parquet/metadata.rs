//! Parquet's `FileMetaData` and the structs it contains.
//!
//! These mirror `parquet.thrift`, but only the fields this reader acts on.
//! Every struct still *loops* over whatever fields it is handed and skips the
//! ones it does not recognise, so a file written by a newer library stays
//! readable -- see `thrift.rs` for why that is the load-bearing part.
//!
//! Two things here are worth knowing before trusting the numbers.
//!
//! **Statistics come in two generations.** The original `min`/`max` fields
//! ordered `BYTE_ARRAY` values by *signed* byte comparison, which put anything
//! with the high bit set below ASCII and made the bounds useless for UTF-8
//! above the ASCII range. `min_value`/`max_value` replaced them with unsigned
//! ordering. This reader only ever uses the newer pair, because a wrong bound
//! does not produce a slow query -- it produces a missing row.
//!
//! **The schema is a flattened depth-first tree**, not a list of columns. The
//! first element is the root and carries no type; every other element is either
//! a leaf (a real column) or a group with `num_children`. This reader supports
//! flat schemas only, so it walks the tree and refuses anything with nesting.

use crate::error::Result;

use super::thrift::*;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalType {
    Boolean,
    Int32,
    Int64,
    Int96,
    Float,
    Double,
    ByteArray,
    FixedLenByteArray,
}

impl PhysicalType {
    pub fn from_i32(v: i32) -> Option<PhysicalType> {
        Some(match v {
            0 => PhysicalType::Boolean,
            1 => PhysicalType::Int32,
            2 => PhysicalType::Int64,
            3 => PhysicalType::Int96,
            4 => PhysicalType::Float,
            5 => PhysicalType::Double,
            6 => PhysicalType::ByteArray,
            7 => PhysicalType::FixedLenByteArray,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            PhysicalType::Boolean => "BOOLEAN",
            PhysicalType::Int32 => "INT32",
            PhysicalType::Int64 => "INT64",
            PhysicalType::Int96 => "INT96",
            PhysicalType::Float => "FLOAT",
            PhysicalType::Double => "DOUBLE",
            PhysicalType::ByteArray => "BYTE_ARRAY",
            PhysicalType::FixedLenByteArray => "FIXED_LEN_BYTE_ARRAY",
        }
    }

    /// Bytes per value for the fixed-width types, in PLAIN encoding.
    pub fn plain_width(&self) -> Option<usize> {
        match self {
            PhysicalType::Int32 | PhysicalType::Float => Some(4),
            PhysicalType::Int64 | PhysicalType::Double => Some(8),
            PhysicalType::Int96 => Some(12),
            _ => None,
        }
    }
}

/// The pre-`LogicalType` annotations. Still written by every library for
/// backward compatibility, and still the only annotation some files carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertedType {
    Utf8,
    Map,
    MapKeyValue,
    List,
    Enum,
    Decimal,
    Date,
    TimeMillis,
    TimeMicros,
    TimestampMillis,
    TimestampMicros,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    Int8,
    Int16,
    Int32,
    Int64,
    Json,
    Bson,
    Interval,
}

impl ConvertedType {
    pub fn from_i32(v: i32) -> Option<ConvertedType> {
        use ConvertedType::*;
        Some(match v {
            0 => Utf8,
            1 => Map,
            2 => MapKeyValue,
            3 => List,
            4 => Enum,
            5 => Decimal,
            6 => Date,
            7 => TimeMillis,
            8 => TimeMicros,
            9 => TimestampMillis,
            10 => TimestampMicros,
            11 => Uint8,
            12 => Uint16,
            13 => Uint32,
            14 => Uint64,
            15 => Int8,
            16 => Int16,
            17 => Int32,
            18 => Int64,
            19 => Json,
            20 => Bson,
            21 => Interval,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        use ConvertedType::*;
        match self {
            Utf8 => "UTF8",
            Map => "MAP",
            MapKeyValue => "MAP_KEY_VALUE",
            List => "LIST",
            Enum => "ENUM",
            Decimal => "DECIMAL",
            Date => "DATE",
            TimeMillis => "TIME_MILLIS",
            TimeMicros => "TIME_MICROS",
            TimestampMillis => "TIMESTAMP_MILLIS",
            TimestampMicros => "TIMESTAMP_MICROS",
            Uint8 => "UINT_8",
            Uint16 => "UINT_16",
            Uint32 => "UINT_32",
            Uint64 => "UINT_64",
            Int8 => "INT_8",
            Int16 => "INT_16",
            Int32 => "INT_32",
            Int64 => "INT_64",
            Json => "JSON",
            Bson => "BSON",
            Interval => "INTERVAL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    Millis,
    Micros,
    Nanos,
}

/// The modern annotation. A union in Thrift, so exactly one field is present
/// and its *field id* is what identifies the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalType {
    String,
    Map,
    List,
    Enum,
    Decimal { scale: i32, precision: i32 },
    Date,
    Time { unit: TimeUnit },
    Timestamp { utc: bool, unit: TimeUnit },
    Integer { bits: i8, signed: bool },
    Unknown,
    Json,
    Bson,
    Uuid,
    Float16,
    /// A variant this reader has not been taught. Named by field id so the
    /// diagnostic can say which.
    Other(i16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repetition {
    Required,
    Optional,
    Repeated,
}

impl Repetition {
    fn from_i32(v: i32) -> Option<Repetition> {
        Some(match v {
            0 => Repetition::Required,
            1 => Repetition::Optional,
            2 => Repetition::Repeated,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Plain,
    PlainDictionary,
    Rle,
    BitPacked,
    DeltaBinaryPacked,
    DeltaLengthByteArray,
    DeltaByteArray,
    RleDictionary,
    ByteStreamSplit,
    Other(i32),
}

impl Encoding {
    pub fn from_i32(v: i32) -> Encoding {
        match v {
            0 => Encoding::Plain,
            2 => Encoding::PlainDictionary,
            3 => Encoding::Rle,
            4 => Encoding::BitPacked,
            5 => Encoding::DeltaBinaryPacked,
            6 => Encoding::DeltaLengthByteArray,
            7 => Encoding::DeltaByteArray,
            8 => Encoding::RleDictionary,
            9 => Encoding::ByteStreamSplit,
            other => Encoding::Other(other),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Encoding::Plain => "PLAIN".into(),
            Encoding::PlainDictionary => "PLAIN_DICTIONARY".into(),
            Encoding::Rle => "RLE".into(),
            Encoding::BitPacked => "BIT_PACKED".into(),
            Encoding::DeltaBinaryPacked => "DELTA_BINARY_PACKED".into(),
            Encoding::DeltaLengthByteArray => "DELTA_LENGTH_BYTE_ARRAY".into(),
            Encoding::DeltaByteArray => "DELTA_BYTE_ARRAY".into(),
            Encoding::RleDictionary => "RLE_DICTIONARY".into(),
            Encoding::ByteStreamSplit => "BYTE_STREAM_SPLIT".into(),
            Encoding::Other(v) => format!("encoding #{v}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Uncompressed,
    Snappy,
    Gzip,
    Lzo,
    Brotli,
    Lz4,
    Zstd,
    Lz4Raw,
    Other(i32),
}

impl Compression {
    fn from_i32(v: i32) -> Compression {
        match v {
            0 => Compression::Uncompressed,
            1 => Compression::Snappy,
            2 => Compression::Gzip,
            3 => Compression::Lzo,
            4 => Compression::Brotli,
            5 => Compression::Lz4,
            6 => Compression::Zstd,
            7 => Compression::Lz4Raw,
            other => Compression::Other(other),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Compression::Uncompressed => "UNCOMPRESSED".into(),
            Compression::Snappy => "SNAPPY".into(),
            Compression::Gzip => "GZIP".into(),
            Compression::Lzo => "LZO".into(),
            Compression::Brotli => "BROTLI".into(),
            Compression::Lz4 => "LZ4".into(),
            Compression::Zstd => "ZSTD".into(),
            Compression::Lz4Raw => "LZ4_RAW".into(),
            Compression::Other(v) => format!("codec #{v}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    DataPage,
    IndexPage,
    DictionaryPage,
    DataPageV2,
    Other(i32),
}

impl PageType {
    fn from_i32(v: i32) -> PageType {
        match v {
            0 => PageType::DataPage,
            1 => PageType::IndexPage,
            2 => PageType::DictionaryPage,
            3 => PageType::DataPageV2,
            other => PageType::Other(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Statistics {
    pub null_count: Option<i64>,
    pub distinct_count: Option<i64>,
    /// Only ever the modern `min_value`/`max_value`; see the module comment.
    pub min_value: Option<Vec<u8>>,
    pub max_value: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct SchemaElement {
    pub physical_type: Option<PhysicalType>,
    pub type_length: Option<i32>,
    pub repetition: Option<Repetition>,
    pub name: String,
    pub num_children: Option<i32>,
    pub converted_type: Option<ConvertedType>,
    pub scale: Option<i32>,
    pub precision: Option<i32>,
    pub logical_type: Option<LogicalType>,
}

#[derive(Debug, Clone)]
pub struct ColumnMetaData {
    pub physical_type: PhysicalType,
    pub encodings: Vec<Encoding>,
    pub path_in_schema: Vec<String>,
    pub codec: Compression,
    pub num_values: i64,
    pub total_uncompressed_size: i64,
    pub total_compressed_size: i64,
    pub data_page_offset: i64,
    pub dictionary_page_offset: Option<i64>,
    pub statistics: Option<Statistics>,
}

impl ColumnMetaData {
    /// Where this column's pages begin.
    ///
    /// A dictionary page, when present, precedes the data pages, and
    /// `data_page_offset` points past it -- so starting there would skip the
    /// dictionary and leave every index unresolvable.
    pub fn start_offset(&self) -> i64 {
        match self.dictionary_page_offset {
            Some(d) if d > 0 && d < self.data_page_offset => d,
            _ => self.data_page_offset,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColumnChunk {
    pub file_path: Option<String>,
    pub meta_data: Option<ColumnMetaData>,
}

#[derive(Debug, Clone)]
pub struct RowGroupMeta {
    pub columns: Vec<ColumnChunk>,
    pub total_byte_size: i64,
    pub num_rows: i64,
}

#[derive(Debug, Clone)]
pub struct FileMetaData {
    pub version: i32,
    pub schema: Vec<SchemaElement>,
    pub num_rows: i64,
    pub row_groups: Vec<RowGroupMeta>,
    pub created_by: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DataPageHeader {
    pub num_values: i32,
    pub encoding: Encoding,
    pub definition_level_encoding: Encoding,
}

#[derive(Debug, Clone)]
pub struct DataPageHeaderV2 {
    pub num_values: i32,
    pub num_nulls: i32,
    pub num_rows: i32,
    pub encoding: Encoding,
    pub definition_levels_byte_length: i32,
    pub repetition_levels_byte_length: i32,
    pub is_compressed: bool,
}

#[derive(Debug, Clone)]
pub struct DictionaryPageHeader {
    pub num_values: i32,
    pub encoding: Encoding,
}

#[derive(Debug, Clone)]
pub struct PageHeader {
    pub page_type: PageType,
    pub uncompressed_page_size: i32,
    pub compressed_page_size: i32,
    pub data_page: Option<DataPageHeader>,
    pub data_page_v2: Option<DataPageHeaderV2>,
    pub dictionary_page: Option<DictionaryPageHeader>,
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Read a value only if the field's type is the one expected, so that a field
/// whose type changed between format versions is skipped rather than misread.
macro_rules! typed {
    ($r:expr, $ty:expr, $want:expr, $read:expr) => {
        if $ty == $want {
            Some($read?)
        } else {
            $r.skip($ty)?;
            None
        }
    };
}

pub fn read_statistics(r: &mut ThriftReader) -> Result<Statistics> {
    let mut s = Statistics::default();
    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            // 1 (max) and 2 (min) are the deprecated signed-comparison pair and
            // are deliberately dropped on the floor.
            3 => s.null_count = typed!(r, ty, T_I64, r.read_i64()),
            4 => s.distinct_count = typed!(r, ty, T_I64, r.read_i64()),
            5 => s.max_value = typed!(r, ty, T_BINARY, r.read_binary().map(<[u8]>::to_vec)),
            6 => s.min_value = typed!(r, ty, T_BINARY, r.read_binary().map(<[u8]>::to_vec)),
            _ => r.skip(ty)?,
        }
    }
    r.end_struct();
    Ok(s)
}

fn read_time_unit(r: &mut ThriftReader) -> Result<TimeUnit> {
    // A union: whichever field is present names the unit.
    let mut unit = TimeUnit::Millis;
    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            1 => {
                unit = TimeUnit::Millis;
                r.skip(ty)?;
            }
            2 => {
                unit = TimeUnit::Micros;
                r.skip(ty)?;
            }
            3 => {
                unit = TimeUnit::Nanos;
                r.skip(ty)?;
            }
            _ => r.skip(ty)?,
        }
    }
    r.end_struct();
    Ok(unit)
}

fn read_logical_type(r: &mut ThriftReader) -> Result<LogicalType> {
    let mut out = LogicalType::Other(-1);
    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            1 => {
                out = LogicalType::String;
                r.skip(ty)?;
            }
            2 => {
                out = LogicalType::Map;
                r.skip(ty)?;
            }
            3 => {
                out = LogicalType::List;
                r.skip(ty)?;
            }
            4 => {
                out = LogicalType::Enum;
                r.skip(ty)?;
            }
            5 => {
                let (mut scale, mut precision) = (0, 0);
                r.begin_struct();
                while let Some((f, fty)) = r.next_field()? {
                    match f {
                        1 => scale = typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0),
                        2 => precision = typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0),
                        _ => r.skip(fty)?,
                    }
                }
                r.end_struct();
                out = LogicalType::Decimal { scale, precision };
            }
            6 => {
                out = LogicalType::Date;
                r.skip(ty)?;
            }
            7 => {
                let mut unit = TimeUnit::Millis;
                r.begin_struct();
                while let Some((f, fty)) = r.next_field()? {
                    match f {
                        2 if fty == T_STRUCT => unit = read_time_unit(r)?,
                        _ => r.skip(fty)?,
                    }
                }
                r.end_struct();
                out = LogicalType::Time { unit };
            }
            8 => {
                let (mut utc, mut unit) = (false, TimeUnit::Millis);
                r.begin_struct();
                while let Some((f, fty)) = r.next_field()? {
                    match f {
                        1 => utc = r.read_bool_field(fty)?,
                        2 if fty == T_STRUCT => unit = read_time_unit(r)?,
                        _ => r.skip(fty)?,
                    }
                }
                r.end_struct();
                out = LogicalType::Timestamp { utc, unit };
            }
            10 => {
                let (mut bits, mut signed) = (64i8, true);
                r.begin_struct();
                while let Some((f, fty)) = r.next_field()? {
                    match f {
                        1 => bits = typed!(r, fty, T_BYTE, r.read_byte()).unwrap_or(64),
                        2 => signed = r.read_bool_field(fty)?,
                        _ => r.skip(fty)?,
                    }
                }
                r.end_struct();
                out = LogicalType::Integer { bits, signed };
            }
            11 => {
                out = LogicalType::Unknown;
                r.skip(ty)?;
            }
            12 => {
                out = LogicalType::Json;
                r.skip(ty)?;
            }
            13 => {
                out = LogicalType::Bson;
                r.skip(ty)?;
            }
            14 => {
                out = LogicalType::Uuid;
                r.skip(ty)?;
            }
            15 => {
                out = LogicalType::Float16;
                r.skip(ty)?;
            }
            other => {
                out = LogicalType::Other(other);
                r.skip(ty)?;
            }
        }
    }
    r.end_struct();
    Ok(out)
}

fn read_schema_element(r: &mut ThriftReader) -> Result<SchemaElement> {
    let mut e = SchemaElement {
        physical_type: None,
        type_length: None,
        repetition: None,
        name: String::new(),
        num_children: None,
        converted_type: None,
        scale: None,
        precision: None,
        logical_type: None,
    };
    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            1 => e.physical_type = typed!(r, ty, T_I32, r.read_i32()).and_then(PhysicalType::from_i32),
            2 => e.type_length = typed!(r, ty, T_I32, r.read_i32()),
            3 => e.repetition = typed!(r, ty, T_I32, r.read_i32()).and_then(Repetition::from_i32),
            4 => e.name = typed!(r, ty, T_BINARY, r.read_string()).unwrap_or_default(),
            5 => e.num_children = typed!(r, ty, T_I32, r.read_i32()),
            6 => {
                e.converted_type = typed!(r, ty, T_I32, r.read_i32()).and_then(ConvertedType::from_i32)
            }
            7 => e.scale = typed!(r, ty, T_I32, r.read_i32()),
            8 => e.precision = typed!(r, ty, T_I32, r.read_i32()),
            10 if ty == T_STRUCT => e.logical_type = Some(read_logical_type(r)?),
            _ => r.skip(ty)?,
        }
    }
    r.end_struct();
    Ok(e)
}

fn read_column_meta_data(r: &mut ThriftReader) -> Result<ColumnMetaData> {
    let mut physical_type = PhysicalType::Boolean;
    let mut encodings = Vec::new();
    let mut path_in_schema = Vec::new();
    let mut codec = Compression::Uncompressed;
    let mut num_values = 0;
    let mut total_uncompressed_size = 0;
    let mut total_compressed_size = 0;
    let mut data_page_offset = 0;
    let mut dictionary_page_offset = None;
    let mut statistics = None;

    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            1 => {
                physical_type = typed!(r, ty, T_I32, r.read_i32())
                    .and_then(PhysicalType::from_i32)
                    .unwrap_or(PhysicalType::Boolean)
            }
            2 if ty == T_LIST => {
                let (len, elem) = r.read_list_header()?;
                for _ in 0..len {
                    if elem == T_I32 {
                        encodings.push(Encoding::from_i32(r.read_i32()?));
                    } else {
                        r.skip(elem)?;
                    }
                }
            }
            3 if ty == T_LIST => {
                let (len, elem) = r.read_list_header()?;
                for _ in 0..len {
                    if elem == T_BINARY {
                        path_in_schema.push(r.read_string()?);
                    } else {
                        r.skip(elem)?;
                    }
                }
            }
            4 => {
                codec = typed!(r, ty, T_I32, r.read_i32())
                    .map(Compression::from_i32)
                    .unwrap_or(Compression::Uncompressed)
            }
            5 => num_values = typed!(r, ty, T_I64, r.read_i64()).unwrap_or(0),
            6 => total_uncompressed_size = typed!(r, ty, T_I64, r.read_i64()).unwrap_or(0),
            7 => total_compressed_size = typed!(r, ty, T_I64, r.read_i64()).unwrap_or(0),
            9 => data_page_offset = typed!(r, ty, T_I64, r.read_i64()).unwrap_or(0),
            11 => dictionary_page_offset = typed!(r, ty, T_I64, r.read_i64()),
            12 if ty == T_STRUCT => statistics = Some(read_statistics(r)?),
            _ => r.skip(ty)?,
        }
    }
    r.end_struct();

    Ok(ColumnMetaData {
        physical_type,
        encodings,
        path_in_schema,
        codec,
        num_values,
        total_uncompressed_size,
        total_compressed_size,
        data_page_offset,
        dictionary_page_offset,
        statistics,
    })
}

fn read_column_chunk(r: &mut ThriftReader) -> Result<ColumnChunk> {
    let mut file_path = None;
    let mut meta_data = None;
    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            1 => file_path = typed!(r, ty, T_BINARY, r.read_string()),
            3 if ty == T_STRUCT => meta_data = Some(read_column_meta_data(r)?),
            _ => r.skip(ty)?,
        }
    }
    r.end_struct();
    Ok(ColumnChunk {
        file_path,
        meta_data,
    })
}

fn read_row_group(r: &mut ThriftReader) -> Result<RowGroupMeta> {
    let mut columns = Vec::new();
    let mut total_byte_size = 0;
    let mut num_rows = 0;
    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            1 if ty == T_LIST => {
                let (len, elem) = r.read_list_header()?;
                for _ in 0..len {
                    if elem == T_STRUCT {
                        columns.push(read_column_chunk(r)?);
                    } else {
                        r.skip(elem)?;
                    }
                }
            }
            2 => total_byte_size = typed!(r, ty, T_I64, r.read_i64()).unwrap_or(0),
            3 => num_rows = typed!(r, ty, T_I64, r.read_i64()).unwrap_or(0),
            _ => r.skip(ty)?,
        }
    }
    r.end_struct();
    Ok(RowGroupMeta {
        columns,
        total_byte_size,
        num_rows,
    })
}

pub fn read_file_metadata(bytes: &[u8]) -> Result<FileMetaData> {
    let mut r = ThriftReader::new(bytes);
    let mut version = 0;
    let mut schema = Vec::new();
    let mut num_rows = 0;
    let mut row_groups = Vec::new();
    let mut created_by = None;

    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            1 => version = typed!(r, ty, T_I32, r.read_i32()).unwrap_or(0),
            2 if ty == T_LIST => {
                let (len, elem) = r.read_list_header()?;
                for _ in 0..len {
                    if elem == T_STRUCT {
                        schema.push(read_schema_element(&mut r)?);
                    } else {
                        r.skip(elem)?;
                    }
                }
            }
            3 => num_rows = typed!(r, ty, T_I64, r.read_i64()).unwrap_or(0),
            4 if ty == T_LIST => {
                let (len, elem) = r.read_list_header()?;
                for _ in 0..len {
                    if elem == T_STRUCT {
                        row_groups.push(read_row_group(&mut r)?);
                    } else {
                        r.skip(elem)?;
                    }
                }
            }
            6 => created_by = typed!(r, ty, T_BINARY, r.read_string()),
            _ => r.skip(ty)?,
        }
    }
    r.end_struct();

    if schema.is_empty() {
        return Err(err("Parquet footer carries no schema"));
    }
    Ok(FileMetaData {
        version,
        schema,
        num_rows,
        row_groups,
        created_by,
    })
}

pub fn read_page_header(r: &mut ThriftReader) -> Result<PageHeader> {
    let mut page_type = PageType::Other(-1);
    let mut uncompressed_page_size = 0;
    let mut compressed_page_size = 0;
    let mut data_page = None;
    let mut data_page_v2 = None;
    let mut dictionary_page = None;

    r.begin_struct();
    while let Some((id, ty)) = r.next_field()? {
        match id {
            1 => {
                page_type = typed!(r, ty, T_I32, r.read_i32())
                    .map(PageType::from_i32)
                    .unwrap_or(PageType::Other(-1))
            }
            2 => uncompressed_page_size = typed!(r, ty, T_I32, r.read_i32()).unwrap_or(0),
            3 => compressed_page_size = typed!(r, ty, T_I32, r.read_i32()).unwrap_or(0),
            5 if ty == T_STRUCT => {
                let (mut num_values, mut encoding, mut def) =
                    (0, Encoding::Plain, Encoding::Rle);
                r.begin_struct();
                while let Some((f, fty)) = r.next_field()? {
                    match f {
                        1 => num_values = typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0),
                        2 => {
                            encoding = typed!(r, fty, T_I32, r.read_i32())
                                .map(Encoding::from_i32)
                                .unwrap_or(Encoding::Plain)
                        }
                        3 => {
                            def = typed!(r, fty, T_I32, r.read_i32())
                                .map(Encoding::from_i32)
                                .unwrap_or(Encoding::Rle)
                        }
                        _ => r.skip(fty)?,
                    }
                }
                r.end_struct();
                data_page = Some(DataPageHeader {
                    num_values,
                    encoding,
                    definition_level_encoding: def,
                });
            }
            7 if ty == T_STRUCT => {
                let (mut num_values, mut encoding) = (0, Encoding::Plain);
                r.begin_struct();
                while let Some((f, fty)) = r.next_field()? {
                    match f {
                        1 => num_values = typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0),
                        2 => {
                            encoding = typed!(r, fty, T_I32, r.read_i32())
                                .map(Encoding::from_i32)
                                .unwrap_or(Encoding::Plain)
                        }
                        _ => r.skip(fty)?,
                    }
                }
                r.end_struct();
                dictionary_page = Some(DictionaryPageHeader {
                    num_values,
                    encoding,
                });
            }
            8 if ty == T_STRUCT => {
                let mut h = DataPageHeaderV2 {
                    num_values: 0,
                    num_nulls: 0,
                    num_rows: 0,
                    encoding: Encoding::Plain,
                    definition_levels_byte_length: 0,
                    repetition_levels_byte_length: 0,
                    // Defaults to true in the schema, and writers omit it when
                    // it is true -- so the default must be true here too.
                    is_compressed: true,
                };
                r.begin_struct();
                while let Some((f, fty)) = r.next_field()? {
                    match f {
                        1 => h.num_values = typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0),
                        2 => h.num_nulls = typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0),
                        3 => h.num_rows = typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0),
                        4 => {
                            h.encoding = typed!(r, fty, T_I32, r.read_i32())
                                .map(Encoding::from_i32)
                                .unwrap_or(Encoding::Plain)
                        }
                        5 => {
                            h.definition_levels_byte_length =
                                typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0)
                        }
                        6 => {
                            h.repetition_levels_byte_length =
                                typed!(r, fty, T_I32, r.read_i32()).unwrap_or(0)
                        }
                        7 => h.is_compressed = r.read_bool_field(fty)?,
                        _ => r.skip(fty)?,
                    }
                }
                r.end_struct();
                data_page_v2 = Some(h);
            }
            _ => r.skip(ty)?,
        }
    }
    r.end_struct();

    Ok(PageHeader {
        page_type,
        uncompressed_page_size,
        compressed_page_size,
        data_page,
        data_page_v2,
        dictionary_page,
    })
}
