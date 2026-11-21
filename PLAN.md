# Shopping Sessions: plan

Last updated 2026-11-20.

## Where things stand

| Item | Status |
|---|---|
| Engine | **DuckDB.** `solution/pipeline/pipeline.py` (written by another Claude session) |
| 36 edge-case fixtures | **36/36 pass** (checked natively, no Docker) |
| 1M-event dev set (`dev/`, seed 1) | **Exact MATCH** with `dev-answer/`; 4.3 s on an M4 |
| Other seeds (stage 3) | **Done.** Independent reference `reference/reference.py` (36/36, dev MATCH) agrees with the pipeline on seeds 2-7, 15, 62, 249 at 300k events and seeds 3, 249 at 1M. Seeds 15/62/249 contain 2028-02-29. Re-run with `reference/cross_check.sh` after every pipeline change |
| Who is doing what | Session `databrickstrack-ad` owns `solution/pipeline/pipeline.py` and is on stage 4 (speed at 20M events). No repo restructuring until both sessions agree |
| Generator | Built at `generator/target/release/generate` |
| Local Python | `.venv/` (Python 3.12, same as the Docker image): duckdb 1.5.6, polars 2.1.0, numpy 2.5.5, pyarrow 26.0.1, scipy 1.18.1 |
| Docker | **Not installed** |
| Disk | **99% full, ~2.8 GB free.** Blocks Docker and big test sets |
| GitHub | No remote yet; local branch is `master` |

The rules logic is basically done. What's left is packaging, robustness on other seeds, speed at full size, and submitting.

## Your to-dos (Claude can't do these)

1. **Free 20+ GB of disk.** Docker Desktop needs roughly 10 GB, and a 10M-event test set is about 1 GB.
2. **Install Docker Desktop** for Apple silicon (docker.com). In Settings > Resources give it 8 CPUs and 12 GB RAM (you have 16 GB).
3. **Create an empty public GitHub repo** and give Claude its URL in stage 5.
4. **Run one Claude session at a time on this folder.** Two sessions doing the same work costs double.

## Stages

Run each stage in a **fresh** Claude Code session (`/clear` between stages) and commit when it passes.
Copy and paste the prompt for each stage.

### Stage 1: make the repo submittable (cheap, use Sonnet)

> Read PLAN.md. Restructure the repo so its root is the submittable DuckDB project: move `starter-duckdb/`'s Dockerfile, Makefile, run.sh, .dockerignore, scripts/, fixtures/, README.md and TEAM.md to the root. Put `solution/pipeline/pipeline.py` at `pipeline/pipeline.py`. Pin `duckdb==1.5.6 numpy==2.5.5 pyarrow==26.0.1 scipy==1.18.1` in requirements.txt. Move `starter-polars/`, `starter-pyspark/` and the now-empty `starter-duckdb/` and `solution/` into `_archive/` (gitignored). Rename branch `master` to `main`. Write a short CLAUDE.md (see "CLAUDE.md contents" below). Verify with `.venv/bin/python scripts/run_fixtures.py fixtures -- .venv/bin/python pipeline/pipeline.py 2>&1 | grep -v '^ok'`. Commit.

### Stage 2: Docker image, native arm64 (after you install Docker)

> Build and test the image natively: `make build fixtures PLATFORM=linux/arm64`, then `make run-dev check-dev PLATFORM=linux/arm64 CPUS=8 MEM=12g`. Only show me failures and the timing line. Fix anything that breaks only in the container (read-only root, uid 1000, no network, /tmp only 64 MB). Commit.

### Stage 3: robustness across seeds (the main correctness risk, use Opus)

There's only one published answer (seed 1). Other seeds bring windows with Feb 29 2028, different whales and shared-device mega-persons, and different dirt rates. Plan: write a **second, independent, deliberately simple** implementation (pure Python or Polars, no shared code) and diff the two on several seeds. Any disagreement is a bug in one of them.

> Read rules.md and PLAN.md stage 3. Write `scripts/reference.py`: an independent, simple, slow, obviously-correct implementation of rules.md in pure Python (no DuckDB, don't read pipeline/pipeline.py). Confirm it passes all fixtures and matches dev-answer. Then for seeds 2 to 8, generate `--events 200000 --clicks 50000` into `seeds/N` (delete each after use; disk is tight), run both, and compare with `scripts/compare.py`. Make sure at least one seed has a window containing 2028-02-29 (look at the generated manifest.json). For each mismatch, find the rule in rules.md that decides it and fix whichever side is wrong. Show me only mismatch summaries.

### Stage 4: speed at full size

The final run is at a bigger, unknown scale under 8 vCPU and 24 GB with no swap. Find out where the time goes and make sure memory can't blow up.

> Generate `--seed 3 --events 10000000 --clicks 2500000 --output big` (about 1 GB). Run `pipeline/pipeline.py` natively with `NCPU=8 MEM_GB=12` and print per-step timings (add an env-gated timer). Optimize the slowest steps without changing results. After each change, re-run fixtures and `make check-dev`, both must still pass exactly. Watch peak memory (`/usr/bin/time -l`); it must scale safely to 24 GB at larger sizes. Report a before/after timing table. Commit each win separately.

Ideas worth checking: read each CSV exactly once; avoid the Python round trip for linked IDs on giant components; check that `preserve_insertion_order=false` and the thread count actually apply; write Parquet without needless sorts; time the shared-device mega-person that shows up at full size.

### Stage 5: amd64 check and submit

> Build the real scorer image `make build` (linux/amd64, emulated: slow, that's fine) and run `make fixtures`. Fill in TEAM.md. Add the GitHub remote `<URL>`, push `main`, and print the commit SHA to submit.

Then submit the repo URL and SHA on the track page. After it's scored, optional stage 6: use the leaderboard time to decide whether more stage-4 work is worth it.

## CLAUDE.md contents (stage 1 writes this)

```
Shopping Sessions buildathon. rules.md is the spec and always wins. Engine: DuckDB, single file pipeline/pipeline.py.
Python: always .venv/bin/python (3.12). Fixtures: .venv/bin/python scripts/run_fixtures.py fixtures -- .venv/bin/python pipeline/pipeline.py 2>&1 | grep -v '^ok'
Dev check: DATA_DIR=dev OUTPUT_DIR=out SCRATCH_DIR=/tmp NCPU=8 MEM_GB=12 .venv/bin/python pipeline/pipeline.py && (cd scripts && ../.venv/bin/python compare.py ../out ../dev-answer)
Generator: generator/target/release/generate --seed N --events E --clicks C --output DIR
NEVER cat/read data files (dev/, big/, seeds/, dev-answer/, fixtures/*/input with many rows): use head -5, wc -l, or compare.py. Pipe long command output through tail/grep.
Disk is nearly full: delete generated datasets after use. Ignore _archive/.
Mac is Apple silicon: use PLATFORM=linux/arm64 for local Docker loops; linux/amd64 only before submit.
```

## Credit rules

- One session per stage, `/clear` between stages, and no subagents or workflows.
- Never let Claude read the big CSVs. Summaries only (`compare.py`, `wc -l`, `head`).
- When debugging, name the failing fixture or seed and paste the short diff.
- Use Sonnet for stages 1, 2 and 5 (mechanical) and Opus for stages 3 and 4 (reasoning).
- Stop when it's good enough: once stages 1 to 3 pass, you have a correct submission. Stage 4 is only about ranking.
