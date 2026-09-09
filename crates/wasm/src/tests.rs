//! The boundary's conversions, tested natively.
//!
//! These run under `cargo test` on the host, not in a browser: everything worth
//! checking here is the layout of the buffers handed across, and that is the
//! same however the module is loaded. A browser is needed to check that the
//! *page* reads them correctly, which is what `web/` and a real page do.

use super::*;
use engine::storage::CsvOptions;

const CSV: &str = "\
id,name,score,active,joined
1,Ada,9.5,true,2001-05-01
2,Grace,,false,1999-11-30
3,Alan,7.25,true,2003-02-14
";

fn engine() -> QueryEngine {
    let mut e = QueryEngine::new();
    e.inner
        .load_csv("t", CSV.as_bytes(), &CsvOptions::default())
        .unwrap();
    e
}

/// Read a little-endian typed value out of a column's buffer, the way the
/// page's typed-array view does.
fn i32_at(c: &OutColumn, row: usize) -> i32 {
    match &c.values {
        Buffer::I32(v) => v[row],
        other => panic!("expected an i32 buffer, found {other:?}"),
    }
}
fn f64_at(c: &OutColumn, row: usize) -> f64 {
    match &c.values {
        Buffer::F64(v) => v[row],
        other => panic!("expected an f64 buffer, found {other:?}"),
    }
}
fn bytes_of(c: &OutColumn) -> &[u8] {
    match &c.values {
        Buffer::Bytes(v) => v,
        other => panic!("expected a byte buffer, found {other:?}"),
    }
}
fn str_at(c: &OutColumn, row: usize) -> &str {
    let (a, b) = (c.offsets[row] as usize, c.offsets[row + 1] as usize);
    std::str::from_utf8(&bytes_of(c)[a..b]).unwrap()
}
fn valid(c: &OutColumn, row: usize) -> bool {
    c.validity.is_empty() || c.validity[row / 8] & (1 << (row % 8)) != 0
}

#[test]
fn columns_arrive_as_dense_typed_buffers() {
    let mut e = engine();
    let out = e.query_inner("SELECT id, name, score FROM t ORDER BY id").unwrap();
    assert_eq!(out.width(), 3);
    assert_eq!(out.len(0), 3);

    let (id, name, score) = (&out.columns[0], &out.columns[1], &out.columns[2]);
    assert_eq!(id.kind, ColumnKind::Int32);
    assert_eq!(name.kind, ColumnKind::Utf8);
    assert_eq!(score.kind, ColumnKind::Float64);

    assert_eq!([i32_at(id, 0), i32_at(id, 1), i32_at(id, 2)], [1, 2, 3]);
    assert_eq!(str_at(name, 0), "Ada");
    assert_eq!(str_at(name, 2), "Alan");
    assert_eq!(f64_at(score, 0), 9.5);
    assert_eq!(f64_at(score, 2), 7.25);

    // Four bytes per int, eight per float, and one offset more than there are
    // rows -- the layout the page assumes.
    assert_eq!(id.values.byte_len(), 3 * 4);
    assert_eq!(score.values.byte_len(), 3 * 8);
    assert_eq!(name.offsets.len(), 4);

    // And every buffer is aligned for the typed array that will be built on it.
    // A `Vec<u8>` of little-endian bytes is aligned to 1, and
    // `new Int32Array(buffer, ptr, n)` throws on an unaligned pointer -- which
    // showed up only when the allocator happened to place a buffer oddly.
    assert_eq!(out.values_ptr(0) as usize % 4, 0, "int32 column");
    assert_eq!(out.values_ptr(2) as usize % 8, 0, "float64 column");
}

#[test]
fn a_column_with_no_nulls_hands_back_no_bitmap() {
    // The page checks the validity pointer for null and skips the per-row test
    // entirely, so an all-present column must actually produce one.
    let mut e = engine();
    let out = e.query_inner("SELECT id, score FROM t ORDER BY id").unwrap();
    assert!(out.columns[0].validity.is_empty(), "id has no NULLs");
    assert!(out.validity_ptr(0).is_null());

    assert!(!out.columns[1].validity.is_empty(), "score has one NULL");
    assert!(!out.validity_ptr(1).is_null());
    assert!(valid(&out.columns[1], 0));
    assert!(!valid(&out.columns[1], 1));
    assert!(valid(&out.columns[1], 2));
}

#[test]
fn every_buffer_is_aligned_for_its_typed_array() {
    // Checked over many columns and shapes, because the failure depends on
    // where the allocator lands and a single case can pass by luck.
    let mut e = engine();
    for sql in [
        "SELECT id, score, name, active, joined FROM t",
        "SELECT id * 2, score + 1, name || '!' FROM t",
        "SELECT COUNT(*), AVG(score), MIN(name) FROM t",
    ] {
        let out = e.query_inner(sql).unwrap();
        for i in 0..out.width() {
            let ptr = out.values_ptr(i) as usize;
            let need = match out.kind(i) {
                ColumnKind::Int32 | ColumnKind::Date32 => 4,
                ColumnKind::Int64 | ColumnKind::Timestamp | ColumnKind::Float64 => 8,
                _ => 1,
            };
            assert_eq!(ptr % need, 0, "column {i} of `{sql}` is not {need}-byte aligned");
        }
    }
}

#[test]
fn a_null_string_is_distinguishable_from_an_empty_one() {
    // Both are zero bytes in the value buffer; only the bitmap separates them.
    let mut e = QueryEngine::new();
    e.inner
        .load_csv(
            "s",
            b"a\n\"\"\n" as &[u8],
            &CsvOptions::default(),
        )
        .unwrap();
    let out = e.query_inner("SELECT a FROM s UNION ALL SELECT NULL").unwrap();
    let c = &out.columns[0];
    assert_eq!(c.len, 2);
    assert_eq!(c.offsets, vec![0, 0, 0]);
    assert!(valid(c, 0), "the empty string is present");
    assert!(!valid(c, 1), "the NULL is not");
}

#[test]
fn booleans_are_one_bit_per_row() {
    let mut e = engine();
    let out = e.query_inner("SELECT active FROM t ORDER BY id").unwrap();
    let c = &out.columns[0];
    assert_eq!(c.kind, ColumnKind::Boolean);
    assert_eq!(c.len, 3);
    // true, false, true -> 0b101
    assert_eq!(bytes_of(c), &[0b101]);
}

#[test]
fn dates_and_timestamps_cross_as_numbers() {
    let mut e = engine();
    let out = e.query_inner("SELECT joined FROM t ORDER BY id").unwrap();
    let c = &out.columns[0];
    assert_eq!(c.kind, ColumnKind::Date32);
    // 2001-05-01 is 11443 days after the epoch; the page turns it into a Date.
    assert_eq!(i32_at(c, 0), 11443);
}

#[test]
fn decimals_cross_as_text_to_keep_their_exactness() {
    // JavaScript has no 128-bit integer and its `number` would round -- which
    // is the one thing a decimal column exists to avoid.
    let mut e = engine();
    let out = e.query_inner("SELECT CAST('1.05' AS DECIMAL(10,2))").unwrap();
    let c = &out.columns[0];
    assert_eq!(c.kind, ColumnKind::Utf8);
    assert_eq!(str_at(c, 0), "1.05");
}

#[test]
fn the_metadata_carries_the_schema_and_the_operator_tree() {
    let mut e = engine();
    let out = e.query_inner("SELECT COUNT(*) AS n FROM t WHERE score > 8").unwrap();
    let meta: serde_json::Value = serde_json::from_str(&out.meta()).unwrap();
    assert_eq!(meta["num_rows"], 1);
    assert_eq!(meta["columns"][0]["name"], "n");
    assert_eq!(meta["columns"][0]["type"], "INT64");
    assert_eq!(meta["truncated"], false);

    // The statistics tree is nested the way the operators are.
    assert_eq!(meta["stats"]["name"], "Project");
    let agg = &meta["stats"]["children"][0];
    assert_eq!(agg["name"], "HashAggregate");
    assert!(agg["children"][0]["name"].as_str().unwrap().contains("Filter"));
    // Estimates are reported next to reality, which is the whole point.
    assert!(agg["estimated_rows"].is_number());
    assert!(agg["q_error"].is_number());
}

#[test]
fn a_large_result_is_truncated_but_counted_in_full() {
    let mut csv = String::from("n\n");
    for i in 0..(MAX_ROWS_RETURNED + 500) {
        csv.push_str(&format!("{i}\n"));
    }
    let mut e = QueryEngine::new();
    e.inner
        .load_csv("big", csv.as_bytes(), &CsvOptions::default())
        .unwrap();

    let out = e.query_inner("SELECT n FROM big").unwrap();
    assert_eq!(out.len(0), MAX_ROWS_RETURNED);
    let meta: serde_json::Value = serde_json::from_str(&out.meta()).unwrap();
    // The query really did produce every row; only the handover is capped.
    assert_eq!(meta["num_rows"], MAX_ROWS_RETURNED + 500);
    assert_eq!(meta["truncated"], true);
    assert_eq!(meta["stats"]["children"][0]["rows_out"], MAX_ROWS_RETURNED + 500);
}

#[test]
fn errors_arrive_rendered_with_their_caret() {
    let mut e = engine();
    let text = e.query_inner("SELECT naem FROM t").unwrap_err();
    assert!(text.contains("no such column `naem`"), "{text}");
    assert!(text.contains('^'), "the caret underline is missing:\n{text}");
    assert!(text.contains("did you mean `name`"), "{text}");
}

#[test]
fn explain_returns_every_stage() {
    let e = engine();
    let json = e
        .explain("SELECT name FROM t WHERE score > 8 ORDER BY name LIMIT 1")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(!v["tokens"].as_array().unwrap().is_empty());
    assert!(v["ast"].as_str().unwrap().contains("Select"));
    assert!(v["bound"].as_str().unwrap().contains("Filter"));
    assert!(v["optimized"].as_str().unwrap().contains("Scan"));
    // The typed rendering annotates every leaf with its resolved type.
    assert!(v["typed"].as_str().unwrap().contains("::"));
    // And every rewrite is there, before and after. The filter already sits
    // directly on the scan in this query, so predicate pushdown has nothing to
    // do and only projection pushdown fires.
    let steps = v["steps"].as_array().unwrap();
    assert!(!steps.is_empty());
    assert!(
        steps.iter().any(|s| s["rule"] == "projection_pushdown"),
        "{:?}",
        steps.iter().map(|s| &s["rule"]).collect::<Vec<_>>()
    );
    assert!(steps.iter().all(|s| s["before"] != s["after"]));

    // A join does give predicate pushdown something to move.
    let json = e
        .explain_inner("SELECT a.name FROM t a JOIN t b ON a.id = b.id WHERE a.score > 8")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let rules: Vec<String> = v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["rule"].as_str().unwrap().to_string())
        .collect();
    assert!(rules.iter().any(|r| r == "predicate_pushdown"), "{rules:?}");
}

#[test]
fn tables_and_indexes_are_reported() {
    let mut e = engine();
    let tables: serde_json::Value = serde_json::from_str(&e.tables_inner()).unwrap();
    assert_eq!(tables[0]["name"], "t");
    assert_eq!(tables[0]["rows"], 3);
    assert_eq!(tables[0]["pending"], false);
    assert_eq!(tables[0]["columns"].as_array().unwrap().len(), 5);

    let idx: serde_json::Value =
        serde_json::from_str(&e.create_index_inner("t", "id").unwrap()).unwrap();
    assert_eq!(idx["column"], "id");
    assert_eq!(idx["rows"], 3);

    let tables: serde_json::Value = serde_json::from_str(&e.tables_inner()).unwrap();
    assert_eq!(tables[0]["indexes"][0], "id");
}

#[test]
fn a_parquet_buffer_is_recognised_by_its_bytes() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("tests/parquet/people.parquet");
    let Ok(bytes) = std::fs::read(path) else {
        return; // fixtures not generated; the engine's own tests cover this
    };
    let mut e = QueryEngine::new();
    let info: serde_json::Value = serde_json::from_str(&e.load_inner("p", bytes).unwrap()).unwrap();
    assert_eq!(info["rows"], 10);
    assert_eq!(info["pending"], true, "Parquet loads lazily");

    let out = e.query_inner("SELECT name FROM p WHERE age > 55 ORDER BY name").unwrap();
    assert_eq!(out.len(0), 2);
    assert_eq!(str_at(&out.columns[0], 0), "Donald Knuth");
}

#[test]
fn the_trace_names_the_node_each_rule_fired_at() {
    // The slider highlights a subtree rather than diffing two blocks of text,
    // which only works if the step says where it happened.
    let e = engine();
    let json = e
        .explain_inner("SELECT a.name FROM t a JOIN t b ON a.id = b.id WHERE a.score > 8")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let steps = v["steps"].as_array().unwrap();
    assert!(!steps.is_empty());

    for step in steps {
        // The plans are trees now, not strings.
        assert!(step["before"]["label"].is_string());
        assert!(step["before"]["children"].is_array());
        let target = step["target"].as_u64().unwrap() as u32;
        // And the node it names is somewhere in the plan it produced.
        fn contains(node: &serde_json::Value, rel: u32) -> bool {
            node["rel"].as_u64() == Some(rel as u64)
                || node["children"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|c| contains(c, rel))
        }
        assert!(
            contains(&step["before"], target),
            "step {} targets #{target}, which is not in its plan",
            step["rule"]
        );
    }
}

#[test]
fn the_storage_inspector_reports_zone_maps() {
    let e = engine();
    let v: serde_json::Value = serde_json::from_str(&e.storage_inner("t").unwrap()).unwrap();
    assert_eq!(v["rows"], 3);
    assert_eq!(v["pending"], false);
    let rg = &v["row_groups"][0];
    assert_eq!(rg["rows"], 3);
    // A CSV table is resident by definition, so there is no residency to show.
    assert!(rg["resident"].is_null());

    let id = &rg["columns"][0];
    assert_eq!(id["name"], "id");
    assert_eq!(id["min"], "1");
    assert_eq!(id["max"], "3");
    assert_eq!(id["nulls"], 0);

    let score = rg["columns"].as_array().unwrap().iter().find(|c| c["name"] == "score").unwrap();
    assert_eq!(score["nulls"], 1);

    assert!(e.storage_inner("nope").is_err());
}

#[test]
fn the_index_visualizer_reports_the_traversal_path() {
    let mut csv = String::from("k\n");
    for i in 0..5000 {
        csv.push_str(&format!("{}\n", (i * 7919) % 5000));
    }
    let mut e = QueryEngine::new();
    e.inner
        .load_csv("big", csv.as_bytes(), &CsvOptions::default())
        .unwrap();
    e.create_index_inner("big", "k").unwrap();

    let json = e.index_tree_inner("big", "k", Some("1234"), 8).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["column"], "k");
    assert_eq!(v["keys"], 5000);
    assert_eq!(v["matched"], 1);

    let height = v["height"].as_u64().unwrap() as usize;
    let path = v["path"].as_array().unwrap();
    assert_eq!(path.len(), height, "one node per level");

    // Every node on the path is in the sample, marked, whatever the cap.
    let nodes = v["tree"].as_array().unwrap();
    for id in path {
        let node = nodes.iter().find(|n| n["id"] == *id).expect("path node kept");
        assert_eq!(node["on_path"], true);
    }
    // And levels wider than the cap say how many they stood in for.
    assert!(nodes.iter().any(|n| n["level_total"].as_u64().unwrap() > 8));

    // A probe of the wrong type is refused rather than compared as text.
    assert!(e.index_tree_inner("big", "k", Some("not-a-number"), 8).is_err());
    // No index on that column.
    assert!(e.index_tree_inner("big", "nope", None, 8).is_err());

    // Without a probe there is a tree but no path.
    let v: serde_json::Value =
        serde_json::from_str(&e.index_tree_inner("big", "k", None, 8).unwrap()).unwrap();
    assert!(v["path"].as_array().unwrap().is_empty());
    assert!(v["matched"].is_null());
}
