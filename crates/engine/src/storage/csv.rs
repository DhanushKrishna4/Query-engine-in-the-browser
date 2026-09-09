//! CSV ingestion with type inference.
//!
//! Two passes over the input buffer: the first samples records to decide each
//! column's type, the second builds the columns. Two passes over bytes we
//! already hold beats one pass that materializes every field as a `String`,
//! and it keeps peak memory close to the size of the finished columns.
//!
//! Quoting follows RFC 4180: fields may be wrapped in `"`, a doubled `""` is a
//! literal quote, and a quoted field may contain commas and newlines. An
//! *unquoted* empty field is NULL; a *quoted* empty field (`""`) is the empty
//! string. That distinction is the only way a CSV can express "empty text" as
//! opposed to "missing", so it is worth honouring.

use std::sync::Arc;

use crate::error::{Diagnostic, Result, Stage};
use crate::storage::column::ColumnBuilder;
use crate::storage::rowgroup::{RowGroup, ROW_GROUP_SIZE};
use crate::storage::schema::{Field, Schema};
use crate::storage::table::Table;
use crate::types::{self, DataType, ScalarValue};

/// How many records to sample before committing to a set of column types.
const INFER_SAMPLE_RECORDS: usize = 1000;

#[derive(Debug, Clone)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub has_header: bool,
    /// `None` samples the whole file.
    pub infer_records: Option<usize>,
    pub row_group_size: usize,
}

impl Default for CsvOptions {
    fn default() -> CsvOptions {
        CsvOptions {
            delimiter: b',',
            has_header: true,
            infer_records: Some(INFER_SAMPLE_RECORDS),
            row_group_size: ROW_GROUP_SIZE,
        }
    }
}

/// Read a CSV buffer into a `Table`.
pub fn read_csv(name: &str, bytes: &[u8], opts: &CsvOptions) -> Result<Table> {
    let mut reader = RecordReader::new(bytes, opts.delimiter);
    let mut record: Vec<CsvField> = Vec::new();

    // -- header -----------------------------------------------------------
    if !reader.next_record(&mut record)? {
        return Err(err("CSV input is empty"));
    }
    let header: Vec<String> = if opts.has_header {
        record.iter().map(|f| f.text.clone()).collect()
    } else {
        (0..record.len()).map(|i| format!("column_{i}")).collect()
    };
    let num_cols = header.len();
    if num_cols == 0 {
        return Err(err("CSV header has no columns"));
    }
    check_duplicate_headers(&header)?;

    let body_start = if opts.has_header { reader.pos } else { 0 };

    // -- pass 1: infer types ----------------------------------------------
    let mut inferences: Vec<TypeInference> = vec![TypeInference::default(); num_cols];
    {
        let mut r = RecordReader::new(bytes, opts.delimiter);
        r.pos = body_start;
        let limit = opts.infer_records.unwrap_or(usize::MAX);
        let mut seen = 0usize;
        while seen < limit && r.next_record(&mut record)? {
            check_width(&record, num_cols, r.record_index)?;
            for (i, f) in record.iter().enumerate() {
                inferences[i].observe(f);
            }
            seen += 1;
        }
    }

    let fields: Vec<Field> = header
        .iter()
        .zip(&inferences)
        .map(|(name, inf)| Field::new(name.clone(), inf.resolve(), inf.saw_null))
        .collect();
    let schema = Arc::new(Schema::new(fields));

    // -- pass 2: build columns --------------------------------------------
    let mut row_groups = Vec::new();
    let mut builders = new_builders(&schema);
    let mut rows_in_group = 0usize;

    let mut r = RecordReader::new(bytes, opts.delimiter);
    r.pos = body_start;
    while r.next_record(&mut record)? {
        check_width(&record, num_cols, r.record_index)?;
        for (i, f) in record.iter().enumerate() {
            let dt = schema.field(i).data_type;
            match parse_field(f, &dt) {
                Some(v) => builders[i].append(&v)?,
                None => {
                    // The sample said this column was numeric but a later row
                    // disagrees. Rather than silently NULLing the value (which
                    // hides a real data problem) or re-inferring the whole file,
                    // say exactly which row and column disagreed.
                    return Err(err(format!(
                        "row {}, column `{}`: `{}` is not a valid {} \
                         (type was inferred from the first {} rows)",
                        r.record_index,
                        schema.field(i).name,
                        f.text,
                        dt,
                        opts.infer_records
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "all".into()),
                    )));
                }
            }
        }
        rows_in_group += 1;
        if rows_in_group == opts.row_group_size {
            row_groups.push(finish_group(&mut builders, &schema));
            rows_in_group = 0;
        }
    }
    if rows_in_group > 0 {
        row_groups.push(finish_group(&mut builders, &schema));
    }

    Ok(Table::new(name, schema, row_groups))
}

fn err(msg: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Stage::Execute, msg)
}

fn check_duplicate_headers(header: &[String]) -> Result<()> {
    for (i, a) in header.iter().enumerate() {
        for b in &header[i + 1..] {
            if a.eq_ignore_ascii_case(b) {
                return Err(err(format!("duplicate column name `{a}` in CSV header")));
            }
        }
    }
    Ok(())
}

fn check_width(record: &[CsvField], expected: usize, index: usize) -> Result<()> {
    if record.len() != expected {
        return Err(err(format!(
            "row {index} has {} fields, expected {expected}",
            record.len()
        )));
    }
    Ok(())
}

fn new_builders(schema: &Schema) -> Vec<ColumnBuilder> {
    schema
        .fields
        .iter()
        .map(|f| ColumnBuilder::new(&f.data_type))
        .collect()
}

fn finish_group(builders: &mut Vec<ColumnBuilder>, schema: &Arc<Schema>) -> RowGroup {
    let done = std::mem::replace(builders, new_builders(schema));
    RowGroup::new(done.into_iter().map(|b| b.finish()).collect())
}

// ---------------------------------------------------------------------------
// Type inference
// ---------------------------------------------------------------------------

/// Tracks, per column, which candidate types are still viable. Types are
/// eliminated as counter-examples arrive; whatever survives, in priority order,
/// wins. Utf8 always survives, so inference always terminates on something.
#[derive(Debug, Clone)]
struct TypeInference {
    int32: bool,
    int64: bool,
    float64: bool,
    boolean: bool,
    date: bool,
    timestamp: bool,
    saw_value: bool,
    saw_null: bool,
}

impl Default for TypeInference {
    fn default() -> TypeInference {
        TypeInference {
            int32: true,
            int64: true,
            float64: true,
            boolean: true,
            date: true,
            timestamp: true,
            saw_value: false,
            saw_null: false,
        }
    }
}

impl TypeInference {
    fn observe(&mut self, f: &CsvField) {
        if f.is_null() {
            self.saw_null = true;
            return;
        }
        self.saw_value = true;
        let s = f.text.trim();

        self.int64 &= s.parse::<i64>().is_ok();
        self.int32 &= s.parse::<i32>().is_ok();
        // `1e5` parses as f64 but not as an integer, which is exactly the
        // discrimination we want.
        self.float64 &= s.parse::<f64>().is_ok();
        self.boolean &= matches!(
            s.to_ascii_lowercase().as_str(),
            "true" | "false" | "t" | "f" | "yes" | "no"
        );
        self.date &= types::parse_date(s).is_some();
        self.timestamp &= types::parse_timestamp(s).is_some();
    }

    /// Priority order: the most specific type that still fits. Integers before
    /// floats so that an all-integer column does not become Float64; Date32
    /// before Timestamp so that a date-only column stays 4 bytes wide.
    fn resolve(&self) -> DataType {
        if !self.saw_value {
            // A column that is entirely NULL in the sample has no evidence for
            // any type. Utf8 is the safe landing spot: everything casts out of
            // it, and no later row can contradict it.
            return DataType::Utf8;
        }
        if self.boolean {
            DataType::Boolean
        } else if self.int32 {
            DataType::Int32
        } else if self.int64 {
            DataType::Int64
        } else if self.float64 {
            DataType::Float64
        } else if self.date {
            DataType::Date32
        } else if self.timestamp {
            DataType::Timestamp
        } else {
            DataType::Utf8
        }
    }
}

fn parse_field(f: &CsvField, dt: &DataType) -> Option<ScalarValue> {
    if f.is_null() {
        return Some(ScalarValue::Null);
    }
    let s = f.text.trim();
    Some(match dt {
        DataType::Boolean => match s.to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "1" => ScalarValue::Boolean(true),
            "false" | "f" | "no" | "0" => ScalarValue::Boolean(false),
            _ => return None,
        },
        DataType::Int32 => ScalarValue::Int32(s.parse().ok()?),
        DataType::Int64 => ScalarValue::Int64(s.parse().ok()?),
        DataType::Float64 => ScalarValue::Float64(s.parse().ok()?),
        DataType::Date32 => ScalarValue::Date32(types::parse_date(s)?),
        DataType::Timestamp => ScalarValue::Timestamp(types::parse_timestamp(s)?),
        DataType::Decimal128 { precision, scale } => ScalarValue::Decimal128 {
            value: types::parse_decimal(s, *scale)?,
            precision: *precision,
            scale: *scale,
        },
        // Utf8 keeps the field verbatim, including any leading/trailing spaces.
        DataType::Utf8 => ScalarValue::Utf8(f.text.clone()),
        DataType::Null => ScalarValue::Null,
    })
}

// ---------------------------------------------------------------------------
// Record scanning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct CsvField {
    text: String,
    /// Whether the field was written with surrounding quotes.
    quoted: bool,
}

impl CsvField {
    /// Only an *unquoted* empty field means NULL; `""` is the empty string.
    fn is_null(&self) -> bool {
        !self.quoted && self.text.trim().is_empty()
    }
}

struct RecordReader<'a> {
    src: &'a [u8],
    pos: usize,
    delimiter: u8,
    /// 1-based index of the last record returned, for error messages.
    record_index: usize,
}

impl<'a> RecordReader<'a> {
    fn new(src: &'a [u8], delimiter: u8) -> RecordReader<'a> {
        RecordReader {
            src,
            pos: 0,
            delimiter,
            record_index: 0,
        }
    }

    /// Read one record into `out`. Returns false at end of input. Blank lines
    /// are skipped rather than treated as a one-empty-field record.
    fn next_record(&mut self, out: &mut Vec<CsvField>) -> Result<bool> {
        out.clear();
        loop {
            if self.pos >= self.src.len() {
                return Ok(false);
            }
            // Skip blank lines (and a stray \r before a \n).
            match self.src[self.pos] {
                b'\n' => {
                    self.pos += 1;
                    continue;
                }
                b'\r' if self.src.get(self.pos + 1) == Some(&b'\n') => {
                    self.pos += 2;
                    continue;
                }
                _ => break,
            }
        }

        let mut field = Vec::new();
        let mut quoted = false;
        loop {
            if self.pos >= self.src.len() {
                push_field(out, &mut field, &mut quoted)?;
                self.record_index += 1;
                return Ok(true);
            }
            let b = self.src[self.pos];

            if b == b'"' && field.is_empty() && !quoted {
                quoted = true;
                self.pos += 1;
                let start = self.pos;
                loop {
                    match self.src.get(self.pos) {
                        Some(b'"') if self.src.get(self.pos + 1) == Some(&b'"') => {
                            field.push(b'"');
                            self.pos += 2;
                        }
                        Some(b'"') => {
                            self.pos += 1;
                            break;
                        }
                        Some(c) => {
                            field.push(*c);
                            self.pos += 1;
                        }
                        None => {
                            return Err(err(format!(
                                "unterminated quoted field starting at byte {start}"
                            )))
                        }
                    }
                }
                continue;
            }

            if b == self.delimiter {
                self.pos += 1;
                push_field(out, &mut field, &mut quoted)?;
                continue;
            }
            if b == b'\n' {
                self.pos += 1;
                push_field(out, &mut field, &mut quoted)?;
                self.record_index += 1;
                return Ok(true);
            }
            if b == b'\r' && self.src.get(self.pos + 1) == Some(&b'\n') {
                self.pos += 2;
                push_field(out, &mut field, &mut quoted)?;
                self.record_index += 1;
                return Ok(true);
            }
            field.push(b);
            self.pos += 1;
        }
    }
}

fn push_field(out: &mut Vec<CsvField>, buf: &mut Vec<u8>, quoted: &mut bool) -> Result<()> {
    let text = String::from_utf8(std::mem::take(buf))
        .map_err(|_| err("CSV input is not valid UTF-8"))?;
    out.push(CsvField {
        text,
        quoted: *quoted,
    });
    *quoted = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(csv: &str) -> Table {
        read_csv("t", csv.as_bytes(), &CsvOptions::default()).unwrap()
    }

    #[test]
    fn infers_the_narrowest_type_that_fits() {
        let t = load(
            "i,big,f,b,d,ts,s\n\
             1,3000000000,1.5,true,2024-01-02,2024-01-02 03:04:05,hello\n\
             2,3000000001,2.5,false,2024-01-03,2024-01-03 03:04:05,world\n",
        );
        let types: Vec<DataType> = t.schema.fields.iter().map(|f| f.data_type).collect();
        assert_eq!(
            types,
            vec![
                DataType::Int32,
                DataType::Int64,
                DataType::Float64,
                DataType::Boolean,
                DataType::Date32,
                DataType::Timestamp,
                DataType::Utf8,
            ]
        );
    }

    #[test]
    fn empty_unquoted_field_is_null_but_quoted_empty_is_a_string() {
        let t = load("a,b\n,\"\"\nx,y\n");
        let rg = &t.row_groups[0];
        assert!(t.schema.field(0).nullable);
        assert!(rg.column(0).unwrap().value(0).is_null());
        // `""` is an empty string, so column b has no NULLs at all.
        assert!(!t.schema.field(1).nullable);
        assert_eq!(rg.column(1).unwrap().value(0), ScalarValue::Utf8(String::new()));
    }

    #[test]
    fn quoted_fields_may_contain_delimiters_newlines_and_quotes() {
        let t = load("a,b\n\"x,y\",\"line1\nline2\"\n\"he said \"\"hi\"\"\",z\n");
        let c = &t.row_groups[0].column(0).unwrap();
        assert_eq!(c.value(0), ScalarValue::Utf8("x,y".into()));
        assert_eq!(c.value(1), ScalarValue::Utf8(r#"he said "hi""#.into()));
        assert_eq!(
            t.row_groups[0].column(1).unwrap().value(0),
            ScalarValue::Utf8("line1\nline2".into())
        );
    }

    #[test]
    fn handles_crlf_and_a_missing_trailing_newline() {
        let t = load("a\r\n1\r\n2");
        assert_eq!(t.num_rows(), 2);
    }

    #[test]
    fn row_groups_carry_zone_maps() {
        let mut csv = String::from("x\n");
        for i in 0..10 {
            csv.push_str(&format!("{i}\n"));
        }
        let t = read_csv(
            "t",
            csv.as_bytes(),
            &CsvOptions {
                row_group_size: 4,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(t.num_row_groups(), 3); // 4 + 4 + 2
        assert_eq!(t.num_rows(), 10);
        let s = &t.row_groups[1].stats[0];
        assert_eq!(s.min, Some(ScalarValue::Int32(4)));
        assert_eq!(s.max, Some(ScalarValue::Int32(7)));
        assert_eq!(s.null_count, 0);
        assert_eq!(s.distinct_count_estimate, Some(4));
    }

    #[test]
    fn a_value_contradicting_the_inferred_type_names_the_row_and_column() {
        let mut csv = String::from("x\n");
        for _ in 0..5 {
            csv.push_str("1\n");
        }
        csv.push_str("oops\n");
        let e = read_csv(
            "t",
            csv.as_bytes(),
            &CsvOptions {
                infer_records: Some(3),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.message.contains("row 6"), "{}", e.message);
        assert!(e.message.contains("column `x`"), "{}", e.message);
    }

    #[test]
    fn rejects_ragged_rows_and_duplicate_headers() {
        let e = read_csv("t", b"a,b\n1\n", &CsvOptions::default()).unwrap_err();
        assert!(e.message.contains("expected 2"), "{}", e.message);

        let e = read_csv("t", b"a,A\n1,2\n", &CsvOptions::default()).unwrap_err();
        assert!(e.message.contains("duplicate column name"), "{}", e.message);
    }

    #[test]
    fn all_null_column_falls_back_to_utf8() {
        let t = load("a,b\n,1\n,2\n");
        assert_eq!(t.schema.field(0).data_type, DataType::Utf8);
        assert!(t.schema.field(0).nullable);
    }
}
