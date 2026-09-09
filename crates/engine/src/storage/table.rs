//! A materialized table: a schema plus the row groups holding its data.
//!
//! Tables are held behind an `Arc` by the catalog and cloned cheaply into every
//! scan. Nothing here owns a file handle or a path -- data always arrives as an
//! in-memory buffer, because in the browser there is no filesystem to read from.

use std::sync::Arc;

use crate::storage::rowgroup::{RowGroup, ROW_GROUP_SIZE};
use crate::storage::schema::Schema;

#[derive(Debug)]
pub struct Table {
    pub name: String,
    pub schema: Arc<Schema>,
    pub row_groups: Vec<RowGroup>,
}

impl Table {
    pub fn new(name: impl Into<String>, schema: Arc<Schema>, row_groups: Vec<RowGroup>) -> Table {
        Table {
            name: name.into(),
            schema,
            row_groups,
        }
    }

    /// Whether any row group still has columns to decode.
    pub fn is_pending(&self) -> bool {
        self.row_groups.iter().any(|rg| rg.is_pending())
    }

    pub fn num_rows(&self) -> usize {
        self.row_groups.iter().map(|rg| rg.num_rows).sum()
    }

    pub fn num_row_groups(&self) -> usize {
        self.row_groups.len()
    }

    pub fn byte_size(&self) -> usize {
        self.row_groups.iter().map(|rg| rg.byte_size()).sum()
    }

    pub fn row_group_capacity(&self) -> usize {
        ROW_GROUP_SIZE
    }

    /// The first global row id of each row group, plus a trailing total.
    ///
    /// An index names rows by their position in the whole table; a gather has
    /// to turn that back into (row group, offset). One ascending vector and a
    /// binary search does it.
    pub fn row_group_starts(&self) -> Vec<u32> {
        let mut starts = Vec::with_capacity(self.row_groups.len() + 1);
        let mut total = 0u32;
        for rg in &self.row_groups {
            starts.push(total);
            total += rg.num_rows as u32;
        }
        starts.push(total);
        starts
    }

    pub fn bloom_bytes(&self) -> usize {
        self.row_groups.iter().map(|rg| rg.bloom_bytes()).sum()
    }
}
