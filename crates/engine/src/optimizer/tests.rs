//! Rule tests: each rule gets before/after plan assertions.
//!
//! The plans are compared as rendered text, which is the same thing the UI's
//! optimizer trace shows -- so a change in a rule shows up as a readable diff
//! rather than as a mismatch between two `Debug` blobs.
//!
//! Semantics-preservation is not checked here. It is checked in
//! `tests/evaluator_agreement.rs`, over the whole corpus, by running every
//! query with the optimizer off and demanding identical results.

use super::*;
use crate::storage::CsvOptions;
use crate::Engine;

const PEOPLE: &[u8] = b"id,name,age,city\n1,Ada,36,London\n2,Grace,45,New York\n3,Alan,41,London\n";
const ORDERS: &[u8] = b"order_id,person_id,product,qty\n10,1,keyboard,2\n11,3,mouse,4\n";

fn engine() -> Engine {
    let mut e = Engine::new();
    e.load_csv("people", PEOPLE, &CsvOptions::default()).unwrap();
    e.load_csv("orders", ORDERS, &CsvOptions::default()).unwrap();
    e
}

/// A tiny star schema: one larger fact table and two small dimensions, which is
/// the shape join ordering exists for.
fn star() -> Engine {
    let mut e = Engine::new();
    let mut facts = String::from("id,dim_a,dim_b,amount\n");
    for i in 0..2000 {
        facts.push_str(&format!("{i},{},{},{}\n", i % 50, i % 7, i));
    }
    e.load_csv("facts", facts.as_bytes(), &CsvOptions::default())
        .unwrap();

    let mut a = String::from("a_id,a_label\n");
    for i in 0..50 {
        a.push_str(&format!("{i},label{i}\n"));
    }
    e.load_csv("dim_a", a.as_bytes(), &CsvOptions::default())
        .unwrap();

    let mut b = String::from("b_id,b_label\n");
    for i in 0..7 {
        b.push_str(&format!("{i},kind{i}\n"));
    }
    e.load_csv("dim_b", b.as_bytes(), &CsvOptions::default())
        .unwrap();
    e
}

/// The tables at the leaves of a plan, left to right.
fn leaf_order(plan: &crate::plan::LogicalPlan) -> Vec<String> {
    match plan {
        crate::plan::LogicalPlan::Scan { table_name, .. } => vec![table_name.clone()],
        other => other.children().iter().flat_map(|c| leaf_order(c)).collect(),
    }
}

/// The plan after running only `rules`, rendered.
fn with_rules(e: &Engine, sql: &str, rules: Vec<Box<dyn Rule>>) -> String {
    let bound = e
        .bound_plan(sql)
        .unwrap_or_else(|d| panic!("{}", d.render(sql)));
    let plan = Optimizer::with_rules(rules).optimize(bound, e.catalog());
    crate::plan::explain(&plan, false)
}

fn bound(e: &Engine, sql: &str) -> String {
    crate::plan::explain(&e.bound_plan(sql).unwrap(), false)
}

fn folding(e: &Engine, sql: &str) -> String {
    with_rules(e, sql, vec![Box::new(rules::ConstantFolding)])
}

fn pushdown(e: &Engine, sql: &str) -> String {
    with_rules(e, sql, vec![Box::new(rules::PredicatePushdown)])
}

// ---------------------------------------------------------------------------
// Constant folding
// ---------------------------------------------------------------------------

#[test]
fn folds_constant_subexpressions() {
    let e = engine();
    assert!(bound(&e, "SELECT age FROM people WHERE age > 2 * 3").contains("(2 * 3)"));
    assert!(folding(&e, "SELECT age FROM people WHERE age > 2 * 3").contains("(age > 6)"));
}

#[test]
fn boolean_identities_collapse() {
    let e = engine();
    // `1 = 1` folds to TRUE, and `TRUE AND x` is `x`.
    let after = folding(&e, "SELECT age FROM people WHERE 1 = 1 AND age > 5");
    assert!(after.contains("Filter (age > 5)"), "{after}");

    // `1 = 2` folds to FALSE, and `FALSE AND x` is FALSE.
    // A predicate that folds to FALSE removes the filter *and* stops the scan.
    let after = folding(&e, "SELECT age FROM people WHERE 1 = 2 AND age > 5");
    assert!(after.contains("Limit fetch=0"), "{after}");

    let after = folding(&e, "SELECT age FROM people WHERE NOT NOT (age > 5)");
    assert!(after.contains("Filter (age > 5)"), "{after}");
}

#[test]
fn folding_matches_what_execution_would_have_done() {
    let e = engine();

    // A dead branch is never evaluated at runtime either, so folding the whole
    // CASE to 7 is exactly what executing it produces.
    // The output column keeps its name, which is the expression as written --
    // only the expression itself collapses.
    let after = folding(&e, "SELECT CASE WHEN 1 = 2 THEN 1 / 0 ELSE 7 END FROM people");
    assert!(after.starts_with("Project [7 AS CASE WHEN"), "{after}");

    // Likewise `FALSE AND x` short-circuits at runtime, so folding it to FALSE
    // does not hide an error that would otherwise have happened.
    let after = folding(&e, "SELECT age FROM people WHERE 1 = 2 AND 1 / 0 > 1");
    assert!(after.contains("Limit fetch=0"), "{after}");

    // But an expression that genuinely raises is left alone rather than
    // becoming a planning error.
    let after = folding(&e, "SELECT age FROM people WHERE age > 1 / 0");
    assert!(after.contains("(1 / 0)"), "{after}");
}

// ---------------------------------------------------------------------------
// Predicate pushdown
// ---------------------------------------------------------------------------

#[test]
fn pushes_through_an_inner_join_to_both_sides() {
    let e = engine();
    let sql = "SELECT p.name, o.product FROM people p JOIN orders o ON p.id = o.person_id \
               WHERE p.city = 'London' AND o.qty > 1";

    // Before: one filter above the join.
    let before = bound(&e, sql);
    assert_eq!(before.matches("Filter").count(), 1, "{before}");
    let lines: Vec<&str> = before.lines().collect();
    assert!(lines[1].trim_start().starts_with("-> Filter"), "{before}");

    // After: no filter above the join, one on each side.
    let after = pushdown(&e, sql);
    assert_eq!(after.matches("Filter").count(), 2, "{after}");
    let lines: Vec<&str> = after.lines().collect();
    assert!(lines[1].trim_start().starts_with("-> Join"), "{after}");
    assert!(after.contains("-> Filter (city = 'London')"), "{after}");
    assert!(after.contains("-> Filter (qty > 1)"), "{after}");
}

#[test]
fn pushes_only_into_the_preserved_side_of_an_outer_join() {
    let e = engine();

    // The left side of a LEFT join is preserved, so a predicate on it may move.
    let after = pushdown(
        &e,
        "SELECT p.name FROM people p LEFT JOIN orders o ON p.id = o.person_id \
         WHERE p.city = 'London'",
    );
    let lines: Vec<&str> = after.lines().collect();
    assert!(lines[1].trim_start().starts_with("-> Join"), "{after}");
    assert!(after.contains("Filter (city = 'London')"), "{after}");

    // The right side is null-supplying. Filtering it before the join would
    // leave rows padded that the predicate should have removed, so the filter
    // must stay above.
    let sql = "SELECT p.name FROM people p LEFT JOIN orders o ON p.id = o.person_id \
               WHERE o.qty > 1";
    let after = pushdown(&e, sql);
    let lines: Vec<&str> = after.lines().collect();
    assert!(lines[1].trim_start().starts_with("-> Filter"), "{after}");
    assert_eq!(after.matches("Filter").count(), 1, "{after}");

    // A FULL join preserves both sides, so nothing moves in either direction.
    let after = pushdown(
        &e,
        "SELECT p.name FROM people p FULL JOIN orders o ON p.id = o.person_id \
         WHERE p.city = 'London'",
    );
    let lines: Vec<&str> = after.lines().collect();
    assert!(lines[1].trim_start().starts_with("-> Filter"), "{after}");
}

#[test]
fn merges_stacked_filters() {
    let e = engine();
    // Pushing through a join can leave two filters in a row; they become one.
    let after = pushdown(
        &e,
        "SELECT p.name FROM people p JOIN orders o ON p.id = o.person_id \
         WHERE p.city = 'London' AND p.age > 40",
    );
    assert_eq!(after.matches("Filter").count(), 1, "{after}");
    assert!(after.contains("AND"), "{after}");
}

#[test]
fn pushes_a_having_condition_on_a_grouping_key_below_the_aggregate() {
    let e = engine();
    let sql = "SELECT city, COUNT(*) FROM people GROUP BY city HAVING city <> 'London'";

    // Before: Project / Filter / Aggregate / Scan.
    let before = bound(&e, sql);
    let lines: Vec<&str> = before.lines().collect();
    assert!(lines[1].trim_start().starts_with("-> Filter"), "{before}");
    assert!(lines[2].trim_start().starts_with("-> Aggregate"), "{before}");

    // After: the filter is below the aggregate, so fewer rows are grouped.
    let after = pushdown(&e, sql);
    let lines: Vec<&str> = after.lines().collect();
    assert!(lines[1].trim_start().starts_with("-> Aggregate"), "{after}");
    assert!(lines[2].trim_start().starts_with("-> Filter"), "{after}");
    assert!(lines[3].trim_start().starts_with("-> Scan"), "{after}");
}

#[test]
fn a_having_condition_on_an_aggregate_stays_put() {
    let e = engine();
    // COUNT(*) does not exist before grouping, so this cannot move.
    let after = pushdown(
        &e,
        "SELECT city, COUNT(*) FROM people GROUP BY city HAVING COUNT(*) > 1",
    );
    let lines: Vec<&str> = after.lines().collect();
    assert!(lines[1].trim_start().starts_with("-> Filter"), "{after}");
    assert!(lines[2].trim_start().starts_with("-> Aggregate"), "{after}");
}

// ---------------------------------------------------------------------------
// Projection pushdown
// ---------------------------------------------------------------------------

#[test]
fn prunes_columns_a_scan_does_not_need() {
    let e = engine();
    let after = with_rules(
        &engine(),
        "SELECT name FROM people WHERE age > 40",
        vec![Box::new(rules::ProjectionPushdown)],
    );
    // `people` has four columns; only two are read.
    assert!(after.contains("columns=[name:UTF8, age:INT32]"), "{after}");
    assert!(after.contains("2 column(s) pruned"), "{after}");
    let _ = e;
}

#[test]
fn prunes_across_a_join() {
    let after = with_rules(
        &engine(),
        "SELECT p.name, o.product FROM people p JOIN orders o ON p.id = o.person_id",
        vec![Box::new(rules::ProjectionPushdown)],
    );
    assert!(after.contains("columns=[id:INT32, name:UTF8]"), "{after}");
    assert!(after.contains("columns=[person_id:INT32, product:UTF8]"), "{after}");
}

#[test]
fn a_count_star_needs_no_columns_at_all() {
    let after = with_rules(
        &engine(),
        "SELECT COUNT(*) FROM people",
        vec![Box::new(rules::ProjectionPushdown)],
    );
    assert!(after.contains("columns=[]"), "{after}");
    assert!(after.contains("4 column(s) pruned"), "{after}");
}

// ---------------------------------------------------------------------------
// The trace
// ---------------------------------------------------------------------------

#[test]
fn every_rule_application_is_recorded() {
    let e = engine();
    let trace = e
        .optimizer_trace(
            "SELECT p.name FROM people p JOIN orders o ON p.id = o.person_id \
             WHERE 1 = 1 AND p.city = 'London'",
        )
        .unwrap();

    assert!(!trace.is_empty());
    assert!(!trace.truncated);
    // Each step is a complete plan, so the UI can jump to any of them.
    for step in &trace.steps {
        assert!(!crate::plan::explain(&step.before, false).is_empty());
        assert!(!crate::plan::explain(&step.after, false).is_empty());
    }
    // Consecutive steps chain: one step's output is the next one's input.
    for pair in trace.steps.windows(2) {
        assert_eq!(
            crate::plan::explain(&pair[0].after, false),
            crate::plan::explain(&pair[1].before, false)
        );
    }
    assert_eq!(
        crate::plan::explain(&trace.steps.last().unwrap().after, false),
        crate::plan::explain(&trace.final_plan, false)
    );

    let names: Vec<&str> = trace.steps.iter().map(|s| s.rule).collect();
    assert!(names.contains(&"constant_folding"), "{names:?}");
    assert!(names.contains(&"predicate_pushdown"), "{names:?}");
    assert!(names.contains(&"projection_pushdown"), "{names:?}");
}

#[test]
fn a_plan_with_nothing_to_do_produces_no_steps() {
    let e = engine();
    let trace = e.optimizer_trace("SELECT id, name, age, city FROM people").unwrap();
    assert!(trace.is_empty(), "{}", trace.render(false));
}

#[test]
fn the_optimizer_reaches_a_fixpoint() {
    let e = engine();
    // Every corpus-shaped query must settle rather than cycle between rules.
    for sql in [
        "SELECT name FROM people WHERE age > 40 AND 1 = 1",
        "SELECT p.name, o.product FROM people p LEFT JOIN orders o ON p.id = o.person_id WHERE p.age > 1",
        "SELECT city, COUNT(*) FROM people GROUP BY city HAVING COUNT(*) > 1 AND city <> 'x'",
        "SELECT COUNT(*) FROM people, orders",
    ] {
        let trace = e.optimizer_trace(sql).unwrap();
        assert!(!trace.truncated, "did not reach a fixpoint: {sql}");
    }
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

#[test]
fn statistics_are_collected_when_a_table_is_registered() {
    let e = star();
    let stats = e.catalog().statistics("facts").expect("statistics");
    assert_eq!(stats.row_count, 2000);

    // The fact table's `dim_a` column cycles through 50 values.
    let dim_a = &stats.columns[1];
    assert!(
        (dim_a.distinct_count - 50.0).abs() < 3.0,
        "distinct estimate was {}",
        dim_a.distinct_count
    );
    assert!(dim_a.histogram.is_some());
    // 2000 rows over 50 values means every value is common.
    assert!(!dim_a.most_common.is_empty());
    assert_eq!(dim_a.null_count, 0);
}

#[test]
fn selectivity_uses_the_histogram_for_ranges() {
    let e = star();
    let rows = crate::optimizer::stats::estimate_rows(
        &e.bound_plan("SELECT id FROM facts WHERE id < 500").unwrap(),
        e.catalog(),
    );
    assert!((rows - 500.0).abs() < 100.0, "estimated {rows}");

    // Past the maximum, everything matches.
    let rows = crate::optimizer::stats::estimate_rows(
        &e.bound_plan("SELECT id FROM facts WHERE id < 100000").unwrap(),
        e.catalog(),
    );
    assert!((rows - 2000.0).abs() < 50.0, "estimated {rows}");
}

#[test]
fn selectivity_uses_the_mcv_list_for_equality() {
    let e = star();
    // 2000 rows spread over 7 values of dim_b.
    let rows = crate::optimizer::stats::estimate_rows(
        &e.bound_plan("SELECT id FROM facts WHERE dim_b = 3").unwrap(),
        e.catalog(),
    );
    assert!((rows - 286.0).abs() < 40.0, "estimated {rows}");
}

#[test]
fn a_conjunction_multiplies_and_a_disjunction_does_not() {
    let e = star();
    let both = crate::optimizer::stats::estimate_rows(
        &e.bound_plan("SELECT id FROM facts WHERE id < 1000 AND dim_b = 3")
            .unwrap(),
        e.catalog(),
    );
    let either = crate::optimizer::stats::estimate_rows(
        &e.bound_plan("SELECT id FROM facts WHERE id < 1000 OR dim_b = 3")
            .unwrap(),
        e.catalog(),
    );
    // Independence gives ~1000 * ~0.14 for the conjunction; the disjunction
    // must exceed either part alone without reaching their sum.
    assert!(both < 250.0, "AND estimated {both}");
    assert!(either > 1000.0 && either < 1300.0, "OR estimated {either}");
}

#[test]
fn aggregates_are_estimated_at_one_row_per_group() {
    let e = star();
    let rows = crate::optimizer::stats::estimate_rows(
        &e.bound_plan("SELECT dim_b, COUNT(*) FROM facts GROUP BY dim_b")
            .unwrap(),
        e.catalog(),
    );
    assert!((rows - 7.0).abs() < 2.0, "estimated {rows}");

    // A global aggregate is exactly one row, whatever the input.
    assert_eq!(
        crate::optimizer::stats::estimate_rows(
            &e.bound_plan("SELECT COUNT(*) FROM facts").unwrap(),
            e.catalog(),
        ),
        1.0
    );
}

// ---------------------------------------------------------------------------
// Join ordering
// ---------------------------------------------------------------------------

#[test]
fn the_smaller_input_becomes_the_build_side() {
    let e = star();
    // `facts` has 2000 rows and `dim_b` has 7. However the query is written, the
    // small side should end up on the right -- the side a hash table is built
    // from -- because the cost model charges four times more per build row.
    for sql in [
        "SELECT f.id FROM facts f JOIN dim_b b ON f.dim_b = b.b_id",
        "SELECT f.id FROM dim_b b JOIN facts f ON f.dim_b = b.b_id",
    ] {
        let plan = e.plan(sql).unwrap();
        assert_eq!(leaf_order(&plan), vec!["facts", "dim_b"], "for `{sql}`");
    }
}

#[test]
fn a_three_table_join_is_reordered_by_cost() {
    let e = star();
    // Written worst-first: the two dimensions have nothing to join on, so
    // evaluating left to right builds a 350-row cross product before the facts
    // are touched.
    let sql = "SELECT f.id, a.a_label, b.b_label \
               FROM dim_a a JOIN dim_b b ON 1 = 1 \
               JOIN facts f ON f.dim_a = a.a_id AND f.dim_b = b.b_id";
    let bound = e.bound_plan(sql).unwrap();
    let optimized = e.plan(sql).unwrap();

    let before = crate::optimizer::cost::cost_of(&bound, e.catalog());
    let after = crate::optimizer::cost::cost_of(&optimized, e.catalog());
    assert!(after.0 < before.0, "cost {} -> {}", before.0, after.0);

    // The large table ends up on the probe side, which is the decision that
    // matters. The plan the DP picks here is bushy -- it joins the two small
    // dimensions into a 350-row build side and probes it once with the facts,
    // rather than probing the facts twice. That is a real technique, and the
    // cost model prefers it because emitting rows is the dominant cost and the
    // bushy plan emits 2000 rather than 4000.
    assert_eq!(leaf_order(&optimized)[0], "facts", "{}", crate::plan::explain(&optimized, false));
}

#[test]
fn outer_joins_are_not_reordered() {
    let e = star();
    // A LEFT join is not associative with the joins around it, so it stays put
    // and acts as an opaque atom.
    let sql = "SELECT f.id FROM facts f LEFT JOIN dim_a a ON f.dim_a = a.a_id";
    let plan = e.plan(sql).unwrap();
    assert_eq!(leaf_order(&plan), vec!["facts", "dim_a"]);
    assert!(crate::plan::explain(&plan, false).contains("Join LEFT"));
}

#[test]
fn reordering_reaches_a_fixpoint() {
    let e = star();
    for sql in [
        "SELECT f.id FROM facts f JOIN dim_a a ON f.dim_a = a.a_id JOIN dim_b b ON f.dim_b = b.b_id",
        "SELECT f.id FROM dim_b b JOIN dim_a a ON 1 = 1 JOIN facts f ON f.dim_a = a.a_id",
        "SELECT COUNT(*) FROM facts f, dim_a a, dim_b b WHERE f.dim_a = a.a_id AND f.dim_b = b.b_id",
    ] {
        let trace = e.optimizer_trace(sql).unwrap();
        assert!(!trace.truncated, "did not settle: {sql}");
        // The rule must not keep firing on its own output.
        let reorders = trace.steps.iter().filter(|s| s.rule == "join_reorder").count();
        assert!(reorders <= 2, "{sql} reordered {reorders} times");
    }
}

#[test]
fn the_cost_model_prefers_hashing_to_a_nested_loop() {
    let model = crate::optimizer::cost::CostModel::default();
    let hash = model.hash_join(1_000_000.0, 5.0, 1_000_000.0);
    let loops = model.nested_loop_join(1_000_000.0, 5.0, 1_000_000.0);
    assert!(hash.0 < loops.0, "hash {} vs nested loop {}", hash.0, loops.0);

    // And building the smaller side beats building the larger one.
    assert!(
        model.hash_join(1_000_000.0, 5.0, 1_000_000.0).0
            < model.hash_join(5.0, 1_000_000.0, 1_000_000.0).0
    );
}

// ---------------------------------------------------------------------------
// Estimated versus actual
// ---------------------------------------------------------------------------

#[test]
fn every_operator_reports_its_estimate_beside_the_truth() {
    let e = star();
    let r = e
        .execute("SELECT dim_b, COUNT(*) FROM facts WHERE id < 1000 GROUP BY dim_b")
        .unwrap();

    fn check(node: &crate::exec::StatsNode) {
        assert!(
            node.stats.estimated_rows.is_some(),
            "{} has no estimate",
            node.stats.name
        );
        assert!(node.stats.q_error().is_some());
        for c in &node.children {
            check(c);
        }
    }
    check(&r.stats);

    // A scan's estimate comes straight from the catalog, so it is exact.
    fn scan(node: &crate::exec::StatsNode) -> &crate::exec::StatsNode {
        if node.stats.name == "Scan" {
            node
        } else {
            scan(&node.children[0])
        }
    }
    assert_eq!(scan(&r.stats).stats.q_error(), Some(1.0));
}

// ---------------------------------------------------------------------------
// Subqueries
// ---------------------------------------------------------------------------

/// `people`, plus an `orders` table that actually references it.
fn related() -> Engine {
    let mut e = Engine::new();
    e.load_csv("people", PEOPLE, &CsvOptions::default()).unwrap();
    e.load_csv(
        "orders",
        b"order_id,person_id,amount\n10,1,5\n11,1,9\n12,3,2\n",
        &CsvOptions::default(),
    )
    .unwrap();
    e
}

#[test]
fn a_correlated_exists_becomes_a_semi_join() {
    let e = related();
    let sql = "SELECT name FROM people \
               WHERE EXISTS (SELECT 1 FROM orders WHERE orders.person_id = people.id)";

    // Before: a filter holding a subquery expression.
    let before = bound(&e, sql);
    assert!(before.contains("Filter EXISTS"), "{before}");

    // After: one join, no subquery, and the correlated predicate is the
    // join condition.
    let after = crate::plan::explain(&e.plan(sql).unwrap(), false);
    assert!(after.contains("Join SEMI on (person_id = id)"), "{after}");
    assert!(!after.contains("EXISTS"), "{after}");
    assert!(!after.contains("Filter"), "{after}");
}

#[test]
fn not_exists_becomes_an_anti_join() {
    let e = related();
    let after = crate::plan::explain(
        &e.plan(
            "SELECT name FROM people \
             WHERE NOT EXISTS (SELECT 1 FROM orders WHERE orders.person_id = people.id)",
        )
        .unwrap(),
        false,
    );
    assert!(after.contains("Join ANTI"), "{after}");
}

#[test]
fn a_subquery_local_predicate_stays_inside_the_join() {
    let e = related();
    // `amount > 3` mentions only the subquery, so it stays a filter on the
    // right input; only the correlated part becomes the join condition.
    let after = crate::plan::explain(
        &e.plan(
            "SELECT name FROM people WHERE EXISTS \
             (SELECT 1 FROM orders WHERE orders.person_id = people.id AND orders.amount > 3)",
        )
        .unwrap(),
        false,
    );
    assert!(after.contains("Join SEMI on (person_id = id)"), "{after}");
    assert!(after.contains("Filter (amount > 3)"), "{after}");
}

#[test]
fn a_correlated_in_becomes_a_semi_join_on_the_projected_expression() {
    let e = related();
    let after = crate::plan::explain(
        &e.plan(
            "SELECT name FROM people WHERE id IN \
             (SELECT person_id FROM orders WHERE orders.amount > people.age)",
        )
        .unwrap(),
        false,
    );
    assert!(after.contains("Join SEMI"), "{after}");
    // Both the correlation and the IN's own equality are in the condition.
    assert!(after.contains("amount > "), "{after}");
    assert!(after.contains("id = person_id") || after.contains("person_id = id"), "{after}");
}

#[test]
fn a_semi_join_emits_each_row_once() {
    let e = related();
    // Person 1 has two orders. EXISTS asks whether any match exists, not how
    // many, so they must appear once.
    let rows = e
        .execute("SELECT name FROM people WHERE EXISTS (SELECT 1 FROM orders WHERE orders.person_id = people.id)")
        .unwrap();
    assert_eq!(rows.num_rows(), 2);
}

#[test]
fn not_in_is_only_an_anti_join_when_no_null_can_reach_it() {
    let mut e = Engine::new();
    e.load_csv("people", PEOPLE, &CsvOptions::default()).unwrap();
    // `k` is nullable, so a NULL in the subquery's result would make NOT IN
    // unknown rather than true -- which an anti-join would get wrong. The
    // second column is only there because a blank line is not a row.
    let mut nullable = String::from("k,tag\n");
    let mut dense = String::from("k,tag\n");
    for i in 0..40 {
        // A gap in the first column is what makes `nullable` nullable; a blank
        // line would not be a row at all.
        let k = if i == 7 { String::new() } else { i.to_string() };
        nullable.push_str(&format!("{k},row\n"));
        dense.push_str(&format!("{i},row\n"));
    }
    e.load_csv("nullable", nullable.as_bytes(), &CsvOptions::default())
        .unwrap();
    e.load_csv("dense", dense.as_bytes(), &CsvOptions::default())
        .unwrap();

    let with_nulls = crate::plan::explain(
        &e.plan("SELECT id FROM people WHERE id NOT IN (SELECT k FROM nullable)")
            .unwrap(),
        false,
    );
    assert!(!with_nulls.contains("ANTI"), "{with_nulls}");
    assert!(with_nulls.contains("NOT IN"), "{with_nulls}");

    let without = crate::plan::explain(
        &e.plan("SELECT id FROM people WHERE id NOT IN (SELECT k FROM dense)")
            .unwrap(),
        false,
    );
    assert!(without.contains("Join ANTI"), "{without}");
}

#[test]
fn an_uncorrelated_subquery_is_evaluated_at_plan_time() {
    let e = related();

    // A scalar subquery becomes the value it returns.
    let after = crate::plan::explain(
        &e.plan("SELECT name FROM people WHERE age > (SELECT MIN(amount) FROM orders)")
            .unwrap(),
        false,
    );
    assert!(after.contains("(age > 2)"), "{after}");

    // An uncorrelated EXISTS becomes a constant, and a filter on a constant is
    // not a filter -- the operator disappears entirely.
    let after = crate::plan::explain(
        &e.plan("SELECT name FROM people WHERE EXISTS (SELECT 1 FROM orders)")
            .unwrap(),
        false,
    );
    assert!(!after.contains("Filter"), "{after}");
    assert!(!after.contains("Join"), "{after}");

    // The false case is not merely a filter that matches nothing: the plan says
    // so, and the scan below it is never pulled.
    let after = crate::plan::explain(
        &e.plan("SELECT name FROM people WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 999)")
            .unwrap(),
        false,
    );
    assert!(after.contains("Limit fetch=0"), "{after}");
}

#[test]
fn a_small_in_list_is_materialized_rather_than_joined() {
    let e = related();
    // Three values: comparing every row against three literals beats building
    // a hash table, so the subquery is evaluated rather than decorrelated.
    let after = crate::plan::explain(
        &e.plan("SELECT name FROM people WHERE id IN (SELECT person_id FROM orders)")
            .unwrap(),
        false,
    );
    assert!(!after.contains("SEMI"), "{after}");
    assert!(after.contains("IN ("), "{after}");
}

#[test]
fn a_subquery_returning_no_rows_is_null_not_an_error() {
    let e = related();
    let rows = e
        .execute("SELECT (SELECT amount FROM orders WHERE order_id = 999)")
        .unwrap();
    assert!(rows.rows()[0][0].is_null());
}

#[test]
fn a_scalar_subquery_returning_several_rows_is_rejected() {
    let e = related();
    let d = e
        .execute("SELECT (SELECT amount FROM orders) FROM people")
        .unwrap_err();
    assert!(d.message.contains("returned 3 rows"), "{}", d.message);
}

#[test]
fn decorrelation_survives_the_optimizer_being_off() {
    use crate::exec::ExecOptions;
    let e = related();
    // A subquery expression has no execution strategy, so the rewrite that
    // removes it has to run even here.
    for sql in [
        "SELECT name FROM people WHERE EXISTS (SELECT 1 FROM orders WHERE orders.person_id = people.id)",
        "SELECT name FROM people WHERE id IN (SELECT person_id FROM orders)",
        "SELECT name FROM people WHERE age > (SELECT MIN(amount) FROM orders)",
    ] {
        let optimized = e.execute(sql).unwrap().rows();
        let plain = e
            .execute_with(sql, &ExecOptions::unoptimized())
            .unwrap()
            .rows();
        assert_eq!(optimized.len(), plain.len(), "{sql}");
    }
}

#[test]
fn a_derived_table_names_its_columns() {
    let e = related();
    let after = crate::plan::explain(
        &e.plan("SELECT x.city FROM (SELECT city FROM people) AS x WHERE x.city = 'London'")
            .unwrap(),
        false,
    );
    assert!(after.contains("SubqueryAlias x"), "{after}");
}
