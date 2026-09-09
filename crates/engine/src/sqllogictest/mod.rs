//! sqllogictest harness: run a file of queries and compare results against
//! expected output produced by SQLite.
//!
//! This is the highest-value testing investment in the project. Unit tests
//! check the cases you thought of; differential testing against a known-correct
//! engine checks the ones you did not -- and semantic bugs (NULL handling,
//! coercion, operator precedence) are exactly the kind you do not think of.
//!
//! The harness lives in the engine crate rather than in a test file because it
//! does no I/O: it takes the file text and a resolver for `load` paths. That
//! keeps it usable from the CLI, from `cargo test`, and eventually from the
//! browser, where the same corpus can run in wasm.
//!
//! ## Comparing two engines with different type systems
//!
//! Values are rendered according to the record's type string (`I`, `R`, `T`)
//! rather than by each engine's own idea of the result type, which is the
//! original format's design and the only thing that makes the comparison
//! meaningful: SQLite is dynamically typed and has no boolean, date or decimal
//! types at all. Two conventions follow from that and are relied on by
//! `tools/gen_expected.py`:
//!
//!   * booleans render as `1` / `0`, because SQLite stores them as integers;
//!   * dates and timestamps render as their ISO text, because SQLite stores
//!     them as TEXT -- and ISO ordering is lexicographic, so comparisons agree.
//!
//! Float columns should be declared `R`, not `T`: `R` pins both engines to
//! `%.3f` instead of leaving each to choose a shortest representation.

pub mod format;

pub use format::{parse, ParseError, Record, RecordKind, SortMode};

use crate::storage::CsvOptions;
use crate::types::{self, ScalarValue};
use crate::Engine;

/// The label this engine answers to in `skipif` / `onlyif` conditions.
pub const LABEL: &str = "qe";

#[derive(Debug, Clone)]
pub struct Failure {
    pub line: usize,
    pub sql: String,
    pub message: String,
    pub expected: Vec<String>,
    pub actual: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub passed: usize,
    pub skipped: usize,
    pub failures: Vec<Failure>,
}

impl RunReport {
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn merge(&mut self, other: RunReport) {
        self.passed += other.passed;
        self.skipped += other.skipped;
        self.failures.extend(other.failures);
    }
}

/// Run a `.slt` file. `resolve` turns a `load` path into CSV bytes; keeping I/O
/// on the caller's side is what lets this run unchanged in the browser.
pub fn run(
    text: &str,
    resolve: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>,
) -> Result<RunReport, ParseError> {
    let records = parse(text)?;
    Ok(run_records(&records, resolve))
}

pub fn run_records(
    records: &[Record],
    resolve: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>,
) -> RunReport {
    let mut engine = Engine::new();
    let mut report = RunReport::default();

    for record in records {
        if matches!(record.kind, RecordKind::Halt) {
            break;
        }
        if !record.applies_to(LABEL) {
            report.skipped += 1;
            continue;
        }

        match &record.kind {
            RecordKind::Halt => unreachable!("handled above"),

            RecordKind::Load { table, path } => match resolve(path) {
                Ok(bytes) => match engine.load(table, bytes, &CsvOptions::default()) {
                    Ok(_) => report.passed += 1,
                    Err(d) => report.failures.push(Failure {
                        line: record.line,
                        sql: format!("load {table} {path}"),
                        message: format!("loading `{path}` failed: {}", d.headline()),
                        expected: Vec::new(),
                        actual: Vec::new(),
                    }),
                },
                Err(e) => report.failures.push(Failure {
                    line: record.line,
                    sql: format!("load {table} {path}"),
                    message: format!("cannot read `{path}`: {e}"),
                    expected: Vec::new(),
                    actual: Vec::new(),
                }),
            },

            RecordKind::Index { table, column } => {
                match engine.catalog_mut().create_index(table, column) {
                    Ok(_) => report.passed += 1,
                    Err(d) => report.failures.push(Failure {
                        line: record.line,
                        sql: format!("index {table} {column}"),
                        message: format!("building the index failed: {}", d.headline()),
                        expected: Vec::new(),
                        actual: Vec::new(),
                    }),
                }
            }

            RecordKind::Statement { expect_error, sql } => {
                let result = engine.execute(sql);
                match (expect_error, result) {
                    (None, Ok(_)) => report.passed += 1,
                    (None, Err(d)) => report.failures.push(Failure {
                        line: record.line,
                        sql: sql.clone(),
                        message: format!("expected success, got error: {}", d.headline()),
                        expected: Vec::new(),
                        actual: Vec::new(),
                    }),
                    (Some(_), Ok(_)) => report.failures.push(Failure {
                        line: record.line,
                        sql: sql.clone(),
                        message: "expected an error, but the statement succeeded".into(),
                        expected: Vec::new(),
                        actual: Vec::new(),
                    }),
                    (Some(needle), Err(d)) => {
                        let text = d.headline();
                        if needle.is_empty() || text.contains(needle.as_str()) {
                            report.passed += 1;
                        } else {
                            report.failures.push(Failure {
                                line: record.line,
                                sql: sql.clone(),
                                message: format!(
                                    "error message does not contain `{needle}`: {text}"
                                ),
                                expected: Vec::new(),
                                actual: Vec::new(),
                            });
                        }
                    }
                }
            }

            RecordKind::Query { types, sort, sql, expected } => {
                match run_query(&engine, sql, types, *sort) {
                    Ok(actual) => {
                        if &actual == expected {
                            report.passed += 1;
                        } else {
                            report.failures.push(Failure {
                                line: record.line,
                                sql: sql.clone(),
                                message: difference_summary(expected, &actual),
                                expected: expected.clone(),
                                actual,
                            });
                        }
                    }
                    Err(message) => report.failures.push(Failure {
                        line: record.line,
                        sql: sql.clone(),
                        message,
                        expected: expected.clone(),
                        actual: Vec::new(),
                    }),
                }
            }
        }
    }

    report
}

/// Execute one query and render its results as a flat list of values.
fn run_query(
    engine: &Engine,
    sql: &str,
    types: &str,
    sort: SortMode,
) -> Result<Vec<String>, String> {
    let result = engine.execute(sql).map_err(|d| d.render(sql))?;

    let want_columns = types.chars().count();
    if result.schema.len() != want_columns {
        return Err(format!(
            "query returned {} column(s), but the record declares {want_columns} (`{types}`)",
            result.schema.len()
        ));
    }

    let letters: Vec<char> = types.chars().collect();
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(result.num_rows());
    for row in result.rows() {
        rows.push(
            row.iter()
                .zip(&letters)
                .map(|(v, ty)| normalize(v, *ty))
                .collect(),
        );
    }

    Ok(match sort {
        SortMode::NoSort => rows.into_iter().flatten().collect(),
        SortMode::RowSort => {
            rows.sort();
            rows.into_iter().flatten().collect()
        }
        SortMode::ValueSort => {
            let mut values: Vec<String> = rows.into_iter().flatten().collect();
            values.sort();
            values
        }
    })
}

/// Render one value under a declared column type.
///
/// This deliberately *coerces* rather than checking: the type letter says how
/// to print, not what the engine must have produced. That is what the original
/// format does, and it is what keeps the comparison about values instead of
/// about two engines' differing type inference.
pub fn normalize(v: &ScalarValue, ty: char) -> String {
    if v.is_null() {
        return "NULL".to_string();
    }
    match ty {
        'I' => as_integer(v).to_string(),
        'R' => format!("{:.3}", as_real(v)),
        _ => as_text(v),
    }
}

fn as_integer(v: &ScalarValue) -> i64 {
    match v {
        // SQLite has no boolean type; it stores them as 0 / 1, and the CSV
        // loader on the SQLite side does the same, so match that here.
        ScalarValue::Boolean(b) => *b as i64,
        ScalarValue::Int32(i) => *i as i64,
        ScalarValue::Int64(i) => *i,
        ScalarValue::Date32(d) => *d as i64,
        ScalarValue::Timestamp(t) => *t,
        ScalarValue::Float64(f) => f.trunc() as i64,
        ScalarValue::Decimal128 { value, scale, .. } => {
            types::rescale(*value, *scale, 0).and_then(|v| i64::try_from(v).ok()).unwrap_or(0)
        }
        // A non-numeric string coerces to 0, as in the original harness.
        ScalarValue::Utf8(s) => s.trim().parse::<f64>().map(|f| f.trunc() as i64).unwrap_or(0),
        ScalarValue::Null => 0,
    }
}

fn as_real(v: &ScalarValue) -> f64 {
    match v {
        ScalarValue::Boolean(b) => *b as i64 as f64,
        ScalarValue::Utf8(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        ScalarValue::Date32(d) => *d as f64,
        ScalarValue::Timestamp(t) => *t as f64,
        other => other.as_f64().unwrap_or(0.0),
    }
}

fn as_text(v: &ScalarValue) -> String {
    let s = match v {
        ScalarValue::Utf8(s) => s.clone(),
        // Booleans go through the integer rendering for the same reason as above.
        ScalarValue::Boolean(b) => (*b as i64).to_string(),
        other => other.to_string(),
    };
    if s.is_empty() {
        // An empty result cell would otherwise be indistinguishable from a
        // blank line, which is what terminates a record.
        return "(empty)".to_string();
    }
    // Control characters would corrupt the line-oriented format.
    s.chars()
        .map(|c| if c.is_control() { '@' } else { c })
        .collect()
}

/// A one-line summary naming the first place the two lists diverge.
fn difference_summary(expected: &[String], actual: &[String]) -> String {
    if expected.len() != actual.len() {
        return format!(
            "expected {} value(s), got {}",
            expected.len(),
            actual.len()
        );
    }
    match expected.iter().zip(actual).position(|(e, a)| e != a) {
        Some(i) => format!(
            "value {} of {} differs: expected `{}`, got `{}`",
            i + 1,
            expected.len(),
            expected[i],
            actual[i]
        ),
        None => "results differ".to_string(),
    }
}

/// Render a failure with enough context to act on, including a side-by-side
/// of the first few differing values.
pub fn format_failure(source: &str, f: &Failure) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "{source}:{}: {}", f.line, f.message);
    for line in f.sql.lines() {
        let _ = writeln!(out, "    {line}");
    }
    if f.expected.is_empty() && f.actual.is_empty() {
        return out;
    }

    const CONTEXT: usize = 12;
    let n = f.expected.len().max(f.actual.len());
    // Centre the window on the first difference rather than always showing the
    // head, so a mismatch at value 400 is actually visible.
    let first_diff = (0..n)
        .find(|i| f.expected.get(*i) != f.actual.get(*i))
        .unwrap_or(0);
    let start = first_diff.saturating_sub(2);
    let end = n.min(start + CONTEXT);

    let _ = writeln!(out, "    {:>5}  {:<24}  actual", "#", "expected");
    for i in start..end {
        let e = f.expected.get(i).map(String::as_str).unwrap_or("<none>");
        let a = f.actual.get(i).map(String::as_str).unwrap_or("<none>");
        let marker = if e == a { ' ' } else { '!' };
        let _ = writeln!(out, "  {marker} {:>5}  {e:<24}  {a}", i + 1);
    }
    if end < n {
        let _ = writeln!(out, "    ... {} more value(s)", n - end);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEOPLE: &[u8] = b"id,name,score,active\n1,Ada,9.5,true\n2,Grace,,false\n";

    fn resolver() -> impl FnMut(&str) -> Result<Vec<u8>, String> {
        |path: &str| {
            if path == "people.csv" {
                Ok(PEOPLE.to_vec())
            } else {
                Err(format!("no such fixture `{path}`"))
            }
        }
    }

    fn run_text(text: &str) -> RunReport {
        run(text, &mut resolver()).unwrap()
    }

    #[test]
    fn a_passing_file_reports_no_failures() {
        let report = run_text(
            "\
load people people.csv

query ITR rowsort
SELECT id, name, score FROM people
----
1
Ada
9.500
2
Grace
NULL
",
        );
        assert!(report.is_ok(), "{:?}", report.failures);
        assert_eq!(report.passed, 2);
    }

    #[test]
    fn booleans_render_as_integers_to_match_sqlite() {
        let report = run_text(
            "load people people.csv\n\nquery I nosort\nSELECT active FROM people\n----\n1\n0\n",
        );
        assert!(report.is_ok(), "{:?}", report.failures);
    }

    #[test]
    fn a_wrong_value_is_reported_with_its_position() {
        let report = run_text(
            "load people people.csv\n\nquery I nosort\nSELECT id FROM people\n----\n1\n99\n",
        );
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].message.contains("value 2 of 2 differs"),
            "{}",
            report.failures[0].message
        );
        let rendered = format_failure("x.slt", &report.failures[0]);
        assert!(rendered.contains("! "), "{rendered}");
    }

    #[test]
    fn a_wrong_column_count_is_caught_before_comparing() {
        let report = run_text(
            "load people people.csv\n\nquery II nosort\nSELECT id FROM people\n----\n1\n2\n",
        );
        assert!(report.failures[0].message.contains("declares 2"));
    }

    #[test]
    fn sort_modes() {
        // rowsort keeps values grouped by row; valuesort does not.
        let by_row = run_text(
            "load people people.csv\n\nquery IT rowsort\nSELECT id, name FROM people\n----\n1\nAda\n2\nGrace\n",
        );
        assert!(by_row.is_ok(), "{:?}", by_row.failures);

        let by_value = run_text(
            "load people people.csv\n\nquery IT valuesort\nSELECT id, name FROM people\n----\n1\n2\nAda\nGrace\n",
        );
        assert!(by_value.is_ok(), "{:?}", by_value.failures);
    }

    #[test]
    fn statement_records_check_errors() {
        let report = run_text(
            "load people people.csv\n\nstatement error no such column\nSELECT nope FROM people\n\nstatement ok\nSELECT id FROM people\n",
        );
        assert!(report.is_ok(), "{:?}", report.failures);

        let report = run_text(
            "load people people.csv\n\nstatement ok\nSELECT nope FROM people\n",
        );
        assert!(report.failures[0].message.contains("expected success"));
    }

    #[test]
    fn skipif_and_halt_are_honoured() {
        let report = run_text(
            "skipif qe\nquery I nosort\nSELECT 999\n----\n1\n\nhalt\n\nquery I nosort\nSELECT 1\n----\n2\n",
        );
        assert!(report.is_ok(), "{:?}", report.failures);
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn empty_strings_are_distinguishable_from_blank_lines() {
        assert_eq!(normalize(&ScalarValue::Utf8(String::new()), 'T'), "(empty)");
        assert_eq!(normalize(&ScalarValue::Null, 'T'), "NULL");
        assert_eq!(normalize(&ScalarValue::Utf8("a\nb".into()), 'T'), "a@b");
    }
}
