# Benchmark the 2.88-second engine on Linux

This branch adds measurement tools only. `rust/`, `Dockerfile` and the generator
are unchanged from `5cf2f652a7b658d58fd1bac058e3131eb850a877`, the reported
2.88-second competition submission. Do not spend a submission on this branch.
The previous exact-identities candidate scored 3.24 seconds and is not included.

## First run: profile the baseline

Use a native Linux x86-64 machine with Docker, Git and Python 3. Docker must be
accessible without an interactive sudo prompt and use the local daemon. At least
8 available logical CPUs and 24 GiB available to Docker are required; a host with
32 GiB RAM or more is recommended. Allow roughly 10 GB free disk space for image
builds, generated data and output. The script refuses ARM/emulation and undersized
hosts rather than reporting misleading substitute measurements.

From a fresh directory:

```sh
git clone --branch perf/linux-benchmark https://github.com/arv20/databricksTrack-Joinbuilders.git sessions-linux-benchmark
cd sessions-linux-benchmark
python3 scripts/linux_benchmark.py
```

The final command builds the unchanged baseline and its test tools, runs all 36
fixtures, generates 20M events / 5M clicks with seed 3, warms up, and measures three
runs. No Rust or Python packages need to be installed on the host. Image builds and
data generation can take several minutes and are not timed.

Send back **`work/linux-benchmark-*/report.json` and the `baseline-*.log` files**.
If it fails, send `report.json` plus the named build/check log. A failed run is never
reported as a successful comparison. Keep the work folder for future comparisons.

The Docker workflow still needs its first end-to-end Linux execution. Unit tests
for the report calculations and preflight checks pass on the development Mac;
that does not establish Docker integration or performance on Linux.

## Compare a future candidate

After the next candidate has been committed and pushed, fetch it and pass its full
SHA (replace `<candidate-sha>`). Reuse the first run's data by replacing `<data-dir>`:

```sh
git fetch origin
python3 scripts/linux_benchmark.py --candidate <candidate-sha> --data <data-dir> --rounds 5
```

For example, the data directory will be under `work/linux-benchmark-.../data`.
Each invocation creates a new report/output folder. The harness uses **committed
snapshots**, never uncommitted edits. It builds both versions, checks both against
the fixtures, warms both, then alternates baseline/candidate and candidate/baseline
order. Every output is compared exactly against the first baseline output, outside
the timed interval. Any mismatch or process failure stops the comparison.

Do not run builds, generators or other benchmarks alongside these measurements.
Repeat a promising change on another seed before using a practice submission:

```sh
python3 scripts/linux_benchmark.py --candidate <candidate-sha> --seed 249 --rounds 5
```

For a final-size check, set `--events 60000000 --clicks 15000000` and allow additional
disk space. The script gives each timed container up to 240 seconds to produce a
diagnostic result; the actual competition final limit remains **180 seconds**.

## What is measured

- The submitted runtime image plus GNU `time`, with its original pipeline binary.
- `linux/amd64`, uid 1000, no network, read-only root, 64 MB `/tmp`, 8 pinned logical
  CPUs, 24 GiB memory with swap disabled, and the competition directory/environment layout.
- GNU time's process wall time, user/system CPU time, max RSS and major page faults.
  Docker command wall time is recorded separately and is **not** the score estimate.
- `TIMING=1` cumulative stage logs, converted into per-stage durations in the report.
- Exact version SHAs, image IDs, CPU topology, limits, seed and run order.

Peak process RSS is not total cgroup memory including mapped file cache. The first
8 logical CPUs available to this process are pinned consistently for both versions;
their topology may differ from the scorer's 8 Xeon hyperthreads. Caches are warmed,
not flushed. Neither generated data nor another Linux CPU reproduces the hidden
scorer exactly. Keep the 2.88-second official entry until a new official result wins.

The next performance change should target the largest measured stage and be tested
individually. A lower median with overlapping/noisy ranges is not sufficient proof
of an improvement; examine all runs and repeat before submitting.

## Test the harness calculations

```sh
python3 -m unittest discover -s scripts/tests -v
```
