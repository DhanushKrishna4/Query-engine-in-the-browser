//! Cardinality estimation accuracy, measured across the whole corpus.
//!
//! Every query in `tests/sqllogictest/` is executed and every operator's
//! predicted row count compared against what it actually produced, scored by
//! q-error: `max(estimated/actual, actual/estimated)`, so being 10x over counts
//! the same as being 10x under.
//!
//! The assertions here are deliberately loose. The point is not to pass -- an
//! estimator can always be made to pass by asserting nothing -- it is to print
//! the distribution so that a change which quietly makes estimates worse shows
//! up in the numbers. Being honest about where estimates are bad is worth more
//! than a threshold tuned until it goes green.

use std::path::{Path, PathBuf};

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

#[derive(Default)]
struct Sample {
    operator: String,
    q: f64,
    estimated: f64,
    actual: u64,
    /// Scans only: whether zone maps skipped any row groups.
    pruned: bool,
    sql: String,
}

fn collect(node: &engine::exec::StatsNode, sql: &str, out: &mut Vec<Sample>) {
    if let (Some(estimated), Some(q)) = (node.stats.estimated_rows, node.stats.q_error()) {
        out.push(Sample {
            operator: node.stats.name.clone(),
            q,
            estimated,
            actual: node.stats.rows_out,
            pruned: node.stats.row_groups_pruned > 0,
            sql: sql.to_string(),
        });
    }
    for c in &node.children {
        collect(c, sql, out);
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

#[test]
fn cardinality_estimates_are_scored_and_reported() {
    let root = repo_root();
    let dir = root.join("tests/sqllogictest");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "slt"))
        .collect();
    files.sort();

    let mut samples: Vec<Sample> = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let records = parse(&text).unwrap();
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
                RecordKind::Query { sql, .. } => {
                    if let Ok(result) = engine.execute(sql) {
                        collect(&result.stats, sql, &mut samples);
                    }
                }
                _ => {}
            }
        }
    }

    assert!(samples.len() > 500, "only {} samples", samples.len());

    let mut all: Vec<f64> = samples.iter().map(|s| s.q).collect();
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());

    println!("\ncardinality estimation, {} operators over the corpus", all.len());
    println!(
        "  median {:.2}x   p90 {:.2}x   p99 {:.2}x   max {:.2}x",
        percentile(&all, 0.5),
        percentile(&all, 0.9),
        percentile(&all, 0.99),
        all.last().copied().unwrap_or(0.0)
    );

    // Per operator, because the estimator is much better at some than others.
    let mut kinds: Vec<&str> = samples.iter().map(|s| s.operator.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    println!("  {:<16} {:>7} {:>9} {:>9}", "operator", "n", "median", "p90");
    for kind in kinds {
        let mut qs: Vec<f64> = samples
            .iter()
            .filter(|s| s.operator == kind)
            .map(|s| s.q)
            .collect();
        qs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "  {:<16} {:>7} {:>8.2}x {:>8.2}x",
            kind,
            qs.len(),
            percentile(&qs, 0.5),
            percentile(&qs, 0.9)
        );
    }

    // The worst offenders, named, because that is where the next improvement is.
    let mut worst: Vec<&Sample> = samples.iter().collect();
    worst.sort_by(|a, b| b.q.partial_cmp(&a.q).unwrap());
    println!("  worst estimates:");
    for s in worst.iter().take(5) {
        let sql: String = s.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "    {:>8.1}x  {:<14} est {:>8.0}  actual {:>8}  {}",
            s.q,
            s.operator,
            s.estimated,
            s.actual,
            &sql[..sql.len().min(76)]
        );
    }

    // Loose guard rails.
    //
    // A scan's estimate is the table's row count, straight from the catalog, so
    // it is an *upper bound* rather than a prediction: zone maps can skip row
    // groups and a LIMIT above can stop the scan early, both of which make it
    // emit fewer rows. Neither is an estimation error -- counting them as one
    // would reward turning pruning off -- but a scan producing *more* rows than
    // the catalog says the table holds would be a real bug.
    let scans: Vec<&Sample> = samples.iter().filter(|s| s.operator == "Scan").collect();
    assert!(!scans.is_empty());
    for s in &scans {
        assert!(
            s.actual as f64 <= s.estimated + 1e-9,
            "a scan produced {} rows from a table of {}",
            s.actual,
            s.estimated
        );
    }
    let exact = scans
        .iter()
        .filter(|s| !s.pruned && (s.q - 1.0).abs() < 1e-9)
        .count();
    println!(
        "  {exact} of {} scans read the whole table and were estimated exactly",
        scans.len()
    );
    assert!(
        percentile(&all, 0.5) < 5.0,
        "median q-error {:.2}x is worse than the estimator should manage",
        percentile(&all, 0.5)
    );
}
