# Benchmark and validation results — 2026-11-20

**20M-event median: 14.87 s → 4.79 s (3.10× faster; 67.8% less wall time).**
These are native Apple M3 measurements on an 8 GB laptop, not hidden-score or
Linux-container results. Intel performance and the final score still need to be
checked on the competition runner. No claim of a guaranteed cross-platform speedup
is made.

## Repeated comparison

Baseline: `5cf2f652a7b658d58fd1bac058e3131eb850a877`. Both binaries were built
with Rust 1.100.0 using the same release profile. `NCPU=8`, seed 3, 20,000,000
requested events and 5,000,000 requested clicks. Generator-added dirt and duplicates
are included in the actual input. Generation, compilation and checking were outside
the timed region.

| Run within version | Baseline | Optimized |
| --- | ---: | ---: |
| 1 | 25.07 s | 5.82 s |
| 2 | 14.87 s | 4.79 s |
| 3 | 13.98 s | 4.76 s |
| Median | **14.87 s** | **4.79 s** |

Execution order was optimized, baseline, baseline, optimized, optimized, baseline.
No builds, generators or other benchmark runs ran concurrently. OS file caches,
thermal state and other desktop activity were not controlled. The difference
between early and later baseline times illustrates why one timing is insufficient.

The final outputs matched exactly: 2,255,788 user/day rows and 28,014 campaign/day
rows, with no duplicate keys. Raw timings, CPU times, page faults and memory metrics
are in [benchmarks/2026-11-20-m3.json](benchmarks/2026-11-20-m3.json).

## Final-size stress test

Seed 3, 60,000,000 requested events and 15,000,000 requested clicks:

- Wall time: **90.59 seconds**.
- macOS peak memory footprint: **5,211,840,792 bytes** (5.21 GB decimal).
- User/day rows: 6,682,907. Campaign/day rows: 28,014.
- Both tables sum to 10,122,826 sessions and 19,335,517,459 revenue cents.
- Unique-key, DATE-type, nonnegative-count and attribution invariants passed.

This laptop used OS swap. Its memory metrics are not Linux cgroup peak memory,
and this run does not verify the scorer's no-swap limit. The 60M output was checked
for invariants, not compared in full to an independent 60M reference. The generator
also ran slowly under memory pressure; its time is excluded.

## Correctness checks

- Original 36 hand-written fixtures: **36/36**.
- Same 36 fixtures transformed to long UTF-8 event, user and click IDs: **36/36**.
- Exact fallback engine on the original fixtures: **36/36**.
- 1M events / 250k clicks, seed 1: **exact match** to the independent Python reference.
- 300k events / 75k clicks for seeds 2–7, 15, 62, 249: **9/9 exact matches** to
  the reference, including windows spanning February 29, 2028.
- Deliberately forced equal hashes for distinct event IDs, unlinked people, link
  IDs, link lookups and click owners: the fast engine rejects the ambiguous run
  so the exact fallback can handle it. This regression test failed before the fix.
- Inline and long identity tests cover length boundaries, embedded zero bytes,
  distinct allocations with equal bytes and UTF-8 values.

The published `dev-answer.zip` was not available in this checkout. The 1M check
above uses `reference/reference.py`; it is not a claim of a fresh public-answer check.
The linux/amd64 image has not been built or run on this Mac.

## What changed

Short identities are stored exactly in records instead of recovered from scattered
CSV lines. Long identities retain exact byte comparison and are copied into owned
storage when needed for output. Hashes select partitions; they never establish
identity by themselves. Processed shards/partitions are released as work advances,
and large partitions are scheduled ahead of small ones. Output writing no longer
reparses event lines to recover names.

These changes preserve the existing session and attribution algorithms. They
reduce random reads and memory retained between stages; the larger exact records
mean peak memory should still be measured on Linux rather than inferred from the
buffer-lifetime improvement alone.

## Check on the Linux runner

Follow the native and container commands in [README.md](README.md). For a fair
container comparison, build both revisions with the same toolchain and use the
same generated data, 8 CPUs, 24 GB, no swap, read-only root and no network. Alternate
run order and compare the output tables exactly after timing.

Use a fresh output directory for each dataset/thread-count combination. Validate
all fixtures and the public dev answer before spending a practice submission.
The 20M practice target is below 60 seconds; test several 60M seeds against the
180-second limit before relying on a final-score prediction.
