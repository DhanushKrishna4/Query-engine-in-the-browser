//! Every example on the page promises a rewrite. It has to actually happen.
//!
//! Each example query carries a note saying which panel to open and what to
//! look for -- that is the whole reason the list exists rather than being a
//! handful of queries. A note naming a rule that does not fire is worse than
//! no note: it sends the reader to a trace to find something that is not
//! there, and they conclude the engine is broken rather than the label.
//!
//! Which is exactly what happened. The default example claimed
//! `predicate_pushdown` moved its filter under the sort. It never did: `WHERE`
//! binds below `ORDER BY` to begin with, so the filter was already where the
//! rule would have put it, and the trace showed `projection_pushdown` instead.
//! Nobody noticed until someone read the two lines side by side.
//!
//! So the notes are checked. The examples live in `web/src/main.ts` because
//! that is where the page needs them, and this parses that file rather than
//! keeping a second copy here -- a second copy is the same drift one level
//! down.

use std::path::{Path, PathBuf};

use engine::storage::CsvOptions;
use engine::Engine;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/engine has two ancestors")
        .to_path_buf()
}

/// Every rule name the optimizer has, so a note can only name a real one.
const RULES: [&str; 10] = [
    "predicate_pushdown",
    "projection_pushdown",
    "constant_folding",
    "predicate_simplification",
    "decorrelate",
    "join_reorder",
    "outer_to_inner",
    "limit_pushdown",
    "common_subexpression",
    "aggregate_pushdown",
];

/// One example, as the page declares it.
struct Example {
    label: String,
    sql: String,
    /// Rules the note tells the reader to watch for.
    promised: Vec<String>,
}

/// Pull the examples out of the page source.
///
/// Deliberately literal: it walks `label:`, `sql:` and `watch:` in order and
/// concatenates the string literals of each, which is the shape the file is
/// written in. A cleverer parser would be a TypeScript parser.
fn parse_examples(source: &str) -> Vec<Example> {
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find("    label: \"") {
        rest = &rest[at + "    label: ".len()..];
        let Some(label) = first_string(rest) else { break };
        let Some(sql_at) = rest.find("sql:") else { break };
        let Some(watch_at) = rest.find("watch:") else { break };
        if watch_at < sql_at {
            continue;
        }
        let sql = join_strings(&rest[sql_at..watch_at]);
        // The note ends at the line that closes the object.
        let tail = &rest[watch_at..];
        let end = tail.find("\n  },").unwrap_or(tail.len());
        let note = join_strings(&tail[..end]);

        out.push(Example {
            label,
            sql: sql.replace("\\n", "\n"),
            promised: RULES
                .iter()
                .filter(|r| note.contains(**r))
                .map(|r| r.to_string())
                .collect(),
        });
        rest = tail;
    }
    out
}

/// The first `"..."` in `text`, unescaped enough for SQL.
fn first_string(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let mut out = String::new();
    let mut chars = text[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push_str("\\n"),
                Some(other) => out.push(other),
                None => break,
            },
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

/// Every `"..."` in `text`, concatenated -- the page writes long SQL as a
/// chain of literals joined by `+`.
fn join_strings(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(s) = first_string(rest) {
        let at = rest.find('"').unwrap();
        let after = rest[at + 1..]
            .find('"')
            .map(|i| at + 1 + i + 1)
            .unwrap_or(rest.len());
        out.push_str(&s);
        rest = &rest[after.min(rest.len())..];
    }
    out
}

#[test]
fn every_example_shows_the_rule_it_promises() {
    let root = repo_root();
    let source = std::fs::read_to_string(root.join("web/src/main.ts")).unwrap();
    let examples = parse_examples(&source);

    // A parser that quietly matches nothing would make this test pass while
    // checking nothing at all, which is the failure mode it exists to prevent.
    assert!(
        examples.len() >= 10,
        "only parsed {} examples out of web/src/main.ts",
        examples.len()
    );
    assert!(
        examples.iter().filter(|e| !e.promised.is_empty()).count() >= 4,
        "no example seems to promise a rule; the note parser is probably broken"
    );

    let mut engine = Engine::new();
    let options = CsvOptions::default();
    for (table, file) in [("people", "people.csv"), ("purchases", "orders.csv")] {
        let bytes = std::fs::read(root.join("data").join(file)).unwrap();
        engine.load(table, bytes, &options).unwrap();
    }

    let mut failures = Vec::new();
    for example in &examples {
        if example.promised.is_empty() {
            continue;
        }
        // The TPC-H and taxi examples name tables that arrive from a CDN.
        let Ok(trace) = engine.optimizer_trace(&example.sql) else { continue };
        let fired: Vec<&str> = trace.steps.iter().map(|s| s.rule).collect();
        for rule in &example.promised {
            if !fired.contains(&rule.as_str()) {
                failures.push(format!(
                    "{}: the note promises `{rule}`, the trace shows {fired:?}",
                    example.label
                ));
            }
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
