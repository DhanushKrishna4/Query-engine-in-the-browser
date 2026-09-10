//! Row groups and their metadata.
//!
//! A table is stored as a sequence of row groups of ~64K rows. Each row group
//! carries per-column min/max/null-count/distinct-estimate. That metadata *is*
//! the zone map -- the cheapest index there is, because building it costs one
//! pass over data you were already writing, and it lets a scan skip an entire
//! row group without reading a byte of its values.
//!
//! The statistics are computed and stored now; the scan-side pruning that
//! consumes them arrives with the optimizer work.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::error::Result;

use crate::storage::bitmap::Bitmap;
use crate::storage::bloom::BloomFilter;
use crate::storage::column::Column;
use crate::types::ScalarValue;

/// Rows per row group. 64K is the usual analytical default: big enough that
/// per-group metadata is a rounding error, small enough that zone-map pruning
/// still has useful resolution.
pub const ROW_GROUP_SIZE: usize = 65_536;

/// Above this many distinct values we stop counting exactly and report
/// `None`. HyperLogLog replaces this with a bounded-memory estimate for every
/// cardinality once the statistics work lands.
const DISTINCT_EXACT_LIMIT: usize = 8192;

/// Build a bloom filter for a column whose distinct count exceeds this share of
/// its rows.
///
/// The filter earns its ~1.2 bytes per row only when most values are absent
/// from most row groups, which is what high cardinality means here. A column
/// with twelve distinct values appears in every row group, so its filter would
/// answer "maybe" every time while still costing memory -- and a zone map or a
/// dictionary serves that column better anyway.
const BLOOM_CARDINALITY_RATIO: f64 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    /// Smallest non-NULL value, or `None` when there is no bound to state.
    ///
    /// Two different situations produce `None`, and code that prunes on these
    /// must not confuse them: a column where every value is NULL has no
    /// bounds and matches no comparison, while a Parquet writer that omitted
    /// statistics leaves bounds simply *unknown* and rules nothing out.
    /// `null_count` against the group's row count is what tells them apart.
    pub min: Option<ScalarValue>,
    /// Largest non-NULL value; see `min` for what `None` means.
    pub max: Option<ScalarValue>,
    /// How many values are NULL, or `None` when the source did not say.
    ///
    /// Same trap as `min`: a Parquet footer may carry bounds and no null
    /// count, or neither. Reading "not recorded" as zero turns `IS NULL` into
    /// a predicate that prunes away the very rows it is looking for.
    pub null_count: Option<usize>,
    /// Exact when the column has few distinct values, `None` when it has too
    /// many to track cheaply.
    pub distinct_count_estimate: Option<usize>,
}

impl ColumnStats {
    pub fn compute(col: &Column) -> ColumnStats {
        let (min, max) = col.min_max();
        ColumnStats {
            min,
            max,
            null_count: Some(col.null_count()),
            distinct_count_estimate: distinct_estimate(col),
        }
    }
}

fn distinct_estimate(col: &Column) -> Option<usize> {
    let mut seen: HashSet<u64> = HashSet::new();
    for i in 0..col.len() {
        let Some(hash) = col.hash_at(i) else { continue };
        seen.insert(hash);
        if seen.len() > DISTINCT_EXACT_LIMIT {
            return None;
        }
    }
    Some(seen.len())
}

/// Somewhere a column's bytes can be decoded from, on demand.
///
/// A trait rather than a Parquet type so that `storage` does not depend on any
/// particular file format, and so that a source which fetches over the network
/// -- the browser's eventual range requests -- slots in without touching a
/// scan.
pub trait ChunkSource: std::fmt::Debug + Send + Sync {
    /// Decode one column of this row group.
    fn decode(&self, column: usize) -> Result<Column>;
    /// Compressed bytes this row group occupies in the file.
    fn byte_size(&self) -> usize;

    /// Whether a decoded column should be kept in memory afterwards.
    ///
    /// True for Parquet, where reproducing it means decompressing a page
    /// again. False for an in-memory encoding, where caching would defeat the
    /// point: the column would then cost its encoded *and* its decoded size,
    /// which is more than never encoding it at all. Unpacking bits per query
    /// is cheap; storing both forms is not.
    fn cache_decoded(&self) -> bool {
        true
    }

    /// Whether decoding is expensive enough to be worth avoiding.
    ///
    /// A Parquet chunk is: it means decompressing a page. An in-memory encoding
    /// is not -- unpacking bits is cheap, and treating it as deferred would
    /// make the statistics pass sample one row group instead of reading the
    /// table it already has.
    fn is_deferred(&self) -> bool {
        true
    }

    /// The encoding's name and its compression ratio, for the inspector.
    fn describe(&self, _column: usize) -> Option<(&'static str, f64)> {
        None
    }

    /// The encoded form of a column, for a predicate that can be answered
    /// without decoding it.
    ///
    /// On the trait rather than reached by downcasting: `dyn ChunkSource` would
    /// have to require `Any` for that, and a source that has no encoded form to
    /// offer can simply say so.
    fn encoded(
        &self,
        _column: usize,
    ) -> Option<(&crate::storage::encoding::Encoded, Option<&Bitmap>)> {
        None
    }
}

/// Columns held in one of the `storage::encoding` forms.
///
/// The encoded bytes stay resident and the decoded column is cached beside
/// them on first use, so a column a query reads costs both. That is the right
/// trade only because projection pushdown means a query reads the columns it
/// named and no others -- and because a predicate on an encoded column is
/// answered without decoding at all.
#[derive(Debug)]
pub struct EncodedChunks {
    columns: Vec<Option<EncodedColumn>>,
}

#[derive(Debug)]
struct EncodedColumn {
    encoded: crate::storage::encoding::Encoded,
    validity: Option<Bitmap>,
    /// What the plain column occupied, so the ratio can be reported.
    plain_bytes: usize,
}

impl ChunkSource for EncodedChunks {
    fn decode(&self, column: usize) -> Result<Column> {
        let c = self.columns[column].as_ref().ok_or_else(|| {
            crate::error::Diagnostic::exec(format!("column {column} is not encoded"))
        })?;
        Ok(crate::storage::encoding::decode(&c.encoded, c.validity.clone()))
    }

    fn byte_size(&self) -> usize {
        self.columns
            .iter()
            .flatten()
            .map(|c| c.encoded.byte_size())
            .sum()
    }

    fn is_deferred(&self) -> bool {
        false
    }

    fn cache_decoded(&self) -> bool {
        false
    }

    fn describe(&self, column: usize) -> Option<(&'static str, f64)> {
        let c = self.columns.get(column)?.as_ref()?;
        Some((
            c.encoded.name(),
            c.plain_bytes as f64 / c.encoded.byte_size().max(1) as f64,
        ))
    }

    fn encoded(
        &self,
        column: usize,
    ) -> Option<(&crate::storage::encoding::Encoded, Option<&Bitmap>)> {
        let c = self.columns.get(column)?.as_ref()?;
        Some((&c.encoded, c.validity.as_ref()))
    }
}

#[derive(Debug)]
pub struct RowGroup {
    /// One slot per column. A resident column is `Some` from the start; a
    /// pending one fills in the first time it is asked for.
    ///
    /// Behind a `Mutex` because a table is shared through an `Arc` and decoding
    /// mutates this cache. A lock per column per row group is nothing next to
    /// decoding a chunk, and it keeps the door open for the parallel scans the
    /// operators are already shaped for -- a `RefCell` would have closed it.
    columns: Mutex<Vec<Option<Arc<Column>>>>,
    /// `None` when every column is already resident.
    source: Option<Arc<dyn ChunkSource>>,
    pub num_rows: usize,
    /// One entry per column, in schema order.
    pub stats: Vec<ColumnStats>,
    /// One entry per column, in schema order; `None` for a column whose
    /// cardinality is too low for a filter to pay for itself.
    ///
    /// Always `None` for a pending column: a filter cannot be built from data
    /// that was deliberately not read. Parquet has its own bloom filters in the
    /// file, which is the right way to get them back.
    pub blooms: Vec<Option<BloomFilter>>,
}

impl RowGroup {
    pub fn new(columns: Vec<Column>) -> RowGroup {
        let num_rows = columns.first().map_or(0, |c| c.len());
        debug_assert!(columns.iter().all(|c| c.len() == num_rows));
        let stats: Vec<ColumnStats> = columns.iter().map(ColumnStats::compute).collect();
        let blooms = columns
            .iter()
            .zip(&stats)
            .map(|(col, st)| build_bloom(col, st, num_rows))
            .collect();
        // Encode what pays. An encoded column is left out of the cache so the
        // lazy path decodes it on first use, exactly as a Parquet chunk is.
        let mut resident: Vec<Option<Arc<Column>>> = Vec::with_capacity(columns.len());
        let mut encoded: Vec<Option<EncodedColumn>> = Vec::with_capacity(columns.len());
        for column in columns {
            match crate::storage::encoding::encode(&column) {
                Some(e) => {
                    encoded.push(Some(EncodedColumn {
                        encoded: e,
                        validity: column.validity.clone(),
                        plain_bytes: column.byte_size(),
                    }));
                    resident.push(None);
                }
                None => {
                    encoded.push(None);
                    resident.push(Some(Arc::new(column)));
                }
            }
        }
        let any = encoded.iter().any(|e| e.is_some());

        RowGroup {
            columns: Mutex::new(resident),
            source: any.then(|| Arc::new(EncodedChunks { columns: encoded }) as Arc<dyn ChunkSource>),
            num_rows,
            stats,
            blooms,
        }
    }

    /// Build from columns that are already shared, for a loader that produced
    /// them itself. Statistics and filters are computed the same way.
    pub fn from_columns(columns: Vec<Arc<Column>>, num_rows: usize) -> RowGroup {
        let stats: Vec<ColumnStats> = columns.iter().map(|c| ColumnStats::compute(c)).collect();
        let blooms = columns
            .iter()
            .zip(&stats)
            .map(|(col, st)| build_bloom(col, st, num_rows))
            .collect();
        RowGroup {
            columns: Mutex::new(columns.into_iter().map(Some).collect()),
            source: None,
            num_rows,
            stats,
            blooms,
        }
    }

    /// A row group whose columns are described but not yet decoded.
    ///
    /// The statistics come from the file's own metadata, so pruning works
    /// before anything is read. Bloom filters do not, for the reason given on
    /// the field.
    pub fn pending(
        source: Arc<dyn ChunkSource>,
        stats: Vec<ColumnStats>,
        num_rows: usize,
    ) -> RowGroup {
        let width = stats.len();
        RowGroup {
            columns: Mutex::new(vec![None; width]),
            source: Some(source),
            num_rows,
            stats,
            blooms: vec![None; width],
        }
    }

    /// One column, decoding it first if it has not been read yet.
    pub fn column(&self, i: usize) -> Result<Arc<Column>> {
        {
            let cache = self.columns.lock().expect("column cache lock");
            if let Some(Some(c)) = cache.get(i) {
                return Ok(Arc::clone(c));
            }
        }
        let source = self.source.as_ref().ok_or_else(|| {
            crate::error::Diagnostic::exec(format!(
                "column {i} is out of range for a row group of {} columns",
                self.stats.len()
            ))
        })?;
        // Decoded outside the lock: a chunk can take milliseconds, and holding
        // the cache while it does would serialize every other column.
        let decoded = Arc::new(source.decode(i)?);
        if !source.cache_decoded() {
            // An in-memory encoding is cheap to unpack and expensive to keep,
            // so the caller gets the column and the row group keeps only the
            // encoded bytes.
            return Ok(decoded);
        }
        let mut cache = self.columns.lock().expect("column cache lock");
        // Another caller may have decoded it meanwhile; either copy is correct,
        // so keep whichever landed first and let this one drop.
        match &cache[i] {
            Some(existing) => Ok(Arc::clone(existing)),
            None => {
                cache[i] = Some(Arc::clone(&decoded));
                Ok(decoded)
            }
        }
    }

    /// Every column, decoding whatever is still pending.
    pub fn all_columns(&self) -> Result<Vec<Arc<Column>>> {
        (0..self.stats.len()).map(|i| self.column(i)).collect()
    }

    /// Whether reading this row group means work worth avoiding.
    ///
    /// An encoded in-memory row group has a source but is not *pending* in this
    /// sense: unpacking it is cheap, and callers that use this to decide
    /// whether to touch the data at all should not be put off by it.
    pub fn is_pending(&self) -> bool {
        self.source.as_ref().is_some_and(|s| s.is_deferred())
    }

    /// Whether any column is held in an encoded form.
    pub fn is_encoded(&self) -> bool {
        self.source.is_some()
    }

    /// The encoding of one column and how much it saved, if it has one.
    pub fn encoding_of(&self, column: usize) -> Option<(&'static str, f64)> {
        self.source.as_ref()?.describe(column)
    }

    /// The encoded form of a column, for a predicate that can be answered
    /// without decoding it.
    pub fn encoded(
        &self,
        column: usize,
    ) -> Option<(&crate::storage::encoding::Encoded, Option<&Bitmap>)> {
        self.source.as_ref()?.encoded(column)
    }

    /// How many columns are decoded and in memory right now. The storage
    /// inspector shows this; it is the difference laziness makes, made visible.
    pub fn resident_columns(&self) -> usize {
        self.columns
            .lock()
            .expect("column cache lock")
            .iter()
            .filter(|c| c.is_some())
            .count()
    }

    /// Memory this row group currently occupies.
    ///
    /// For a pending row group that is the compressed size on the source plus
    /// whatever has been decoded so far -- not what it *would* cost fully
    /// decoded, which is the number a CSV table reports.
    pub fn byte_size(&self) -> usize {
        let resident: usize = self
            .columns
            .lock()
            .expect("column cache lock")
            .iter()
            .flatten()
            .map(|c| c.byte_size())
            .sum();
        resident + self.source.as_ref().map_or(0, |s| s.byte_size())
    }

    pub fn bloom_bytes(&self) -> usize {
        self.blooms
            .iter()
            .filter_map(|b| b.as_ref())
            .map(|b| b.byte_size())
            .sum()
    }
}

/// A filter for a high-cardinality column, or `None`.
///
/// `distinct_count_estimate` is `None` precisely when the column has more
/// distinct values than we bothered to count, which is the strongest possible
/// signal that a filter will pay off.
fn build_bloom(col: &Column, stats: &ColumnStats, num_rows: usize) -> Option<BloomFilter> {
    if num_rows == 0 {
        return None;
    }
    let distinct = match stats.distinct_count_estimate {
        // Too many distinct values to have counted. Size for the upper bound --
        // one distinct value per row -- rather than for the counting limit.
        // An over-sized filter merely costs memory; an under-sized one is
        // saturated, answers "maybe" to every probe, and prunes nothing at all
        // while still costing the memory.
        None => num_rows,
        Some(d) if (d as f64) >= BLOOM_CARDINALITY_RATIO * num_rows as f64 => d,
        Some(_) => return None,
    };
    let mut filter = BloomFilter::with_capacity(distinct);
    for i in 0..col.len() {
        if let Some(hash) = col.hash_at(i) {
            filter.insert_hash(hash);
        }
    }
    Some(filter)
}
