#!/usr/bin/env python3
"""Independent pure-Python reference for the Shopping Sessions rules (stdlib + pyarrow)."""
import json
import os
import sys
from bisect import bisect_left, bisect_right

DAY_MS = 86400000
WEEK_MS = 7 * DAY_MS
GAP_MS = 1800000
LATE_MS = DAY_MS
MAX_PRICE = 10000000


def split_line(line, n):
    """Split a line (bytes) into n fields with CSV-dialect quote handling; None if malformed."""
    parts = line.split(b',')
    if len(parts) != n:
        return None
    for i, p in enumerate(parts):
        if b'"' in p:
            if len(p) < 2 or p[0] != 34 or p[-1] != 34 or b'"' in p[1:-1]:
                return None
            parts[i] = p[1:-1]
    return parts


def read_rows(path, n):
    with open(path, 'rb') as f:
        data = f.read()
    lines = data.split(b'\n')
    if lines and lines[-1] == b'':
        lines.pop()          # trailing newline artifact (or empty file)
    for line in lines[1:]:   # skip header
        r = split_line(line, n)
        if r is not None:
            yield r


def csv_files(d):
    if not os.path.isdir(d):
        return []
    return [os.path.join(d, f) for f in sorted(os.listdir(d)) if f.endswith('.csv') and os.path.isfile(os.path.join(d, f))]


def all_digits(b):
    if not b:
        return False
    for c in b:
        if c < 48 or c > 57:
            return False
    return True


def num(b):
    """digits-only bytes (known ASCII digits) -> int"""
    return int(b)


def days_from_civil(y, m, d):
    y -= m <= 2
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    doy = (153 * (m + (-3 if m > 2 else 9)) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def is_leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


MDAYS = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]


def parse_ts(b):
    """bytes -> epoch ms or None"""
    n = len(b)
    if n == 0:
        return None
    if all_digits(b):
        return int(b)
    if n != 20 and n != 25:
        return None
    # YYYY-MM-DDTHH:MM:SS
    for i in (0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18):
        c = b[i]
        if c < 48 or c > 57:
            return None
    if b[4] != 45 or b[7] != 45 or b[10] != 84 or b[13] != 58 or b[16] != 58:
        return None
    y = int(b[0:4]); mo = int(b[5:7]); d = int(b[8:10])
    hh = int(b[11:13]); mi = int(b[14:16]); ss = int(b[17:19])
    if mo < 1 or mo > 12:
        return None
    dim = MDAYS[mo - 1] + (1 if mo == 2 and is_leap(y) else 0)
    if d < 1 or d > dim or hh > 23 or mi > 59 or ss > 59:
        return None
    off = 0
    if n == 20:
        if b[19] != 90:
            return None
    else:
        sign = b[19]
        if sign != 43 and sign != 45:
            return None
        if b[22] != 58:
            return None
        for i in (20, 21, 23, 24):
            if b[i] < 48 or b[i] > 57:
                return None
        oh = int(b[20:22]); om = int(b[23:25])
        if oh > 23 or om > 59:
            return None
        off = (oh * 60 + om) * 60000
        if sign == 45:
            off = -off
    return ((days_from_civil(y, mo, d) * 86400 + hh * 3600 + mi * 60 + ss) * 1000) - off


def main():
    args = sys.argv[1:]
    data_dir = os.environ.get('DATA_DIR')
    out_dir = os.environ.get('OUTPUT_DIR')
    if len(args) >= 2:
        data_dir, out_dir = args[-2], args[-1]
    if not data_dir or not out_dir:
        sys.exit('usage: pyref.py DATA_DIR OUT_DIR')

    with open(os.path.join(data_dir, 'manifest.json'), 'rb') as f:
        man = json.loads(f.read().decode('utf-8'))
    w_start = parse_ts(man['window_start'].encode())
    w_end = parse_ts(man['window_end'].encode())

    # products / campaigns
    cat_of = {}
    for r in read_rows(os.path.join(data_dir, 'products.csv'), 2):
        cat_of[r[0]] = r[1]
    chan_of = {}
    for r in read_rows(os.path.join(data_dir, 'campaigns.csv'), 2):
        chan_of[r[0]] = r[1]

    # links -> union-find
    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    for p in csv_files(os.path.join(data_dir, 'links')):
        for a, b in read_rows(p, 2):
            if not a or not b:
                continue
            if a not in parent:
                parent[a] = a
            if b not in parent:
                parent[b] = b
            ra, rb = find(a), find(b)
            if ra != rb:
                parent[ra] = rb
    comp_min = {}
    for x in parent:
        r = find(x)
        if r not in comp_min or x < comp_min[r]:
            comp_min[r] = x
    person_cache = {}

    def person(uid):
        v = person_cache.get(uid)
        if v is None:
            v = comp_min[find(uid)] if uid in parent else uid
            person_cache[uid] = v
        return v

    # events: parse + dedup on event_id (smallest ingest_ts)
    best = {}
    INF = float('inf')
    for p in csv_files(os.path.join(data_dir, 'events')):
        for r in read_rows(p, 7):
            eid = r[0]
            if not eid:
                continue
            ing = parse_ts(r[3])
            if ing is None:
                ing = INF
            cur = best.get(eid)
            if cur is None or ing < cur[0]:
                best[eid] = (ing, r)
    per_events = {}  # person -> list of (ts, price_or_0, category_or_None)
    for ing, r in best.values():
        uid, tsb, etype, pid, priceb = r[1], r[2], r[4], r[5], r[6]
        if not uid or not pid:
            continue
        ts = parse_ts(tsb)
        if ts is None or ts < w_start or ts >= w_end:
            continue
        price = 0
        cat = None
        if etype == b'purchase':
            if not all_digits(priceb):
                continue
            price = int(priceb)
            if price < 1 or price > MAX_PRICE:
                continue
            cat = cat_of.get(pid, b'unknown')
        elif etype != b'view' and etype != b'cart':
            continue
        if ing == INF:
            continue
        if ing - ts > LATE_MS:
            continue
        per_events.setdefault(person(uid), []).append((ts, price, cat))
    del best

    # clicks
    seen_click = set()
    per_clicks = {}  # person -> list of (ts, click_id, campaign)
    c_lo = w_start - WEEK_MS
    for p in csv_files(os.path.join(data_dir, 'clicks')):
        for r in read_rows(p, 4):
            cid, uid, camp, tsb = r[0], r[1], r[2], r[3]
            if not cid or cid in seen_click:
                continue
            seen_click.add(cid)
            if not uid:
                continue
            ts = parse_ts(tsb)
            if ts is None or ts < c_lo or ts >= w_end:
                continue
            if camp not in chan_of:
                continue
            per_clicks.setdefault(person(uid), []).append((ts, cid, camp))
    del seen_click
    click_ts = {}
    click_rows = {}
    for pr, lst in per_clicks.items():
        lst.sort(key=lambda t: (t[0], t[1]))
        click_rows[pr] = lst
        click_ts[pr] = [t[0] for t in lst]

    # sessions
    ud = {}   # (person, day) -> [sessions, events, revenue, {cat: sum}]
    cd = {}   # (campaign, day) -> [sessions, purchasing, revenue, assisted]
    for pr, evs in per_events.items():
        evs.sort(key=lambda t: t[0])
        sessions = []
        cur = None
        prev = None
        for ts, price, cat in evs:
            if cur is None or ts - prev > GAP_MS:
                cur = [ts, 0, 0, 0, {}]   # start, events, revenue, purchases, cats
                sessions.append(cur)
            prev = ts
            cur[1] += 1
            if cat is not None:
                cur[2] += price
                cur[3] += 1
                cur[4][cat] = cur[4].get(cat, 0) + price
        tsl = click_ts.get(pr)
        crow = click_rows.get(pr)
        for start, nev, rev, npur, cats in sessions:
            day = start // DAY_MS
            u = ud.get((pr, day))
            if u is None:
                u = ud[(pr, day)] = [0, 0, 0, {}]
            u[0] += 1
            u[1] += nev
            u[2] += rev
            for c, v in cats.items():
                u[3][c] = u[3].get(c, 0) + v
            credit = b'organic'
            if tsl:
                hi = bisect_right(tsl, start)
                lo = bisect_left(tsl, start - WEEK_MS)
                if hi > lo:
                    top_ts = tsl[hi - 1]
                    g = bisect_left(tsl, top_ts, lo, hi)
                    credit = crow[g][2]
                    for camp in set(t[2] for t in crow[lo:hi]):
                        k = (camp, day)
                        row = cd.get(k)
                        if row is None:
                            row = cd[k] = [0, 0, 0, 0]
                        row[3] += 1
            k = (credit, day)
            row = cd.get(k)
            if row is None:
                row = cd[k] = [0, 0, 0, 0]
            row[0] += 1
            if npur:
                row[1] += 1
            row[2] += rev

    # output
    import pyarrow as pa
    import pyarrow.parquet as pq

    def s(b):
        return b.decode('utf-8', 'replace')

    def date_arr(days):
        return pa.array(days, pa.int32()).cast(pa.date32())

    rows = sorted(ud.items(), key=lambda kv: (kv[0][0], kv[0][1]))
    tops = []
    for (pr, day), (ns, ne, rev, cats) in rows:
        if cats:
            m = max(cats.values())
            tops.append(s(min(c for c, v in cats.items() if v == m)))
        else:
            tops.append(None)
    t1 = pa.table({
        'user_id': pa.array([s(k[0]) for k, _ in rows], pa.string()),
        'day': date_arr([k[1] for k, _ in rows]),
        'sessions': pa.array([v[0] for _, v in rows], pa.int64()),
        'events': pa.array([v[1] for _, v in rows], pa.int64()),
        'revenue_cents': pa.array([v[2] for _, v in rows], pa.int64()),
        'top_category': pa.array(tops, pa.string()),
    })
    rows = sorted(cd.items(), key=lambda kv: (kv[0][0], kv[0][1]))
    t2 = pa.table({
        'campaign_id': pa.array([s(k[0]) for k, _ in rows], pa.string()),
        'channel': pa.array([s(chan_of.get(k[0], b'organic')) for k, _ in rows], pa.string()),
        'day': date_arr([k[1] for k, _ in rows]),
        'sessions': pa.array([v[0] for _, v in rows], pa.int64()),
        'purchasing_sessions': pa.array([v[1] for _, v in rows], pa.int64()),
        'revenue_cents': pa.array([v[2] for _, v in rows], pa.int64()),
        'assisted_sessions': pa.array([v[3] for _, v in rows], pa.int64()),
    })
    for name, t in (('user_daily', t1), ('campaign_daily', t2)):
        d = os.path.join(out_dir, name)
        os.makedirs(d, exist_ok=True)
        pq.write_table(t, os.path.join(d, 'part-0.parquet'))


if __name__ == '__main__':
    main()
