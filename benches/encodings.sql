-- Predicates answered on encoded columns, against the same predicates answered
-- on decoded ones.
--
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --bench benches/encodings.sql --bench-baseline no-encodings
--
-- The baseline decodes the column and compares values the ordinary way. The
-- engine column translates the predicate into the encoding's own domain -- a
-- dictionary code, a packed integer -- and answers it there.

-- The case the dictionary is for. 'Chicago' sits between 'Bronx' and 'Staten
-- Island', so the zone map cannot rule it out and every row group survives to
-- be scanned. The dictionary answers exactly, with five comparisons per group.
--
-- This is the bloom filters' job on a high-cardinality column, and dictionaries
-- and bloom filters turn out to be exactly complementary: a filter is built
-- when distinct values exceed half the rows, a dictionary when they do not.
SELECT COUNT(*) FROM trips WHERE pickup_borough = 'Chicago';

SELECT trip_id FROM trips WHERE vendor = 'purple';

SELECT COUNT(*) FROM trips WHERE pickup_borough = 'Chicago' AND fare > 10;

-- Dictionary: one string comparison against a five-entry dictionary, then a
-- scan of u32 codes.
SELECT COUNT(*) FROM trips WHERE pickup_borough = 'Bronx';

SELECT COUNT(*) FROM trips WHERE vendor = 'yellow';

-- The dictionary is sorted, so ranges work on codes too.
SELECT COUNT(*) FROM trips WHERE pickup_borough < 'Manhattan';

-- Bit-packed: values 1..6 in three bits.
SELECT COUNT(*) FROM trips WHERE passengers > 4;

SELECT COUNT(*) FROM trips WHERE passengers = 1;

-- Frame-of-reference on an ascending column: most blocks are ruled out by
-- their own range before anything is unpacked.
SELECT COUNT(*) FROM trips WHERE trip_id < 50000;

SELECT trip_id, vendor FROM trips WHERE trip_id BETWEEN 400000 AND 400100;

-- A date column, bit-packed against its minimum.
SELECT COUNT(*) FROM trips WHERE pickup_date >= '2024-07-01';

-- Selective enough that the narrowed selection carries downstream.
SELECT trip_id, passengers FROM trips WHERE passengers = 6 AND pickup_borough = 'Bronx';

-- Not encodable: a float column, so both configurations run the same path.
SELECT COUNT(*) FROM trips WHERE fare > 45;
