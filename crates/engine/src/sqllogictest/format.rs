//! Parser for the sqllogictest file format.
//!
//! The format is SQLite's, from the original `sqllogictest` C harness: a file
//! of records separated by blank lines, where a record is either a statement to
//! run or a query paired with its expected output. Adopting it rather than
//! inventing one means a public corpus can be pointed at this engine later
//! without changing anything here.
//!
//! ```text
//!   query IR rowsort
//!   SELECT id, score FROM people WHERE age > 40
//!   ----
//!   3
//!   NULL
//!   9
//!   NULL
//! ```
//!
//! Note that expected output is **one value per line**, not one row per line.
//! The type string (`IR` above) says how many columns there are and how each is
//! rendered, which is what lets two engines with different type systems be
//! compared on values rather than on their type inference.
//!
//! Two deliberate deviations from the original:
//!
//!   * a `load <table> <file.csv>` record, because this engine has no DDL --
//!     tables come from CSV buffers, so there is no `CREATE TABLE` to run;
//!   * no `N values hashing to <md5>` form. It exists to keep corpus files
//!     small, and implementing it would mean either an MD5 dependency or
//!     hand-rolled MD5, neither of which is worth it yet.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    /// Compare in the order the engine produced them. Only correct when the
    /// query's order is actually determined (ORDER BY, or a single-table scan
    /// whose order both engines agree on).
    NoSort,
    /// Sort whole rows before comparing.
    RowSort,
    /// Sort individual values before comparing, ignoring row boundaries.
    ValueSort,
}

impl SortMode {
    fn parse(s: &str) -> Option<SortMode> {
        Some(match s {
            "nosort" => SortMode::NoSort,
            "rowsort" => SortMode::RowSort,
            "valuesort" => SortMode::ValueSort,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SortMode::NoSort => "nosort",
            SortMode::RowSort => "rowsort",
            SortMode::ValueSort => "valuesort",
        }
    }
}

#[derive(Debug, Clone)]
pub enum RecordKind {
    /// `load <table> <path>` -- register a CSV file as a table.
    Load { table: String, path: String },
    /// `index <table> <column>` -- build a B+ tree index.
    ///
    /// Engine-only, and deliberately not SQL: there is no DDL yet, and an
    /// index changes which physical plan is chosen without changing a single
    /// answer. That is exactly what makes it worth having in the corpus --
    /// every query in a file carrying this directive is checked against
    /// SQLite's answer while running through the index path.
    Index { table: String, column: String },
    /// `statement ok` / `statement error [substring]`.
    Statement {
        /// `None` means the statement must succeed. `Some("")` means it must
        /// fail with any message; `Some(s)` means the message must contain `s`.
        expect_error: Option<String>,
        sql: String,
    },
    Query {
        /// One letter per output column: `I` integer, `R` real, `T` text.
        types: String,
        sort: SortMode,
        sql: String,
        /// One value per line, already normalized.
        expected: Vec<String>,
    },
    /// Stop processing the rest of the file.
    Halt,
}

#[derive(Debug, Clone)]
pub struct Record {
    /// 1-based line number of the record's header, for error messages.
    pub line: usize,
    /// `skipif <label>`: skip this record for the named engine.
    pub skip_if: Vec<String>,
    /// `onlyif <label>`: run this record *only* for the named engines.
    pub only_if: Vec<String>,
    pub kind: RecordKind,
}

impl Record {
    /// Whether an engine identifying itself as `label` should run this record.
    pub fn applies_to(&self, label: &str) -> bool {
        if self.skip_if.iter().any(|s| s == label) {
            return false;
        }
        if !self.only_if.is_empty() && !self.only_if.iter().any(|s| s == label) {
            return false;
        }
        true
    }

    pub fn sql(&self) -> Option<&str> {
        match &self.kind {
            RecordKind::Statement { sql, .. } | RecordKind::Query { sql, .. } => Some(sql),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse a whole `.slt` file.
pub fn parse(text: &str) -> Result<Vec<Record>, ParseError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut records = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        // Between records: blank lines and `#` comments.
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        let mut skip_if = Vec::new();
        let mut only_if = Vec::new();
        // Conditions stack ahead of the header line.
        loop {
            let t = lines[i].trim();
            if let Some(rest) = t.strip_prefix("skipif ") {
                skip_if.push(rest.trim().to_string());
            } else if let Some(rest) = t.strip_prefix("onlyif ") {
                only_if.push(rest.trim().to_string());
            } else {
                break;
            }
            i += 1;
            if i >= lines.len() {
                return Err(ParseError {
                    line: i,
                    message: "file ends after a skipif/onlyif with no record".into(),
                });
            }
        }

        let header_line = i + 1;
        let header = lines[i].trim().to_string();
        i += 1;

        let kind = if header == "halt" {
            RecordKind::Halt
        } else if let Some(rest) = header.strip_prefix("load ") {
            let mut parts = rest.split_whitespace();
            let (Some(table), Some(path)) = (parts.next(), parts.next()) else {
                return Err(ParseError {
                    line: header_line,
                    message: "expected `load <table> <file.csv>`".into(),
                });
            };
            RecordKind::Load {
                table: table.to_string(),
                path: path.to_string(),
            }
        } else if let Some(rest) = header.strip_prefix("index ") {
            let mut parts = rest.split_whitespace();
            let (Some(table), Some(column)) = (parts.next(), parts.next()) else {
                return Err(ParseError {
                    line: header_line,
                    message: "expected `index <table> <column>`".into(),
                });
            };
            RecordKind::Index {
                table: table.to_string(),
                column: column.to_string(),
            }
        } else if let Some(rest) = header.strip_prefix("statement ") {
            let rest = rest.trim();
            let expect_error = if rest == "ok" {
                None
            } else if let Some(msg) = rest.strip_prefix("error") {
                Some(msg.trim().to_string())
            } else {
                return Err(ParseError {
                    line: header_line,
                    message: format!("expected `statement ok` or `statement error`, got `{rest}`"),
                });
            };
            let sql = take_sql(&lines, &mut i, false);
            if sql.is_empty() {
                return Err(ParseError {
                    line: header_line,
                    message: "statement record has no SQL".into(),
                });
            }
            RecordKind::Statement { expect_error, sql }
        } else if let Some(rest) = header.strip_prefix("query ") {
            let mut parts = rest.split_whitespace();
            let Some(types) = parts.next() else {
                return Err(ParseError {
                    line: header_line,
                    message: "query record needs a type string, e.g. `query II rowsort`".into(),
                });
            };
            if let Some(bad) = types.chars().find(|c| !matches!(c, 'I' | 'R' | 'T')) {
                return Err(ParseError {
                    line: header_line,
                    message: format!(
                        "unknown column type `{bad}` in `{types}`; expected I, R or T"
                    ),
                });
            }
            // The sort mode is optional; anything after it is a free-form label.
            let sort = match parts.next() {
                Some(word) => SortMode::parse(word).ok_or_else(|| ParseError {
                    line: header_line,
                    message: format!(
                        "unknown sort mode `{word}`; expected nosort, rowsort or valuesort"
                    ),
                })?,
                None => SortMode::NoSort,
            };

            let sql = take_sql(&lines, &mut i, true);
            if sql.is_empty() {
                return Err(ParseError {
                    line: header_line,
                    message: "query record has no SQL".into(),
                });
            }
            // `take_sql` stopped at either `----` or a blank line.
            let has_separator = i < lines.len() && lines[i].trim() == "----";
            if !has_separator {
                return Err(ParseError {
                    line: header_line,
                    message: "query record is missing its `----` separator".into(),
                });
            }
            i += 1;

            let mut expected = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                expected.push(lines[i].to_string());
                i += 1;
            }
            RecordKind::Query {
                types: types.to_string(),
                sort,
                sql,
                expected,
            }
        } else {
            return Err(ParseError {
                line: header_line,
                message: format!("unknown record type `{header}`"),
            });
        };

        records.push(Record {
            line: header_line,
            skip_if,
            only_if,
            kind,
        });
    }

    Ok(records)
}

/// Consume SQL lines, stopping at a blank line or (for queries) at `----`.
fn take_sql(lines: &[&str], i: &mut usize, stop_at_separator: bool) -> String {
    let mut sql: Vec<&str> = Vec::new();
    while *i < lines.len() {
        let t = lines[*i].trim();
        if t.is_empty() || (stop_at_separator && t == "----") {
            break;
        }
        sql.push(lines[*i].trim_end());
        *i += 1;
    }
    sql.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_whole_file() {
        let text = "\
# a comment
load people data/people.csv

query IT rowsort
SELECT id, name
FROM people
----
1
Ada
2
Grace

skipif qe
query I nosort
SELECT 1
----
1

statement error no such column
SELECT nope FROM people

halt
";
        let records = parse(text).unwrap();
        assert_eq!(records.len(), 5);

        let RecordKind::Load { table, path } = &records[0].kind else {
            panic!("expected load");
        };
        assert_eq!((table.as_str(), path.as_str()), ("people", "data/people.csv"));

        let RecordKind::Query { types, sort, sql, expected } = &records[1].kind else {
            panic!("expected query");
        };
        assert_eq!(types, "IT");
        assert_eq!(*sort, SortMode::RowSort);
        assert_eq!(sql, "SELECT id, name\nFROM people");
        assert_eq!(expected, &["1", "Ada", "2", "Grace"]);

        assert_eq!(records[2].skip_if, vec!["qe"]);
        assert!(!records[2].applies_to("qe"));
        assert!(records[2].applies_to("sqlite"));

        let RecordKind::Statement { expect_error, .. } = &records[3].kind else {
            panic!("expected statement");
        };
        assert_eq!(expect_error.as_deref(), Some("no such column"));

        assert!(matches!(records[4].kind, RecordKind::Halt));
    }

    #[test]
    fn a_query_with_no_rows_has_an_empty_block() {
        let records = parse("query I nosort\nSELECT 1 WHERE 1=2\n----\n").unwrap();
        let RecordKind::Query { expected, .. } = &records[0].kind else {
            panic!()
        };
        assert!(expected.is_empty());
    }

    #[test]
    fn onlyif_restricts_to_named_engines() {
        let records = parse("onlyif qe\nquery I\nSELECT 1\n----\n1\n").unwrap();
        assert!(records[0].applies_to("qe"));
        assert!(!records[0].applies_to("sqlite"));
    }

    #[test]
    fn malformed_records_report_their_line() {
        let e = parse("query\nSELECT 1\n----\n").unwrap_err();
        assert_eq!(e.line, 1);

        let e = parse("query IZ\nSELECT 1\n----\n").unwrap_err();
        assert!(e.message.contains("unknown column type `Z`"), "{}", e.message);

        let e = parse("\n\nquery I nosort\nSELECT 1\n\n").unwrap_err();
        assert_eq!(e.line, 3);
        assert!(e.message.contains("`----`"), "{}", e.message);

        let e = parse("nonsense\n").unwrap_err();
        assert!(e.message.contains("unknown record type"), "{}", e.message);
    }
}
