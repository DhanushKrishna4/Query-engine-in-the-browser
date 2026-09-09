//! A Parquet reader, written from the format specification.
//!
//! ```text
//!   PAR1 <column chunks, page by page> <FileMetaData> <footer length> PAR1
//! ```
//!
//! The footer is last so that a writer can stream data without knowing the
//! offsets in advance, and it is what makes the format worth reading: it
//! carries the schema, the row-group boundaries and per-column statistics, so
//! a query can decide which column chunks it needs *before* decoding any of
//! them. That is the whole reason this step exists -- with CSV the zone maps
//! describe data already parsed, and here they describe data never read.
//!
//! What is supported, and why the boundary sits where it does:
//!
//! | | |
//! | --- | --- |
//! | schemas | flat only -- lists, maps and structs are refused by name |
//! | pages | v1, v2, dictionary |
//! | encodings | PLAIN, PLAIN_DICTIONARY, RLE_DICTIONARY, RLE, DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY, BYTE_STREAM_SPLIT |
//! | compression | UNCOMPRESSED, SNAPPY |
//!
//! Anything else produces a diagnostic naming it, which is the same contract
//! the SQL surface keeps: `GZIP compression is not supported yet` beats a
//! stream of wrong bytes.

pub mod encoding;
pub mod metadata;
pub mod reader;
pub mod snappy;
pub mod thrift;

use std::sync::Arc;

use crate::error::Result;
use crate::storage::column::Column;
use crate::storage::rowgroup::{ChunkSource, RowGroup};
use crate::storage::table::Table;

use metadata::FileMetaData;
use thrift::{err, ThriftReader};

/// Both ends of the file carry this, which is how a Parquet file is recognised
/// and how a truncated one is caught before anything else is believed.
const MAGIC: &[u8; 4] = b"PAR1";

/// The trailing `PAR1` plus the four-byte footer length that precedes it.
const FOOTER_TAIL: usize = 8;

/// Parse the footer: schema, row groups and per-column statistics.
///
/// Reads only the tail of the file. Nothing in the data pages is touched.
pub fn read_footer(bytes: &[u8]) -> Result<FileMetaData> {
    if bytes.len() < FOOTER_TAIL + MAGIC.len() {
        return Err(err(format!(
            "not a Parquet file: {} bytes is too short to hold a footer",
            bytes.len()
        )));
    }
    if &bytes[..4] != MAGIC {
        return Err(err(
            "not a Parquet file: it does not begin with the PAR1 magic bytes",
        ));
    }
    let tail = &bytes[bytes.len() - 4..];
    if tail != MAGIC {
        // An encrypted footer ends with PARE instead, which is worth naming
        // rather than reporting as corruption.
        if tail == b"PARE" {
            return Err(err("this Parquet file has an encrypted footer, which is not supported"));
        }
        return Err(err(
            "truncated Parquet file: the trailing PAR1 magic bytes are missing",
        ));
    }

    let len_at = bytes.len() - FOOTER_TAIL;
    let footer_len = u32::from_le_bytes([
        bytes[len_at],
        bytes[len_at + 1],
        bytes[len_at + 2],
        bytes[len_at + 3],
    ]) as usize;

    let start = len_at
        .checked_sub(footer_len)
        .filter(|s| *s >= MAGIC.len())
        .ok_or_else(|| {
            err(format!(
                "Parquet footer claims {footer_len} bytes but the file is only {} long",
                bytes.len()
            ))
        })?;

    metadata::read_file_metadata(&bytes[start..len_at])
}

/// Read one page header at `offset`, returning it and the offset of the page
/// body.
///
/// Page headers are Thrift structs of no declared length, written back to back
/// with their bodies, so the only way to find the next page is to decode this
/// one and add the sizes it reports.
pub fn read_page_header(bytes: &[u8], offset: usize) -> Result<(metadata::PageHeader, usize)> {
    let slice = bytes
        .get(offset..)
        .ok_or_else(|| err(format!("page offset {offset} is past the end of the file")))?;
    let mut r = ThriftReader::new(slice);
    let header = metadata::read_page_header(&mut r)?;
    Ok((header, offset + r.position()))
}

/// One row group's column chunks, decodable on demand.
///
/// Holds the whole file behind an `Arc` rather than copying each chunk out:
/// the bytes are shared by every row group and every column, and a chunk is a
/// slice of them. In the browser this is the buffer that came off the network.
#[derive(Debug)]
struct ParquetChunks {
    bytes: Arc<[u8]>,
    chunks: Vec<metadata::ColumnMetaData>,
    leaves: Arc<Vec<reader::LeafColumn>>,
    num_rows: usize,
}

impl ChunkSource for ParquetChunks {
    fn decode(&self, column: usize) -> Result<Column> {
        let meta = self
            .chunks
            .get(column)
            .ok_or_else(|| err(format!("row group has no chunk for column {column}")))?;
        reader::decode_chunk(&self.bytes, meta, &self.leaves[column], self.num_rows)
    }

    fn byte_size(&self) -> usize {
        self.chunks
            .iter()
            .map(|c| c.total_compressed_size as usize)
            .sum()
    }
}

/// Load a Parquet file as a table.
///
/// Only the footer is read. The schema and every row group's zone map come out
/// of it, so the table is queryable -- and prunable -- before a single column
/// chunk has been decompressed. Chunks are decoded the first time a scan asks
/// for one and cached from then on, which means a query that touches two
/// columns of a twelve-column file reads two columns of it.
pub fn read_parquet(name: &str, bytes: Arc<[u8]>) -> Result<Table> {
    let meta = read_footer(&bytes)?;
    let leaves = Arc::new(reader::schema_leaves(&meta)?);
    let schema = Arc::new(reader::arrow_schema(&leaves));

    let mut row_groups = Vec::with_capacity(meta.row_groups.len());
    for rg in &meta.row_groups {
        let num_rows = rg.num_rows as usize;
        let mut chunks = Vec::with_capacity(leaves.len());
        let mut stats = Vec::with_capacity(leaves.len());
        for (i, leaf) in leaves.iter().enumerate() {
            let chunk = rg
                .columns
                .get(i)
                .and_then(|c| c.meta_data.as_ref())
                .ok_or_else(|| {
                    err(format!(
                        "row group has no metadata for column `{}`",
                        leaf.field.name
                    ))
                })?;
            if let Some(path) = rg.columns[i].file_path.as_deref().filter(|p| !p.is_empty()) {
                return Err(err(format!(
                    "column `{}` lives in a separate file (`{path}`);                      multi-file Parquet datasets are not supported",
                    leaf.field.name
                )));
            }
            reader::check_supported(chunk, leaf)?;
            stats.push(reader::chunk_stats(chunk, leaf, num_rows));
            chunks.push(chunk.clone());
        }
        let source = Arc::new(ParquetChunks {
            bytes: Arc::clone(&bytes),
            chunks,
            leaves: Arc::clone(&leaves),
            num_rows,
        });
        row_groups.push(RowGroup::pending(source, stats, num_rows));
    }

    Ok(Table::new(name, schema, row_groups))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_arc(name: &str) -> std::sync::Arc<[u8]> {
        fixture(name).into()
    }

    pub(super) fn fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .unwrap()
            .join("tests/parquet")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read {}: {e}\nrun `python3 tools/gen_parquet.py` to create the fixtures",
                path.display()
            )
        })
    }

    #[test]
    fn reads_a_real_footer() {
        let bytes = fixture("people.parquet");
        let meta = read_footer(&bytes).unwrap();
        assert_eq!(meta.num_rows, 10);
        assert_eq!(meta.row_groups.len(), 1);
        // The root element plus nine columns.
        assert_eq!(meta.schema.len(), 10);
        assert_eq!(meta.schema[0].num_children, Some(9));
        let names: Vec<&str> = meta.schema[1..].iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["id", "name", "age", "city", "department", "salary", "score", "hired", "active"]
        );
        assert!(meta.created_by.as_deref().unwrap_or("").contains("parquet"));

        let rg = &meta.row_groups[0];
        assert_eq!(rg.num_rows, 10);
        assert_eq!(rg.columns.len(), 9);
        let score = rg.columns[6].meta_data.as_ref().unwrap();
        assert_eq!(score.physical_type, metadata::PhysicalType::Double);
        // `score` has two NULLs in the fixture, and the footer knows it without
        // a byte of the column being read.
        assert_eq!(score.statistics.as_ref().unwrap().null_count, Some(2));
    }

    #[test]
    fn statistics_carry_bounds_for_the_zone_maps() {
        let bytes = fixture("people.parquet");
        let meta = read_footer(&bytes).unwrap();
        let age = meta.row_groups[0].columns[2].meta_data.as_ref().unwrap();
        let stats = age.statistics.as_ref().unwrap();
        let min = i32::from_le_bytes(stats.min_value.as_ref().unwrap()[..4].try_into().unwrap());
        let max = i32::from_le_bytes(stats.max_value.as_ref().unwrap()[..4].try_into().unwrap());
        assert_eq!((min, max), (36, 61));
    }

    #[test]
    fn walks_every_page_of_a_chunk() {
        // Small pages, so the chunk holds many of them and the header/body
        // arithmetic is actually exercised.
        let bytes = fixture("many_pages.parquet");
        let meta = read_footer(&bytes).unwrap();
        let chunk = meta.row_groups[0].columns[3].meta_data.as_ref().unwrap();

        let mut offset = chunk.start_offset() as usize;
        let end = offset + chunk.total_compressed_size as usize;
        let mut values = 0i64;
        let mut pages = 0;
        while offset < end {
            let (header, body) = read_page_header(&bytes, offset).unwrap();
            pages += 1;
            match header.page_type {
                metadata::PageType::DataPage => {
                    values += header.data_page.as_ref().unwrap().num_values as i64
                }
                metadata::PageType::DataPageV2 => {
                    values += header.data_page_v2.as_ref().unwrap().num_values as i64
                }
                _ => {}
            }
            offset = body + header.compressed_page_size as usize;
        }
        assert!(pages > 3, "expected several pages, saw {pages}");
        assert_eq!(values, chunk.num_values, "page values must sum to the chunk's");
        assert_eq!(offset, end, "page walk must land exactly on the chunk end");
    }

    #[test]
    fn multiple_row_groups_partition_the_rows() {
        let bytes = fixture("multi_rowgroup.parquet");
        let meta = read_footer(&bytes).unwrap();
        assert_eq!(meta.row_groups.len(), 8);
        let total: i64 = meta.row_groups.iter().map(|g| g.num_rows).sum();
        assert_eq!(total, meta.num_rows);
        assert_eq!(total, 1000);
    }

    #[test]
    fn every_fixture_footer_parses() {
        // Including the ones the reader will refuse to *decode*: refusing a
        // codec is a decision made from metadata that parsed correctly.
        for name in [
            "people.parquet", "orders.parquet", "uncompressed.parquet", "snappy.parquet",
            "plain.parquet", "dictionary_v1.parquet", "dictionary_v2.parquet", "delta.parquet",
            "byte_stream_split.parquet", "pages_v1.parquet", "pages_v2.parquet",
            "multi_rowgroup.parquet", "many_pages.parquet", "empty.parquet", "int96.parquet",
            "zstd.parquet", "nested.parquet",
        ] {
            let meta = read_footer(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert!(!meta.schema.is_empty(), "{name}");
        }
    }

    #[test]
    fn non_parquet_input_is_rejected_clearly() {
        assert!(read_footer(b"").is_err());
        assert!(read_footer(b"id,name\n1,ada\n").is_err());
        // Right magic at both ends, nonsense in between.
        let mut b = MAGIC.to_vec();
        b.extend([0u8; 32]);
        b.extend(9999u32.to_le_bytes());
        b.extend(MAGIC);
        let e = read_footer(&b).unwrap_err();
        assert!(e.headline().contains("footer claims"), "{}", e.headline());
    }

    // -- decoding ---------------------------------------------------------

    fn csv_fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .unwrap()
            .join("data")
            .join(name);
        std::fs::read(path).unwrap()
    }

    /// Render a table as strings so two loaders can be compared exactly.
    fn dump(t: &Table) -> Vec<String> {
        let mut out = vec![t
            .schema
            .fields
            .iter()
            .map(|f| format!("{}:{}{}", f.name, f.data_type, if f.nullable { "?" } else { "" }))
            .collect::<Vec<_>>()
            .join(",")];
        for rg in &t.row_groups {
            for row in 0..rg.num_rows {
                let cells: Vec<String> = rg
                    .all_columns().unwrap()
                    .iter()
                    .map(|c| {
                        if c.is_valid(row) {
                            format!("{}", c.value(row))
                        } else {
                            "NULL".into()
                        }
                    })
                    .collect();
                out.push(cells.join("|"));
            }
        }
        out
    }

    /// The central claim of this step: a Parquet table and a CSV table holding
    /// the same data are indistinguishable, down to types and nullability.
    #[test]
    fn parquet_and_csv_agree_value_for_value() {
        for name in ["people", "orders"] {
            let csv = crate::storage::read_csv(
                name,
                &csv_fixture(&format!("{name}.csv")),
                &crate::storage::CsvOptions::default(),
            )
            .unwrap();
            let parquet = read_parquet(name, fixture_arc(&format!("{name}.parquet"))).unwrap();

            assert_eq!(parquet.num_rows(), csv.num_rows(), "{name}: row count");
            let (a, b) = (dump(&csv), dump(&parquet));
            assert_eq!(a[0], b[0], "{name}: schema");
            for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                assert_eq!(x, y, "{name}: row {i}");
            }
            assert_eq!(a.len(), b.len(), "{name}: row count after dump");
        }
    }

    /// Every encoding, page version and codec must produce the same table.
    ///
    /// The files below hold identical data written a dozen different ways, so
    /// any disagreement is a decoder bug rather than a data difference -- which
    /// is exactly the property that makes this worth testing over a matrix.
    #[test]
    fn every_encoding_decodes_to_the_same_table() {
        let baseline = read_parquet("m", fixture_arc("plain.parquet")).unwrap();
        let want = dump(&baseline);
        assert_eq!(baseline.num_rows(), 1000);

        for name in [
            "uncompressed.parquet",
            "snappy.parquet",
            "dictionary_v1.parquet",
            "dictionary_v2.parquet",
            "delta.parquet",
            "byte_stream_split.parquet",
            "pages_v1.parquet",
            "pages_v2.parquet",
            "many_pages.parquet",
            "multi_rowgroup.parquet",
        ] {
            let t = read_parquet("m", fixture_arc(name))
                .unwrap_or_else(|e| panic!("{name}: {}", e.headline()));
            let got = dump(&t);
            assert_eq!(got.len(), want.len(), "{name}: row count");
            for (i, (x, y)) in want.iter().zip(&got).enumerate() {
                assert_eq!(x, y, "{name}: row {i}");
            }
        }
    }

    #[test]
    fn nulls_survive_every_encoding() {
        // `maybe` is null every seventh row and `maybe_text` every fifth, so
        // the definition levels are non-trivial on every page.
        for name in ["plain.parquet", "dictionary_v2.parquet", "delta.parquet", "pages_v2.parquet"] {
            let t = read_parquet("m", fixture_arc(name)).unwrap();
            let rg = &t.row_groups[0];
            let maybe = &rg.column(6).unwrap();
            let text = &rg.column(7).unwrap();
            assert_eq!(maybe.null_count(), 1000 / 7 + 1, "{name}: maybe");
            assert_eq!(text.null_count(), 1000 / 5, "{name}: maybe_text");
            for row in 0..1000 {
                assert_eq!(maybe.is_valid(row), row % 7 != 3, "{name}: maybe row {row}");
                assert_eq!(text.is_valid(row), row % 5 != 2, "{name}: text row {row}");
            }
        }
    }

    #[test]
    fn an_empty_file_loads_as_an_empty_table() {
        let t = read_parquet("m", fixture_arc("empty.parquet")).unwrap();
        assert_eq!(t.num_rows(), 0);
        assert_eq!(t.schema.len(), 10);
    }

    #[test]
    fn deprecated_int96_timestamps_are_read() {
        let t = read_parquet("t", fixture_arc("int96.parquet")).unwrap();
        let micros = read_parquet("t", fixture_arc("plain.parquet")).unwrap();
        // The same instants, written the modern way in one file and the
        // 1970-relative-Julian-day way in the other.
        let a = &t.row_groups[0].column(0).unwrap();
        let b = &micros.row_groups[0].column(9).unwrap();
        assert_eq!(t.schema.field(0).data_type, crate::types::DataType::Timestamp);
        for row in 0..1000 {
            assert_eq!(a.value(row), b.value(row), "row {row}");
        }
    }

    /// Refused at load, from the footer -- not on the first query that happens
    /// to touch the column.
    #[test]
    fn unsupported_input_is_refused_by_name() {
        let e = read_parquet("z", fixture_arc("zstd.parquet")).unwrap_err();
        assert!(e.headline().contains("ZSTD"), "{}", e.headline());

        let e = read_parquet("n", fixture_arc("nested.parquet")).unwrap_err();
        assert!(
            e.headline().contains("nested") || e.headline().contains("repeated"),
            "{}",
            e.headline()
        );
    }

    #[test]
    fn a_truncated_file_is_named_as_such() {
        let bytes = fixture("people.parquet");
        let e = read_footer(&bytes[..bytes.len() - 1]).unwrap_err();
        assert!(e.headline().contains("truncated"), "{}", e.headline());
    }
}
