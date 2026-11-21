#!/usr/bin/env python3
"""
Independent, deliberately simple reference implementation of rules.md.

Pure Python logic (polars is only used to write the Parquet output). Written
straight from rules.md without looking at the real pipeline, so the two can be
diffed on many seeds: any disagreement is a bug in one of them.

Slow by design; fine up to a few hundred thousand events.
Usage: DATA_DIR=... OUTPUT_DIR=... python reference.py
"""
import bisect
import datetime
import glob
import json
import os
import re
from collections import defaultdict

DAY_MS = 86_400_000
GAP_MS = 1_800_000
LOOKBACK_MS = 7 * DAY_MS
LATE_MS = DAY_MS
MAX_PRICE = 10_000_000
EVENT_TYPES = {"view", "cart", "purchase"}


# ------------------------------------------------------------------ CSV dialect

def read_lines(path):
    """Every line of a file except the header. Only \n ends a line."""
    with open(path, "rb") as f:
        text = f.read().decode("utf-8", errors="surrogateescape")
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()  # the newline after the last line does not start another line
    return lines[1:]


def parse_line(line, n):
    """List of n values (None for empty), or None if the line is malformed."""
    if line == "":
        return None
    fields = line.split(",")
    if len(fields) != n:
        return None
    out = []
    for field in fields:
        if '"' in field:
            if len(field) < 2 or field[0] != '"' or field[-1] != '"' or '"' in field[1:-1]:
                return None
            field = field[1:-1]
        out.append(field if field != "" else None)
    return out


def read_table(pattern, n):
    rows = []
    for path in sorted(glob.glob(pattern)):
        for line in read_lines(path):
            row = parse_line(line, n)
            if row is not None:
                rows.append(row)
    return rows


# ------------------------------------------------------------------ timestamps

def is_digits(s):
    return s != "" and all("0" <= c <= "9" for c in s)


def days_from_civil(y, m, d):
    y -= m <= 2
    era = y // 400
    yoe = y - era * 400
    doy = (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def days_in_month(y, m):
    if m == 2:
        return 29 if (y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)) else 28
    return 30 if m in (4, 6, 9, 11) else 31


TS_RE = re.compile(r"([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})(Z|[+-][0-9]{2}:[0-9]{2})", re.ASCII)


def parse_ts(s):
    """Epoch ms, or None for a bad timestamp (window not checked here)."""
    if s is None:
        return None
    if is_digits(s):
        return int(s)  # F3; may be huge or tiny, the window check rejects it
    m = TS_RE.fullmatch(s)
    if not m:
        return None
    y, mo, d, h, mi, sec = (int(m.group(i)) for i in range(1, 7))
    if not (1 <= mo <= 12 and 1 <= d <= days_in_month(y, mo) and h <= 23 and mi <= 59 and sec <= 59):
        return None
    ms = ((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + sec * 1000
    zone = m.group(7)
    if zone != "Z":
        oh, om = int(zone[1:3]), int(zone[4:6])
        if oh > 23 or om > 59:
            return None
        off = (oh * 60 + om) * 60_000
        ms -= off if zone[0] == "+" else -off
    return ms


# ------------------------------------------------------------------ pipeline

def main():
    data = os.environ.get("DATA_DIR", "/data")
    out = os.environ.get("OUTPUT_DIR", "/output")

    with open(os.path.join(data, "manifest.json")) as f:
        manifest = json.load(f)
    ws, we = parse_ts(manifest["window_start"]), parse_ts(manifest["window_end"])

    products = {r[0]: r[1] for r in read_table(os.path.join(data, "products.csv"), 2)}
    channels = {r[0]: r[1] for r in read_table(os.path.join(data, "campaigns.csv"), 2)}

    # Linked IDs: union-find, person named by the smallest ID.
    parent = {}

    def find(x):
        root = x
        while parent.get(root, root) != root:
            root = parent[root]
        while parent.get(x, x) != root:  # path compression
            parent[x], x = root, parent[x]
        return root

    for a, b in read_table(os.path.join(data, "links", "*.csv"), 2):
        if a is None or b is None:
            continue
        ra, rb = find(a), find(b)
        if ra != rb:
            # keep the smaller ID as root, so the root is the person's name
            if rb < ra:
                ra, rb = rb, ra
            parent[rb] = ra
    person = find

    # Events: parse, dedup on event_id (smallest ingest_ts), validate, drop late.
    kept = {}
    for row in read_table(os.path.join(data, "events", "*.csv"), 7):
        event_id = row[0]
        if event_id is None:
            continue
        ingest = parse_ts(row[3])
        prev = kept.get(event_id)
        if prev is None or ingest < prev[0]:
            kept[event_id] = (ingest, row)

    by_person = defaultdict(list)  # person -> [(ts, category or None, price)]
    for ingest, (event_id, user_id, ts_s, _ing, etype, product_id, price_s) in kept.values():
        if user_id is None or product_id is None or etype not in EVENT_TYPES:
            continue
        ts = parse_ts(ts_s)
        if ts is None or not (ws <= ts < we):
            continue
        price = 0
        if etype == "purchase":
            if price_s is None or not is_digits(price_s):
                continue
            price = int(price_s)
            if not (1 <= price <= MAX_PRICE):
                continue
        if ingest - ts > LATE_MS:
            continue
        category = products.get(product_id, "unknown")
        by_person[person(user_id)].append((ts, category, price, etype == "purchase"))

    # Clicks: parse, dedup on click_id, validate.
    seen_clicks = set()
    clicks_by_person = defaultdict(list)  # person -> [(ts, click_id, campaign)]
    for click_id, user_id, campaign_id, ts_s in read_table(os.path.join(data, "clicks", "*.csv"), 4):
        if click_id is None or click_id in seen_clicks:
            continue
        seen_clicks.add(click_id)
        if user_id is None or campaign_id not in channels:
            continue
        ts = parse_ts(ts_s)
        if ts is None or not (ws - LOOKBACK_MS <= ts < we):
            continue
        clicks_by_person[person(user_id)].append((ts, click_id, campaign_id))
    for lst in clicks_by_person.values():
        lst.sort()

    # Sessionize, attribute, aggregate.
    user_daily = {}       # (person, day) -> [sessions, events, revenue, {category: cents}]
    campaign_daily = {}   # (campaign, day) -> [sessions, purchasing, revenue, assisted]

    def camp_row(key):
        if key not in campaign_daily:
            campaign_daily[key] = [0, 0, 0, 0]
        return campaign_daily[key]

    for who, events in by_person.items():
        events.sort(key=lambda e: e[0])
        sessions = []
        for ev in events:
            if sessions and ev[0] - sessions[-1][-1][0] <= GAP_MS:
                sessions[-1].append(ev)
            else:
                sessions.append([ev])
        clicks = clicks_by_person.get(who, [])
        click_ts = [c[0] for c in clicks]
        for sess in sessions:
            start = sess[0][0]
            day = start // DAY_MS
            revenue = sum(e[2] for e in sess if e[3])
            purchased = any(e[3] for e in sess)

            u = user_daily.setdefault((who, day), [0, 0, 0, defaultdict(int)])
            u[0] += 1
            u[1] += len(sess)
            u[2] += revenue
            for e in sess:
                if e[3]:
                    u[3][e[1]] += e[2]

            lo = bisect.bisect_left(click_ts, start - LOOKBACK_MS)
            hi = bisect.bisect_right(click_ts, start)
            if hi > lo:
                latest = click_ts[hi - 1]
                first = bisect.bisect_left(click_ts, latest)  # clicks sorted by (ts, click_id)
                credited = clicks[first][2]
                for campaign in {clicks[i][2] for i in range(lo, hi)}:
                    camp_row((campaign, day))[3] += 1
            else:
                credited = "organic"
            c = camp_row((credited, day))
            c[0] += 1
            c[1] += 1 if purchased else 0
            c[2] += revenue

    import polars as pl  # only for writing Parquet

    epoch = datetime.date(1970, 1, 1)
    ud_rows = []
    for (who, day), (s, n, rev, cats) in user_daily.items():
        top = min(cats, key=lambda k: (-cats[k], k)) if cats else None
        ud_rows.append((who, epoch + datetime.timedelta(days=day), s, n, rev, top))
    cd_rows = []
    for (campaign, day), (s, p, rev, a) in campaign_daily.items():
        channel = "organic" if campaign == "organic" else channels[campaign]
        cd_rows.append((campaign, channel, epoch + datetime.timedelta(days=day), s, p, rev, a))

    tables = {
        "user_daily": (ud_rows, {"user_id": pl.String, "day": pl.Date, "sessions": pl.Int64, "events": pl.Int64,
                                 "revenue_cents": pl.Int64, "top_category": pl.String}),
        "campaign_daily": (cd_rows, {"campaign_id": pl.String, "channel": pl.String, "day": pl.Date,
                                     "sessions": pl.Int64, "purchasing_sessions": pl.Int64,
                                     "revenue_cents": pl.Int64, "assisted_sessions": pl.Int64}),
    }
    for name, (rows, schema) in tables.items():
        folder = os.path.join(out, name)
        os.makedirs(folder, exist_ok=True)
        pl.DataFrame(rows, schema=schema, orient="row").write_parquet(os.path.join(folder, "part-0.parquet"))


if __name__ == "__main__":
    main()
