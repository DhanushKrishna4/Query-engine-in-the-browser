//! The catalog: named relations and (later) their statistics.
//!
//! Lookup follows the same folding rule as column resolution -- an unquoted
//! table name matches case-insensitively, a `"quoted"` one must match exactly.
//!
//! The catalog also holds indexes. They live here rather than inside `Table`
//! because a table is shared behind an `Arc` the moment it is registered, and
//! building an index later would mean either mutating through that `Arc` or
//! rebuilding the table. Keeping them beside the tables costs one extra lookup
//! at plan time and leaves loaded data immutable.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Diagnostic, Result};
use crate::optimizer::stats::TableStatistics;
use crate::storage::schema::Resolution;
use crate::storage::{BPlusTree, Table};

#[derive(Debug, Default)]
pub struct Catalog {
    /// Keyed by lowercased name so that unquoted lookups are a single probe.
    tables: HashMap<String, Arc<Table>>,
    /// Statistics, computed once when a table is registered. Every plan is
    /// costed against these, so they are part of the catalog rather than
    /// something the optimizer recomputes per query.
    statistics: HashMap<String, Arc<TableStatistics>>,
    /// Indexes, keyed by the same folded table name. Explicit: nothing is
    /// indexed until asked for, because building one costs a pass over the
    /// column and most analytical queries never want it.
    indexes: HashMap<String, Vec<Arc<TableIndex>>>,
}

/// A B+ tree over one column of one table.
#[derive(Debug)]
pub struct TableIndex {
    pub table: String,
    /// Position in the *table's* schema, not in any projection. Column
    /// references in a plan stay table-absolute all the way down to the scan,
    /// which is what lets this be compared against them directly.
    pub column: usize,
    pub column_name: String,
    pub tree: BPlusTree,
    /// Nanoseconds spent building it, reported by `.index`.
    pub build_nanos: u64,
}

impl Catalog {
    pub fn new() -> Catalog {
        Catalog::default()
    }

    /// Register a table, replacing any table with the same (case-folded) name.
    ///
    /// Statistics are collected here, in one pass, so that a table is never in
    /// the catalog without them -- an optimizer that has to ask "do I have
    /// statistics?" ends up with two code paths and only one of them tested.
    pub fn register(&mut self, table: Table) -> Arc<Table> {
        let key = table.name.to_ascii_lowercase();
        let stats = Arc::new(TableStatistics::analyze(&table));
        let table = Arc::new(table);
        self.tables.insert(key.clone(), Arc::clone(&table));
        self.statistics.insert(key, stats);
        table
    }

    pub fn statistics(&self, name: &str) -> Option<Arc<TableStatistics>> {
        self.statistics
            .get(&name.to_ascii_lowercase())
            .map(Arc::clone)
    }

    pub fn get(&self, name: &str, quoted: bool) -> Option<Arc<Table>> {
        let t = self.tables.get(&name.to_ascii_lowercase())?;
        if quoted && t.name != name {
            return None;
        }
        Some(Arc::clone(t))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tables.contains_key(&name.to_ascii_lowercase())
    }

    pub fn remove(&mut self, name: &str) -> Option<Arc<Table>> {
        let key = name.to_ascii_lowercase();
        self.statistics.remove(&key);
        self.indexes.remove(&key);
        self.tables.remove(&key)
    }

    /// Build a B+ tree over one column, replacing any existing index on it.
    pub fn create_index(&mut self, table_name: &str, column_name: &str) -> Result<Arc<TableIndex>> {
        let table = self.get(table_name, false).ok_or_else(|| {
            Diagnostic::exec(format!("unknown table `{table_name}`"))
        })?;
        let column = match table
            .schema
            .resolve(&column_name.to_ascii_lowercase(), false)
        {
            Resolution::Found(i) => i,
            Resolution::NotFound => {
                return Err(Diagnostic::exec(format!(
                    "table `{}` has no column `{column_name}`",
                    table.name
                )))
            }
            Resolution::Ambiguous(_) => {
                return Err(Diagnostic::exec(format!(
                    "`{column_name}` is ambiguous in table `{}`",
                    table.name
                )))
            }
        };
        if table.num_rows() > u32::MAX as usize {
            return Err(Diagnostic::exec(
                "cannot index a table with more than 2^32 rows: row ids are 32-bit".to_string(),
            ));
        }

        let timer = crate::exec::Timer::start();
        let mut tree = BPlusTree::new();
        let mut row = 0u32;
        for rg in &table.row_groups {
            // Decodes the chunk if the table is lazily loaded -- building an
            // index is exactly the kind of explicit, one-off pass that is
            // allowed to pay for it.
            let col = rg.column(column)?;
            for i in 0..rg.num_rows {
                // NULLs are dropped by `insert`; the row id still advances, so
                // positions stay aligned with the table.
                tree.insert(col.value(i), row);
                row += 1;
            }
        }

        let index = Arc::new(TableIndex {
            table: table.name.clone(),
            column,
            column_name: table.schema.field(column).name.clone(),
            tree,
            build_nanos: timer.elapsed_nanos(),
        });
        let key = table.name.to_ascii_lowercase();
        let entry = self.indexes.entry(key).or_default();
        entry.retain(|i| i.column != column);
        entry.push(Arc::clone(&index));
        Ok(index)
    }

    /// An index on a specific column of a table, if one was built.
    pub fn index_on(&self, table_name: &str, column: usize) -> Option<&Arc<TableIndex>> {
        self.indexes
            .get(&table_name.to_ascii_lowercase())?
            .iter()
            .find(|i| i.column == column)
    }

    /// Every index on a table, in the order they were built.
    pub fn indexes_on(&self, table_name: &str) -> &[Arc<TableIndex>] {
        self.indexes
            .get(&table_name.to_ascii_lowercase())
            .map_or(&[], |v| v.as_slice())
    }

    pub fn drop_index(&mut self, table_name: &str, column: usize) -> bool {
        let Some(v) = self.indexes.get_mut(&table_name.to_ascii_lowercase()) else {
            return false;
        };
        let before = v.len();
        v.retain(|i| i.column != column);
        v.len() != before
    }

    /// Table names in stable (sorted) order, for `.tables` and for the
    /// "did you mean" hint on an unknown relation.
    pub fn table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.values().map(|t| t.name.clone()).collect();
        names.sort();
        names
    }

    pub fn tables(&self) -> Vec<Arc<Table>> {
        let mut out: Vec<Arc<Table>> = self.tables.values().map(Arc::clone).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}
