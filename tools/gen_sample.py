#!/usr/bin/env python3
"""Generate a larger CSV for exercising row groups, batching and (later) zone maps.

Real datasets are not committed to the repo -- they are fetched from a CDN at
runtime in the browser. This is just enough synthetic data to make the native
CLI's timings and row-group counts interesting.

    python3 tools/gen_sample.py 1000000 > /tmp/trips.csv
"""
import random
import sys

rows = int(sys.argv[1]) if len(sys.argv) > 1 else 100_000
random.seed(7)

vendors = ["yellow", "green", "fhv"]
boroughs = ["Manhattan", "Brooklyn", "Queens", "Bronx", "Staten Island"]

# A high-cardinality column scattered across the whole table, so that the two
# metadata structures can be told apart. `trip_id` ascends, so its zone maps are
# tight and a bloom filter on it is redundant; a medallion recurs a handful of
# times at random positions, so every row group's min/max spans nearly the whole
# domain and only a bloom filter can rule one out.
medallions = rows // 5 or 1
# Its own stream, so that adding this column left every other column bit for bit
# unchanged. Drawing from the shared generator would have shifted the sequence
# by one value per row and quietly invalidated every measurement in the README.
medallion_rng = random.Random(11)

print(
    "trip_id,vendor,pickup_borough,passengers,distance_mi,fare,tip,pickup_date,medallion"
)
for i in range(rows):
    dist = round(random.lognormvariate(0.6, 0.8), 2)
    fare = round(2.5 + dist * 2.75, 2)
    tip = round(fare * random.choice([0.0, 0.1, 0.15, 0.2, 0.25]), 2)
    # ~2% of tips are missing, so NULL handling gets exercised on real volume.
    tip_field = "" if random.random() < 0.02 else tip
    day = random.randint(1, 28)
    month = random.randint(1, 12)
    print(
        f"{i},{random.choice(vendors)},{random.choice(boroughs)},"
        f"{random.randint(1, 6)},{dist},{fare},{tip_field},2024-{month:02d}-{day:02d},"
        f"M{medallion_rng.randrange(medallions):07d}"
    )
