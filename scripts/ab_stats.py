#!/usr/bin/env python3
"""Summarise vm_ab.sh output: per binary, min/median wall and median per phase."""
import sys, statistics as st
from collections import defaultdict
d = defaultdict(lambda: defaultdict(list))
for line in sys.stdin:
    parts = line.split()
    if len(parts) < 2 or "=" not in parts[1]:
        continue
    for kv in parts[1:]:
        k, v = kv.split("=")
        d[parts[0]][k].append(float(v))
for b, m in d.items():
    w = m.pop("wall")
    phases = " ".join(f"{k}={st.median(v):.1f}" for k, v in m.items())
    print(f"{b:10} wall min={min(w):.0f} med={st.median(w):.0f} n={len(w)} | {phases}")
