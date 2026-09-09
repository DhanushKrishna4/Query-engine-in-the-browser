//! Runs every `.slt` file in `tests/sqllogictest/` against the engine.
//!
//! The expected results in those files were produced by SQLite
//! (`tools/gen_expected.py`), so a failure here means this engine and SQLite
//! disagree about what a query means -- which is the whole point.

use std::path::{Path, PathBuf};

use engine::sqllogictest::{self, format_failure, RunReport};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/engine.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/engine has two ancestors")
        .to_path_buf()
}

fn slt_files() -> Vec<PathBuf> {
    let dir = repo_root().join("tests/sqllogictest");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "slt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .slt files in {}", dir.display());
    files
}

#[test]
fn sqllogictest_corpus_matches_sqlite() {
    let root = repo_root();
    let mut total = RunReport::default();
    let mut rendered = String::new();

    for path in slt_files() {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

        // `load` paths in a .slt file are relative to the repo root. The
        // harness itself does no I/O, so resolving them is the caller's job.
        let mut resolve = |rel: &str| -> Result<Vec<u8>, String> {
            std::fs::read(root.join(rel)).map_err(|e| e.to_string())
        };

        let report = sqllogictest::run(&text, &mut resolve)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));

        let name = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        for f in &report.failures {
            rendered.push_str(&format_failure(&name, f));
            rendered.push('\n');
        }
        total.merge(report);
    }

    println!(
        "sqllogictest: {} passed, {} skipped, {} failed",
        total.passed,
        total.skipped,
        total.failures.len()
    );
    assert!(
        total.is_ok(),
        "\n{rendered}{} sqllogictest record(s) failed",
        total.failures.len()
    );
}
