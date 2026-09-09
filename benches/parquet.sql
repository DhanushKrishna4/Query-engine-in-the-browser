-- The same queries against the same data, loaded from CSV and from Parquet.
--
--   python3 tools/gen_sample.py 1000000 > /tmp/trips.csv
--   python3 tools/gen_parquet.py
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv     --bench benches/parquet.sql
--   cargo run --release --bin qe -- --load trips=/tmp/trips.parquet --bench benches/parquet.sql
--
-- Read the `engine` column of each run against the other. The baseline column
-- is not the interesting axis here: the comparison is between two *loaders*,
-- and the bench harness only varies execution options within one table.
--
-- A CSV table is fully decoded before the first query runs. A Parquet table has
-- read nothing but its footer, and decodes a column chunk the first time a scan
-- asks for one -- so the first query pays and the rest do not. Both effects are
-- in here on purpose.

-- Touches one column of nine. On Parquet the other eight are never decoded.
SELECT COUNT(*) FROM trips WHERE fare > 45;

-- A second query over the same column: the chunk is already decoded.
SELECT COUNT(*) FROM trips WHERE fare > 100;

-- Clustered predicate: fifteen row groups of sixteen are ruled out by the zone
-- map, and on Parquet those groups are never read at all.
SELECT trip_id, fare FROM trips WHERE trip_id BETWEEN 400000 AND 410000;

SELECT trip_id, vendor FROM trips WHERE trip_id = 777777;

-- Nothing anywhere: every row group eliminated on metadata alone.
SELECT trip_id FROM trips WHERE fare > 300;

-- Two columns of nine.
SELECT vendor, COUNT(*), AVG(fare) FROM trips GROUP BY vendor;

-- Wide: every column of every row.
SELECT COUNT(*), SUM(fare), AVG(distance_mi), MIN(tip), MAX(passengers) FROM trips;

-- A string column, where Parquet's dictionary encoding is doing real work.
SELECT COUNT(*) FROM trips WHERE pickup_borough = 'Bronx';

SELECT pickup_borough, COUNT(*) FROM trips GROUP BY pickup_borough;

-- Sort and top-N over a decoded column.
SELECT trip_id, fare FROM trips ORDER BY fare DESC LIMIT 10;
