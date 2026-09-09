-- Cost-based join ordering, measured against the same queries with reordering
-- turned off (every other rule still applies).
--
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --load boroughs=data/boroughs.csv --load vendors=data/vendors.csv \
--       --bench benches/reorder.sql --bench-baseline no-reorder
--
-- Each query is written in the order a person would naturally write it -- small
-- tables first, reading like the sentence in their head -- which is close to
-- the worst order to execute.

-- Dimension first: without reordering, the 5-row table is probed by nothing and
-- the million-row table gets built into a hash table.
SELECT COUNT(*) FROM boroughs b JOIN trips t ON t.pickup_borough = b.borough;

-- Two dimensions before the fact table.
SELECT COUNT(*) FROM vendors v JOIN trips t ON t.vendor = v.vendor_code JOIN boroughs b ON t.pickup_borough = b.borough;

-- The same three tables written fact-first, which is already a good order --
-- reordering should leave it alone rather than making it worse.
SELECT COUNT(*) FROM trips t JOIN boroughs b ON t.pickup_borough = b.borough JOIN vendors v ON t.vendor = v.vendor_code;

-- Comma join with the conditions in WHERE, so predicate pushdown turns cross
-- joins into inner joins and reordering then has something to arrange.
SELECT COUNT(*) FROM boroughs b, vendors v, trips t WHERE t.pickup_borough = b.borough AND t.vendor = v.vendor_code;

-- A selective filter on the fact table changes which order is cheapest.
SELECT b.region, COUNT(*) FROM boroughs b JOIN trips t ON t.pickup_borough = b.borough WHERE t.distance_mi > 20 GROUP BY b.region;
