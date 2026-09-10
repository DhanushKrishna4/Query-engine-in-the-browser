//! End-to-end tests: SQL text in, rows out.
//!
//! The NULL-semantics section is the important one. Three-valued logic is where
//! correctness quietly dies, and every case here is one an engine can get wrong
//! while still looking right on a happy-path demo.

use crate::storage::CsvOptions;
use crate::{Engine, QueryResult};

const PEOPLE: &str = "\
id,name,age,city,score,hired
1,Ada,36,London,9.5,2001-05-01
2,Grace,45,New York,8.25,1999-11-30
3,Alan,41,London,,2003-02-14
4,Edsger,52,Austin,7.0,
5,Barbara,38,New York,9.5,2010-07-04
";

fn engine() -> Engine {
    let mut e = Engine::new();
    e.load_csv("people", PEOPLE.as_bytes(), &CsvOptions::default())
        .unwrap();
    e
}

/// Run a query, asserting it succeeds, and render the rows as strings so the
/// expectations below read like a result grid.
fn run(e: &Engine, sql: &str) -> Vec<Vec<String>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|d| panic!("{}", d.render(sql)));
    rows_of(&r)
}

fn rows_of(r: &QueryResult) -> Vec<Vec<String>> {
    r.rows()
        .into_iter()
        .map(|row| row.iter().map(|v| v.to_string()).collect())
        .collect()
}

fn one(e: &Engine, sql: &str) -> String {
    let rows = run(e, sql);
    assert_eq!(rows.len(), 1, "expected exactly one row from `{sql}`");
    assert_eq!(rows[0].len(), 1, "expected exactly one column from `{sql}`");
    rows[0][0].clone()
}

fn err(e: &Engine, sql: &str) -> String {
    e.execute(sql)
        .expect_err(&format!("expected `{sql}` to fail"))
        .message
}

// ---------------------------------------------------------------------------
// The milestone query
// ---------------------------------------------------------------------------

#[test]
fn filtered_scan_returns_correct_rows() {
    let e = engine();
    assert_eq!(
        run(&e, "SELECT name FROM people WHERE age > 40"),
        vec![vec!["Grace"], vec!["Alan"], vec!["Edsger"]]
    );
}

#[test]
fn projection_order_and_expressions() {
    let e = engine();
    assert_eq!(
        run(&e, "SELECT age, name FROM people WHERE id = 1"),
        vec![vec!["36", "Ada"]]
    );
    assert_eq!(
        run(&e, "SELECT age * 2 AS doubled FROM people WHERE id = 1"),
        vec![vec!["72"]]
    );
}

#[test]
fn star_expands_to_every_column_in_order() {
    let e = engine();
    let r = e.execute("SELECT * FROM people WHERE id = 2").unwrap();
    assert_eq!(
        r.schema.names().collect::<Vec<_>>(),
        vec!["id", "name", "age", "city", "score", "hired"]
    );
    assert_eq!(
        rows_of(&r)[0],
        vec!["2", "Grace", "45", "New York", "8.25", "1999-11-30"]
    );
}

#[test]
fn output_column_names() {
    let e = engine();
    let r = e
        .execute("SELECT name, age + 1, age AS years FROM people WHERE id = 1")
        .unwrap();
    assert_eq!(
        r.schema.names().collect::<Vec<_>>(),
        vec!["name", "age + 1", "years"]
    );
}

#[test]
fn table_alias_replaces_the_table_name_as_qualifier() {
    let e = engine();
    assert_eq!(run(&e, "SELECT p.name FROM people p WHERE p.id = 1"), vec![vec!["Ada"]]);
    assert!(err(&e, "SELECT people.name FROM people p").contains("no table or alias named `people`"));
}

#[test]
fn identifiers_are_case_insensitive_unless_quoted() {
    let e = engine();
    assert_eq!(run(&e, "select NAME from PEOPLE where ID = 1"), vec![vec!["Ada"]]);
    assert_eq!(run(&e, r#"SELECT "name" FROM people WHERE id = 1"#), vec![vec!["Ada"]]);
    // A quoted identifier must match the stored case exactly.
    assert!(err(&e, r#"SELECT "NAME" FROM people"#).contains("no such column"));
}

#[test]
fn limit_and_offset() {
    let e = engine();
    assert_eq!(run(&e, "SELECT name FROM people LIMIT 2"), vec![vec!["Ada"], vec!["Grace"]]);
    assert_eq!(
        run(&e, "SELECT name FROM people LIMIT 2 OFFSET 3"),
        vec![vec!["Edsger"], vec!["Barbara"]]
    );
    assert_eq!(run(&e, "SELECT name FROM people LIMIT 0").len(), 0);
    assert_eq!(run(&e, "SELECT name FROM people OFFSET 4"), vec![vec!["Barbara"]]);
}

#[test]
fn from_less_select_evaluates_once() {
    let e = Engine::new();
    assert_eq!(one(&e, "SELECT 1 + 2 * 3"), "7");
    assert_eq!(one(&e, "SELECT 'a' || 'b'"), "ab");
}

// ---------------------------------------------------------------------------
// NULL semantics -- three-valued logic
// ---------------------------------------------------------------------------

#[test]
fn comparison_with_null_is_unknown_so_the_row_is_dropped() {
    let e = engine();
    // Alan's score is NULL. Neither the predicate nor its negation matches him.
    let matched = run(&e, "SELECT name FROM people WHERE score > 8");
    let unmatched = run(&e, "SELECT name FROM people WHERE NOT (score > 8)");
    assert!(!matched.iter().any(|r| r[0] == "Alan"));
    assert!(!unmatched.iter().any(|r| r[0] == "Alan"));

    // `= NULL` is never true, not even for a NULL value.
    assert_eq!(run(&e, "SELECT name FROM people WHERE score = NULL").len(), 0);
    assert_eq!(run(&e, "SELECT name FROM people WHERE NULL = NULL").len(), 0);
}

#[test]
fn is_null_is_the_only_way_to_find_nulls() {
    let e = engine();
    assert_eq!(run(&e, "SELECT name FROM people WHERE score IS NULL"), vec![vec!["Alan"]]);
    assert_eq!(run(&e, "SELECT name FROM people WHERE score IS NOT NULL").len(), 4);
    assert_eq!(run(&e, "SELECT name FROM people WHERE hired IS NULL"), vec![vec!["Edsger"]]);
}

#[test]
fn and_or_follow_the_three_valued_truth_tables() {
    let e = engine();
    // FALSE AND UNKNOWN is FALSE, so this matches nobody...
    assert_eq!(run(&e, "SELECT name FROM people WHERE 1 = 2 AND score > 8").len(), 0);
    // ...and TRUE OR UNKNOWN is TRUE, so this matches everybody, Alan included.
    let all = run(&e, "SELECT name FROM people WHERE 1 = 1 OR score > 8");
    assert_eq!(all.len(), 5);
    // TRUE AND UNKNOWN is UNKNOWN, so Alan is excluded here.
    let some = run(&e, "SELECT name FROM people WHERE 1 = 1 AND score > 8");
    assert!(!some.iter().any(|r| r[0] == "Alan"));
}

#[test]
fn not_in_with_a_null_in_the_list_matches_nothing() {
    let e = engine();
    // The classic trap. `city NOT IN ('London', NULL)` is never TRUE: for a
    // non-London city the comparison against NULL is UNKNOWN, so the whole
    // expression is UNKNOWN rather than TRUE.
    assert_eq!(
        run(&e, "SELECT name FROM people WHERE city NOT IN ('London', NULL)").len(),
        0
    );
    // Without the NULL it behaves as expected.
    assert_eq!(
        run(&e, "SELECT name FROM people WHERE city NOT IN ('London')").len(),
        3
    );
    // IN with a NULL still matches on a real hit.
    assert_eq!(
        run(&e, "SELECT name FROM people WHERE city IN ('Austin', NULL)"),
        vec![vec!["Edsger"]]
    );
}

#[test]
fn arithmetic_and_concat_propagate_null() {
    let e = engine();
    assert_eq!(one(&e, "SELECT score + 1 FROM people WHERE id = 3"), "NULL");
    assert_eq!(one(&e, "SELECT 'x' || NULL"), "NULL");
    assert_eq!(one(&e, "SELECT -NULL"), "NULL");
}

#[test]
fn case_treats_an_unknown_condition_as_no_match() {
    let e = engine();
    assert_eq!(
        one(
            &e,
            "SELECT CASE WHEN score > 9 THEN 'high' ELSE 'other' END FROM people WHERE id = 3"
        ),
        "other"
    );
    // No ELSE means NULL rather than an error.
    assert_eq!(
        one(&e, "SELECT CASE WHEN 1 = 2 THEN 'x' END FROM people WHERE id = 1"),
        "NULL"
    );
    // A simple CASE against a NULL operand falls through, because `NULL = v`
    // is UNKNOWN rather than true.
    assert_eq!(
        one(&e, "SELECT CASE score WHEN 9.5 THEN 'nine' ELSE 'no' END FROM people WHERE id = 3"),
        "no"
    );
}

#[test]
fn between_and_like_propagate_null() {
    let e = engine();
    assert_eq!(one(&e, "SELECT score BETWEEN 1 AND 10 FROM people WHERE id = 3"), "NULL");
    assert_eq!(one(&e, "SELECT NULL LIKE 'a%'"), "NULL");
    assert_eq!(run(&e, "SELECT name FROM people WHERE score BETWEEN 9 AND 10").len(), 2);
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[test]
fn like_and_ilike() {
    let e = engine();
    assert_eq!(run(&e, "SELECT name FROM people WHERE name LIKE 'A%'").len(), 2);
    assert_eq!(run(&e, "SELECT name FROM people WHERE name LIKE 'a%'").len(), 0);
    assert_eq!(run(&e, "SELECT name FROM people WHERE name ILIKE 'a%'").len(), 2);
    assert_eq!(run(&e, "SELECT name FROM people WHERE name LIKE '_da'"), vec![vec!["Ada"]]);
    assert_eq!(
        run(&e, "SELECT name FROM people WHERE name NOT LIKE 'A%'").len(),
        3
    );
}

#[test]
fn between_in_and_boolean_output() {
    let e = engine();
    assert_eq!(run(&e, "SELECT name FROM people WHERE age BETWEEN 38 AND 45").len(), 3);
    assert_eq!(run(&e, "SELECT name FROM people WHERE age NOT BETWEEN 38 AND 45").len(), 2);
    assert_eq!(run(&e, "SELECT name FROM people WHERE id IN (1, 3, 99)").len(), 2);
    assert_eq!(one(&e, "SELECT 1 = 1"), "true");
}

#[test]
fn string_literal_is_folded_into_a_date_comparison() {
    let e = engine();
    // `hired > '2002-01-01'` only works because the binder folds the string
    // literal into a DATE32 rather than trying to compare text with a date.
    assert_eq!(
        run(&e, "SELECT name FROM people WHERE hired > '2002-01-01'"),
        vec![vec!["Alan"], vec!["Barbara"]]
    );
    // A literal that is not a valid date is reported at bind time.
    assert!(err(&e, "SELECT name FROM people WHERE hired > '2002-13-99'")
        .contains("is not a valid DATE32"));
}

#[test]
fn implicit_numeric_widening() {
    let e = engine();
    // age is INT32, score is FLOAT64; comparing them widens to FLOAT64.
    assert_eq!(run(&e, "SELECT name FROM people WHERE score > age").len(), 0);
    assert_eq!(one(&e, "SELECT 1 + 2.5"), "3.5");
}

#[test]
fn explicit_cast() {
    let e = engine();
    assert_eq!(one(&e, "SELECT CAST(age AS FLOAT) FROM people WHERE id = 1"), "36.0");
    assert_eq!(one(&e, "SELECT CAST('42' AS BIGINT)"), "42");
    assert_eq!(one(&e, "SELECT CAST(9.9 AS INT)"), "9");
    assert_eq!(one(&e, "SELECT CAST(NULL AS INT)"), "NULL");
}

#[test]
fn division_by_zero_is_an_error_not_a_null() {
    let e = engine();
    assert!(err(&e, "SELECT 1 / 0").contains("division by zero"));
    assert!(err(&e, "SELECT 1.0 / 0.0").contains("division by zero"));
    // ...but AND short-circuits, so a guarded division never runs.
    assert_eq!(run(&e, "SELECT name FROM people WHERE age <> 36 AND 100 / (age - 36) > 1").len(), 4);
}

#[test]
fn integer_overflow_is_reported() {
    let e = Engine::new();
    assert!(err(&e, "SELECT 9223372036854775807 + 1").contains("overflow"));
}

#[test]
fn integer_arithmetic_is_evaluated_at_64_bits() {
    let e = engine();
    // id and salary are both INT32 because the CSV loader saw values that fit
    // in 32 bits. That is a storage decision; the arithmetic must not inherit
    // it, or an ordinary product overflows for no reason the user can see.
    // Found by the differential fuzzer against SQLite.
    let r = e.execute("SELECT id * age FROM people WHERE id = 1").unwrap();
    assert_eq!(r.schema.field(0).data_type, crate::types::DataType::Int64);
    assert_eq!(rows_of(&r)[0][0], "36");

    // Products that do not fit in 32 bits are computed exactly rather than
    // overflowing.
    assert_eq!(one(&e, "SELECT 100000 * 100000"), "10000000000");
    assert_eq!(one(&e, "SELECT 2147483647 + 1"), "2147483648");
}

#[test]
fn comparisons_do_not_widen() {
    let e = engine();
    // Widening a comparison would defeat zone-map bounds and dictionary codes,
    // which are stored in the column's own width.
    let plan = e.plan("SELECT id FROM people WHERE id > 3").unwrap();
    let typed = crate::plan::explain(&plan, true);
    assert!(typed.contains("id::INT32 > 3::INT32"), "{typed}");
    assert!(!typed.contains("CAST(#0.id"), "{typed}");
}

#[test]
fn implicit_casts_are_hidden_from_user_facing_names_but_kept_in_the_plan() {
    let e = engine();
    // The user wrote `age + 1`, so that is what the output column is called...
    let r = e.execute("SELECT age + 1 FROM people WHERE id = 1").unwrap();
    assert_eq!(r.schema.names().collect::<Vec<_>>(), vec!["age + 1"]);

    // ...but the bound plan still records the widening that actually happens.
    let typed = crate::plan::explain(&e.plan("SELECT age + 1 FROM people").unwrap(), true);
    assert!(typed.contains("CAST(#0.age::INT32 AS INT64)"), "{typed}");

    // An explicit cast is always shown, in both renderings.
    let r = e.execute("SELECT CAST(age AS BIGINT) FROM people WHERE id = 1").unwrap();
    assert_eq!(
        r.schema.names().collect::<Vec<_>>(),
        vec!["CAST(age AS INT64)"]
    );
}

// ---------------------------------------------------------------------------
// Binder diagnostics
// ---------------------------------------------------------------------------

#[test]
fn unknown_column_suggests_a_close_one() {
    let e = engine();
    let d = e.execute("SELECT naem FROM people").unwrap_err();
    assert_eq!(d.message, "no such column `naem`");
    assert_eq!(d.hint.as_deref(), Some("did you mean `name`?"));
    // The caret must land on the offending identifier.
    let sql = "SELECT naem FROM people";
    let rendered = d.render(sql);
    assert!(rendered.contains("line 1, column 8"), "{rendered}");
}

#[test]
fn unknown_table_suggests_a_close_one() {
    let e = engine();
    let d = e.execute("SELECT * FROM peple").unwrap_err();
    assert_eq!(d.message, "no such table `peple`");
    assert_eq!(d.hint.as_deref(), Some("did you mean `people`?"));
}

#[test]
fn type_errors_are_caught_at_bind_time() {
    let e = engine();
    assert!(err(&e, "SELECT name + 1 FROM people").contains("cannot combine"));
    assert!(err(&e, "SELECT * FROM people WHERE age").contains("requires a boolean"));
    assert!(err(&e, "SELECT * FROM people WHERE name AND age").contains("requires a boolean"));
    assert!(err(&e, "SELECT age LIKE 'x' FROM people").contains("LIKE requires UTF8"));
}

#[test]
fn where_cannot_see_select_aliases() {
    let e = engine();
    // Clauses are bound in semantic order, so `x` does not exist during WHERE.
    assert!(err(&e, "SELECT age AS x FROM people WHERE x > 1").contains("no such column `x`"));
}

#[test]
fn unsupported_features_are_named() {
    let e = engine();
    assert!(err(&e, "SELECT abs(age) FROM people").contains("no function named `abs`"));
    assert!(err(&e, "SELECT age FROM people LIMIT age").contains("LIMIT must be a constant"));
    assert!(err(&e, "SELECT age FROM people LIMIT -1").contains("must not be negative"));
}

#[test]
fn column_without_a_from_clause_is_reported_clearly() {
    let e = Engine::new();
    assert!(err(&e, "SELECT x").contains("this query has no FROM clause"));
    assert!(err(&e, "SELECT *").contains("`*` requires a FROM clause"));
}

// ---------------------------------------------------------------------------
// Plan shape and instrumentation
// ---------------------------------------------------------------------------

#[test]
fn plan_is_project_over_filter_over_scan() {
    let e = engine();
    let plan = e.plan("SELECT name FROM people WHERE age > 40").unwrap();
    let text = crate::plan::explain(&plan, false);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("Project"), "{text}");
    assert!(lines[1].trim_start().starts_with("-> Filter"), "{text}");
    assert!(lines[2].trim_start().starts_with("-> Scan"), "{text}");
}

#[test]
fn bound_plan_annotates_types() {
    let e = engine();
    let plan = e.plan("SELECT name FROM people WHERE age > 40").unwrap();
    let typed = crate::plan::explain(&plan, true);
    assert!(typed.contains("::INT32"), "{typed}");
    assert!(typed.contains("name:UTF8"), "{typed}");
}

#[test]
fn every_operator_reports_rows_in_and_out() {
    let e = engine();
    let r = e.execute("SELECT name FROM people WHERE age > 40").unwrap();
    assert_eq!(r.stats.stats.name, "Project");
    assert_eq!(r.stats.stats.rows_out, 3);

    let filter = &r.stats.children[0];
    assert_eq!(filter.stats.name, "Filter");
    assert_eq!(filter.stats.rows_in, 5);
    assert_eq!(filter.stats.rows_out, 3);

    let scan = &filter.children[0];
    assert_eq!(scan.stats.name, "Scan");
    assert_eq!(scan.stats.rows_out, 5);
    assert_eq!(scan.stats.row_groups_total, 1);
    assert_eq!(scan.stats.row_groups_scanned, 1);
}

#[test]
fn scan_emits_multiple_batches_and_row_groups() {
    // 5000 rows at a row-group size of 1024 and a batch size of 2048 exercises
    // both the row-group boundary and the batch boundary.
    let mut csv = String::from("x\n");
    for i in 0..5000 {
        csv.push_str(&format!("{i}\n"));
    }
    let mut e = Engine::new();
    e.load_csv(
        "big",
        csv.as_bytes(),
        &CsvOptions {
            row_group_size: 1024,
            ..Default::default()
        },
    )
    .unwrap();

    // Without pruning the whole table is read.
    let r = e
        .execute_with(
            "SELECT x FROM big WHERE x >= 4990",
            &crate::exec::ExecOptions::without_pruning(),
        )
        .unwrap();
    assert_eq!(r.num_rows(), 10);
    let scan = &r.stats.children[0].children[0];
    assert_eq!(scan.stats.row_groups_total, 5); // 1024 * 4 + 904
    assert_eq!(scan.stats.rows_out, 5000);
    // Batches never exceed the row-group size here, so there is one per group.
    assert_eq!(scan.stats.batches_out, 5);

    // With it, the four row groups whose maximum is below 4990 are skipped on
    // their metadata alone.
    let r = e.execute("SELECT x FROM big WHERE x >= 4990").unwrap();
    assert_eq!(r.num_rows(), 10);
    let scan = &r.stats.children[0].children[0];
    assert_eq!(scan.stats.row_groups_pruned, 4);
    assert_eq!(scan.stats.row_groups_scanned, 1);
    // The whole surviving group: the encodings prove a group *empty* without
    // decoding it, but they do not narrow within one -- see the note in
    // `ScanExec` for why that retreat was measured rather than assumed.
    assert_eq!(scan.stats.rows_out, 904);

    // A limit stops the scan early instead of draining the table.
    let r = e.execute("SELECT x FROM big LIMIT 3").unwrap();
    assert_eq!(r.num_rows(), 3);
    let scan = &r.stats.children[0].children[0];
    assert_eq!(scan.stats.batches_out, 1);
}

// ---------------------------------------------------------------------------
// Vectorized execution
// ---------------------------------------------------------------------------

#[test]
fn scan_batches_are_views_onto_the_row_group() {
    use crate::exec::{Operator as _, ScanExec};

    let mut csv = String::from("x\n");
    for i in 0..5000 {
        csv.push_str(&format!("{i}\n"));
    }
    let mut e = Engine::new();
    let table = e
        .load_csv(
            "big",
            csv.as_bytes(),
            &CsvOptions {
                row_group_size: 4096,
                ..Default::default()
            },
        )
        .unwrap();

    let mut scan = ScanExec::new(table).with_batch_size(2048);
    let batch = scan.next().unwrap().unwrap();
    // 2048 live rows, but the column behind them is the whole 4096-row group:
    // the batch is a window, not a copy.
    assert_eq!(batch.num_rows(), 2048);
    assert_eq!(batch.column_len(), 4096);
    assert!(batch.selection().is_contiguous());
}

#[test]
fn filter_narrows_the_selection_without_rewriting_columns() {
    use crate::exec::build;
    use crate::plan::LogicalPlan;

    let e = engine();
    let plan = e.plan("SELECT name FROM people WHERE age > 40").unwrap();
    // Build only the sub-plan below the projection, so the filter's own output
    // can be inspected before anything materializes it.
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("expected a projection at the root");
    };
    let mut filter = build(input, e.catalog()).unwrap();
    let batch = filter.next().unwrap().unwrap();

    assert_eq!(batch.num_rows(), 3);
    // Three rows survived, but the underlying column is still all five: the
    // filter recorded which rows it kept instead of copying them.
    assert_eq!(batch.column_len(), 5);
    assert!(!batch.selection().is_contiguous());
}

#[test]
fn conjunctions_short_circuit_so_a_guarded_expression_never_raises() {
    let e = engine();
    // The right-hand side divides by zero exactly on the row the left rejects.
    // A vectorized AND that evaluated both sides over the whole batch would
    // raise; evaluating the right only over undecided rows must not.
    assert_eq!(
        run(&e, "SELECT name FROM people WHERE age <> 36 AND 100 / (age - 36) > 1").len(),
        4
    );
    // OR is the mirror image: a TRUE on the left settles the row.
    assert_eq!(
        run(&e, "SELECT name FROM people WHERE age = 36 OR 100 / (age - 36) > 1").len(),
        5
    );
}

#[test]
fn case_branches_are_only_evaluated_for_the_rows_that_take_them() {
    use crate::exec::ExecOptions;
    let e = engine();
    // The guard is the whole point: rows where age = 36 must not reach the
    // division. A vectorized CASE that evaluated every branch over the whole
    // batch would raise here.
    let sql = "SELECT CASE WHEN age <> 36 THEN 100 / (age - 36) ELSE 0 END FROM people";
    for options in [ExecOptions::default(), ExecOptions::scalar()] {
        let rows = e
            .execute_with(sql, &options)
            .unwrap_or_else(|d| panic!("{:?}: {}", options.evaluator, d.render(sql)))
            .rows();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0].to_string(), "0");
    }

    // A condition after one that already matched is not evaluated either.
    for options in [ExecOptions::default(), ExecOptions::scalar()] {
        assert_eq!(
            e.execute_with(
                "SELECT CASE WHEN 1 = 1 THEN 'a' WHEN 1 / 0 > 1 THEN 'b' END FROM people",
                &options,
            )
            .unwrap()
            .rows()[0][0]
                .to_string(),
            "a"
        );
    }
}

#[test]
fn nan_comparisons_are_unknown_in_both_evaluators() {
    use crate::exec::ExecOptions;

    let mut e = Engine::new();
    e.load_csv("f", b"x
1.5
NaN
3.5
", &CsvOptions::default())
        .unwrap();

    // NaN is unordered, so any comparison with it is UNKNOWN -- the row is
    // dropped by the predicate and by its negation alike.
    for (sql, want) in [
        ("SELECT x FROM f WHERE x > 0", 2),
        ("SELECT x FROM f WHERE NOT (x > 0)", 0),
        ("SELECT x FROM f WHERE x = x", 2),
    ] {
        for options in [ExecOptions::default(), ExecOptions::scalar()] {
            let got = e.execute_with(sql, &options).unwrap().num_rows();
            assert_eq!(got, want, "`{sql}` under {:?}", options.evaluator);
        }
    }
}

#[test]
fn results_do_not_depend_on_batch_size_or_compaction() {
    use crate::exec::ExecOptions;

    let mut csv = String::from("x,label
");
    for i in 0..5000 {
        csv.push_str(&format!("{i},row{}
", i % 7));
    }
    let mut e = Engine::new();
    e.load_csv(
        "t",
        csv.as_bytes(),
        &CsvOptions {
            row_group_size: 1024,
            ..Default::default()
        },
    )
    .unwrap();

    let sql = "SELECT x, label FROM t WHERE x % 97 = 0 AND label <> 'row3'";
    let baseline = e.execute(sql).unwrap().rows();
    assert!(!baseline.is_empty());

    for batch_size in [1, 7, 64, 2048, 100_000] {
        for compact_threshold in [0.0, 0.5, 1.0] {
            let options = ExecOptions {
                batch_size,
                compact_threshold,
                ..Default::default()
            };
            let got = e.execute_with(sql, &options).unwrap().rows();
            assert_eq!(
                got, baseline,
                "batch_size={batch_size} compact_threshold={compact_threshold}"
            );
        }
    }
}

#[test]
fn compaction_reports_itself_and_preserves_results() {
    use crate::exec::ExecOptions;

    let mut csv = String::from("x
");
    for i in 0..8192 {
        csv.push_str(&format!("{i}
"));
    }
    let mut e = Engine::new();
    e.load_csv("t", csv.as_bytes(), &CsvOptions::default())
        .unwrap();

    let sql = "SELECT x FROM t WHERE x % 1000 = 0";
    let never = ExecOptions {
        compact_threshold: 0.0,
        ..Default::default()
    };
    let always = ExecOptions {
        compact_threshold: 1.0,
        ..Default::default()
    };

    let a = e.execute_with(sql, &never).unwrap();
    let b = e.execute_with(sql, &always).unwrap();
    assert_eq!(a.rows(), b.rows());
    assert_eq!(a.stats.children[0].stats.compactions, 0);
    assert!(b.stats.children[0].stats.compactions > 0);
}

// ---------------------------------------------------------------------------
// Joins
// ---------------------------------------------------------------------------

fn joined() -> Engine {
    let mut e = Engine::new();
    e.load_csv(
        "l",
        b"id,tag\n1,a\n2,b\n3,c\n",
        &CsvOptions::default(),
    )
    .unwrap();
    // `k` is nullable, which is what makes the NULL-key cases reachable.
    e.load_csv(
        "r",
        b"k,note\n1,one\n1,uno\n2,two\n,orphan\n",
        &CsvOptions::default(),
    )
    .unwrap();
    e
}

#[test]
fn hash_join_is_chosen_only_when_there_is_an_equality_to_hash_on() {
    let e = joined();

    let r = e
        .execute("SELECT l.tag, r.note FROM l JOIN r ON l.id = r.k")
        .unwrap();
    assert_eq!(r.stats.children[0].stats.name, "HashJoin");

    // An inequality gives a hash nothing to work with.
    let r = e
        .execute("SELECT l.tag, r.note FROM l JOIN r ON l.id < r.k")
        .unwrap();
    assert_eq!(r.stats.children[0].stats.name, "NestedLoopJoin");

    let r = e.execute("SELECT l.tag, r.note FROM l CROSS JOIN r").unwrap();
    assert_eq!(r.stats.children[0].stats.name, "NestedLoopJoin");
    assert_eq!(r.num_rows(), 12);
}

#[test]
fn equi_joins_never_match_on_null() {
    let e = joined();
    // `r` has a row whose key is NULL. It must not join to anything, including
    // other NULL keys, because `NULL = NULL` is unknown.
    let rows = run(&e, "SELECT l.tag, r.note FROM l JOIN r ON l.id = r.k");
    assert_eq!(rows.len(), 3); // 1->one, 1->uno, 2->two

    // ...but an outer join still emits it, padded on the other side.
    let rows = run(&e, "SELECT l.tag, r.note FROM l RIGHT JOIN r ON l.id = r.k");
    assert!(rows.iter().any(|row| row[0] == "NULL" && row[1] == "orphan"));
}

#[test]
fn outer_joins_widen_nullability_in_the_schema() {
    let e = joined();
    // `tag` and `note` are NOT NULL in their tables, but a LEFT join can emit a
    // row where the right side is padded -- so the plan must say so.
    let plan = e
        .plan("SELECT l.tag, r.note FROM l LEFT JOIN r ON l.id = r.k")
        .unwrap();
    let schema = plan.schema();
    assert!(!schema.field(0).nullable, "left side stays NOT NULL");
    assert!(schema.field(1).nullable, "right side must become nullable");

    let plan = e
        .plan("SELECT l.tag, r.note FROM l RIGHT JOIN r ON l.id = r.k")
        .unwrap();
    assert!(plan.schema().field(0).nullable, "left side pads under RIGHT");
}

#[test]
fn a_residual_on_the_join_condition_is_not_a_where_filter() {
    let e = joined();
    // Under a LEFT join, a row whose only candidate fails the extra condition
    // is still emitted NULL-padded...
    let on = run(
        &e,
        "SELECT l.tag, r.note FROM l LEFT JOIN r ON l.id = r.k AND r.note = 'uno'",
    );
    assert_eq!(on.len(), 3);
    assert_eq!(on.iter().filter(|row| row[1] == "NULL").count(), 2);

    // ...whereas the same condition in WHERE removes it.
    let filtered = run(
        &e,
        "SELECT l.tag, r.note FROM l LEFT JOIN r ON l.id = r.k WHERE r.note = 'uno'",
    );
    assert_eq!(filtered.len(), 1);
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

#[test]
fn a_global_aggregate_over_no_rows_still_produces_one_row() {
    let e = engine();
    // The classic trap: COUNT is 0, everything else is NULL, and there is
    // exactly one row rather than none.
    let rows = run(
        &e,
        "SELECT COUNT(*), SUM(age), AVG(age), MIN(age), MAX(age) FROM people WHERE 1 = 2",
    );
    assert_eq!(rows, vec![vec!["0", "NULL", "NULL", "NULL", "NULL"]]);

    // Adding a GROUP BY changes that: no rows means no groups.
    assert_eq!(
        run(&e, "SELECT city, COUNT(*) FROM people WHERE 1 = 2 GROUP BY city").len(),
        0
    );
}

#[test]
fn count_star_counts_rows_while_count_col_counts_values() {
    let e = engine();
    // Two of the five scores are NULL.
    assert_eq!(
        run(&e, "SELECT COUNT(*), COUNT(score), COUNT(DISTINCT score) FROM people"),
        vec![vec!["5", "4", "3"]]
    );
    // SUM over an all-NULL input is NULL, not 0.
    assert_eq!(one(&e, "SELECT SUM(score) FROM people WHERE score IS NULL"), "NULL");
    assert_eq!(one(&e, "SELECT COUNT(score) FROM people WHERE score IS NULL"), "0");
}

#[test]
fn null_is_a_group() {
    let e = engine();
    // Unlike a join key, every NULL groups together.
    let rows = run(&e, "SELECT score, COUNT(*) FROM people GROUP BY score");
    let null_group: Vec<_> = rows.iter().filter(|r| r[0] == "NULL").collect();
    assert_eq!(null_group.len(), 1, "all NULLs form exactly one group");
    assert_eq!(null_group[0][1], "1");
}

#[test]
fn groups_come_out_in_first_seen_order() {
    let e = engine();
    // Not required by SQL, but guaranteed here: a HashMap's iteration order
    // would make repeated runs of the same query return different rows under a
    // LIMIT.
    let rows = run(&e, "SELECT city FROM people GROUP BY city");
    assert_eq!(
        rows.iter().map(|r| r[0].as_str()).collect::<Vec<_>>(),
        vec!["London", "New York", "Austin"]
    );
}

#[test]
fn a_grouping_key_can_be_reused_in_a_larger_expression() {
    let e = engine();
    assert_eq!(
        run(&e, "SELECT city || '!', COUNT(*) FROM people GROUP BY city").len(),
        3
    );
    // A compound key is matched syntactically.
    assert_eq!(
        run(&e, "SELECT age / 10, COUNT(*) FROM people GROUP BY age / 10").len(),
        3
    );
}

#[test]
fn aggregate_plan_shape() {
    let e = engine();
    let plan = e
        .plan("SELECT city, COUNT(*) FROM people GROUP BY city HAVING COUNT(*) > 1")
        .unwrap();
    let text = crate::plan::explain(&plan, false);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("Project"), "{text}");
    // HAVING becomes an ordinary filter above the aggregate.
    assert!(lines[1].trim_start().starts_with("-> Filter"), "{text}");
    assert!(lines[2].trim_start().starts_with("-> Aggregate"), "{text}");
    assert!(lines[2].contains("group=[city]"), "{text}");
    assert!(lines[3].trim_start().starts_with("-> Scan"), "{text}");
}

#[test]
fn empty_result_sets_are_fine() {
    let e = engine();
    let r = e.execute("SELECT name FROM people WHERE age > 1000").unwrap();
    assert_eq!(r.num_rows(), 0);
    assert_eq!(r.schema.len(), 1);
}

// ---------------------------------------------------------------------------
// Ordering, DISTINCT and set operations
// ---------------------------------------------------------------------------

#[test]
fn nulls_sort_first_ascending_and_last_descending() {
    let e = engine();
    // Not something SQL fixes; this engine follows SQLite so that the
    // differential corpus compares like with like.
    let asc = run(&e, "SELECT score FROM people ORDER BY score, id");
    assert_eq!(asc[0][0], "NULL");
    let desc = run(&e, "SELECT score FROM people ORDER BY score DESC, id");
    assert_eq!(desc[desc.len() - 1][0], "NULL");

    // And the query can say otherwise.
    let asc = run(&e, "SELECT score FROM people ORDER BY score NULLS LAST, id");
    assert_eq!(asc[asc.len() - 1][0], "NULL");
}

#[test]
fn sorting_by_something_not_selected_trims_the_hidden_column() {
    let e = engine();
    let r = e
        .execute("SELECT name FROM people ORDER BY age DESC")
        .unwrap();
    // One output column, even though the sort needed two.
    assert_eq!(r.schema.len(), 1);
    assert_eq!(r.rows()[0][0].to_string(), "Edsger");
}

/// Whether an operator with this name appears anywhere in the tree.
fn uses_operator(node: &crate::exec::StatsNode, name: &str) -> bool {
    node.stats.name == name || node.children.iter().any(|c| uses_operator(c, name))
}

#[test]
fn a_limit_above_a_sort_becomes_a_top_n_heap() {
    use crate::exec::ExecOptions;
    let e = engine();
    // `age` is not selected, so binding adds it as a hidden column and trims it
    // afterwards -- putting a projection between the limit and the sort. Limit
    // pushdown moves the limit under it, which is what lets the heap be chosen
    // at all.
    let sql = "SELECT name FROM people ORDER BY age LIMIT 2";

    let r = e.execute(sql).unwrap();
    assert!(uses_operator(&r.stats, "TopN"), "{}", crate::exec::explain_stats(&r.stats));
    assert!(!uses_operator(&r.stats, "Sort"));

    // Same answer either way; the heap is only an optimization.
    let full = e
        .execute_with(sql, &ExecOptions::without_top_n())
        .unwrap();
    assert!(uses_operator(&full.stats, "Sort"));
    assert!(!uses_operator(&full.stats, "TopN"));
    assert_eq!(r.rows(), full.rows());
}

#[test]
fn distinct_treats_nulls_as_one_value() {
    let e = engine();
    // Two rows have a NULL score; they collapse to a single output row, unlike
    // a comparison where NULL never equals NULL.
    let rows = run(&e, "SELECT DISTINCT score FROM people");
    assert_eq!(rows.iter().filter(|r| r[0] == "NULL").count(), 1);
}

#[test]
fn set_operations_are_multisets_only_with_all() {
    let mut e = Engine::new();
    e.load_csv("l", b"k\n1\n1\n2\n3\n", &CsvOptions::default())
        .unwrap();
    e.load_csv("r", b"k\n1\n3\n3\n", &CsvOptions::default())
        .unwrap();

    assert_eq!(run(&e, "SELECT k FROM l UNION ALL SELECT k FROM r").len(), 7);
    assert_eq!(run(&e, "SELECT k FROM l UNION SELECT k FROM r").len(), 3);

    // INTERSECT keeps min(left, right) copies with ALL, one without.
    assert_eq!(
        run(&e, "SELECT k FROM l INTERSECT ALL SELECT k FROM r").len(),
        2
    );
    assert_eq!(run(&e, "SELECT k FROM l INTERSECT SELECT k FROM r").len(), 2);

    // EXCEPT keeps max(0, left - right) copies with ALL.
    assert_eq!(
        run(&e, "SELECT k FROM l EXCEPT ALL SELECT k FROM r").len(),
        2
    );
    assert_eq!(run(&e, "SELECT k FROM l EXCEPT SELECT k FROM r").len(), 1);
}

// ---------------------------------------------------------------------------
// Window functions
// ---------------------------------------------------------------------------

#[test]
fn ranking_functions_differ_on_ties() {
    let mut e = Engine::new();
    e.load_csv("t", b"k,tag\n1,a\n1,b\n2,c\n3,d\n", &CsvOptions::default())
        .unwrap();
    let rows = run(
        &e,
        "SELECT k, ROW_NUMBER() OVER (ORDER BY k, tag), RANK() OVER (ORDER BY k, tag), \
         DENSE_RANK() OVER (ORDER BY k) FROM t",
    );
    // ROW_NUMBER never ties. RANK on (k, tag) is a total order so it does not
    // either; DENSE_RANK on k alone ties the two 1s and then counts 2, 3.
    assert_eq!(
        rows.iter().map(|r| r[1].as_str()).collect::<Vec<_>>(),
        vec!["1", "2", "3", "4"]
    );
    assert_eq!(
        rows.iter().map(|r| r[3].as_str()).collect::<Vec<_>>(),
        vec!["1", "1", "2", "3"]
    );
}

#[test]
fn rank_leaves_gaps_where_dense_rank_does_not() {
    let mut e = Engine::new();
    e.load_csv("t", b"k,tag\n1,a\n1,b\n2,c\n", &CsvOptions::default())
        .unwrap();
    let rows = run(
        &e,
        "SELECT RANK() OVER (ORDER BY k), DENSE_RANK() OVER (ORDER BY k) FROM t",
    );
    // Two rows share rank 1, so RANK skips to 3 while DENSE_RANK goes to 2.
    assert_eq!(rows[2][0], "3");
    assert_eq!(rows[2][1], "2");
}

#[test]
fn the_default_frame_makes_tied_rows_share_a_running_total() {
    let mut e = Engine::new();
    e.load_csv("t", b"k,v\n1,10\n1,20\n2,30\n", &CsvOptions::default())
        .unwrap();
    // With no frame clause and an ORDER BY, the frame runs to the end of the
    // current row's *peers* -- so both k=1 rows see the same total.
    let rows = run(&e, "SELECT k, SUM(v) OVER (ORDER BY k) FROM t");
    assert_eq!(rows[0][1], "30");
    assert_eq!(rows[1][1], "30");
    assert_eq!(rows[2][1], "60");

    // ROWS counts rows instead, so the ties diverge.
    let rows = run(
        &e,
        "SELECT k, SUM(v) OVER (ORDER BY k ROWS UNBOUNDED PRECEDING) FROM t",
    );
    assert_eq!(rows[0][1], "10");
    assert_eq!(rows[1][1], "30");
}

#[test]
fn lag_and_lead_stop_at_partition_edges() {
    let mut e = Engine::new();
    e.load_csv(
        "t",
        b"g,v\na,1\na,2\nb,3\nb,4\n",
        &CsvOptions::default(),
    )
    .unwrap();
    let rows = run(
        &e,
        "SELECT g, v, LAG(v) OVER (PARTITION BY g ORDER BY v), \
         LEAD(v) OVER (PARTITION BY g ORDER BY v) FROM t",
    );
    // The first row of each partition has no predecessor, the last no
    // successor -- and the neighbour is never taken from the other partition.
    assert_eq!(rows[0][2], "NULL");
    assert_eq!(rows[1][3], "NULL");
    assert_eq!(rows[2][2], "NULL");
    assert_eq!(rows[3][3], "NULL");
    assert_eq!(rows[1][2], "1");
}

#[test]
fn a_window_with_no_order_covers_the_whole_partition() {
    let e = engine();
    let rows = run(&e, "SELECT name, COUNT(*) OVER () FROM people");
    assert!(rows.iter().all(|r| r[1] == "5"));
}

#[test]
fn several_windows_each_get_their_own_node() {
    let e = engine();
    let plan = e
        .plan("SELECT ROW_NUMBER() OVER (ORDER BY age), ROW_NUMBER() OVER (ORDER BY id) FROM people")
        .unwrap();
    let text = crate::plan::explain(&plan, false);
    assert_eq!(text.matches("Window").count(), 2, "{text}");
}

// ---------------------------------------------------------------------------
// Indexes and bloom filters
// ---------------------------------------------------------------------------

/// A table with a clustered column and a scattered one, so the two pruning
/// structures can be told apart. `id` ascends, so its zone maps are tight and
/// its bloom filters redundant; `token` is shuffled across the whole domain, so
/// every row group's min/max spans everything and only a bloom filter can
/// reject one.
///
/// The tokens are the *even* numbers below 10000. That leaves a value like
/// 4321 inside every row group's min/max range and present in none of them,
/// which is precisely the case a zone map cannot rule out and a bloom filter
/// can. An absent value outside the range proves nothing: the zone map catches
/// that one on its own.
fn scattered_engine() -> Engine {
    let mut csv = String::from("id,token,bucket\n");
    // A deterministic shuffle so a failure reproduces exactly.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut tokens: Vec<u32> = (0..5000).map(|i| i * 2).collect();
    for i in (1..tokens.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        tokens.swap(i, (state % (i as u64 + 1)) as usize);
    }
    for (id, token) in tokens.iter().enumerate() {
        csv.push_str(&format!("{id},{token},{}\n", id % 4));
    }
    let mut e = Engine::new();
    e.load_csv(
        "wide",
        csv.as_bytes(),
        &CsvOptions {
            row_group_size: 500,
            ..Default::default()
        },
    )
    .unwrap();
    e
}

#[test]
fn a_selective_equality_is_served_from_the_index() {
    use crate::exec::ExecOptions;
    let mut e = scattered_engine();
    e.create_index("wide", "token").unwrap();

    let sql = "SELECT id FROM wide WHERE token = 4320";
    let r = e.execute(sql).unwrap();
    assert!(
        uses_operator(&r.stats, "IndexScan"),
        "{}",
        crate::exec::explain_stats(&r.stats)
    );
    assert_eq!(r.num_rows(), 1);

    // The same answer without the index. This equivalence is the entire
    // licence for making the choice at all.
    let plain = e
        .execute_with(sql, &ExecOptions::without_index_scans())
        .unwrap();
    assert!(!uses_operator(&plain.stats, "IndexScan"));
    assert_eq!(plain.rows(), r.rows());
}

#[test]
fn a_broad_predicate_falls_back_to_a_scan() {
    let mut e = scattered_engine();
    e.create_index("wide", "token").unwrap();

    // Half the table. Gathering 2500 scattered rows is a worse version of
    // reading the ten row groups they live in, so the tree hits the cap and
    // gives up rather than producing a plan that loses.
    let r = e.execute("SELECT id FROM wide WHERE token < 5000").unwrap();
    assert!(
        !uses_operator(&r.stats, "IndexScan"),
        "{}",
        crate::exec::explain_stats(&r.stats)
    );
    assert_eq!(r.num_rows(), 2500);
}

#[test]
fn an_index_scan_touches_only_the_row_groups_it_needs() {
    let mut e = scattered_engine();
    e.create_index("wide", "id").unwrap();

    // `id` ascends, so ten consecutive ids live in one row group of 500.
    let r = e
        .execute("SELECT id FROM wide WHERE id >= 1200 AND id < 1210")
        .unwrap();
    let scan = &r.stats.children[0].children[0];
    assert_eq!(scan.stats.name, "IndexScan");
    assert_eq!(scan.stats.row_groups_total, 10);
    assert_eq!(scan.stats.row_groups_scanned, 1);
    assert_eq!(scan.stats.row_groups_pruned, 9);
    assert_eq!(r.num_rows(), 10);
}

#[test]
fn is_null_is_never_served_from_the_index() {
    // NULLs are deliberately absent from the tree, so an index scan for
    // `IS NULL` would return nothing at all. This is the one predicate whose
    // rows are exactly the ones the index omits.
    let mut e = Engine::new();
    let mut csv = String::from("id,v\n");
    for i in 0..3000 {
        if i % 1000 == 0 {
            csv.push_str(&format!("{i},\n"));
        } else {
            csv.push_str(&format!("{i},{i}\n"));
        }
    }
    e.load_csv(
        "t",
        csv.as_bytes(),
        &CsvOptions {
            row_group_size: 500,
            ..Default::default()
        },
    )
    .unwrap();
    e.create_index("t", "v").unwrap();

    let r = e.execute("SELECT id FROM t WHERE v IS NULL").unwrap();
    assert!(!uses_operator(&r.stats, "IndexScan"));
    assert_eq!(r.num_rows(), 3);

    // `= NULL` is UNKNOWN for every row. The index must not be asked, and the
    // answer is empty either way.
    let r = e.execute("SELECT id FROM t WHERE v = NULL").unwrap();
    assert!(!uses_operator(&r.stats, "IndexScan"));
    assert_eq!(r.num_rows(), 0);

    // A range on the same column still uses it, and still excludes the NULLs.
    let r = e.execute("SELECT id FROM t WHERE v > 2990").unwrap();
    assert!(uses_operator(&r.stats, "IndexScan"));
    assert_eq!(r.num_rows(), 9);
}

#[test]
fn bloom_filters_reject_groups_zone_maps_cannot() {
    use crate::exec::ExecOptions;
    let e = scattered_engine();

    // 4321 is odd, so it is absent -- but it sits inside every row group's
    // min/max, so the zone map cannot say so and every group survives to the
    // bloom filters, which can.
    let r = e.execute("SELECT id FROM wide WHERE token = 4321").unwrap();
    let scan = &r.stats.children[0].children[0];
    assert_eq!(scan.stats.name, "Scan");
    assert_eq!(scan.stats.row_groups_bloom_pruned, 10);
    assert_eq!(scan.stats.row_groups_scanned, 0);
    assert_eq!(r.num_rows(), 0);

    // Turning them off reads everything and reaches the same answer. The
    // encodings have to come off too: `token` is bit-packed, so the fast path
    // would answer the predicate on the packed values and the scan would emit
    // nothing again -- correct, but not what this test is isolating.
    let plain = e
        .execute_with(
            "SELECT id FROM wide WHERE token = 4321",
            &ExecOptions {
                bloom_filters: false,
                encodings: false,
                ..Default::default()
            },
        )
        .unwrap();
    let scan = &plain.stats.children[0].children[0];
    assert_eq!(scan.stats.row_groups_bloom_pruned, 0);
    assert_eq!(scan.stats.rows_out, 5000);
    assert_eq!(plain.num_rows(), 0);
}

#[test]
fn a_present_value_survives_every_bloom_filter() {
    // The direction that must never go wrong: a filter that says "absent" for
    // a value that is present loses rows silently.
    let e = scattered_engine();
    for token in [0, 2, 4998, 5000, 9998] {
        let r = e
            .execute(&format!("SELECT id FROM wide WHERE token = {token}"))
            .unwrap();
        assert_eq!(r.num_rows(), 1, "lost token {token}");
    }
}

#[test]
fn only_high_cardinality_columns_get_a_filter() {
    // `bucket` has four distinct values in every group, so a filter would say
    // "maybe" every time while costing memory. `id` and `token` are unique.
    let e = scattered_engine();
    let t = e.catalog().get("wide", false).unwrap();
    let rg = &t.row_groups[0];
    assert!(rg.blooms[0].is_some(), "id");
    assert!(rg.blooms[1].is_some(), "token");
    assert!(rg.blooms[2].is_none(), "bucket");
}

#[test]
fn an_index_scan_emits_rows_in_table_order() {
    // Not incidental. The tree walks its leaves in *key* order; for a shuffled
    // column that is a completely different order from the table's, and a
    // query with no ORDER BY would silently start returning rows differently
    // depending on whether an index happened to exist.
    let mut e = scattered_engine();
    e.create_index("wide", "token").unwrap();

    let sql = "SELECT id FROM wide WHERE token < 120";
    let indexed = e.execute(sql).unwrap();
    assert!(uses_operator(&indexed.stats, "IndexScan"));
    let scanned = e
        .execute_with(sql, &crate::exec::ExecOptions::without_index_scans())
        .unwrap();
    assert_eq!(indexed.rows(), scanned.rows());
    assert_eq!(indexed.num_rows(), 60);
}

#[test]
fn the_narrowest_index_wins_when_several_apply() {
    let mut e = scattered_engine();
    e.create_index("wide", "id").unwrap();
    e.create_index("wide", "token").unwrap();

    // `id < 3000` names 3000 rows and blows the cap; `token = 7` names one.
    let r = e
        .execute("SELECT id FROM wide WHERE id < 3000 AND token = 14")
        .unwrap();
    let scan = &r.stats.children[0].children[0];
    assert_eq!(scan.stats.name, "IndexScan");
    assert!(scan.stats.detail.contains("token"), "{}", scan.stats.detail);
    assert_eq!(r.num_rows(), 1);
}

#[test]
fn an_index_reports_its_shape() {
    let mut e = scattered_engine();
    let idx = e.create_index("wide", "token").unwrap();
    assert_eq!(idx.column_name, "token");
    assert_eq!(idx.tree.num_entries(), 5000);
    assert_eq!(idx.tree.num_keys(), 5000);
    // Root, one interior level, leaves.
    assert_eq!(idx.tree.height(), 3);
    assert_eq!(idx.tree.level_widths()[0], 1);

    // Rebuilding replaces rather than accumulates.
    e.create_index("wide", "token").unwrap();
    assert_eq!(e.catalog().indexes_on("wide").len(), 1);
}

#[test]
fn indexing_an_unknown_column_is_an_error() {
    let mut e = scattered_engine();
    assert!(e.create_index("wide", "nope").is_err());
    assert!(e.create_index("missing", "token").is_err());
}

// ---------------------------------------------------------------------------
// Parquet
// ---------------------------------------------------------------------------

fn parquet_fixture(name: &str) -> std::sync::Arc<[u8]> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("tests/parquet")
        .join(name);
    std::fs::read(&path)
        .unwrap_or_else(|e| {
            panic!(
                "cannot read {}: {e}\nrun `python3 tools/gen_parquet.py`",
                path.display()
            )
        })
        .into()
}

/// The CSV the Parquet fixtures were generated from. Not the `PEOPLE`
/// constant above, which is a smaller inline fixture with different rows.
fn data_csv(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("data")
        .join(name);
    std::fs::read(path).unwrap()
}

fn parquet_engine(name: &str, file: &str) -> Engine {
    let mut e = Engine::new();
    e.load_parquet(name, parquet_fixture(file)).unwrap();
    e
}

#[test]
fn a_parquet_table_reads_only_the_columns_a_query_names() {
    // Ten columns, one row group. A query touching two of them must decode two.
    let e = parquet_engine("m", "snappy.parquet");
    let table = e.catalog().get("m", false).unwrap();
    // Statistics sample the first row group, and this file has only one -- so
    // registration has already decoded it. The claim below is about *scans*,
    // so measure against a fresh table rather than the analyzed one.
    let fresh = crate::storage::parquet::read_parquet("m", parquet_fixture("snappy.parquet"))
        .unwrap();
    assert_eq!(fresh.row_groups[0].resident_columns(), 0);
    assert!(table.is_pending());

    let r = e.execute("SELECT id, price FROM m WHERE id < 5").unwrap();
    assert_eq!(r.num_rows(), 5);
}

#[test]
fn a_pruned_row_group_is_never_decoded() {
    // Eight row groups of 128 rows. `id` ascends, so its zone maps -- read
    // straight out of the Parquet footer -- are disjoint, and a predicate
    // matching one group must leave the other seven untouched.
    let e = parquet_engine("m", "multi_rowgroup.parquet");
    let table = e.catalog().get("m", false).unwrap();
    assert_eq!(table.num_row_groups(), 8);

    let r = e
        .execute("SELECT id, token FROM m WHERE id >= 700 AND id < 740")
        .unwrap();
    assert_eq!(r.num_rows(), 40);
    let scan = &r.stats.children[0].children[0];
    assert_eq!(scan.stats.row_groups_total, 8);
    assert_eq!(scan.stats.row_groups_scanned, 1);
    assert_eq!(scan.stats.row_groups_pruned, 7);

    // Rows 700..739 live in group 5 (128 rows each). Exactly its two named
    // columns were decoded; the pruned groups hold nothing but their footer
    // statistics.
    //
    // Groups 0, 2, 4 and 7 are exempt: registration decoded them, spread
    // across the file, to build the cost model's histograms. The predicate is
    // aimed at group 5 precisely because it is not one of them, so what this
    // asserts is the scan's own behaviour and not the sampler's.
    assert_eq!(table.row_groups[5].resident_columns(), 2, "the scanned group");
    for i in [1, 3, 6] {
        assert_eq!(
            table.row_groups[i].resident_columns(),
            0,
            "row group {i} was decoded anyway"
        );
    }
}

#[test]
fn zone_maps_come_from_the_footer_without_decoding() {
    // The bounds are available before any column has been read, which is the
    // whole reason the footer is worth parsing separately.
    let table =
        crate::storage::parquet::read_parquet("m", parquet_fixture("multi_rowgroup.parquet"))
            .unwrap();
    for rg in &table.row_groups {
        assert_eq!(rg.resident_columns(), 0);
    }
    let first = &table.row_groups[0];
    assert_eq!(first.stats[0].min, Some(crate::types::ScalarValue::Int32(0)));
    assert_eq!(first.stats[0].max, Some(crate::types::ScalarValue::Int32(127)));
    // Null counts too -- `maybe` is null where `i % 7 == 3`, which over the
    // first 128 rows is 3, 10, ... 122: eighteen of them.
    assert_eq!(first.stats[6].null_count, Some(18));
    assert!(table.row_groups.iter().all(|rg| rg.resident_columns() == 0));
}

#[test]
fn parquet_and_csv_tables_are_interchangeable_in_a_join() {
    // One side loaded from CSV, the other from Parquet, joined together.
    let mut e = Engine::new();
    e.load_csv("people", &data_csv("people.csv"), &CsvOptions::default())
        .unwrap();
    e.load_parquet("p2", parquet_fixture("people.parquet"))
        .unwrap();

    let rows = run(
        &e,
        "SELECT a.name FROM people a JOIN p2 b ON a.id = b.id \
         WHERE b.age > 50 ORDER BY a.name",
    );
    assert_eq!(rows, vec![vec!["Donald Knuth"], vec!["Edsger Dijkstra"], vec!["Frances Allen"]]);
}

#[test]
fn the_format_is_chosen_by_the_bytes_not_the_name() {
    let mut e = Engine::new();
    // Named as if it were a CSV; the PAR1 magic decides.
    e.load("t", parquet_fixture("people.parquet").to_vec(), &CsvOptions::default())
        .unwrap();
    assert_eq!(e.execute("SELECT COUNT(*) FROM t").unwrap().num_rows(), 1);
    assert!(e.catalog().get("t", false).unwrap().is_pending());

    e.load("c", data_csv("people.csv"), &CsvOptions::default())
        .unwrap();
    assert!(!e.catalog().get("c", false).unwrap().is_pending());
}

// ---------------------------------------------------------------------------
// Merge join and the streaming aggregate
// ---------------------------------------------------------------------------

use crate::exec::ExecOptions;

/// Two tables with duplicate and NULL join keys, which is where a merge join
/// is easy to get wrong.
fn merge_engine() -> Engine {
    let mut e = Engine::new();
    // Keys 1,1,1,2,4,NULL on the left; 1,1,3,4,NULL on the right.
    e.load_csv(
        "l",
        b"k,tag\n1,a\n1,b\n1,c\n2,d\n4,e\n,f\n" as &[u8],
        &CsvOptions::default(),
    )
    .unwrap();
    e.load_csv(
        "r",
        b"k,note\n1,X\n1,Y\n3,Z\n4,W\n,V\n" as &[u8],
        &CsvOptions::default(),
    )
    .unwrap();
    e
}

/// Every join type, every algorithm, same answer.
#[test]
fn a_merge_join_agrees_with_the_hash_join_on_duplicate_and_null_keys() {
    let e = merge_engine();
    for join in ["INNER", "LEFT", "RIGHT", "FULL"] {
        let sql = format!(
            "SELECT l.tag, r.note FROM l {join} JOIN r ON l.k = r.k \
             ORDER BY l.tag NULLS FIRST, r.note NULLS FIRST"
        );
        let hash = run(&e, &sql);
        let merge = e
            .execute_with(&sql, &ExecOptions::merge_joins())
            .unwrap_or_else(|d| panic!("{join}: {}", d.render(&sql)));
        let merge: Vec<Vec<String>> = merge
            .rows()
            .iter()
            .map(|r| r.iter().map(|v| v.to_string()).collect())
            .collect();
        assert_eq!(hash, merge, "{join} join");
    }
}

#[test]
fn a_run_of_equal_keys_pairs_with_every_row_of_the_other_run() {
    // Three left rows with key 1 against two right rows: six pairs, not one.
    // Getting this wrong is the classic merge-join bug -- loading the right's
    // run advances past it, so a second left row with the same key has nothing
    // left to compare against unless the buffer is consulted first.
    let e = merge_engine();
    let sql = "SELECT l.tag, r.note FROM l JOIN r ON l.k = r.k WHERE l.k = 1 \
               ORDER BY l.tag, r.note";
    let merge = e.execute_with(sql, &ExecOptions::merge_joins()).unwrap();
    assert_eq!(merge.num_rows(), 6, "three left rows times two right rows");
    assert_eq!(run(&e, sql).len(), 6);
}

#[test]
fn a_null_join_key_matches_nothing_but_still_survives_an_outer_join() {
    let e = merge_engine();
    // NULL = NULL is unknown, so the NULL-keyed rows join to nothing...
    let inner = e
        .execute_with(
            "SELECT COUNT(*) FROM l JOIN r ON l.k = r.k",
            &ExecOptions::merge_joins(),
        )
        .unwrap();
    assert_eq!(inner.rows()[0][0].to_string(), "7"); // 3*2 for key 1, 1 for key 4

    // ...but a LEFT join still emits them, padded.
    let left = e
        .execute_with(
            "SELECT l.tag FROM l LEFT JOIN r ON l.k = r.k WHERE r.note IS NULL \
             ORDER BY l.tag",
            &ExecOptions::merge_joins(),
        )
        .unwrap();
    let tags: Vec<String> = left.rows().iter().map(|r| r[0].to_string()).collect();
    assert_eq!(tags, vec!["d", "f"], "key 2 and the NULL key are unmatched");
}

#[test]
fn semi_and_anti_merge_joins_emit_the_left_row_once() {
    let e = merge_engine();
    for (sql, want) in [
        ("SELECT tag FROM l WHERE k IN (SELECT k FROM r) ORDER BY tag", vec!["a", "b", "c", "e"]),
        // `f` has a NULL key: `r.k = NULL` is unknown for every row, so no
        // row exists, so NOT EXISTS is TRUE and `f` survives. (NOT IN would
        // behave differently -- that is the classic trap, and it is why these
        // two are separate rewrites.)
        ("SELECT tag FROM l WHERE NOT EXISTS (SELECT 1 FROM r WHERE r.k = l.k) ORDER BY tag", vec!["d", "f"]),
    ] {
        let merge = e.execute_with(sql, &ExecOptions::merge_joins()).unwrap();
        let got: Vec<String> = merge.rows().iter().map(|r| r[0].to_string()).collect();
        assert_eq!(got, want, "{sql}");
        // `a` matches two right rows and must appear once, not twice.
        assert_eq!(run(&e, sql).len(), want.len());
    }
}

#[test]
fn a_merge_join_is_chosen_when_both_inputs_already_arrive_sorted() {
    // Derived tables with ORDER BY are the only way a plan produces sorted
    // input today, so they are the case the ordering property was written for.
    let e = merge_engine();
    let sql = "SELECT a.tag, b.note FROM (SELECT k, tag FROM l ORDER BY k) a \
               JOIN (SELECT k, note FROM r ORDER BY k) b ON a.k = b.k";
    let r = e.execute(sql).unwrap();
    assert!(
        uses_operator(&r.stats, "MergeJoin"),
        "{}",
        crate::exec::explain_stats(&r.stats)
    );
    assert_eq!(r.num_rows(), 7);

    // Sorted the other way on one side, the orderings no longer agree and the
    // hash join is correct again.
    let mismatched = "SELECT a.tag, b.note FROM (SELECT k, tag FROM l ORDER BY k) a \
                      JOIN (SELECT k, note FROM r ORDER BY k DESC) b ON a.k = b.k";
    let r = e.execute(mismatched).unwrap();
    assert!(!uses_operator(&r.stats, "MergeJoin"), "directions disagree");
    assert_eq!(r.num_rows(), 7);

    // And with nothing sorted, a hash join.
    let r = e.execute("SELECT l.tag FROM l JOIN r ON l.k = r.k").unwrap();
    assert!(!uses_operator(&r.stats, "MergeJoin"));
    assert!(uses_operator(&r.stats, "HashJoin"));
}

#[test]
fn a_streaming_aggregate_is_chosen_when_the_input_arrives_sorted() {
    let e = engine();
    let sorted = "SELECT city, COUNT(*) FROM (SELECT city FROM people ORDER BY city) s \
                  GROUP BY city";
    let r = e.execute(sorted).unwrap();
    assert!(
        uses_operator(&r.stats, "StreamAggregate"),
        "{}",
        crate::exec::explain_stats(&r.stats)
    );

    let unsorted = "SELECT city, COUNT(*) FROM people GROUP BY city";
    let r = e.execute(unsorted).unwrap();
    assert!(uses_operator(&r.stats, "HashAggregate"));
    assert!(!uses_operator(&r.stats, "StreamAggregate"));
}

#[test]
fn a_streaming_aggregate_holds_one_group_and_emits_in_key_order() {
    let mut csv = String::from("k,v\n");
    for i in 0..3000 {
        csv.push_str(&format!("{},{}\n", i % 500, i));
    }
    let mut e = Engine::new();
    e.load_csv("t", csv.as_bytes(), &CsvOptions::default())
        .unwrap();

    let sql = "SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k";
    let hash = run(&e, sql);
    let streamed = e
        .execute_with(sql, &ExecOptions::sorted_aggregates())
        .unwrap();
    assert_eq!(streamed.num_rows(), 500);

    // Same groups either way, and the streaming one comes out in key order.
    let mut hash_sorted = hash.clone();
    hash_sorted.sort();
    let mut got: Vec<Vec<String>> = streamed
        .rows()
        .iter()
        .map(|r| r.iter().map(|v| v.to_string()).collect())
        .collect();
    let keys: Vec<i64> = streamed
        .rows()
        .iter()
        .map(|r| r[0].to_string().parse().unwrap())
        .collect();
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "not in key order");
    got.sort();
    assert_eq!(got, hash_sorted);
}

#[test]
fn a_streaming_aggregate_groups_nulls_together() {
    // Grouping treats NULLs as equal even though comparison does not, and the
    // sort puts them in one contiguous run -- so they must land in one group,
    // not one group per row.
    let mut e = Engine::new();
    e.load_csv(
        "n",
        b"k,v\n1,10\n,20\n1,30\n,40\n,50\n" as &[u8],
        &CsvOptions::default(),
    )
    .unwrap();
    let sql = "SELECT k, COUNT(*), SUM(v) FROM n GROUP BY k";
    let streamed = e
        .execute_with(sql, &ExecOptions::sorted_aggregates())
        .unwrap();
    assert_eq!(streamed.num_rows(), 2);
    let rows: Vec<Vec<String>> = streamed
        .rows()
        .iter()
        .map(|r| r.iter().map(|v| v.to_string()).collect())
        .collect();
    assert!(rows.iter().any(|r| r[0] == "NULL" && r[1] == "3" && r[2] == "110"));
    assert!(rows.iter().any(|r| r[0] == "1" && r[1] == "2" && r[2] == "40"));
}

#[test]
fn a_streaming_aggregate_with_no_grouping_still_emits_one_row() {
    // COUNT returns 0 and SUM returns NULL over an empty input -- the trap the
    // hash aggregate handles by creating its single group up front, and which
    // the streaming one has to handle at end of input instead.
    let e = engine();
    let r = e
        .execute_with(
            "SELECT COUNT(*), SUM(age) FROM people WHERE age > 1000",
            &ExecOptions::sorted_aggregates(),
        )
        .unwrap();
    assert_eq!(r.num_rows(), 1);
    assert_eq!(r.rows()[0][0].to_string(), "0");
    assert_eq!(r.rows()[0][1].to_string(), "NULL");
}
