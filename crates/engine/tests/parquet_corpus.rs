//! The whole sqllogictest corpus, run against Parquet instead of CSV.
//!
//! `tests/parquet/people.parquet` and `orders.parquet` hold exactly the data of
//! the CSV fixtures, written by pyarrow. Pointing the corpus at them means
//! nine hundred queries -- every join, aggregate, window, subquery and NULL
//! case the engine has -- are answered from Parquet and compared against the
//! expectations SQLite produced from the CSV.
//!
//! That is a stronger claim than "the reader decodes the fixtures correctly".
//! It says the two loaders are interchangeable: same types, same nullability,
//! same NULLs, same ordering, same everything a query can observe. A decoder
//! bug in a rarely used encoding shows up here as a wrong answer to an ordinary
//! query rather than as a subtle difference nobody looks at.

use std::path::{Path, PathBuf};

use engine::sqllogictest::{self, RunReport};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/engine has two ancestors")
        .to_path_buf()
}

#[test]
fn the_corpus_answers_the_same_from_parquet() {
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

    let mut total = RunReport::default();
    let mut swapped = 0usize;

    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        // Redirect every load to the Parquet twin of the same table. Nothing
        // else in the file changes -- not a query, not an expectation.
        let mut rewritten = String::with_capacity(text.len());
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("load ") {
                let mut parts = rest.split_whitespace();
                if let (Some(table), Some(csv)) = (parts.next(), parts.next()) {
                    let stem = Path::new(csv).file_stem().unwrap().to_string_lossy();
                    rewritten.push_str(&format!("load {table} tests/parquet/{stem}.parquet\n"));
                    swapped += 1;
                    continue;
                }
            }
            rewritten.push_str(line);
            rewritten.push('\n');
        }

        let report = sqllogictest::run(&rewritten, &mut |p| {
            std::fs::read(root.join(p)).map_err(|e| e.to_string())
        })
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));

        for f in &report.failures {
            eprintln!(
                "{}:{}\n  {}\n  {}\n  expected {:?}\n  actual   {:?}",
                path.file_name().unwrap().to_string_lossy(),
                f.line,
                f.sql,
                f.message,
                f.expected,
                f.actual
            );
        }
        total.merge(report);
    }

    println!(
        "parquet corpus: {} passed, {} skipped, {} failed across {} load(s)",
        total.passed,
        total.skipped,
        total.failures.len(),
        swapped
    );
    assert!(swapped >= 20, "expected the loads to have been redirected");
    assert!(total.passed > 900, "corpus too small: {}", total.passed);
    assert!(
        total.is_ok(),
        "{} record(s) answered differently from Parquet",
        total.failures.len()
    );
}
