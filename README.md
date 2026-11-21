# Shopping Sessions: Rust pipeline

An exact CSV-to-Parquet pipeline for the Shopping Sessions buildathon. **`rules.md`
is the specification.** The root Dockerfile builds the Rust engine in `rust/`;
`pipeline/pipeline.py` and `Dockerfile.python` preserve the older DuckDB alternative.
`PLAN.md` is historical and does not describe the current engine.

The pipeline resolves linked IDs, deduplicates and validates events, builds
30-minute sessions, applies seven-day last-click attribution and campaign assists,
and writes `user_daily/` and `campaign_daily/` as Parquet with DATE columns.

## Build and check locally

Use a current stable Rust toolchain and Python 3.10+ with Polars. Run from the repo
root. Generated datasets, binaries, environments and outputs are gitignored.

```sh
python3 -m venv .venv
.venv/bin/python -m pip install polars
cargo build --release --locked --manifest-path rust/Cargo.toml
cargo build --release --locked --manifest-path generator/Cargo.toml
cargo test --release --locked --manifest-path rust/Cargo.toml
.venv/bin/python scripts/run_fixtures.py fixtures -- rust/target/release/rproto
.venv/bin/python scripts/test_long_ids.py -- rust/target/release/rproto
PIPELINE=rust/target/release/rproto reference/cross_check.sh
```

The last command compares nine generated seeds against the independent Python
reference, including leap-day windows. It deletes each generated seed after use.
`FORCE_SLOW=1` selects the exact fallback engine for additional validation.

For the public 1M development answer, obtain `dev-answer.zip` from the competition,
unzip it as `dev-answer/`, then:

```sh
generator/target/release/generate --seed 1 --events 1000000 --clicks 250000 --output dev
DATA_DIR=dev OUTPUT_DIR=out NCPU=8 rust/target/release/rproto
make check-dev
```

## Reproduce a practice-size benchmark

Use a fresh output directory for each different dataset or thread count. Output
files from an earlier run must not be mixed with the new run's files.

```sh
generator/target/release/generate --seed 3 --events 20000000 --clicks 5000000 --output big
DATA_DIR=big OUTPUT_DIR=out NCPU=8 TIMING=1 /usr/bin/time -v rust/target/release/rproto
```

On macOS use `/usr/bin/time -l` instead of `-v`. `TIMING=1` prints cumulative stage
elapsed times to stderr. Build time, generation and correctness comparisons are
outside the measured run. See [BENCHMARKS.md](BENCHMARKS.md) for measured results
and the differences between local hardware and the scorer.

## Verify the submitted container

```sh
make build                         # linux/amd64
make fixtures                      # all 36 fixtures, scorer-like restrictions
CPUS=8 MEM=24g scripts/docker_run.sh big out-container
.venv/bin/python scripts/compare.py out out-container
```

The container runs as uid 1000 with read-only root, no network, a 64 MB `/tmp`,
read-only `/data`, and writable `/output` and `/scratch`. Defaults are 8 CPUs and
24 GB with no swap. Set `CPUS`/`MEM` lower if your development machine cannot
provide those resources. Apple-silicon native container builds use
`PLATFORM=linux/arm64`; they do not establish amd64 performance.

## Implementation

- Memory-mapped inputs and parallel CSV parsing; quoted fields, malformed lines
  and timestamp validation follow `rules.md` rather than a general CSV reader.
- Union-find resolves linked IDs to the byte-wise smallest name.
- Hash partitions accelerate deduplication and person grouping. Hash equality is
  checked against exact bytes; a collision triggers the exact fallback.
- IDs up to 15 bytes are copied into records. Longer IDs remain fully supported.
- Consumed shards and partitions are released as the next stage progresses.
- Sessions, attribution, assists and aggregates are computed together per partition.
- Output names are retained independently of the input mappings, avoiding random
  input rereads during Parquet writing.

`fixtures/` contains 36 hand-written edge cases. `reference/reference.py` is an
independent correctness oracle. `ship.sh` is an older convenience script that
commits and pushes after checks; the commands above allow checking without those
side effects.
