//! Random tables, random queries, optimized against unoptimized.
//!
//! The sqllogictest corpus covers the cases someone thought of, and
//! `tools/fuzz_queries.py` covers several hundred more against the two fixture
//! schemas. Neither varies the *data*: the same ten people and twelve orders
//! answer every query, so a rewrite that is wrong only when a column happens to
//! be entirely NULL, or when a join key repeats, or when a table has one row,
//! never meets the case that breaks it.
//!
//! This generates the tables too. Every property here is the same shape --
//! **the optimizer must not change the answer** -- which is the definition of a
//! rewrite being semantics-preserving and the one invariant worth checking
//! against arbitrary input.
//!
//! ## Why proptest rather than more of the Python fuzzer
//!
//! Shrinking. A generated corpus that fails hands you a 200-character query
//! over 1,000 rows and leaves the bisection to you; proptest reduces both the
//! table and the query to the smallest pair that still fails, which is usually
//! small enough to read. That is the whole reason it is worth a dependency,
//! and it is a dev-dependency, so `cargo tree --edges normal` still reports
//! that the engine has none.
//!
//! ## Generation is type-directed
//!
//! Values are rendered so the CSV loader's inference cannot be surprised: a
//! float always carries a fraction (so it is not an integer), text always
//! starts with a letter (so it is not a number, a date or `no`), and every
//! column has at least one non-NULL value (so the column does not fall back to
//! Utf8 for lack of evidence). The query is then built from the kinds that were
//! generated, so every comparison is between operands the binder accepts and a
//! failure is a real failure rather than a type error.

use engine::exec::ExecOptions;
use engine::storage::CsvOptions;
use engine::Engine;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

// ---------------------------------------------------------------------------
// The data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Float,
    Text,
    Bool,
}

impl Kind {
    /// Whether `SUM` and `AVG` will accept a column of this kind.
    fn numeric(self) -> bool {
        matches!(self, Kind::Int | Kind::Float)
    }

    /// Whether `<` and friends are worth generating for it.
    fn ordered(self) -> bool {
        !matches!(self, Kind::Bool)
    }
}

/// One cell. `None` is NULL, which is the point of half these tests.
type Cell = Option<i32>;

/// A generated table: a kind per column, and rows of small integers that each
/// column renders in its own way.
///
/// Cells are integers whatever the column's kind, and the rendering turns them
/// into the kind's syntax. That keeps the generator small and, more usefully,
/// keeps shrinking meaningful -- proptest reduces a cell towards zero, and
/// zero is a legible value in every kind.
#[derive(Debug, Clone)]
struct Table {
    kinds: Vec<Kind>,
    rows: Vec<Vec<Cell>>,
}

impl Table {
    fn width(&self) -> usize {
        self.kinds.len()
    }

    /// The CSV the engine will load, with `c0`, `c1`, ... as column names.
    fn to_csv(&self) -> String {
        let mut out = String::new();
        for i in 0..self.width() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("c{i}"));
        }
        out.push('\n');
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&render(self.kinds[i], *cell));
            }
            out.push('\n');
        }
        out
    }
}

/// A cell as CSV, in a form the loader's inference cannot mistake.
fn render(kind: Kind, cell: Cell) -> String {
    let Some(v) = cell else { return String::new() };
    match kind {
        Kind::Int => v.to_string(),
        // A fraction, so it never parses as an integer.
        Kind::Float => format!("{}.5", v),
        // A letter first, so it is never a number, a date, or `no`/`t`/`f`.
        Kind::Text => format!("x{}", v.rem_euclid(7)),
        Kind::Bool => if v % 2 == 0 { "true" } else { "false" }.to_string(),
    }
}

/// A literal as SQL, which differs from CSV only in that text is quoted.
fn sql_literal(kind: Kind, v: i32) -> String {
    match kind {
        Kind::Text => format!("'{}'", render(kind, Some(v))),
        _ => render(kind, Some(v)),
    }
}

fn table_strategy() -> impl Strategy<Value = Table> {
    // Small on purpose. A rewrite that is wrong is wrong on three rows, and
    // three rows is what a failure should print.
    prop::collection::vec(
        prop_oneof![Just(Kind::Int), Just(Kind::Float), Just(Kind::Text), Just(Kind::Bool)],
        1..=3usize,
    )
    .prop_flat_map(|kinds| {
        let width = kinds.len();
        let cell = prop_oneof![
            // NULLs often enough to matter, since three-valued logic is where
            // an optimizer rewrite most easily changes an answer.
            2 => Just(None),
            8 => (-4i32..=4).prop_map(Some),
        ];
        prop::collection::vec(prop::collection::vec(cell, width..=width), 1..=6usize)
            .prop_map(move |rows| Table { kinds: kinds.clone(), rows })
    })
    .prop_map(|mut t| {
        // Every column needs one non-NULL value or the loader has no evidence
        // for a type and lands on Utf8, which would make the generated
        // comparisons bind against a type the generator did not choose.
        for c in 0..t.width() {
            if t.rows.iter().all(|r| r[c].is_none()) {
                t.rows[0][c] = Some(0);
            }
        }
        t
    })
}

// ---------------------------------------------------------------------------
// The query
// ---------------------------------------------------------------------------

/// A predicate over one table, as a shape to be rendered against real kinds.
///
/// Seeds rather than literals: the strategy does not know the column kinds
/// when it runs, so it generates a column index and an integer and the
/// rendering turns them into something the binder will accept.
#[derive(Debug, Clone)]
enum Pred {
    IsNull { col: usize, negated: bool },
    Compare { col: usize, op: usize, seed: i32 },
    InList { col: usize, a: i32, b: i32 },
    Between { col: usize, a: i32, b: i32 },
    Truthy { col: usize },
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
    Not(Box<Pred>),
}

const OPS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

impl Pred {
    /// Render against a table's real kinds.
    ///
    /// Total rather than fallible. A shape that does not fit the column it
    /// landed on -- `BETWEEN` over a boolean, a bare boolean test on a text
    /// column -- falls back to a predicate that does, instead of collapsing
    /// the whole tree to nothing. An `AND` whose right side failed to render
    /// used to take the left side down with it, and two thirds of the
    /// generated queries came out with no `WHERE` clause at all.
    fn render(&self, t: &Table, alias: &str) -> String {
        let name = |c: usize| format!("{alias}.c{}", c % t.width());
        let kind = |c: usize| t.kinds[c % t.width()];
        match self {
            Pred::IsNull { col, negated } => {
                format!("{} IS {}NULL", name(*col), if *negated { "NOT " } else { "" })
            }
            Pred::Compare { col, op, seed } => {
                let k = kind(*col);
                let op = OPS[*op % if k.ordered() { OPS.len() } else { 2 }];
                format!("{} {} {}", name(*col), op, sql_literal(k, *seed))
            }
            Pred::InList { col, a, b } => {
                let k = kind(*col);
                format!(
                    "{} IN ({}, {})",
                    name(*col),
                    sql_literal(k, *a),
                    sql_literal(k, *b)
                )
            }
            Pred::Between { col, a, b } => {
                let k = kind(*col);
                if !k.ordered() {
                    // A boolean has no range worth naming; test it directly.
                    return format!("{} = {}", name(*col), sql_literal(k, *a));
                }
                format!(
                    "{} BETWEEN {} AND {}",
                    name(*col),
                    sql_literal(k, *a.min(b)),
                    sql_literal(k, *a.max(b))
                )
            }
            Pred::Truthy { col } => {
                if kind(*col) != Kind::Bool {
                    // Not a boolean, so not usable as a bare condition. The
                    // nearest predicate that is always well typed keeps the
                    // NULL semantics this shape was generated to exercise.
                    return format!("{} IS NOT NULL", name(*col));
                }
                name(*col)
            }
            Pred::And(l, r) => format!("({} AND {})", l.render(t, alias), r.render(t, alias)),
            Pred::Or(l, r) => format!("({} OR {})", l.render(t, alias), r.render(t, alias)),
            Pred::Not(p) => format!("(NOT {})", p.render(t, alias)),
        }
    }
}

fn pred_strategy() -> impl Strategy<Value = Pred> {
    let leaf = prop_oneof![
        (0usize..3, any::<bool>()).prop_map(|(col, negated)| Pred::IsNull { col, negated }),
        (0usize..3, 0usize..6, -4i32..=4)
            .prop_map(|(col, op, seed)| Pred::Compare { col, op, seed }),
        (0usize..3, -4i32..=4, -4i32..=4).prop_map(|(col, a, b)| Pred::InList { col, a, b }),
        (0usize..3, -4i32..=4, -4i32..=4).prop_map(|(col, a, b)| Pred::Between { col, a, b }),
        (0usize..3).prop_map(|col| Pred::Truthy { col }),
    ];
    // Two levels of nesting: enough for `AND` over `OR` over comparisons,
    // which is where predicate pushdown and simplification actually work.
    leaf.prop_recursive(2, 8, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(l, r)| Pred::And(Box::new(l), Box::new(r))),
            (inner.clone(), inner.clone()).prop_map(|(l, r)| Pred::Or(Box::new(l), Box::new(r))),
            inner.prop_map(|p| Pred::Not(Box::new(p))),
        ]
    })
}

/// What to select. Rendered against the real kinds, like the predicate.
#[derive(Debug, Clone)]
enum Shape {
    /// `SELECT <columns> FROM t0 [WHERE p] [ORDER BY ... LIMIT n]`
    Rows { distinct: bool, limit: Option<usize> },
    /// `SELECT g, COUNT(*), ... FROM t0 [WHERE p] GROUP BY g`
    Grouped { group: usize },
    /// `SELECT ... FROM t0 JOIN t1 ON t0.ci = t1.cj [WHERE p]`
    ///
    /// `on_right` puts the filter on the null-supplying side, which is the
    /// condition `outer_to_inner` fires on -- and the rule whose arms were
    /// swapped once already, deleting every row of a FULL join.
    Joined { left: usize, right: usize, kind: usize, on_right: bool },
    /// `SELECT b.cj, COUNT(*), ... FROM t0 JOIN t1 ... GROUP BY b.cj`
    ///
    /// An aggregate above a join is what `aggregate_pushdown` rewrites, and it
    /// is only correct when the pushed-down grouping cannot change the
    /// multiplicity the join produces.
    GroupedJoin { left: usize, right: usize, kind: usize },
    /// `SELECT ... FROM t0 a WHERE [NOT] EXISTS (SELECT 1 FROM t1 b WHERE ...)`
    ///
    /// The correlated form `decorrelate` turns into a semi or anti join. The
    /// uncorrelated `IN` form is the one whose NULL rules differ.
    Subquery { left: usize, right: usize, negated: bool, in_form: bool },
}

fn shape_strategy() -> impl Strategy<Value = Shape> {
    prop_oneof![
        (any::<bool>(), prop::option::of(0usize..4))
            .prop_map(|(distinct, limit)| Shape::Rows { distinct, limit }),
        (0usize..3).prop_map(|group| Shape::Grouped { group }),
        (0usize..3, 0usize..3, 0usize..4, any::<bool>())
            .prop_map(|(left, right, kind, on_right)| Shape::Joined {
                left,
                right,
                kind,
                on_right,
            }),
        (0usize..3, 0usize..3, 0usize..4)
            .prop_map(|(left, right, kind)| Shape::GroupedJoin { left, right, kind }),
        (0usize..3, 0usize..3, any::<bool>(), any::<bool>()).prop_map(
            |(left, right, negated, in_form)| Shape::Subquery {
                left,
                right,
                negated,
                in_form,
            }
        ),
    ]
}

const JOINS: [&str; 4] = ["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN"];

/// A pair of columns of the same kind, preferring the generated one.
///
/// `None` only when the two tables share no kind at all, which is the one case
/// where there is genuinely no join to write.
fn join_columns(t0: &Table, t1: &Table, left: usize, right: usize) -> Option<(usize, usize)> {
    let l = left % t0.width();
    let r = right % t1.width();
    if t0.kinds[l] == t1.kinds[r] {
        return Some((l, r));
    }
    (0..t0.width())
        .flat_map(|a| (0..t1.width()).map(move |b| (a, b)))
        .find(|(a, b)| t0.kinds[*a] == t1.kinds[*b])
}

/// Build the SQL, or `None` when the shape does not fit these tables.
///
/// Every query is ordered by all of its output columns. Without a total order
/// the two configurations may legitimately return the same rows in different
/// orders, and comparing them as ordered lists would fail on a difference that
/// is not one -- while comparing them as multisets would stop checking that
/// `ORDER BY` and `LIMIT` mean anything.
fn render_sql(t0: &Table, t1: &Table, shape: &Shape, pred: &Pred) -> Option<String> {
    let where_clause = |alias: &str| format!(" WHERE {}", pred.render(t0, alias));

    Some(match shape {
        Shape::Rows { distinct, limit } => {
            let cols: Vec<String> = (0..t0.width()).map(|c| format!("a.c{c}")).collect();
            let order: Vec<String> = (1..=cols.len()).map(|i| i.to_string()).collect();
            format!(
                "SELECT {}{} FROM t0 a{} ORDER BY {}{}",
                if *distinct { "DISTINCT " } else { "" },
                cols.join(", "),
                where_clause("a"),
                order.join(", "),
                match limit {
                    Some(n) => format!(" LIMIT {n}"),
                    None => String::new(),
                }
            )
        }

        Shape::Grouped { group } => {
            let g = group % t0.width();
            // A numeric column for SUM and AVG, or those aggregates are left
            // out rather than generated to fail binding.
            let numeric = (0..t0.width()).find(|c| t0.kinds[*c].numeric());
            let mut aggs = vec!["COUNT(*)".to_string()];
            if let Some(n) = numeric {
                aggs.push(format!("SUM(a.c{n})"));
                aggs.push(format!("AVG(a.c{n})"));
            }
            // MIN and MAX over the grouping column itself are always well
            // typed and always defined.
            aggs.push(format!("MIN(a.c{g})"));
            aggs.push(format!("MAX(a.c{g})"));
            let order: Vec<String> = (1..=aggs.len() + 1).map(|i| i.to_string()).collect();
            format!(
                "SELECT a.c{g}, {} FROM t0 a{} GROUP BY a.c{g} ORDER BY {}",
                aggs.join(", "),
                where_clause("a"),
                order.join(", ")
            )
        }

        Shape::Joined { left, right, kind, on_right } => {
            // Equality across kinds is a bind error, not a bug worth finding,
            // so the generated pair is used when the kinds agree and the first
            // pair that does agree is used otherwise. Skipping instead threw
            // away three joins in four, which is most of what the optimizer
            // has rules about.
            let (l, r) = join_columns(t0, t1, *left, *right)?;
            let cols: Vec<String> = (0..t0.width())
                .map(|c| format!("a.c{c}"))
                .chain((0..t1.width()).map(|c| format!("b.c{c}")))
                .collect();
            let order: Vec<String> = (1..=cols.len()).map(|i| i.to_string()).collect();
            // The predicate is written against `t0`'s kinds either way, so
            // filtering the right side needs `t1`'s. Rendered against whichever
            // table the alias names.
            let filter = if *on_right {
                format!(" WHERE {}", pred.render(t1, "b"))
            } else {
                where_clause("a")
            };
            format!(
                "SELECT {} FROM t0 a {} t1 b ON a.c{l} = b.c{r}{} ORDER BY {}",
                cols.join(", "),
                JOINS[kind % JOINS.len()],
                filter,
                order.join(", ")
            )
        }

        Shape::GroupedJoin { left, right, kind } => {
            let (l, r) = join_columns(t0, t1, *left, *right)?;
            let numeric = (0..t0.width()).find(|c| t0.kinds[*c].numeric());
            let mut aggs = vec!["COUNT(*)".to_string()];
            if let Some(n) = numeric {
                aggs.push(format!("SUM(a.c{n})"));
            }
            aggs.push(format!("MAX(b.c{r})"));
            let order: Vec<String> = (1..=aggs.len() + 1).map(|i| i.to_string()).collect();
            format!(
                "SELECT b.c{r}, {} FROM t0 a {} t1 b ON a.c{l} = b.c{r}{} GROUP BY b.c{r} ORDER BY {}",
                aggs.join(", "),
                JOINS[kind % JOINS.len()],
                where_clause("a"),
                order.join(", ")
            )
        }

        Shape::Subquery { left, right, negated, in_form } => {
            let (l, r) = join_columns(t0, t1, *left, *right)?;
            let cols: Vec<String> = (0..t0.width()).map(|c| format!("a.c{c}")).collect();
            let order: Vec<String> = (1..=cols.len()).map(|i| i.to_string()).collect();
            let not = if *negated { "NOT " } else { "" };
            let inner = if *in_form {
                // Uncorrelated. `NOT IN` with a NULL anywhere in the list is
                // UNKNOWN for every row, which is the trap this shape exists
                // to keep walking into.
                format!("a.c{l} {not}IN (SELECT b.c{r} FROM t1 b)")
            } else {
                format!(
                    "{not}EXISTS (SELECT 1 FROM t1 b WHERE b.c{r} = a.c{l} AND {})",
                    pred.render(t1, "b")
                )
            };
            format!(
                "SELECT {} FROM t0 a WHERE {} ORDER BY {}",
                cols.join(", "),
                inner,
                order.join(", ")
            )
        }
    })
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

/// Run a query and render the result exactly, so two runs can be compared.
///
/// An error is an answer too: if one configuration raises and the other does
/// not, that is as much a semantic difference as a missing row.
fn answer(engine: &Engine, sql: &str, options: &ExecOptions) -> Result<Vec<String>, String> {
    match engine.execute_with(sql, options) {
        Ok(r) => Ok(r
            .rows()
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()),
        Err(d) => Err(d.headline()),
    }
}

fn load(t0: &Table, t1: &Table) -> Engine {
    let mut engine = Engine::new();
    let options = CsvOptions::default();
    engine
        .load("t0", t0.to_csv().into_bytes(), &options)
        .expect("the generated CSV must load");
    engine
        .load("t1", t1.to_csv().into_bytes(), &options)
        .expect("the generated CSV must load");
    engine
}

proptest! {
    // The default is 256, which takes long enough here to be annoying in a
    // pre-commit run. Raise it with PROPTEST_CASES when hunting something.
    //
    // A failure is written to `tests/property.proptest-regressions` and rerun
    // first on every later run, so a bug found once cannot come back unnoticed.
    // The path is explicit because proptest's default looks for `lib.rs` next
    // to the test, which an integration test does not have.
    #![proptest_config(ProptestConfig {
        cases: 192,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/property.proptest-regressions",
        ))),
        ..ProptestConfig::default()
    })]

    /// The whole point: a rewrite may change the plan and must not change the
    /// answer.
    #[test]
    fn the_optimizer_never_changes_the_answer(
        t0 in table_strategy(),
        t1 in table_strategy(),
        shape in shape_strategy(),
        pred in pred_strategy(),
    ) {
        let Some(sql) = render_sql(&t0, &t1, &shape, &pred) else { return Ok(()) };
        let engine = load(&t0, &t1);

        let optimized = answer(&engine, &sql, &ExecOptions::default());
        let plain = answer(&engine, &sql, &ExecOptions::unoptimized());

        prop_assert_eq!(
            &optimized,
            &plain,
            "\n  sql: {}\n  t0:\n{}\n  t1:\n{}",
            sql,
            t0.to_csv(),
            t1.to_csv()
        );
    }

    /// The same claim for the structures that skip data rather than rewrite
    /// the plan. A zone map or a bloom filter that skips a row group must skip
    /// one that truly had nothing in it, and an encoded column answering a
    /// predicate must agree with the decoded one.
    #[test]
    fn skipping_and_encodings_never_change_the_answer(
        t0 in table_strategy(),
        t1 in table_strategy(),
        shape in shape_strategy(),
        pred in pred_strategy(),
    ) {
        let Some(sql) = render_sql(&t0, &t1, &shape, &pred) else { return Ok(()) };
        let engine = load(&t0, &t1);

        let full = answer(&engine, &sql, &ExecOptions::default());
        for (name, options) in [
            ("no-pruning", ExecOptions::without_pruning()),
            ("no-encodings", ExecOptions::without_encodings()),
            ("scalar", ExecOptions::scalar()),
        ] {
            let other = answer(&engine, &sql, &options);
            prop_assert_eq!(
                &full,
                &other,
                "\n  {} disagreed\n  sql: {}\n  t0:\n{}\n  t1:\n{}",
                name,
                sql,
                t0.to_csv(),
                t1.to_csv()
            );
        }
    }
}

/// The generator has to actually generate.
///
/// A property test that silently skips most of its cases passes for the wrong
/// reason, and nothing about `prop_assert_eq!` would say so. This samples the
/// strategies directly and asserts that the queries come out varied, bind, and
/// mostly return rows -- the three ways a generator quietly stops earning its
/// keep.
#[test]
fn the_generator_covers_the_shapes() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let mut rendered = 0;
    let mut bound = 0;
    let mut with_rows = 0;
    let mut joins = 0;
    let mut groups = 0;
    let mut wheres = 0;
    let mut subqueries = 0;

    for _ in 0..400 {
        let t0 = table_strategy().new_tree(&mut runner).unwrap().current();
        let t1 = table_strategy().new_tree(&mut runner).unwrap().current();
        let shape = shape_strategy().new_tree(&mut runner).unwrap().current();
        let pred = pred_strategy().new_tree(&mut runner).unwrap().current();

        let Some(sql) = render_sql(&t0, &t1, &shape, &pred) else { continue };
        rendered += 1;
        if sql.contains(" WHERE ") {
            wheres += 1;
        }
        if sql.contains(" JOIN ") {
            joins += 1;
        }
        if sql.contains("GROUP BY") {
            groups += 1;
        }
        if sql.contains("EXISTS") || sql.contains(" IN (SELECT") {
            subqueries += 1;
        }

        let engine = load(&t0, &t1);
        match engine.execute_with(&sql, &ExecOptions::default()) {
            Ok(r) => {
                bound += 1;
                if r.num_rows() > 0 {
                    with_rows += 1;
                }
            }
            Err(d) => panic!("generated a query that does not bind:\n  {sql}\n  {}", d.headline()),
        }
    }

    println!(
        "generator: {rendered} rendered, {bound} bound, {with_rows} returned rows, \
         {wheres} filtered, {joins} joined, {groups} grouped, {subqueries} with a subquery"
    );
    assert!(rendered > 300, "too many shapes skipped: {rendered}");
    assert_eq!(rendered, bound, "every generated query must bind");
    // Queries that return nothing test something, but a generator producing
    // only those tests almost nothing.
    assert!(with_rows * 2 > rendered, "most queries returned no rows: {with_rows}/{rendered}");
    assert!(wheres > 200, "predicates are not being generated: {wheres}");
    assert!(joins > 20, "joins are not being generated: {joins}");
    assert!(groups > 40, "groups are not being generated: {groups}");
    assert!(subqueries > 20, "subqueries are not being generated: {subqueries}");
}
