#!/usr/bin/env python3
"""Generate a randomized differential corpus and answer it with SQLite.

Hand-written tests only cover the cases you thought of. This walks the fixture
schemas and emits several hundred type-correct queries built from the supported
grammar, then records what SQLite says each one returns. Any disagreement the
engine has with those answers is a real semantic bug.

    python3 tools/fuzz_queries.py --seed 1 --count 400 \
        > tests/sqllogictest/generated.slt

Generation is type-directed, so every query is one both engines will accept:
WHERE always gets a boolean, CASE branches always agree on a type, and literals
are sampled from values that actually occur in the data so predicates select
something. A few constructs are deliberately excluded because the two engines
genuinely disagree there and the difference is a design decision, not a bug --
see KNOWN_DIVERGENCES below.
"""

import argparse
import random
import sys

import gen_expected as ge

# Constructs kept out of the generated corpus, and why:
#
#   x / 0            SQLite returns NULL; this engine raises, per the standard.
#   CAST('abc' AS INT)
#                    SQLite returns 0; this engine raises rather than inventing
#                    a value. Only numeric->numeric and ->TEXT casts are emitted.
#   id IN (1, '2')   SQLite compares across storage classes and never matches;
#                    this engine folds the string literal into the column's type.
#                    Only type-homogeneous lists are emitted.
#   huge integers    SQLite silently promotes an overflowing integer to float;
#                    this engine raises. Magnitudes are kept small.
#   SUM(bool_col)    SQLite has no BOOLEAN and happily sums the underlying 0/1;
#                    this engine rejects SUM over a non-numeric type.
#   ungrouped column SQLite allows a non-aggregated column outside GROUP BY and
#                    picks an arbitrary row for it; this engine rejects it, as
#                    the standard requires.
#   LIMIT over groups
#                    Which groups survive a LIMIT with no ORDER BY is not
#                    defined, and the two engines emit groups in different
#                    orders. Aggregate queries are generated without LIMIT.
#   correlated NOT IN
#                    A NULL on either side makes NOT IN unknown rather than
#                    false, so it is only an anti-join when neither side is
#                    nullable -- and a correlated one that is not rewritable has
#                    nowhere to run. Only uncorrelated NOT IN is generated.
#   multi-row scalar subquery
#                    SQLite returns the first row; this engine raises, as the
#                    standard requires. Scalar subqueries are generated as
#                    aggregates, which always return exactly one row.
KNOWN_DIVERGENCES = True

# Join keys that actually relate the fixtures, so generated joins return rows
# rather than nothing.
JOIN_KEYS = [
    ("people", "id", "orders", "person_id"),
    ("orders", "person_id", "people", "id"),
]

JOIN_TYPES = ["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN", "INNER JOIN"]

FIXTURES = [
    ("people", "data/people.csv"),
    ("orders", "data/orders.csv"),
]


class Column:
    def __init__(self, name, kind, values):
        self.name = name
        self.kind = kind  # int | real | text | bool | date
        self.values = values  # distinct non-NULL values, as source text
        self.nullable = False

    def __repr__(self):
        return f"<{self.name}:{self.kind}>"


def detailed_kind(values):
    """Like gen_expected.infer_column, but keeps `date` and `bool` distinct so
    that literals can be sampled from the right domain."""
    base = ge.infer_column(values)
    if base != "text":
        return base
    non_null = [v.strip() for v in values if v is not None]
    if non_null and all(ge.parse_date(v) for v in non_null):
        return "date"
    return "text"


def letter_for(kind):
    if kind == "real":
        return "R"
    if kind in ("int", "bool"):
        return "I"
    return "T"


def prefixed(columns, alias):
    """The same columns, addressed through a table alias."""
    out = []
    for c in columns:
        p = Column(f"{alias}.{c.name}", c.kind, c.values)
        p.nullable = c.nullable
        out.append(p)
    return out


def load_schema(path):
    rows = list(ge.read_csv(path))
    header = [t for t, _ in rows[0]]
    body = [[None if ge.is_null(t, q) else t for t, q in r] for r in rows[1:]]
    cols = []
    for i, name in enumerate(header):
        raw = [r[i] for r in body]
        col = Column(name, detailed_kind(raw), sorted({v for v in raw if v is not None}))
        col.nullable = any(v is None for v in raw)
        cols.append(col)
    return cols


def quote(s):
    return "'" + s.replace("'", "''") + "'"


class Gen:
    def __init__(self, rng, table, columns, outer_alias=None):
        self.rng = rng
        self.table = table
        self.columns = columns
        # The alias the enclosing query's columns can be reached through, which
        # is what makes a *correlated* subquery possible.
        self.outer_alias = outer_alias
        self.schemas = None
        # Correlated subqueries are only emitted into a WHERE clause: that is
        # the only place this engine can turn one into a join, and anywhere else
        # it is a documented error rather than a difference worth fuzzing.
        self.allow_correlated = False

    def cols(self, *kinds):
        out = [c for c in self.columns if c.kind in kinds]
        return out

    def pick(self, seq):
        return self.rng.choice(seq)

    # -- literals ----------------------------------------------------------

    def numeric_literal(self, col):
        """Sample from the column's own values so predicates land on real
        boundaries, with a little perturbation to catch off-by-one behaviour."""
        base = float(self.pick(col.values))
        choice = self.rng.random()
        if choice < 0.3:
            value = base
        elif choice < 0.6:
            value = base + self.rng.choice([-1, 1])
        else:
            value = base * self.rng.choice([0.5, 1.5])
        if col.kind == "int" and self.rng.random() < 0.7:
            return str(int(value))
        return f"{value:.2f}"

    def text_literal(self, col):
        return quote(self.pick(col.values))

    def like_pattern(self, col):
        s = self.pick(col.values)
        style = self.rng.randrange(5)
        if style == 0:
            return quote(s[:2] + "%")
        if style == 1:
            return quote("%" + s[-2:])
        if style == 2:
            return quote("%" + s[len(s) // 2 : len(s) // 2 + 2] + "%")
        if style == 3:
            return quote("_" * min(3, len(s)) + "%")
        return quote(s)

    # -- expressions -------------------------------------------------------

    def num_expr(self, depth):
        """Returns (sql, is_real)."""
        numeric = self.cols("int", "real")
        if depth <= 0 or not numeric or self.rng.random() < 0.35:
            if numeric and self.rng.random() < 0.75:
                col = self.pick(numeric)
                return col.name, col.kind == "real"
            return str(self.rng.randrange(1, 50)), False

        form = self.rng.randrange(6)
        if form <= 2:
            left, lr = self.num_expr(depth - 1)
            right, rr = self.num_expr(depth - 1)
            op = self.pick(["+", "-", "*"])
            return f"({left} {op} {right})", lr or rr
        if form == 3:
            # Division only by a nonzero constant: SQLite answers NULL on
            # division by zero where this engine raises.
            left, lr = self.num_expr(depth - 1)
            divisor = self.rng.randrange(2, 9)
            return f"({left} / {divisor})", lr
        if form == 4:
            cond = self.bool_expr(depth - 1)
            a, ar = self.num_expr(depth - 1)
            b, br = self.num_expr(depth - 1)
            # Standard SQL unifies CASE branch types, so an INT branch beside a
            # REAL one makes the whole CASE REAL; SQLite, being dynamically
            # typed, hands back whichever branch fired. Emitting an explicit
            # CAST sidesteps a divergence that is a design difference, not a bug.
            if ar != br:
                if not ar:
                    a = f"CAST({a} AS FLOAT)"
                if not br:
                    b = f"CAST({b} AS FLOAT)"
            return f"(CASE WHEN {cond} THEN {a} ELSE {b} END)", ar or br
        inner, _ = self.num_expr(depth - 1)
        target = self.pick(["INT", "BIGINT", "FLOAT"])
        return f"CAST({inner} AS {target})", target == "FLOAT"

    def text_expr(self, depth):
        # `text` only, never `date`: SQLite stores dates as TEXT, so it happily
        # LIKEs them and unifies them with strings in a CASE. Here DATE32 is a
        # distinct type and neither is legal, which is what the standard says.
        textual = self.cols("text")
        if depth <= 0 or not textual or self.rng.random() < 0.5:
            if textual:
                col = self.pick(textual)
                return col.name
            return quote("x")
        form = self.rng.randrange(3)
        if form == 0:
            col = self.pick(textual)
            return f"({col.name} || {quote('-')} || {self.text_expr(depth - 1)})"
        if form == 1:
            cond = self.bool_expr(depth - 1)
            return f"(CASE WHEN {cond} THEN {self.text_expr(depth - 1)} ELSE {self.text_expr(depth - 1)} END)"
        inner, is_real = self.num_expr(depth - 1)
        if is_real:
            # CAST(<real> AS TEXT) compares two float-formatting conventions,
            # not two answers: SQLite prints 15 significant digits, this engine
            # prints the shortest round-trip form (as PostgreSQL 12+ does).
            inner = f"CAST({inner} AS INT)"
        return f"CAST({inner} AS TEXT)"

    def bool_expr(self, depth):
        if depth <= 0:
            return self.comparison()
        form = self.rng.randrange(11)
        if form <= 3:
            return self.comparison()
        if form <= 5:
            op = self.pick(["AND", "OR"])
            if op == "OR":
                # A correlated subquery can only become a join when it is a
                # top-level conjunct: a semi-join *filters*, and there is no way
                # to OR a filter with something else. Generating one under an OR
                # would test a documented limitation rather than a difference.
                saved, self.allow_correlated = self.allow_correlated, False
                out = f"({self.bool_expr(depth - 1)} OR {self.bool_expr(depth - 1)})"
                self.allow_correlated = saved
                return out
            return f"({self.bool_expr(depth - 1)} AND {self.bool_expr(depth - 1)})"
        if form == 6:
            saved, self.allow_correlated = self.allow_correlated, False
            out = f"(NOT {self.bool_expr(depth - 1)})"
            self.allow_correlated = saved
            return out
        if form == 7:
            col = self.pick(self.columns)
            return f"({col.name} IS {'NOT ' if self.rng.random() < 0.5 else ''}NULL)"
        if form == 8:
            return self.membership()
        if form == 9 and self.schemas and self.rng.random() < 0.5:
            sub = self.subquery_predicate()
            if sub:
                return sub
        return self.between()

    # -- subqueries --------------------------------------------------------

    def subquery_predicate(self):
        """An EXISTS / IN condition over another table, correlated or not."""
        inner_table = self.rng.choice(list(self.schemas))
        inner = prefixed(self.schemas[inner_table], "s")
        inner_gen = Gen(self.rng, f"{inner_table} s", inner)

        # A correlation needs a pair of columns that plausibly relate.
        link = None
        if self.outer_alias and self.allow_correlated:
            for lt, lk, rt, rk in JOIN_KEYS:
                if rt == inner_table and any(
                    c.name == f"{self.outer_alias}.{lk}" for c in self.columns
                ):
                    link = f"s.{rk} = {self.outer_alias}.{lk}"
                    break

        form = self.rng.randrange(4)
        where = inner_gen.bool_expr(1)

        if form <= 1:
            # EXISTS / NOT EXISTS, correlated when there is a link to use.
            condition = f"{where} AND {link}" if link and self.rng.random() < 0.7 else where
            negate = "NOT " if form == 1 else ""
            return f"({negate}EXISTS (SELECT 1 FROM {inner_table} s WHERE {condition}))"

        # IN over a single column of the inner table. Correlated IN is fine;
        # correlated NOT IN is not (see the divergence note), so NOT IN is only
        # emitted uncorrelated.
        candidates = [c for c in inner if c.kind in ("int", "text", "real", "date")]
        if not candidates:
            return None
        inner_col = self.pick(candidates)
        outer_candidates = [c for c in self.columns if c.kind == inner_col.kind]
        if not outer_candidates:
            return None
        outer_col = self.pick(outer_candidates)

        correlate = link and form == 2 and self.rng.random() < 0.6
        condition = f"{where} AND {link}" if correlate else where
        negate = "NOT " if form == 3 else ""
        return (
            f"({outer_col.name} {negate}IN "
            f"(SELECT {inner_col.name} FROM {inner_table} s WHERE {condition}))"
        )

    def scalar_subquery(self):
        """An aggregate subquery, which always returns exactly one row."""
        inner_table = self.rng.choice(list(self.schemas))
        inner = prefixed(self.schemas[inner_table], "s")
        numeric = [c for c in inner if c.kind in ("int", "real")]
        if not numeric:
            return None, None
        col = self.pick(numeric)
        func = self.pick(["MIN", "MAX", "AVG", "SUM", "COUNT"])
        letter = "I" if func == "COUNT" else ("R" if func == "AVG" else letter_for(col.kind))
        return f"(SELECT {func}({col.name}) FROM {inner_table} s)", letter

    def comparison(self):
        if self.schemas and self.rng.random() < 0.08:
            numeric = self.cols("int", "real")
            if numeric:
                col = self.pick(numeric)
                sub, _ = self.scalar_subquery()
                if sub:
                    op = self.pick(["<", "<=", ">", ">=", "="])
                    return f"({col.name} {op} {sub})"
        col = self.pick(self.columns)
        # A boolean column is used as a predicate, or compared against TRUE /
        # FALSE. Never against 0 / 1: SQLite has no boolean type so it accepts
        # that, but comparing BOOLEAN with INTEGER is a type error in standard
        # SQL and this engine rejects it.
        if col.kind == "bool":
            form = self.rng.randrange(4)
            if form == 0:
                return f"({col.name})"
            if form == 1:
                return f"(NOT {col.name})"
            op = self.pick(["=", "<>"])
            return f"({col.name} {op} {self.pick(['TRUE', 'FALSE'])})"

        if col.kind == "text" and self.rng.random() < 0.35:
            negate = "NOT " if self.rng.random() < 0.3 else ""
            return f"({col.name} {negate}LIKE {self.like_pattern(col)})"

        op = self.pick(["=", "<>", "<", "<=", ">", ">="])
        rhs = (
            self.numeric_literal(col)
            if col.kind in ("int", "real")
            else self.text_literal(col)
        )
        return f"({col.name} {op} {rhs})"

    def membership(self):
        col = self.pick(self.columns)
        if not col.values:
            return "(1 = 1)"
        # Type-homogeneous lists only: SQLite will not match an INTEGER column
        # against a TEXT list item, while this engine folds the literal.
        n = self.rng.randrange(1, 4)
        picks = self.rng.sample(col.values, min(n, len(col.values)))
        if col.kind in ("int", "real"):
            items = [p if col.kind == "int" else f"{float(p):.2f}" for p in picks]
        elif col.kind == "bool":
            items = ["TRUE" if p.lower() in ("true", "t", "yes", "1") else "FALSE" for p in picks]
        else:
            items = [quote(p) for p in picks]
        # A NULL in the list is the interesting case, so include one sometimes.
        if self.rng.random() < 0.3:
            items.append("NULL")
        negate = "NOT " if self.rng.random() < 0.4 else ""
        return f"({col.name} {negate}IN ({', '.join(items)}))"

    def between(self):
        candidates = self.cols("int", "real", "date", "text")
        col = self.pick(candidates)
        if col.kind in ("int", "real"):
            a, b = sorted([float(self.numeric_literal(col)), float(self.numeric_literal(col))])
            lo = str(int(a)) if col.kind == "int" else f"{a:.2f}"
            hi = str(int(b)) if col.kind == "int" else f"{b:.2f}"
        else:
            lo, hi = sorted(self.rng.sample(col.values, 2) if len(col.values) > 1 else col.values * 2)
            lo, hi = quote(lo), quote(hi)
        negate = "NOT " if self.rng.random() < 0.3 else ""
        return f"({col.name} {negate}BETWEEN {lo} AND {hi})"

    # -- statements --------------------------------------------------------

    # -- aggregate queries -------------------------------------------------

    def aggregate_query(self):
        """A GROUP BY query, or a global aggregate when no keys are chosen."""
        groupable = [c for c in self.columns if c.kind != "real"]
        n_groups = self.rng.choice([0, 1, 1, 2]) if groupable else 0
        groups = self.rng.sample(groupable, min(n_groups, len(groupable)))

        items, letters = [], []
        for g in groups:
            items.append(g.name)
            letters.append(letter_for(g.kind))

        for _ in range(self.rng.randrange(1, 4)):
            sql, letter = self.aggregate_call()
            items.append(sql)
            letters.append(letter)

        sql = f"SELECT {', '.join(items)} FROM {self.table}"
        if self.rng.random() < 0.5:
            self.allow_correlated = True
            sql += f" WHERE {self.bool_expr(self.rng.randrange(1, 3))}"
            self.allow_correlated = False
        if groups:
            sql += f" GROUP BY {', '.join(g.name for g in groups)}"
        if self.rng.random() < 0.4:
            # HAVING may name an aggregate the SELECT list does not.
            call, _ = self.aggregate_call(numeric_only=True)
            op = self.pick(["=", "<>", "<", "<=", ">", ">="])
            sql += f" HAVING {call} {op} {self.rng.randrange(1, 5)}"
        return sql, "".join(letters)

    def aggregate_call(self, numeric_only=False):
        numeric = self.cols("int", "real")
        forms = ["count_star", "count", "min", "max"]
        if numeric:
            forms += ["sum", "avg"]
        if numeric_only:
            forms = ["count_star", "count"] + (["sum", "avg"] if numeric else [])

        form = self.pick(forms)
        if form == "count_star":
            return "COUNT(*)", "I"
        if form == "count":
            col = self.pick(self.columns)
            distinct = "DISTINCT " if self.rng.random() < 0.4 else ""
            return f"COUNT({distinct}{col.name})", "I"
        if form == "sum":
            col = self.pick(numeric)
            distinct = "DISTINCT " if self.rng.random() < 0.3 else ""
            # SUM over a boolean or text column is a type error here even though
            # SQLite accepts it, so only numeric columns are summed.
            return f"SUM({distinct}{col.name})", "I" if col.kind == "int" else "R"
        if form == "avg":
            col = self.pick(numeric)
            return f"AVG({col.name})", "R"
        col = self.pick(self.columns)
        return f"{form.upper()}({col.name})", letter_for(col.kind)

    def window_call(self):
        """A window function, plus the letter its column renders as."""
        orderable = [c for c in self.columns if c.kind in ("int", "real", "text", "date")]
        partitionable = [c for c in self.columns if c.kind in ("text", "bool", "int")]
        if not orderable:
            return None, None

        # Order by *every* orderable column, not one. ROW_NUMBER, LAG and LEAD
        # are only determined when the window's ordering is total: with ties,
        # which row is "previous" is up to the engine, and the two disagree for
        # a perfectly good reason. Ordering by everything means tied rows are
        # identical rows, so any tie-break gives the same answer.
        over = "ORDER BY " + ", ".join(c.name for c in orderable)
        if partitionable and self.rng.random() < 0.5:
            over = f"PARTITION BY {self.pick(partitionable).name} {over}"

        form = self.rng.randrange(6)
        if form == 0:
            return f"ROW_NUMBER() OVER ({over})", "I"
        if form == 1:
            return f"RANK() OVER ({over})", "I"
        if form == 2:
            return f"DENSE_RANK() OVER ({over})", "I"
        if form == 3:
            return f"COUNT(*) OVER ({over})", "I"
        numeric = self.cols("int", "real")
        if not numeric:
            return f"ROW_NUMBER() OVER ({over})", "I"
        col = self.pick(numeric)
        if form == 4:
            return f"SUM({col.name}) OVER ({over})", letter_for(col.kind)
        return f"LAG({col.name}) OVER ({over})", letter_for(col.kind)

    def query(self):
        items, letters = [], []
        for _ in range(self.rng.randrange(1, 4)):
            roll = self.rng.random()
            if roll < 0.45:
                sql, is_real = self.num_expr(self.rng.randrange(1, 3))
                items.append(sql)
                letters.append("R" if is_real else "I")
            elif roll < 0.72:
                items.append(self.text_expr(self.rng.randrange(1, 3)))
                letters.append("T")
            elif roll < 0.8 and self.cols("date"):
                # Dates are legal in a projection and in concatenation, just
                # not as LIKE or CASE operands mixed with strings.
                col = self.pick(self.cols("date"))
                items.append(
                    col.name
                    if self.rng.random() < 0.6
                    else f"({col.name} || {quote('!')})"
                )
                letters.append("T")
            else:
                items.append(self.bool_expr(self.rng.randrange(0, 2)))
                letters.append("I")
        # A window function adds a column computed from the row's neighbours.
        window = None
        if self.rng.random() < 0.18:
            window, letter = self.window_call()
            if window:
                items.append(window)
                letters.append(letter)

        self.allow_correlated = True
        where = self.bool_expr(self.rng.randrange(1, 4))
        self.allow_correlated = False

        # DISTINCT is not combined with a window: the window is computed first,
        # so a row number makes every row unique and the DISTINCT does nothing.
        distinct = "DISTINCT " if window is None and self.rng.random() < 0.15 else ""
        sql = f"SELECT {distinct}{', '.join(items)} FROM {self.table} WHERE {where}"

        # Ordering by every output column is a total order on distinct rows, and
        # rows that tie are identical -- so the result is the same whichever way
        # the two engines break the tie.
        if window or self.rng.random() < 0.25:
            keys = ", ".join(str(i + 1) for i in range(len(items)))
            direction = " DESC" if self.rng.random() < 0.3 else ""
            sql += f" ORDER BY {keys}{direction}"
            return sql, "".join(letters), "nosort"

        return sql, "".join(letters)


def set_op_query(rng, schemas):
    """Two SELECTs over the same columns, combined.

    Both branches project the same columns of the same table so that the arity
    and types line up by construction -- a set operation over randomly chosen
    columns would nearly always be a type error rather than a test.
    """
    table = rng.choice(list(schemas))
    columns = schemas[table]
    keep = rng.sample(columns, min(len(columns), rng.randrange(1, 4)))
    projection = ", ".join(c.name for c in keep)
    letters = "".join(letter_for(c.kind) for c in keep)

    left = Gen(rng, table, columns)
    right = Gen(rng, table, columns)
    op = rng.choice(["UNION", "UNION ALL", "INTERSECT", "EXCEPT"])
    sql = (
        f"SELECT {projection} FROM {table} WHERE {left.bool_expr(2)} "
        f"{op} "
        f"SELECT {projection} FROM {table} WHERE {right.bool_expr(2)}"
    )
    return sql, letters


def derived_gen(rng, schemas):
    """A query over `FROM (SELECT ...) x`, so derived tables get exercised."""
    table = rng.choice(list(schemas))
    columns = schemas[table]
    keep = rng.sample(columns, min(len(columns), rng.randrange(2, 5)))
    inner = ", ".join(c.name for c in keep)
    inner_gen = Gen(rng, table, columns)
    where = f" WHERE {inner_gen.bool_expr(1)}" if rng.random() < 0.6 else ""
    relation = f"(SELECT {inner} FROM {table}{where}) x"
    return Gen(rng, relation, prefixed(keep, "x"), outer_alias="x")


def join_gen(rng, schemas):
    """A generator over a two-table join, with every column alias-qualified."""
    if rng.random() < 0.55:
        lt, lk, rt, rk = rng.choice(JOIN_KEYS)
        on = f"ON a.{lk} = b.{rk}"
    else:
        # Self join on some column both sides obviously share.
        lt = rt = rng.choice(list(schemas))
        key = rng.choice([c for c in schemas[lt] if c.kind in ("int", "text", "date")])
        on = f"ON a.{key.name} = b.{key.name}"

    join_type = rng.choice(JOIN_TYPES)
    columns = prefixed(schemas[lt], "a") + prefixed(schemas[rt], "b")

    # A residual condition on the ON clause is the interesting case, and it is
    # not the same as putting it in WHERE: under an outer join a row whose only
    # candidate fails the residual is still emitted, NULL-padded.
    if rng.random() < 0.4:
        side = rng.choice(["a", "b"])
        source = schemas[lt] if side == "a" else schemas[rt]
        residual = Gen(rng, "", prefixed(source, side)).comparison()
        on = f"{on} AND {residual}"

    relation = f"{lt} a {join_type} {rt} b {on}"
    return Gen(rng, relation, columns), rng.random() < 0.4


# Columns to index in the generated corpus. Deliberately a mix: an integer key,
# a text column, and a nullable numeric one -- the last so that generated
# predicates keep testing that NULLs, which are absent from the tree, are never
# served from it.
INDEXES = [
    ("people", "id"),
    ("people", "city"),
    ("people", "score"),
    ("orders", "person_id"),
    ("orders", "unit_price"),
]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--count", type=int, default=400)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    schemas = {name: load_schema(f"{ge.REPO_ROOT}/{path}") for name, path in FIXTURES}

    print("# GENERATED FILE -- do not edit by hand.")
    print(f"# python3 tools/fuzz_queries.py --seed {args.seed} --count {args.count}")
    print("#")
    print("# Randomized differential corpus. Expected results come from SQLite;")
    print("# a failure here is a real semantic disagreement.")
    print("#")
    print("# The tables are indexed, so every query that filters on an indexed")
    print("# column may be planned as an index scan -- a completely different")
    print("# set of rows read, in a different way, and still checked against")
    print("# SQLite's answer. The evaluator-agreement test runs the same corpus")
    print("# with index scans disabled, so both paths are compared to each")
    print("# other as well as to the oracle.")
    print()
    for name, path in FIXTURES:
        print(f"load {name} {path}")
    print()
    for table, column in INDEXES:
        print(f"index {table} {column}")
    print()

    seen = set()
    emitted = 0
    while emitted < args.count:
        roll = rng.random()
        if roll < 0.34:
            table = rng.choice(list(schemas))
            gen = Gen(rng, table, prefixed(schemas[table], "a"), outer_alias="a")
            gen.table = f"{table} a"
            gen.schemas = schemas
            result = gen.query()
        elif roll < 0.55:
            table = rng.choice(list(schemas))
            gen = Gen(rng, table, prefixed(schemas[table], "a"), outer_alias="a")
            gen.table = f"{table} a"
            gen.schemas = schemas
            result = gen.aggregate_query()
        elif roll < 0.68:
            gen = derived_gen(rng, schemas)
            result = gen.query()
        elif roll < 0.80:
            result = set_op_query(rng, schemas)
        else:
            gen, aggregating = join_gen(rng, schemas)
            gen.schemas = schemas
            result = gen.aggregate_query() if aggregating else gen.query()

        if len(result) == 3:
            sql, letters, mode = result
        else:
            sql, letters = result
            mode = "rowsort"

        if sql in seen:
            continue
        seen.add(sql)
        print(f"query {letters} {mode}")
        print(sql)
        print("----")
        print()
        emitted += 1

    print(f"# {emitted} generated queries", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
