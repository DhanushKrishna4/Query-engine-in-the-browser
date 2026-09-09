//! The scalar and vectorized evaluators must produce identical results.
//!
//! Every query in the sqllogictest corpus -- including the several hundred
//! randomly generated ones -- is run twice, once through each evaluator, and
//! the results compared value for value. A vectorized kernel is only allowed to
//! be clever if it agrees with the obvious implementation on every input, and
//! this is what keeps that true as kernels are added.
//!
//! Errors count too: if one evaluator raises, the other must raise the same
//! way. That is how the short-circuit behaviour of `AND` stays consistent
//! between a per-row and a per-batch implementation.
//!
//! The same test runs every query several more ways, each pairing a fast path
//! with the obvious implementation it has to agree with:
//!
//!   * joins forced onto the nested loop, which compares every pair against the
//!     whole condition rather than hashing an equality out of it;
//!   * joins forced onto a sort-merge, which walks both inputs once in key
//!     order and never looks back -- a completely different way of finding the
//!     same pairs, and the one that loses rows silently if it is wrong;
//!   * aggregates forced onto the streaming implementation, which holds one
//!     accumulator set instead of a hash table and depends entirely on its
//!     input being sorted;
//!   * the optimizer disabled, which is the definition of a rewrite being
//!     semantics-preserving -- if any rule changes an answer, this fails;
//!   * zone-map pruning disabled, since a scan that skips a row group must skip
//!     one that truly had nothing in it;
//!   * bloom filters disabled, for the same reason and on a different structure;
//!   * index scans disabled, which is the sharpest of the lot -- an index scan
//!     reads a completely different set of rows in a completely different way,
//!     and must still produce the same answer.
//!
//! Unlike the corpus comparison against SQLite, these also cover the queries
//! where both engines happen to return nothing.

use std::path::{Path, PathBuf};

use engine::exec::ExecOptions;
use engine::sqllogictest::{parse, RecordKind};
use engine::storage::CsvOptions;
use engine::Engine;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/engine has two ancestors")
        .to_path_buf()
}

/// Both evaluators' answers, rendered so they can be compared exactly.
fn outcome(engine: &Engine, sql: &str, options: &ExecOptions) -> Result<Vec<String>, String> {
    match engine.execute_with(sql, options) {
        Ok(r) => Ok(r
            .rows()
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(|v| format!("{}|{:?}", v, v.data_type()))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect()),
        Err(d) => Err(d.headline()),
    }
}

#[test]
fn execution_strategies_agree() {
    let root = repo_root();
    let dir = root.join("tests/sqllogictest");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "slt"))
        .collect();
    files.sort();
    assert!(!files.is_empty());

    // The flag says whether a configuration may legally emit rows in a
    // different order. Results are compared as multisets throughout, so order
    // alone never matters -- but `LIMIT` without a total `ORDER BY` turns a
    // reordering into a different *set* of rows, and SQL says both are right.
    // A streaming aggregate emits groups in key order where a hash aggregate
    // emits them in insertion order; `GROUP BY city LIMIT 3` over seven cities
    // then returns a different three, and neither is wrong.
    let configs: [(&str, ExecOptions, bool); 8] = [
        ("scalar evaluator", ExecOptions::scalar(), false),
        ("nested loop joins", ExecOptions::nested_loop_joins(), false),
        ("sort-merge joins", ExecOptions::merge_joins(), true),
        ("streaming aggregates", ExecOptions::sorted_aggregates(), true),
        ("optimizer disabled", ExecOptions::unoptimized(), false),
        ("zone maps disabled", ExecOptions::without_pruning(), false),
        ("bloom filters disabled", ExecOptions::without_bloom_filters(), false),
        ("index scans disabled", ExecOptions::without_index_scans(), false),
    ];
    let vectorized = ExecOptions::default();
    let mut compared = 0usize;
    let mut skipped = 0usize;
    let mut mismatches = Vec::new();

    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let records = parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

        let mut engine = Engine::new();
        for record in &records {
            if matches!(record.kind, RecordKind::Halt) {
                break;
            }
            if !record.applies_to(engine::sqllogictest::LABEL) {
                continue;
            }
            match &record.kind {
                RecordKind::Load { table, path: csv } => {
                    let bytes = std::fs::read(root.join(csv)).unwrap();
                    engine
                        .load_csv(table, &bytes, &CsvOptions::default())
                        .unwrap();
                }
                RecordKind::Index { table, column } => {
                    engine.create_index(table, column).unwrap();
                }
                RecordKind::Statement { sql, .. } | RecordKind::Query { sql, .. } => {
                    let mut baseline = outcome(&engine, sql, &vectorized);
                    // Row order is not defined across join algorithms, so
                    // compare as multisets.
                    if let Ok(rows) = &mut baseline {
                        rows.sort();
                    }
                    compared += 1;

                    for (label, options, reorders) in &configs {
                        // Conservative: any LIMIT at all, not only one without
                        // an ORDER BY, because `ORDER BY x LIMIT 3` with ties
                        // in `x` is just as under-determined.
                        if *reorders && sql.to_ascii_uppercase().contains("LIMIT") {
                            skipped += 1;
                            continue;
                        }
                        let mut other = outcome(&engine, sql, options);
                        if let Ok(rows) = &mut other {
                            rows.sort();
                        }
                        if baseline == other {
                            continue;
                        }
                        mismatches.push(format!(
                            "{}:{} ({label})\n    {sql}\n      default: {:?}\n      other:   {:?}",
                            path.file_name().unwrap().to_string_lossy(),
                            record.line,
                            baseline.as_ref().map(|v| v.len()).map_err(|e| e.as_str()),
                            other.as_ref().map(|v| v.len()).map_err(|e| e.as_str()),
                        ));
                        if let (Ok(x), Ok(y)) = (&baseline, &other) {
                            if let Some(i) = x.iter().zip(y).position(|(p, q)| p != q) {
                                mismatches.push(format!(
                                    "      first differing row {i}: `{}` vs `{}`",
                                    x[i], y[i]
                                ));
                            }
                        }
                    }
                }
                RecordKind::Halt => unreachable!(),
            }
        }
    }

    println!(
        "compared {compared} queries across {} configurations \
         ({skipped} comparisons skipped: LIMIT under a row-reordering strategy)",
        configs.len() + 1
    );
    assert!(compared > 400, "corpus too small to be meaningful");
    assert!(
        mismatches.is_empty(),
        "\n{}\n{} evaluator disagreement(s)",
        mismatches.join("\n"),
        mismatches.len()
    );
}
