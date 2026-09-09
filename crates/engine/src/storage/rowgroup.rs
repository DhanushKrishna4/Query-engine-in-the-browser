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
    /// Smallest non-NULL value; `None` if every value is NULL.
    pub min: Option<ScalarValue>,
    /// Largest non-NULL value; `None` if every value is NULL.
    pub max: Option<ScalarValue>,
    pub null_count: usize,
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
            null_count: col.null_count(),
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
        RowGroup {
            columns: Mutex::new(columns.into_iter().map(|c| Some(Arc::new(c))).collect()),
            source: None,
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

    pub fn is_pending(&self) -> bool {
        self.source.is_some()
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
