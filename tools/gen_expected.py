#!/usr/bin/env python3
"""Fill in the expected results of a .slt file by running it through SQLite.

This is the other half of the differential testing setup: the engine is checked
against output that a known-correct implementation produced, not against output
someone wrote by hand and convinced themselves was right.

    python3 tools/gen_expected.py --write tests/sqllogictest/*.slt
    python3 tools/gen_expected.py --check tests/sqllogictest/*.slt   # for CI

Records marked `skipif sqlite` or `onlyif qe` are left untouched, which is how
engine-specific behaviour (ILIKE, error messages, division by zero) stays in the
corpus without SQLite needing an opinion about it.

Everything here that parses CSV, infers types or renders a value deliberately
mirrors the Rust side; where the two must agree, the reason is commented in
crates/engine/src/sqllogictest/mod.rs.
"""

import argparse
import datetime
import os
import re
import sqlite3
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

INT_RE = re.compile(r"^[+-]?[0-9]+$")
I32_MIN, I32_MAX = -(2**31), 2**31 - 1
I64_MIN, I64_MAX = -(2**63), 2**63 - 1


# --------------------------------------------------------------------------
# CSV reading -- mirrors crates/engine/src/storage/csv.rs
# --------------------------------------------------------------------------

def read_csv(path):
    """Yield records as lists of (text, was_quoted).

    Hand-rolled rather than using the csv module so that the unquoted-empty
    (NULL) versus quoted-empty (empty string) distinction is handled exactly
    the way the Rust loader handles it.
    """
    with open(path, "rb") as fh:
        src = fh.read()

    i, n = 0, len(src)
    while i < n:
        # Skip blank lines.
        if src[i : i + 1] == b"\n":
            i += 1
            continue
        if src[i : i + 2] == b"\r\n":
            i += 2
            continue

        record, field, quoted = [], bytearray(), False
        while True:
            if i >= n:
                record.append((field.decode("utf-8"), quoted))
                yield record
                break
            c = src[i : i + 1]
            if c == b'"' and not field and not quoted:
                quoted = True
                i += 1
                while True:
                    if src[i : i + 2] == b'""':
                        field += b'"'
                        i += 2
                    elif src[i : i + 1] == b'"':
                        i += 1
                        break
                    elif i >= n:
                        raise SystemExit(f"{path}: unterminated quoted field")
                    else:
                        field += src[i : i + 1]
                        i += 1
                continue
            if c == b",":
                record.append((field.decode("utf-8"), quoted))
                field, quoted = bytearray(), False
                i += 1
                continue
            if c == b"\n":
                record.append((field.decode("utf-8"), quoted))
                i += 1
                yield record
                break
            if src[i : i + 2] == b"\r\n":
                record.append((field.decode("utf-8"), quoted))
                i += 2
                yield record
                break
            field += c
            i += 1


def is_null(text, quoted):
    return not quoted and text.strip() == ""


# --------------------------------------------------------------------------
# Type inference -- mirrors storage/csv.rs TypeInference
# --------------------------------------------------------------------------

def parse_i32(s):
    return INT_RE.match(s) and I32_MIN <= int(s) <= I32_MAX


def parse_i64(s):
    return INT_RE.match(s) and I64_MIN <= int(s) <= I64_MAX


def parse_f64(s):
    try:
        float(s)
        return True
    except ValueError:
        return False


def parse_date(s):
    if len(s) != 10 or s[4] != "-" or s[7] != "-":
        return False
    try:
        datetime.date(int(s[0:4]), int(s[5:7]), int(s[8:10]))
        return True
    except ValueError:
        return False


def parse_timestamp(s):
    s = s.rstrip("Z")
    for sep in (" ", "T"):
        if sep in s:
            date_part, time_part = s.split(sep, 1)
            break
    else:
        return parse_date(s)
    if not parse_date(date_part):
        return False
    hms, _, frac = time_part.partition(".")
    parts = hms.split(":")
    if not 2 <= len(parts) <= 3:
        return False
    try:
        h, m = int(parts[0]), int(parts[1])
        sec = int(parts[2]) if len(parts) == 3 else 0
    except ValueError:
        return False
    if frac and not frac.isdigit():
        return False
    return h <= 23 and m <= 59 and sec <= 59


BOOL_WORDS = {"true", "false", "t", "f", "yes", "no"}


def infer_column(values):
    """Return one of 'bool', 'int', 'real', 'text'.

    Priority mirrors the Rust side: boolean, int32, int64, float64, date,
    timestamp, text. SQLite has no boolean or date type, so booleans collapse
    onto INTEGER and temporal columns onto TEXT -- see the module comment in
    crates/engine/src/sqllogictest/mod.rs for why that keeps results comparable.
    """
    seen = False
    ok = dict(boolean=True, i32=True, i64=True, f64=True, date=True, ts=True)
    for v in values:
        if v is None:
            continue
        seen = True
        s = v.strip()
        ok["boolean"] &= s.lower() in BOOL_WORDS
        ok["i32"] &= bool(parse_i32(s))
        ok["i64"] &= bool(parse_i64(s))
        ok["f64"] &= parse_f64(s)
        ok["date"] &= parse_date(s)
        ok["ts"] &= parse_timestamp(s)
    if not seen:
        return "text"
    if ok["boolean"]:
        return "bool"
    if ok["i32"] or ok["i64"]:
        return "int"
    if ok["f64"]:
        return "real"
    # date / timestamp both land on TEXT in SQLite.
    return "text"


SQLITE_TYPE = {"bool": "INTEGER", "int": "INTEGER", "real": "REAL", "text": "TEXT"}


def convert(value, kind):
    if value is None:
        return None
    if kind == "bool":
        return 1 if value.strip().lower() in ("true", "t", "yes", "1") else 0
    if kind == "int":
        return int(value.strip())
    if kind == "real":
        return float(value.strip())
    return value


def load_table(conn, table, path):
    rows = list(read_csv(path))
    if not rows:
        raise SystemExit(f"{path}: empty CSV")
    header = [t for t, _ in rows[0]]
    body = [[None if is_null(t, q) else t for t, q in r] for r in rows[1:]]

    kinds = [infer_column([r[i] for r in body]) for i in range(len(header))]
    cols = ", ".join(f'"{h}" {SQLITE_TYPE[k]}' for h, k in zip(header, kinds))
    conn.execute(f'DROP TABLE IF EXISTS "{table}"')
    conn.execute(f'CREATE TABLE "{table}" ({cols})')
    placeholders = ", ".join("?" * len(header))
    conn.executemany(
        f'INSERT INTO "{table}" VALUES ({placeholders})',
        [[convert(v, k) for v, k in zip(row, kinds)] for row in body],
    )
    conn.commit()


# --------------------------------------------------------------------------
# Result rendering -- mirrors sqllogictest::normalize
# --------------------------------------------------------------------------

def render(value, letter):
    if value is None:
        return "NULL"
    if letter == "I":
        if isinstance(value, (int, float)):
            return str(int(value))
        try:
            return str(int(float(str(value).strip())))
        except ValueError:
            return "0"
    if letter == "R":
        if isinstance(value, (int, float)):
            return "%.3f" % float(value)
        try:
            return "%.3f" % float(str(value).strip())
        except ValueError:
            return "0.000"
    if isinstance(value, bytes):
        value = value.decode("utf-8", "replace")
    if isinstance(value, float):
        # Match the Rust Display for f64, which prints an integral float with
        # one decimal place. Declaring float columns as R avoids relying on this.
        text = "%.1f" % value if value == int(value) and abs(value) < 1e15 else repr(value)
    else:
        text = str(value)
    if text == "":
        return "(empty)"
    return "".join("@" if ord(c) < 32 or ord(c) == 127 else c for c in text)


def render_results(cursor_rows, types, sort):
    letters = list(types)
    rows = [[render(v, t) for v, t in zip(row, letters)] for row in cursor_rows]
    if sort == "rowsort":
        rows.sort()
    values = [v for row in rows for v in row]
    if sort == "valuesort":
        values.sort()
    return values


# --------------------------------------------------------------------------
# .slt rewriting
# --------------------------------------------------------------------------

def process(path, write):
    with open(path) as fh:
        lines = fh.read().split("\n")

    conn = sqlite3.connect(":memory:")
    # SQLite's LIKE is case-insensitive for ASCII by default, which the SQL
    # standard is not and this engine is not. Without this pragma every LIKE
    # test would disagree for the wrong reason.
    conn.execute("PRAGMA case_sensitive_like = ON")

    out = []
    i = 0
    changed = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            out.append(line)
            i += 1
            continue

        conditions = []
        while stripped.startswith("skipif ") or stripped.startswith("onlyif "):
            conditions.append(stripped)
            out.append(line)
            i += 1
            line = lines[i]
            stripped = line.strip()

        skip = any(c == "skipif sqlite" for c in conditions) or any(
            c.startswith("onlyif ") and c != "onlyif sqlite" for c in conditions
        )

        if stripped == "halt":
            out.append(line)
            break

        if stripped.startswith("load "):
            _, table, csv_path = stripped.split(None, 2)
            load_table(conn, table, os.path.join(REPO_ROOT, csv_path))
            out.append(line)
            i += 1
            continue

        if stripped.startswith("index "):
            # `index <table> <column>` is an engine-only directive. SQLite is
            # the oracle for *answers*, and an index must not change one, so
            # the right thing here is to pass it through untouched and let the
            # comparison prove that.
            out.append(line)
            i += 1
            continue

        if stripped.startswith("statement "):
            # Statements are left alone: this engine has no DDL, so the only
            # ones in the corpus are error expectations, whose messages are
            # engine-specific by definition.
            out.append(line)
            i += 1
            while i < len(lines) and lines[i].strip():
                out.append(lines[i])
                i += 1
            continue

        if stripped.startswith("query "):
            parts = stripped.split()
            types = parts[1]
            sort = parts[2] if len(parts) > 2 else "nosort"
            out.append(line)
            i += 1
            sql = []
            while i < len(lines) and lines[i].strip() and lines[i].strip() != "----":
                sql.append(lines[i])
                out.append(lines[i])
                i += 1
            if i >= len(lines) or lines[i].strip() != "----":
                raise SystemExit(f"{path}: query near line {i} has no `----`")
            out.append("----")
            i += 1
            old = []
            while i < len(lines) and lines[i].strip():
                old.append(lines[i])
                i += 1

            if skip:
                out.extend(old)
                continue

            statement = "\n".join(sql)
            try:
                rows = conn.execute(statement).fetchall()
            except sqlite3.Error as e:
                raise SystemExit(f"{path}: sqlite rejected `{statement}`: {e}")
            new = render_results(rows, types, sort)
            if new != old:
                changed += 1
                if not write:
                    print(f"{path}: would update results for:\n    {statement}")
                    print(f"      expected in file: {old}")
                    print(f"      sqlite says:      {new}")
            out.extend(new)
            continue

        raise SystemExit(f"{path}: unknown record at line {i + 1}: {stripped}")

    if write and changed:
        with open(path, "w") as fh:
            fh.write("\n".join(out))
        print(f"{path}: updated {changed} result block(s)")
    elif not changed:
        print(f"{path}: up to date")
    return changed


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("files", nargs="+")
    group = ap.add_mutually_exclusive_group(required=True)
    group.add_argument("--write", action="store_true", help="rewrite expected blocks in place")
    group.add_argument("--check", action="store_true", help="exit non-zero if anything differs")
    args = ap.parse_args()

    changed = sum(process(p, args.write) for p in args.files)
    if args.check and changed:
        print(f"\n{changed} result block(s) disagree with sqlite", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
