#!/usr/bin/env python3
"""Generate the Parquet fixtures the reader is tested against.

    python3 tools/gen_parquet.py

pyarrow is the oracle here, exactly as SQLite is the oracle for SQL semantics:
it is a mature, independent implementation of the format, so a file it writes is
the definition of "correct input" and anything this engine reads back must match
what the CSV loader produced from the same data.

Two families of fixture come out of this:

  * mirrors of `data/*.csv`, typed by the same rules the Rust CSV loader uses,
    so a Parquet table and a CSV table of the same data must be indistinguishable
    to every query in the sqllogictest corpus;
  * a matrix of encodings, page versions, compression codecs and types, because
    the encoding a column ends up in is a writer's choice and a reader that only
    handles the common one works right up until it does not.

pyarrow is a *tool* dependency, not a library one -- the engine crate still has
no dependencies at all, and nothing in `crates/` knows this script exists.
"""
import os
import sys

import pyarrow as pa
import pyarrow.parquet as pq

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gen_expected as ge  # noqa: E402

REPO_ROOT = ge.REPO_ROOT
OUT_DIR = os.path.join(REPO_ROOT, "tests", "parquet")


# ---------------------------------------------------------------------------
# CSV mirrors
# ---------------------------------------------------------------------------

def infer_engine_type(values):
    """The Rust loader's inference, at its full resolution.

    `gen_expected.infer_column` deliberately collapses this onto SQLite's four
    storage classes; here the finer distinctions matter, because a Parquet file
    has to declare DATE32 where the CSV loader would have inferred DATE32.
    """
    seen = False
    ok = dict(boolean=True, i32=True, i64=True, f64=True, date=True, ts=True)
    for v in values:
        if v is None:
            continue
        seen = True
        s = v.strip()
        ok["boolean"] &= s.lower() in ge.BOOL_WORDS
        ok["i32"] &= bool(ge.parse_i32(s))
        ok["i64"] &= bool(ge.parse_i64(s))
        ok["f64"] &= bool(ge.parse_f64(s))
        ok["date"] &= bool(ge.parse_date(s))
        ok["ts"] &= bool(ge.parse_timestamp(s))
    if not seen:
        return "utf8"
    for kind in ("boolean", "i32", "i64", "f64", "date", "ts"):
        if ok[kind]:
            return kind
    return "utf8"


ARROW_TYPE = {
    "boolean": pa.bool_(),
    "i32": pa.int32(),
    "i64": pa.int64(),
    "f64": pa.float64(),
    "date": pa.date32(),
    "ts": pa.timestamp("us"),
    "utf8": pa.string(),
}


def coerce(text, kind):
    if text is None:
        return None
    s = text.strip()
    if kind == "boolean":
        return s.lower() in ("true", "t", "yes", "y", "1")
    if kind in ("i32", "i64"):
        return int(s)
    if kind == "f64":
        return float(s)
    if kind == "date":
        import datetime

        return datetime.date.fromisoformat(s)
    if kind == "ts":
        import datetime

        return datetime.datetime.fromisoformat(s.replace("Z", ""))
    return text


def csv_to_table(path):
    """Load a CSV the way the Rust loader does, as an Arrow table.

    Nullability is set the same way too -- a column is nullable only if some
    value in it is actually NULL. Leaving every field nullable (Arrow's default)
    would make the mirror differ from its CSV twin in the schema even though
    every value matched, and the point of these two files is that nothing at
    all distinguishes them.
    """
    records = list(ge.read_csv(path))
    header = [t for t, _ in records[0]]
    rows = records[1:]
    columns, fields = [], []
    for i in range(len(header)):
        # An unquoted empty field is NULL; a quoted empty one is the empty
        # string. That distinction is the whole reason gen_expected reads CSV
        # by hand instead of using the csv module.
        raw = [None if (r[i][0] == "" and not r[i][1]) else r[i][0] for r in rows]
        kind = infer_engine_type(raw)
        values = [coerce(v, kind) for v in raw]
        nullable = any(v is None for v in raw)
        columns.append(pa.array(values, type=ARROW_TYPE[kind]))
        fields.append(pa.field(header[i], ARROW_TYPE[kind], nullable=nullable))
    return pa.table(columns, schema=pa.schema(fields))


# ---------------------------------------------------------------------------
# The encoding / type matrix
# ---------------------------------------------------------------------------

def matrix_table(n=1000):
    """A table built to land in as many different encodings as possible."""
    import datetime

    return pa.table(
        {
            # Ascending, so DELTA_BINARY_PACKED is dramatic on it.
            "id": pa.array(range(n), type=pa.int32()),
            "big": pa.array([i * 1_000_003 for i in range(n)], type=pa.int64()),
            # Few distinct values: the dictionary case.
            "category": pa.array([f"cat-{i % 7}" for i in range(n)], type=pa.string()),
            # Every value distinct: the plain / delta-byte-array case.
            "token": pa.array([f"tok-{i:08d}" for i in range(n)], type=pa.string()),
            "price": pa.array([i * 0.25 for i in range(n)], type=pa.float64()),
            "flag": pa.array([i % 3 == 0 for i in range(n)], type=pa.bool_()),
            # One null in every seventh row, so definition levels are exercised
            # on every page rather than only at the edges.
            "maybe": pa.array(
                [None if i % 7 == 3 else i for i in range(n)], type=pa.int64()
            ),
            "maybe_text": pa.array(
                [None if i % 5 == 2 else f"v{i}" for i in range(n)], type=pa.string()
            ),
            "day": pa.array(
                [datetime.date(2024, 1, 1) + datetime.timedelta(days=i % 365) for i in range(n)],
                type=pa.date32(),
            ),
            "moment": pa.array(
                [
                    datetime.datetime(2024, 1, 1) + datetime.timedelta(seconds=i * 97)
                    for i in range(n)
                ],
                type=pa.timestamp("us"),
            ),
        }
    )


def write(name, table, **kwargs):
    path = os.path.join(OUT_DIR, name)
    pq.write_table(table, path, **kwargs)
    size = os.path.getsize(path)
    meta = pq.ParquetFile(path).metadata
    encs = set()
    for rg in range(meta.num_row_groups):
        for c in range(meta.num_columns):
            encs.update(meta.row_group(rg).column(c).encodings)
    print(
        f"  {name:<34} {size:>7} B  {meta.num_rows:>5} rows  "
        f"{meta.num_row_groups} group(s)  {','.join(sorted(encs))}"
    )


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    print(f"writing fixtures to {OUT_DIR}")

    # -- mirrors of the CSV fixtures ---------------------------------------
    for name in ("people", "orders"):
        t = csv_to_table(os.path.join(REPO_ROOT, "data", f"{name}.csv"))
        write(f"{name}.parquet", t, compression="snappy")

    # The same data with the footer statistics left out. Plenty of writers do
    # this, and a reader that mistakes "no bounds recorded" for "no values"
    # prunes every row group and answers every query with nothing -- silently,
    # which is why it gets a fixture of its own rather than a unit test.
    for name in ("people", "orders"):
        t = csv_to_table(os.path.join(REPO_ROOT, "data", f"{name}.csv"))
        write(f"{name}_no_stats.parquet", t, compression="snappy", write_statistics=False)

    m = matrix_table()

    # -- compression --------------------------------------------------------
    write("uncompressed.parquet", m, compression="none", use_dictionary=False)
    write("snappy.parquet", m, compression="snappy")

    # -- encodings ----------------------------------------------------------
    write("plain.parquet", m, compression="none", use_dictionary=False, version="1.0")
    write("dictionary_v1.parquet", m, compression="none", use_dictionary=True, version="1.0")
    write("dictionary_v2.parquet", m, compression="snappy", use_dictionary=True, version="2.6")
    write(
        "delta.parquet",
        m,
        compression="none",
        use_dictionary=False,
        version="2.6",
        data_page_version="2.0",
        column_encoding={
            "id": "DELTA_BINARY_PACKED",
            "big": "DELTA_BINARY_PACKED",
            "maybe": "DELTA_BINARY_PACKED",
            "token": "DELTA_BYTE_ARRAY",
            "category": "DELTA_LENGTH_BYTE_ARRAY",
            "maybe_text": "DELTA_BYTE_ARRAY",
        },
    )
    write(
        "byte_stream_split.parquet",
        m,
        compression="none",
        use_dictionary=False,
        version="2.6",
        column_encoding={"price": "BYTE_STREAM_SPLIT"},
    )

    # -- page versions ------------------------------------------------------
    write("pages_v1.parquet", m, compression="snappy", data_page_version="1.0")
    write("pages_v2.parquet", m, compression="snappy", data_page_version="2.0")

    # -- structure ----------------------------------------------------------
    write("multi_rowgroup.parquet", m, compression="snappy", row_group_size=128)
    # Small pages force many pages per chunk, so page-boundary handling is
    # exercised rather than assumed. Dictionary encoding is off here on
    # purpose: it would compress these columns into a single page and there
    # would be no boundary left to test.
    # `data_page_size` is only consulted every `write_batch_size` values, so
    # both have to shrink or the whole column still lands in one page.
    write(
        "many_pages.parquet",
        m,
        compression="snappy",
        use_dictionary=False,
        data_page_size=512,
        write_batch_size=64,
    )
    write("empty.parquet", m.slice(0, 0), compression="snappy")

    # -- deprecated but still in the wild ----------------------------------
    ts = pa.table({"moment": m.column("moment")})
    write("int96.parquet", ts, compression="none", use_deprecated_int96_timestamps=True)

    # -- things the reader must refuse by name ------------------------------
    write("zstd.parquet", m, compression="zstd")
    nested = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "tags": pa.array([["a", "b"], ["c"], []], type=pa.list_(pa.string())),
        }
    )
    write("nested.parquet", nested, compression="snappy")

    # The million-row benchmark fixture, if it has been generated. Written
    # outside the repo like its CSV twin: it is a dataset, not a test fixture.
    trips_csv = "/tmp/trips.csv"
    if os.path.exists(trips_csv):
        t = csv_to_table(trips_csv)
        for name, kwargs in [
            ("/tmp/trips.parquet", dict(compression="snappy")),
            ("/tmp/trips-uncompressed.parquet", dict(compression="none")),
        ]:
            pq.write_table(t, name, row_group_size=65536, **kwargs)
            print(f"  {name:<34} {os.path.getsize(name):>9} B  {t.num_rows} rows")
    else:
        print(f"  (skipped {trips_csv}: run tools/gen_sample.py first)")

    print("done")


if __name__ == "__main__":
    main()
