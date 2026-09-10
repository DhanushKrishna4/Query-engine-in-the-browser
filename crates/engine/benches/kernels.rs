//! Microbenchmarks for the pieces underneath a query.
//!
//! `qe --bench` answers the question this project is mostly about: how does
//! the whole engine compare against *itself* configured differently --
//! vectorized against scalar, hash join against merge, pruning against none.
//! It reports a ratio, and a ratio is the right shape for that.
//!
//! Criterion answers a different question: how fast is *this one thing*, with
//! a confidence interval, and did it get slower since last time. That is what
//! the pieces below want, because a SQL query never isolates any of them: a
//! CSV parse, a B+ tree probe, a bloom filter, an encoding round trip, a
//! distinct-count sketch. Each is a hot path that a query only ever exercises
//! mixed together with the others.
//!
//!     cargo bench -p engine
//!
//! Sizes are small enough to finish in seconds and large enough to be past the
//! point where allocation dominates.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use engine::parser::ast::BinaryOperator;
use engine::storage::{encoding, BPlusTree, BloomFilter, Bound, ColumnBuilder, CsvOptions};
use engine::types::{DataType, ScalarValue};
use engine::Engine;

/// Deterministic, so two runs measure the same rows rather than similar ones.
fn mulberry32(mut seed: u32) -> impl FnMut() -> u32 {
    move || {
        seed = seed.wrapping_add(0x6d2b_79f5);
        let mut t = seed;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        t ^ (t >> 14)
    }
}

const ROWS: usize = 100_000;

fn taxi_csv(rows: usize) -> Vec<u8> {
    let mut rng = mulberry32(7);
    let mut out = String::from("id,vendor,borough,passengers,distance,fare\n");
    let vendors = ["yellow", "green", "fhv"];
    let boroughs = ["Manhattan", "Brooklyn", "Queens", "Bronx", "Staten Island"];
    for i in 0..rows {
        let r = rng();
        out.push_str(&format!(
            "{i},{},{},{},{}.{},{}.{}\n",
            vendors[(r % 3) as usize],
            boroughs[((r >> 3) % 5) as usize],
            1 + (r >> 6) % 6,
            (r >> 9) % 30,
            (r >> 13) % 100,
            5 + (r >> 17) % 90,
            (r >> 23) % 100,
        ));
    }
    out.into_bytes()
}

/// The CSV reader, including type inference over the whole file.
fn csv(c: &mut Criterion) {
    let bytes = taxi_csv(ROWS);
    let mut group = c.benchmark_group("csv");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("read_csv 100k rows, 6 columns", |b| {
        b.iter(|| {
            engine::storage::read_csv("trips", black_box(&bytes), &CsvOptions::default()).unwrap()
        })
    });
    group.finish();
}

/// Building the tree by repeated insertion, and probing it once it exists.
///
/// Insertion is the interesting half: it is what exercises the splits, and it
/// is several times slower than bulk-loading from sorted input would be.
fn btree(c: &mut Criterion) {
    let mut rng = mulberry32(11);
    let keys: Vec<i32> = (0..ROWS).map(|_| (rng() % 1_000_000) as i32).collect();

    let mut group = c.benchmark_group("btree");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("insert 100k keys", |b| {
        b.iter_batched(
            BPlusTree::new,
            |mut tree| {
                for (row, key) in keys.iter().enumerate() {
                    tree.insert(ScalarValue::Int32(*key), row as u32);
                }
                tree
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();

    let mut tree = BPlusTree::new();
    for (row, key) in keys.iter().enumerate() {
        tree.insert(ScalarValue::Int32(*key), row as u32);
    }
    let mut group = c.benchmark_group("btree/probe");
    group.throughput(Throughput::Elements(1_000));
    group.bench_function("1k point lookups", |b| {
        b.iter(|| {
            for key in keys.iter().take(1_000) {
                black_box(tree.lookup(&ScalarValue::Int32(*key)));
            }
        })
    });
    group.bench_function("range of ~1k keys", |b| {
        b.iter(|| {
            black_box(tree.range(
                &Bound::Included(ScalarValue::Int32(500_000)),
                &Bound::Included(ScalarValue::Int32(510_000)),
            ))
        })
    });
    group.finish();
}

fn int_column(n: usize, spread: u32) -> engine::storage::Column {
    let mut rng = mulberry32(13);
    let mut b = ColumnBuilder::new(&DataType::Int32);
    for _ in 0..n {
        b.append(&ScalarValue::Int32((rng() % spread) as i32)).unwrap();
    }
    b.finish()
}

/// Encoding a column, decoding it back, and asking whether a value can be in
/// it without decoding at all.
fn encodings(c: &mut Criterion) {
    // A narrow range, so bit-packing and frame-of-reference both apply.
    let column = int_column(ROWS, 500);
    let encoded = encoding::encode(&column).expect("this column compresses");

    let mut group = c.benchmark_group("encoding");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("encode 100k int32", |b| {
        b.iter(|| black_box(encoding::encode(black_box(&column))))
    });
    group.bench_function("decode 100k int32", |b| {
        b.iter(|| black_box(encoding::decode(black_box(&encoded), None)))
    });
    group.finish();

    // Not per element: this is the point of `can_match`, which answers from
    // the encoding's own summary rather than from the values.
    c.bench_function("encoding/can_match on an absent value", |b| {
        b.iter(|| {
            black_box(encoding::can_match(
                black_box(&encoded),
                BinaryOperator::Eq,
                &ScalarValue::Int32(999_999),
            ))
        })
    });
}

/// The structure a scan consults after the zone map fails to rule a group out.
fn bloom(c: &mut Criterion) {
    let mut rng = mulberry32(17);
    let values: Vec<ScalarValue> =
        (0..ROWS).map(|_| ScalarValue::Int32(rng() as i32)).collect();

    let mut group = c.benchmark_group("bloom");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("build over 100k values", |b| {
        b.iter(|| {
            let mut f = BloomFilter::with_capacity(ROWS);
            for v in &values {
                f.insert(v);
            }
            f
        })
    });
    group.finish();

    let mut filter = BloomFilter::with_capacity(ROWS);
    for v in &values {
        filter.insert(v);
    }
    let mut group = c.benchmark_group("bloom/probe");
    group.throughput(Throughput::Elements(1_000));
    group.bench_function("1k probes, all absent", |b| {
        b.iter(|| {
            for i in 0..1_000i32 {
                black_box(filter.might_contain(&ScalarValue::Int32(-i - 1)));
            }
        })
    });
    group.finish();
}

/// End to end, but only to pin the two evaluators against each other with
/// confidence intervals -- `qe --bench` reports the ratio, this reports whether
/// either side moved.
fn evaluators(c: &mut Criterion) {
    let mut e = Engine::new();
    e.load("trips", taxi_csv(ROWS), &CsvOptions::default()).unwrap();
    let sql = "SELECT borough, COUNT(*), AVG(fare) FROM trips WHERE distance > 12 GROUP BY borough";

    let mut group = c.benchmark_group("query");
    group.throughput(Throughput::Elements(ROWS as u64));
    for (name, options) in [
        ("vectorized", engine::exec::ExecOptions::default()),
        ("scalar", engine::exec::ExecOptions::scalar()),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| black_box(e.execute_with(black_box(sql), &options).unwrap()))
        });
    }
    group.finish();
}

criterion_group!(benches, csv, btree, encodings, bloom, evaluators);
criterion_main!(benches);
