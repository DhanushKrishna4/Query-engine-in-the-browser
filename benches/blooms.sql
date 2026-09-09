-- Per-row-group bloom filters, measured against the same queries with them
-- turned off.
--
--   python3 tools/gen_sample.py 1000000 > /tmp/trips.csv
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --bench benches/blooms.sql --bench-baseline no-bloom
--
-- A bloom filter answers the question a zone map cannot: not "could a value in
-- this range be here" but "could this exact value be here". The difference only
-- matters for a column that is *scattered*, where every row group's min/max
-- spans the whole domain and a range test rules nothing out.
--
-- `medallion` is that column: 200000 values shuffled through a million rows, so
-- each appears about five times and every row group's range runs end to end.
-- `trip_id` is the opposite -- clustered and unique -- and it is in here to show
-- that its filters, which are built and do cost memory, buy nothing at all
-- because the zone map already had the answer.

-- Absent value, in range. The zone map cannot rule out a single group; the
-- filters rule out all sixteen without reading a byte of data.
SELECT trip_id FROM trips WHERE medallion = 'M0000064';

SELECT trip_id, fare FROM trips WHERE medallion = 'M0000088';

-- Present, about five rows scattered over a handful of groups. The filters skip
-- the groups that do not hold one.
SELECT trip_id, fare FROM trips WHERE medallion = 'M0100000';

SELECT trip_id, vendor, fare FROM trips WHERE medallion = 'M0050000';

-- A disjunction of equalities: a group is skipped only when every candidate is
-- absent from it.
SELECT trip_id FROM trips WHERE medallion IN ('M0000064', 'M0000088', 'M0000287');

-- Mixed: one present value keeps the groups that hold it.
SELECT trip_id FROM trips WHERE medallion IN ('M0000064', 'M0100000');

-- An aggregate over a bloom-filtered scan.
SELECT COUNT(*), AVG(fare) FROM trips WHERE medallion = 'M0150000';

-- The domain maximum. Even on a scattered column the zone map is perfect here:
-- a group whose max is below 'M0199999' cannot hold it, and the only groups
-- that survive are the ones that do. The filters are consulted and find nothing
-- left to skip -- which is the point of keeping this line.
SELECT COUNT(*), AVG(fare) FROM trips WHERE medallion = 'M0199999';

-- Clustered and unique: the zone map already skips fifteen of sixteen groups,
-- so the filter on `trip_id` is asked and has nothing left to contribute.
SELECT trip_id, fare FROM trips WHERE trip_id = 777777;

-- Absent and out of range. The zone map rules everything out on two
-- comparisons and the filters are never consulted.
SELECT trip_id FROM trips WHERE trip_id = 9999999;

-- Not an equality, so a membership test has nothing to say: a filter records
-- which values are present, not how they order.
SELECT COUNT(*) FROM trips WHERE medallion > 'M0199990';

-- Low cardinality, so no filter was built at all -- three vendors appear in
-- every row group and a filter would answer "maybe" every time.
SELECT COUNT(*) FROM trips WHERE vendor = 'yellow';
