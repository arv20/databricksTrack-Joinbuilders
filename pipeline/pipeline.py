#!/usr/bin/env python3
"""
Shopping Sessions: DuckDB pipeline implementing rules.md.

Reads  $DATA_DIR    (default /data)
Writes $OUTPUT_DIR  (default /output): user_daily/ and campaign_daily/ as Parquet
Temp   $SCRATCH_DIR (default /scratch)

Design notes
- Every CSV file is read as text and split on "\n" ourselves. DuckDB's CSV reader
  treats a bare "\r" as a line break, which rules.md forbids.
- The CSV dialect (field count, quoting) is checked with a regex per field.
- Timestamps are parsed with plain integer arithmetic (no date library), so bad
  values become NULL instead of errors.
- Linked IDs are merged with scipy's connected_components.
- Sessions, clicks and assists are computed with window functions, ASOF JOIN and
  interval merging, never with a per-session x per-click cross join.
"""
import json
import os
import glob as globmod

import duckdb
import numpy as np
import pyarrow as pa
from scipy.sparse import csr_matrix
from scipy.sparse.csgraph import connected_components

DATA = os.environ.get("DATA_DIR", "/data")
OUT = os.environ.get("OUTPUT_DIR", "/output")
SCRATCH = os.environ.get("SCRATCH_DIR", "/scratch")
NCPU = int(os.environ.get("NCPU", os.cpu_count() or 1))
MEM_GB = int(os.environ.get("MEM_GB", "8"))

DAY_MS = 86_400_000
SESSION_GAP_MS = 1_800_000
CLICK_LOOKBACK_MS = 7 * DAY_MS
LATE_MS = DAY_MS
MAX_PRICE = 10_000_000

con = duckdb.connect()
con.execute(f"SET threads = {NCPU}")
con.execute(f"SET memory_limit = '{max(1, int(MEM_GB * 0.8))}GB'")
con.execute(f"SET temp_directory = '{os.path.join(SCRATCH, 'duckdb_tmp')}'")
con.execute("SET preserve_insertion_order = false")
con.execute("SET TimeZone = 'UTC'")

# ---------------------------------------------------------------- SQL macros
# Civil date -> days since 1970-01-01 (Howard Hinnant's algorithm, integers only).
# Timestamps are parsed with substr + try_cast, so any malformed input is NULL.
MACROS = r"""
CREATE OR REPLACE MACRO is_leap(y) AS (y % 4 = 0 AND (y % 100 <> 0 OR y % 400 = 0));
CREATE OR REPLACE MACRO days_in_month(y, mo) AS (
    CASE mo WHEN 2 THEN (CASE WHEN is_leap(y) THEN 29 ELSE 28 END)
            WHEN 4 THEN 30 WHEN 6 THEN 30 WHEN 9 THEN 30 WHEN 11 THEN 30 ELSE 31 END);
CREATE OR REPLACE MACRO civil_year(y, mo) AS (CASE WHEN mo <= 2 THEN y - 1 ELSE y END);
CREATE OR REPLACE MACRO days_from_civil(y, mo, dd) AS (
    (civil_year(y, mo) // 400) * 146097
    + (civil_year(y, mo) - (civil_year(y, mo) // 400) * 400) * 365
    + (civil_year(y, mo) - (civil_year(y, mo) // 400) * 400) // 4
    - (civil_year(y, mo) - (civil_year(y, mo) // 400) * 400) // 100
    + (153 * ((mo + 9) % 12) + 2) // 5 + dd - 1
    - 719468);
CREATE OR REPLACE MACRO ts_ms(s) AS (
    CASE
        WHEN length(s) > 0 AND ltrim(s, '0123456789') = '' THEN try_cast(s AS BIGINT)
        WHEN regexp_full_match(s, '[0-9]{4}-[0-9]{2}-[0-9]{2}T([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9](Z|[+-]([01][0-9]|2[0-3]):[0-5][0-9])')
            THEN epoch_ms(try_cast(s AS TIMESTAMPTZ))
        ELSE NULL
    END);
"""


TIMING = bool(os.environ.get("TIMING"))


def run(sql):
    import sys
    import time
    t0 = time.time()
    con.execute(sql)
    if TIMING:
        name = " ".join(sql.split()[:5])
        print(f"{time.time() - t0:8.2f}s  {name}", file=sys.stderr)


def scalar(sql):
    return con.execute(sql).fetchone()[0]


run(MACROS)

with open(os.path.join(DATA, "manifest.json")) as f:
    manifest = json.load(f)
W0 = scalar(f"SELECT ts_ms('{manifest['window_start']}')")
W1 = scalar(f"SELECT ts_ms('{manifest['window_end']}')")
print("window:", manifest["window_start"], "to", manifest["window_end"], file=__import__("sys").stderr)


# ---------------------------------------------------------------- CSV reading
def line_source(pattern):
    """SQL yielding one row per physical line (header removed) of every file matching pattern."""
    if not globmod.glob(pattern):
        return "SELECT CAST(NULL AS VARCHAR) AS line WHERE false"
    return f"""
        SELECT unnest(list_slice(p, 2, len(p) - CASE WHEN endnl THEN 1 ELSE 0 END)) AS line
        FROM (
            SELECT string_split(content, chr(10)) AS p,
                   content LIKE ('%' || chr(10)) AS endnl
            FROM read_text('{pattern}')
        )"""


def dialect_fields(pattern, n, cols):
    """Rows of a CSV folder that satisfy the dialect, with fields named `cols`.

    A line is kept only if it has exactly n fields, and each field is either
    unquoted with no quote character, or wrapped in quotes with no quote inside.
    One regex checks the whole line, so malformed lines are rejected before any
    split. Quotes are removed; an empty value becomes NULL.
    """
    field = '([^",]*|"[^",]*")'
    line_re = field + "," + ",".join([field] * (n - 1))

    def unq(i):
        x = f"p[{i + 1}]"
        return (f"NULLIF(CASE WHEN starts_with({x}, '\"') THEN substr({x}, 2, length({x}) - 2) "
                f"ELSE {x} END, '')")
    selects = ", ".join(f"{unq(i)} AS {c}" for i, c in enumerate(cols))
    return f"""
        SELECT {selects}
        FROM (
            SELECT string_split(line, ',') AS p
            FROM ({line_source(pattern)})
            WHERE regexp_full_match(line, '{line_re}')
        )"""


# ---------------------------------------------------------------- small tables
run(f"""CREATE TEMP TABLE products AS
        SELECT product_id, NULLIF(category, '') AS category FROM ({dialect_fields(os.path.join(DATA, 'products.csv'), 2, ['product_id', 'category'])})
        WHERE product_id IS NOT NULL
        QUALIFY row_number() OVER (PARTITION BY product_id ORDER BY category) = 1""")

run(f"""CREATE TEMP TABLE campaigns AS
        SELECT campaign_id, channel FROM ({dialect_fields(os.path.join(DATA, 'campaigns.csv'), 2, ['campaign_id', 'channel'])})
        WHERE campaign_id IS NOT NULL
        QUALIFY row_number() OVER (PARTITION BY campaign_id ORDER BY channel) = 1""")

# ---------------------------------------------------------------- linked IDs
links = dialect_fields(os.path.join(DATA, "links", "*.csv"), 2, ["a", "b"])
run(f"CREATE TEMP TABLE lk AS SELECT a, b FROM ({links}) WHERE a IS NOT NULL AND b IS NOT NULL")

n_nodes = scalar("SELECT count(*) FROM (SELECT a AS id FROM lk UNION SELECT b FROM lk)")
if n_nodes == 0:
    run("CREATE TEMP TABLE idmap (id VARCHAR, person VARCHAR)")
else:
    run("""CREATE TEMP TABLE node AS
           SELECT id, CAST(row_number() OVER (ORDER BY id) - 1 AS BIGINT) AS idx
           FROM (SELECT a AS id FROM lk UNION SELECT b FROM lk)""")
    edges = con.execute("""
        SELECT na.idx AS ia, nb.idx AS ib FROM lk
        JOIN node na ON lk.a = na.id
        JOIN node nb ON lk.b = nb.id""").fetchnumpy()
    ia = np.asarray(edges["ia"], dtype=np.int64)
    ib = np.asarray(edges["ib"], dtype=np.int64)
    # int32 so that many duplicate edges summed by csr_matrix cannot overflow.
    graph = csr_matrix((np.ones(len(ia), dtype=np.int32), (ia, ib)), shape=(n_nodes, n_nodes))
    _, labels = connected_components(graph, directed=False)
    comp = pa.table({"idx": np.arange(n_nodes, dtype=np.int64), "lab": labels.astype(np.int64)})
    con.register("comp_arrow", comp)
    run("CREATE TEMP TABLE comp AS SELECT * FROM comp_arrow")
    # Name = byte-wise smallest ID in the component.
    run("""CREATE TEMP TABLE idmap AS
           SELECT n.id AS id, min(n.id) OVER (PARTITION BY c.lab) AS person
           FROM node n JOIN comp c USING (idx)""")

# ---------------------------------------------------------------- events
events = dialect_fields(os.path.join(DATA, "events", "*.csv"), 7,
                        ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"])
# Keep the copy with the smallest ingest_ts. arg_min keeps one row per event_id in
# a hash aggregate, far cheaper than a window over every row.
# Keep the copy with the smallest ingest_ts. arg_min keeps one row per event_id in
# a hash aggregate. It is a subquery, not a table, so the deduplicated rows are never
# written out before validation.
ev_ded = f"""
    SELECT event_id,
           (r).user_id AS user_id, (r).ts AS ts_raw, (r).ingest_ms AS ingest_ms,
           (r).event_type AS event_type, (r).product_id AS product_id, (r).price_cents AS price_cents
    FROM (
        SELECT event_id,
               arg_min(struct_pack(user_id := user_id, ts := ts, ingest_ms := ingest_ms,
                                   event_type := event_type, product_id := product_id,
                                   price_cents := price_cents),
                       ingest_ms) AS r
        FROM (SELECT event_id, user_id, ts, event_type, product_id, price_cents,
                     ts_ms(ingest_ts) AS ingest_ms
              FROM ({events}) WHERE event_id IS NOT NULL)
        GROUP BY event_id
    )"""

# Validate the kept copy, then apply the window and the late-event rule.
run(f"""CREATE TEMP TABLE ev_v AS
        SELECT user_id, ts, ingest_ms, event_type, product_id, price
        FROM (
            SELECT user_id, ts_ms(ts_raw) AS ts, ingest_ms, event_type, product_id,
                   CASE WHEN event_type = 'purchase' THEN try_cast(price_cents AS BIGINT) END AS price,
                   CASE WHEN event_type = 'purchase' THEN price_cents END AS price_raw
            FROM ({ev_ded})
            WHERE user_id IS NOT NULL
              AND product_id IS NOT NULL
              AND event_type IN ('view', 'cart', 'purchase')
        )
        WHERE ts >= {W0} AND ts < {W1}
          AND ingest_ms IS NOT NULL
          AND ingest_ms - ts <= {LATE_MS}
          AND (event_type <> 'purchase'
               OR (regexp_full_match(price_raw, '[0-9]+') AND price BETWEEN 1 AND {MAX_PRICE}))""")

run("""CREATE TEMP TABLE ev_p AS
       SELECT COALESCE(m.person, e.user_id) AS pid, e.ts, e.event_type, e.product_id, e.price
       FROM ev_v e LEFT JOIN idmap m ON m.id = e.user_id""")

# ---------------------------------------------------------------- sessions
# Gaps are measured between distinct timestamps, so equal ts share a session.
run(f"""CREATE TEMP TABLE dts AS
        SELECT pid, ts,
               CASE WHEN prev IS NULL OR ts - prev > {SESSION_GAP_MS} THEN 1 ELSE 0 END AS newsess
        FROM (
            SELECT pid, ts, lag(ts) OVER (PARTITION BY pid ORDER BY ts) AS prev
            FROM (SELECT DISTINCT pid, ts FROM ev_p)
        )""")

run("""CREATE TEMP TABLE dsess AS
       SELECT pid, ts, min(ts) OVER (PARTITION BY pid, sid) AS sess_start
       FROM (
           SELECT pid, ts, sum(newsess) OVER (PARTITION BY pid ORDER BY ts) AS sid
           FROM dts
       )""")

# Session table: one row per (person, session start).
run("""CREATE TEMP TABLE sess AS
       SELECT e.pid, d.sess_start,
              CAST(epoch_ms(d.sess_start) AS DATE) AS sday,
              count(*) AS events,
              CAST(COALESCE(sum(e.price), 0) AS BIGINT) AS revenue,
              bool_or(e.event_type = 'purchase') AS purch
       FROM ev_p e
       JOIN dsess d ON e.pid = d.pid AND e.ts = d.ts
       GROUP BY e.pid, d.sess_start""")

# Revenue per (person, day, category) for top_category.
run("""CREATE TEMP TABLE user_cat AS
       SELECT pid, sday, cat, CAST(sum(price) AS BIGINT) AS s
       FROM (
           SELECT e.pid, e.price, CAST(epoch_ms(d.sess_start) AS DATE) AS sday,
                  COALESCE(p.category, 'unknown') AS cat
           FROM ev_p e
           JOIN dsess d ON e.pid = d.pid AND e.ts = d.ts
           LEFT JOIN products p ON p.product_id = e.product_id
           WHERE e.event_type = 'purchase'
       )
       GROUP BY pid, sday, cat""")

# ---------------------------------------------------------------- clicks
clicks = dialect_fields(os.path.join(DATA, "clicks", "*.csv"), 4,
                        ["click_id", "user_id", "campaign_id", "ts"])
run(f"""CREATE TEMP TABLE ck_v AS
        SELECT c.click_id, c.user_id, c.campaign_id, c.ts
        FROM (
            SELECT click_id, user_id, campaign_id, ts_ms(ts) AS ts
            FROM ({clicks})
            WHERE click_id IS NOT NULL AND user_id IS NOT NULL AND campaign_id IS NOT NULL
            QUALIFY row_number() OVER (PARTITION BY click_id ORDER BY click_id) = 1
        ) c
        JOIN campaigns k ON k.campaign_id = c.campaign_id
        WHERE c.ts >= {W0} - {CLICK_LOOKBACK_MS} AND c.ts < {W1}""")

run("""CREATE TEMP TABLE ck_p AS
       SELECT COALESCE(m.person, c.user_id) AS pid, c.click_id, c.campaign_id, c.ts
       FROM ck_v c LEFT JOIN idmap m ON m.id = c.user_id""")

# One row per (person, ts): the click with the smallest click_id wins a tie.
run("""CREATE TEMP TABLE ck_ts AS
       SELECT pid, ts, campaign_id, click_id
       FROM ck_p
       QUALIFY row_number() OVER (PARTITION BY pid, ts ORDER BY click_id) = 1""")

# Credit: latest click at or before the session start, within 7 days.
run(f"""CREATE TEMP TABLE credit AS
        SELECT s.pid, s.sess_start, s.sday, s.events, s.revenue, s.purch,
               COALESCE(CASE WHEN s.sess_start - c.ts <= {CLICK_LOOKBACK_MS} THEN c.campaign_id END,
                        'organic') AS campaign_id
        FROM sess s
        ASOF LEFT JOIN ck_ts c ON s.pid = c.pid AND s.sess_start >= c.ts""")

# Assists: merge each (person, campaign)'s click windows [ts, ts + 7 days] into disjoint intervals.
run(f"""CREATE TEMP TABLE cg AS
        SELECT pid, campaign_id, ts,
               sum(newg) OVER (PARTITION BY pid, campaign_id ORDER BY ts) AS gid
        FROM (
            SELECT pid, campaign_id, ts,
                   CASE WHEN prev IS NULL OR ts - prev > {CLICK_LOOKBACK_MS} THEN 1 ELSE 0 END AS newg
            FROM (
                SELECT pid, campaign_id, ts,
                       lag(ts) OVER (PARTITION BY pid, campaign_id ORDER BY ts) AS prev
                FROM (SELECT DISTINCT pid, campaign_id, ts FROM ck_p)
            )
        )""")

run(f"""CREATE TEMP TABLE intervals AS
        SELECT pid, campaign_id, min(ts) AS lo, max(ts) + {CLICK_LOOKBACK_MS} AS hi
        FROM cg
        GROUP BY pid, campaign_id, gid""")

run("""CREATE TEMP TABLE assists AS
       SELECT s.sday, i.campaign_id, count(*) AS n
       FROM sess s
       JOIN intervals i ON s.pid = i.pid AND s.sess_start BETWEEN i.lo AND i.hi
       GROUP BY s.sday, i.campaign_id""")

# ---------------------------------------------------------------- output: user_daily
run("""CREATE TEMP TABLE top_cat AS
       SELECT pid, sday, list(cat ORDER BY s DESC, cat ASC)[1] AS top_category
       FROM user_cat
       GROUP BY pid, sday""")

user_daily = """
    SELECT s.pid AS user_id,
           s.sday AS day,
           CAST(count(*) AS BIGINT) AS sessions,
           CAST(sum(s.events) AS BIGINT) AS events,
           CAST(sum(s.revenue) AS BIGINT) AS revenue_cents,
           any_value(t.top_category) AS top_category
    FROM sess s
    LEFT JOIN top_cat t ON t.pid = s.pid AND t.sday = s.sday
    GROUP BY s.pid, s.sday
"""

# ---------------------------------------------------------------- output: campaign_daily
run("""CREATE TEMP TABLE credited AS
       SELECT campaign_id, sday AS day,
              CAST(count(*) AS BIGINT) AS sessions,
              CAST(sum(CASE WHEN purch THEN 1 ELSE 0 END) AS BIGINT) AS purchasing_sessions,
              CAST(sum(revenue) AS BIGINT) AS revenue_cents
       FROM credit
       GROUP BY campaign_id, sday""")

campaign_daily = """
    WITH keys AS (
        SELECT campaign_id, day FROM credited
        UNION
        SELECT campaign_id, sday AS day FROM assists
    )
    SELECT k.campaign_id,
           CASE WHEN k.campaign_id = 'organic' THEN 'organic' ELSE c.channel END AS channel,
           k.day,
           CAST(COALESCE(cr.sessions, 0) AS BIGINT) AS sessions,
           CAST(COALESCE(cr.purchasing_sessions, 0) AS BIGINT) AS purchasing_sessions,
           CAST(COALESCE(cr.revenue_cents, 0) AS BIGINT) AS revenue_cents,
           CAST(COALESCE(a.n, 0) AS BIGINT) AS assisted_sessions
    FROM keys k
    LEFT JOIN credited cr ON cr.campaign_id = k.campaign_id AND cr.day = k.day
    LEFT JOIN assists a ON a.campaign_id = k.campaign_id AND a.sday = k.day
    LEFT JOIN campaigns c ON c.campaign_id = k.campaign_id
"""

# ---------------------------------------------------------------- write
os.makedirs(os.path.join(OUT, "user_daily"), exist_ok=True)
os.makedirs(os.path.join(OUT, "campaign_daily"), exist_ok=True)
run(f"COPY ({user_daily}) TO '{os.path.join(OUT, 'user_daily', 'part-0.parquet')}' (FORMAT parquet)")
run(f"COPY ({campaign_daily}) TO '{os.path.join(OUT, 'campaign_daily', 'part-0.parquet')}' (FORMAT parquet)")
print("wrote user_daily and campaign_daily", file=__import__("sys").stderr)
