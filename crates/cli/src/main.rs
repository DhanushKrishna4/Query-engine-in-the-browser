//! Native REPL for the query engine.
//!
//! The CLI is the primary development surface: every stage of the pipeline is
//! reachable here before any of it is compiled to wasm. `.tokens`, `.ast` and
//! `.plan` print exactly what the browser UI's corresponding tabs will render.
//!
//! Usage:
//!   qe [--load name=file.csv]... [-c "SQL"]
//!
//! With no `-c`, it starts a REPL. Type `.help` for commands.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use engine::error::Diagnostic;
use engine::exec::{self, ExecOptions};
use engine::lexer;
use engine::parser::ast;
use engine::plan;
use engine::sqllogictest;
use engine::storage::CsvOptions;
use engine::types::{DataType, ScalarValue};
use engine::{Engine, QueryResult};

struct Repl {
    engine: Engine,
    show_stats: bool,
    show_timing: bool,
}

fn main() -> ExitCode {
    let mut repl = Repl {
        engine: Engine::new(),
        show_stats: false,
        show_timing: true,
    };

    let mut command: Option<String> = None;
    let mut slt_files: Vec<String> = Vec::new();
    let mut bench_file: Option<String> = None;
    let mut bench_runs = 5usize;
    let mut compact_threshold: Option<f64> = None;
    let mut bench_baseline = String::from("scalar");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--slt" => match args.next() {
                Some(path) => slt_files.push(path),
                None => {
                    eprintln!("--slt needs a file path");
                    return ExitCode::FAILURE;
                }
            },
            "--load" => {
                let Some(spec) = args.next() else {
                    eprintln!("--load needs an argument of the form name=file.csv");
                    return ExitCode::FAILURE;
                };
                if let Err(e) = load_spec(&mut repl.engine, &spec) {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
            "--index" => {
                let Some(spec) = args.next() else {
                    eprintln!("--index needs an argument of the form table.column");
                    return ExitCode::FAILURE;
                };
                let Some((table, column)) = spec.split_once('.') else {
                    eprintln!("--index needs an argument of the form table.column");
                    return ExitCode::FAILURE;
                };
                match repl.engine.create_index(table, column) {
                    Ok(idx) => eprintln!(
                        "indexed {}.{}: {} keys, {} levels, {} in {:.0}ms",
                        idx.table,
                        idx.column_name,
                        idx.tree.num_keys(),
                        idx.tree.height(),
                        human_bytes(idx.tree.byte_size()),
                        idx.build_nanos as f64 / 1e6,
                    ),
                    Err(d) => {
                        eprintln!("error: {}", d.headline());
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--bench" => match args.next() {
                Some(path) => bench_file = Some(path),
                None => {
                    eprintln!("--bench needs a file of SQL statements");
                    return ExitCode::FAILURE;
                }
            },
            "--compact-threshold" => match args.next().and_then(|v| v.parse().ok()) {
                Some(t) => compact_threshold = Some(t),
                None => {
                    eprintln!("--compact-threshold needs a number between 0 and 1");
                    return ExitCode::FAILURE;
                }
            },
            "--bench-baseline" => match args.next() {
                Some(v)
                    if v == "scalar"
                        || v == "nested-loop"
                        || v == "unoptimized"
                        || v == "no-pruning"
                        || v == "no-bloom"
                        || v == "no-index"
                        || v == "no-reorder"
                        || v == "no-decorrelation"
                        || v == "no-top-n" =>
                {
                    bench_baseline = v
                }
                _ => {
                    eprintln!(
                        "--bench-baseline must be one of: scalar, nested-loop, unoptimized, no-pruning, no-bloom, no-index, no-reorder, no-decorrelation, no-top-n"
                    );
                    return ExitCode::FAILURE;
                }
            },
            "--bench-runs" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => bench_runs = n,
                None => {
                    eprintln!("--bench-runs needs a positive integer");
                    return ExitCode::FAILURE;
                }
            },
            "-c" | "--command" => match args.next() {
                Some(sql) => command = Some(sql),
                None => {
                    eprintln!("-c needs a SQL string");
                    return ExitCode::FAILURE;
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument `{other}`\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    if !slt_files.is_empty() {
        return run_slt_files(&slt_files);
    }

    if let Some(path) = bench_file {
        return run_benchmark(
            &repl.engine,
            &path,
            bench_runs,
            compact_threshold,
            &bench_baseline,
        );
    }

    if let Some(sql) = command {
        return match repl.dispatch(&sql) {
            Ok(()) => ExitCode::SUCCESS,
            Err(()) => ExitCode::FAILURE,
        };
    }

    repl.run()
}

const USAGE: &str = "\
qe -- native REPL for the query engine

usage:
  qe [--load <name>=<file.csv>]... [-c \"<SQL>\"]
  qe --slt <file.slt>...

options:
  --load name=file.csv   load a CSV file as a table before starting
  -c, --command SQL      run one statement and exit
  --slt file.slt         run a sqllogictest file and report failures
  --bench file.sql       time each statement under both evaluators
  --bench-runs N         repetitions per query (default 5)
  --bench-baseline B     compare against `scalar` (default), `nested-loop`,
                         `unoptimized`, `no-pruning`, `no-bloom`, `no-index`,
                         `no-reorder`, `no-decorrelation` or `no-top-n`
  --index table.column   build a B+ tree index before running
  --compact-threshold F  filter compaction threshold, 0 disables (default 0.2)
  -h, --help             show this message";

const HELP: &str = "\
commands:
  .help                  this message
  .tables                list loaded tables
  .schema [table]        column names and types
  .rowgroups <table>     row groups, zone maps and bloom filters
  .index <table> <col>   build a B+ tree index on a column
  .indexes [table]       indexes and their shape
  .load <name> <file>    load a CSV or Parquet file as a table
  .tokens <sql>          lexer output
  .ast <sql>             parse tree
  .bound <sql>           logical plan as the binder produced it
  .plan <sql>            optimized logical plan, with resolved types
  .trace <sql>           every optimizer rewrite, step by step
  .stats on|off          show per-operator counters after each query
  .timing on|off         show query wall time (default on)
  .quit                  exit

anything else is run as SQL. a statement ends at `;` -- until then input keeps
accumulating across lines. a blank line discards a partial statement.";

impl Repl {
    fn run(&mut self) -> ExitCode {
        let stdin = io::stdin();
        let interactive = stdin.is_terminal();
        if interactive {
            println!("query engine (SELECT / FROM / WHERE / LIMIT). `.help` for commands.");
        }

        // Statements are terminated by `;`, as in sqlite3. Guessing at
        // completeness by trying to parse does not work here, because a prefix
        // of a real query is often itself a valid statement -- `SELECT name` is
        // a complete FROM-less SELECT, so the first line of a multi-line query
        // would run on its own. End-of-input also flushes whatever is pending,
        // so a piped one-liner needs no semicolon.
        let mut buffer = String::new();
        let mut failed = false;

        loop {
            if interactive {
                print!("{}", if buffer.is_empty() { "qe> " } else { "..> " });
                let _ = io::stdout().flush();
            }
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("error reading input: {e}");
                    return ExitCode::FAILURE;
                }
            }

            let trimmed = line.trim();
            if buffer.is_empty() {
                if trimmed.is_empty() || trimmed.starts_with("--") {
                    continue;
                }
                if trimmed == ".quit" || trimmed == ".exit" {
                    break;
                }
                if trimmed.starts_with('.') {
                    if self.dispatch(trimmed).is_err() {
                        failed = true;
                    }
                    continue;
                }
            } else if trimmed.is_empty() {
                buffer.clear();
                continue;
            }

            if !buffer.is_empty() {
                buffer.push('\n');
            }
            buffer.push_str(line.trim_end());

            if !buffer.trim_end().ends_with(';') {
                continue;
            }
            let sql = std::mem::take(&mut buffer);
            if self.run_sql(&sql).is_err() {
                failed = true;
            }
        }

        // Flush a trailing statement that was never terminated.
        if !buffer.trim().is_empty() {
            let sql = std::mem::take(&mut buffer);
            if self.run_sql(&sql).is_err() {
                failed = true;
            }
        }
        if failed && !interactive {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }

    /// Run one line: a dot-command or a SQL statement.
    fn dispatch(&mut self, input: &str) -> Result<(), ()> {
        let input = input.trim();
        if !input.starts_with('.') {
            return self.run_sql(input);
        }
        let (cmd, rest) = match input.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (input, ""),
        };

        match cmd {
            ".help" => println!("{HELP}"),
            ".quit" | ".exit" => {}
            ".tables" => self.cmd_tables(),
            ".schema" => self.cmd_schema(rest),
            ".rowgroups" => return self.cmd_rowgroups(rest),
            ".index" => return self.cmd_index(rest),
            ".indexes" => return self.cmd_indexes(rest),
            ".load" => return self.cmd_load(rest),
            ".tokens" => return self.cmd_tokens(rest),
            ".ast" => return self.cmd_ast(rest),
            ".bound" => return self.cmd_bound(rest),
            ".plan" => return self.cmd_plan(rest),
            ".trace" => return self.cmd_trace(rest),
            ".stats" => self.show_stats = parse_toggle(rest, self.show_stats),
            ".timing" => self.show_timing = parse_toggle(rest, self.show_timing),
            other => {
                eprintln!("unknown command `{other}`; try `.help`");
                return Err(());
            }
        }
        Ok(())
    }

    fn run_sql(&mut self, sql: &str) -> Result<(), ()> {
        match self.engine.execute(sql) {
            Ok(result) => {
                print!("{}", format_result(&result));
                if self.show_timing {
                    println!(
                        "{} row{} in {:.3} ms",
                        result.num_rows(),
                        if result.num_rows() == 1 { "" } else { "s" },
                        result.elapsed_ms()
                    );
                }
                if self.show_stats {
                    println!("\n{}", exec::explain_stats(&result.stats));
                }
                Ok(())
            }
            Err(d) => {
                report(&d, sql);
                Err(())
            }
        }
    }

    fn cmd_tables(&self) {
        let tables = self.engine.catalog().tables();
        if tables.is_empty() {
            println!("(no tables loaded -- try `.load people people.csv`)");
            return;
        }
        for t in tables {
            println!(
                "{:<20} {:>10} rows  {:>4} row groups  {:>10}",
                t.name,
                t.num_rows(),
                t.num_row_groups(),
                human_bytes(t.byte_size())
            );
        }
    }

    fn cmd_schema(&self, name: &str) {
        let tables = if name.is_empty() {
            self.engine.catalog().tables()
        } else {
            match self.engine.catalog().get(name, false) {
                Some(t) => vec![t],
                None => {
                    eprintln!("no such table `{name}`");
                    return;
                }
            }
        };
        for t in tables {
            println!("{} ({} rows)", t.name, t.num_rows());
            for f in &t.schema.fields {
                println!(
                    "  {:<24} {:<16} {}",
                    f.name,
                    f.data_type.to_string(),
                    if f.nullable { "NULL" } else { "NOT NULL" }
                );
            }
        }
    }

    /// The precursor to the browser's storage inspector: what the zone maps
    /// actually contain, per row group per column.
    fn cmd_rowgroups(&self, name: &str) -> Result<(), ()> {
        let Some(t) = self.engine.catalog().get(name, false) else {
            eprintln!("no such table `{name}`");
            return Err(());
        };
        println!(
            "{}: {} row group(s), {} rows, {}",
            t.name,
            t.num_row_groups(),
            t.num_rows(),
            human_bytes(t.byte_size())
        );
        for (i, rg) in t.row_groups.iter().enumerate() {
            let residency = if rg.is_pending() {
                format!(
                    ", {}/{} column(s) decoded",
                    rg.resident_columns(),
                    t.schema.len()
                )
            } else {
                String::new()
            };
            println!(
                "  row group {i}: {} rows, {}{residency}",
                rg.num_rows,
                human_bytes(rg.byte_size())
            );
            for (f, s) in t.schema.fields.iter().zip(&rg.stats) {
                let show = |v: &Option<ScalarValue>| match v {
                    Some(x) => x.to_string(),
                    None => "-".to_string(),
                };
                println!(
                    "    {:<20} {:<12} min={:<20} max={:<20} nulls={:<6} distinct~{}",
                    f.name,
                    f.data_type.to_string(),
                    show(&s.min),
                    show(&s.max),
                    s.null_count,
                    s.distinct_count_estimate
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| ">8192".into())
                );
            }
            let filtered: Vec<&str> = t
                .schema
                .fields
                .iter()
                .zip(&rg.blooms)
                .filter(|(_, b)| b.is_some())
                .map(|(f, _)| f.name.as_str())
                .collect();
            if !filtered.is_empty() {
                println!(
                    "    bloom filters ({}): {}",
                    human_bytes(rg.bloom_bytes()),
                    filtered.join(", ")
                );
            }
        }
        Ok(())
    }

    fn cmd_index(&mut self, rest: &str) -> Result<(), ()> {
        let mut parts = rest.split_whitespace();
        let (Some(table), Some(column)) = (parts.next(), parts.next()) else {
            eprintln!("usage: .index <table> <column>");
            return Err(());
        };
        match self.engine.create_index(table, column) {
            Ok(idx) => {
                println!(
                    "built an index on {}.{}: {} keys over {} rows, {} levels, {} nodes, {} in {:.0}ms",
                    idx.table,
                    idx.column_name,
                    idx.tree.num_keys(),
                    idx.tree.num_entries(),
                    idx.tree.height(),
                    idx.tree.num_nodes(),
                    human_bytes(idx.tree.byte_size()),
                    idx.build_nanos as f64 / 1e6,
                );
                Ok(())
            }
            Err(d) => {
                eprintln!("{}", d.headline());
                Err(())
            }
        }
    }

    fn cmd_indexes(&self, rest: &str) -> Result<(), ()> {
        let tables = if rest.trim().is_empty() {
            self.engine.catalog().tables()
        } else {
            match self.engine.catalog().get(rest.trim(), false) {
                Some(t) => vec![t],
                None => {
                    eprintln!("no such table `{}`", rest.trim());
                    return Err(());
                }
            }
        };
        let mut any = false;
        for t in tables {
            for idx in self.engine.catalog().indexes_on(&t.name) {
                any = true;
                println!(
                    "{}.{}  {} keys / {} rows, {} levels, widths {:?}, {}",
                    idx.table,
                    idx.column_name,
                    idx.tree.num_keys(),
                    idx.tree.num_entries(),
                    idx.tree.height(),
                    idx.tree.level_widths(),
                    human_bytes(idx.tree.byte_size()),
                );
            }
        }
        if !any {
            println!("no indexes. build one with `.index <table> <column>`");
        }
        Ok(())
    }

    fn cmd_load(&mut self, rest: &str) -> Result<(), ()> {
        let mut parts = rest.split_whitespace();
        let (Some(name), Some(path)) = (parts.next(), parts.next()) else {
            eprintln!("usage: .load <name> <file.csv|file.parquet>");
            return Err(());
        };
        match load_file(&mut self.engine, name, Path::new(path)) {
            Ok(msg) => {
                println!("{msg}");
                Ok(())
            }
            Err(e) => {
                eprintln!("error: {e}");
                Err(())
            }
        }
    }

    fn cmd_tokens(&self, sql: &str) -> Result<(), ()> {
        match self.engine.tokenize(sql) {
            Ok(tokens) => {
                print!("{}", lexer::format_tokens(&tokens));
                Ok(())
            }
            Err(d) => {
                report(&d, sql);
                Err(())
            }
        }
    }

    fn cmd_ast(&self, sql: &str) -> Result<(), ()> {
        match self.engine.parse(sql) {
            Ok(stmt) => {
                print!("{}", ast::pretty(&stmt));
                Ok(())
            }
            Err(d) => {
                report(&d, sql);
                Err(())
            }
        }
    }

    fn cmd_bound(&self, sql: &str) -> Result<(), ()> {
        match self.engine.bound_plan(sql) {
            Ok(p) => {
                print!("{}", plan::explain(&p, true));
                Ok(())
            }
            Err(d) => {
                report(&d, sql);
                Err(())
            }
        }
    }

    /// The optimizer trace: one complete plan per rule application. This is
    /// what the browser UI's trace slider steps through.
    fn cmd_trace(&self, sql: &str) -> Result<(), ()> {
        match self.engine.optimizer_trace(sql) {
            Ok(t) => {
                print!("{}", t.render(false));
                Ok(())
            }
            Err(d) => {
                report(&d, sql);
                Err(())
            }
        }
    }

    fn cmd_plan(&self, sql: &str) -> Result<(), ()> {
        match self.engine.plan(sql) {
            Ok(p) => {
                print!("{}", plan::explain(&p, true));
                Ok(())
            }
            Err(d) => {
                report(&d, sql);
                Err(())
            }
        }
    }
}

fn parse_toggle(arg: &str, current: bool) -> bool {
    match arg {
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        "" => !current,
        other => {
            eprintln!("expected `on` or `off`, got `{other}`");
            current
        }
    }
}

fn load_spec(engine: &mut Engine, spec: &str) -> Result<(), String> {
    let Some((name, path)) = spec.split_once('=') else {
        return Err(format!("expected name=file.csv, got `{spec}`"));
    };
    let msg = load_file(engine, name, Path::new(path))?;
    println!("{msg}");
    Ok(())
}

fn load_file(engine: &mut Engine, name: &str, path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let table = engine
        .load(name, bytes, &CsvOptions::default())
        .map_err(|d| d.headline())?;
    let groups = table.num_row_groups();
    Ok(format!(
        "loaded `{}`: {} rows, {} columns, {groups} row group{}, {}{}",
        table.name,
        table.num_rows(),
        table.schema.len(),
        if groups == 1 { "" } else { "s" },
        human_bytes(table.byte_size()),
        // A Parquet table has read nothing but its footer at this point, and
        // saying so is more honest than reporting a size that will grow.
        if table.is_pending() { " compressed, columns not yet decoded" } else { "" },
    ))
}

/// Run one or more sqllogictest files, printing a failure report.
///
/// The same harness `cargo test` uses, exposed here so a single file can be
/// iterated on without a full test run. `load` paths inside a file are
/// resolved relative to the current directory.
fn run_slt_files(paths: &[String]) -> ExitCode {
    let mut total = sqllogictest::RunReport::default();

    for path in paths {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: cannot read {path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let mut resolve =
            |rel: &str| -> Result<Vec<u8>, String> { std::fs::read(rel).map_err(|e| e.to_string()) };

        match sqllogictest::run(&text, &mut resolve) {
            Ok(report) => {
                for f in &report.failures {
                    print!("{}", sqllogictest::format_failure(path, f));
                    println!();
                }
                println!(
                    "{path}: {} passed, {} skipped, {} failed",
                    report.passed,
                    report.skipped,
                    report.failures.len()
                );
                total.merge(report);
            }
            Err(e) => {
                eprintln!("error: {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if paths.len() > 1 {
        println!(
            "\ntotal: {} passed, {} skipped, {} failed",
            total.passed,
            total.skipped,
            total.failures.len()
        );
    }
    if total.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Time every statement in a file under both evaluators.
///
/// Reporting the minimum rather than the mean is deliberate: the minimum is the
/// time the machine is capable of, and everything above it is interference from
/// something else on the system. For comparing two implementations of the same
/// work, that is the number that means something.
fn run_benchmark(
    engine: &Engine,
    path: &str,
    runs: usize,
    compact_threshold: Option<f64>,
    baseline_name: &str,
) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let queries: Vec<String> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .split(';')
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
        .collect();

    if queries.is_empty() {
        eprintln!("error: {path} contains no statements");
        return ExitCode::FAILURE;
    }

    let mut fast = ExecOptions::default();
    let mut baseline = match baseline_name {
        "nested-loop" => ExecOptions::nested_loop_joins(),
        "unoptimized" => ExecOptions::unoptimized(),
        "no-pruning" => ExecOptions::without_pruning(),
        "no-bloom" => ExecOptions::without_bloom_filters(),
        "no-index" => ExecOptions::without_index_scans(),
        "no-reorder" => ExecOptions::without_join_reorder(),
        "no-decorrelation" => ExecOptions::without_decorrelation(),
        "no-top-n" => ExecOptions::without_top_n(),
        _ => ExecOptions::scalar(),
    };
    if let Some(t) = compact_threshold {
        fast.compact_threshold = t;
        baseline.compact_threshold = t;
    }
    let configs = [("fast", fast), ("baseline", baseline)];

    println!(
        "{:<46} {:>12} {:>12} {:>9} {:>10}",
        "query", "engine", baseline_name, "speedup", "rows"
    );
    println!("{}", "-".repeat(93));

    let mut failed = false;
    let mut totals = [0.0f64; 2];

    for sql in &queries {
        let mut best = [f64::INFINITY; 2];
        let mut rows = 0usize;

        for (i, (_, options)) in configs.iter().enumerate() {
            // One untimed run so that any first-touch cost is not attributed
            // to whichever evaluator happens to go first.
            match engine.execute_with(sql, options) {
                Ok(r) => rows = r.num_rows(),
                Err(d) => {
                    println!("{:<46} error: {}", truncate(sql, 46), d.headline());
                    failed = true;
                    break;
                }
            }
            for _ in 0..runs {
                match engine.execute_with(sql, options) {
                    Ok(r) => best[i] = best[i].min(r.elapsed_ms()),
                    Err(d) => {
                        println!("{:<46} error: {}", truncate(sql, 46), d.headline());
                        failed = true;
                        break;
                    }
                }
            }
        }

        if best.iter().any(|v| !v.is_finite()) {
            continue;
        }
        totals[0] += best[0];
        totals[1] += best[1];
        println!(
            "{:<46} {:>9.2} ms {:>9.2} ms {:>8.1}x {:>10}",
            truncate(sql, 46),
            best[0],
            best[1],
            best[1] / best[0],
            rows
        );
    }

    println!("{}", "-".repeat(93));
    println!(
        "{:<46} {:>9.2} ms {:>9.2} ms {:>8.1}x",
        "total",
        totals[0],
        totals[1],
        totals[1] / totals[0]
    );

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn truncate(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        let mut out: String = flat.chars().take(n - 1).collect();
        out.push('\u{2026}');
        out
    }
}

fn report(d: &Diagnostic, sql: &str) {
    eprint!("{}", d.render(sql));
}

// ---------------------------------------------------------------------------
// Result rendering
// ---------------------------------------------------------------------------

fn format_result(result: &QueryResult) -> String {
    let headers: Vec<String> = result.schema.names().map(|s| s.to_string()).collect();
    if headers.is_empty() {
        return String::new();
    }
    let rows: Vec<Vec<String>> = result
        .rows()
        .into_iter()
        .map(|r| r.iter().map(|v| v.to_string()).collect())
        .collect();

    // Numbers read better right-aligned; text and dates left-aligned.
    let right: Vec<bool> = result
        .schema
        .fields
        .iter()
        .map(|f| f.data_type.is_numeric() || f.data_type == DataType::Null)
        .collect();

    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    let pad = |s: &str, w: usize, right: bool| {
        let fill = w.saturating_sub(s.chars().count());
        if right {
            format!("{}{}", " ".repeat(fill), s)
        } else {
            format!("{}{}", s, " ".repeat(fill))
        }
    };

    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| pad(h, widths[i], right[i]))
        .collect();
    out.push_str(&header_line.join(" | "));
    out.push('\n');

    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    out.push_str(&rule.join("-+-"));
    out.push('\n');

    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, widths[i], right[i]))
            .collect();
        out.push_str(cells.join(" | ").trim_end());
        out.push('\n');
    }
    out
}

fn human_bytes(n: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
