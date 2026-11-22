# theonepiece — staged performance rewrite

## Goal

Reduce the v5 (20M-event) official practice time from 1.036 s toward the
leader's ~0.45 s, through staged, independently measured changes. Each stage
is committed separately, passes the full correctness gate, and is submitted
for official timing before the next stage begins.

## Baseline and isolation

- Base commit: `4a5017b` (current best, official 1.036 s, submission `BB-2B0B206B`).
- Branch: `theonepiece` on fork `arv20/databricksTrack-Joinbuilders`.
- The selected final entry is never replaced until a stage wins an official
  attempt. A rejected or slower entry does not change the best score.
- All changes are original modifications to this codebase (the `f2a2f1e` →
  `a23a613` → `4a5017b` lineage). No other team's files are copied.

## Enabler: PGO toolchain match

Local toolchain is rustc 1.100.0 / LLVM 23.1.5, byte-for-byte the same LLVM
version as the scorer's pinned `rust:1.100.0-slim-bookworm` (digest
`sha256:7c7d4eb4d05b7781187a327809466cfddf29753f6defdc3d19a0e291951b7a37`).
LLVM PGO profiles are keyed by source-derived function hashes, not machine
code, so a profile trained locally on the exact source is usable by the x86-64
scorer build. `-Cllvm-args=-no-pgo-warn-mismatch` tolerates residual hash
differences. This removes the cross-architecture blocker.

## Stages

### Stage 1 — PGO (highest confidence, lowest risk)
- Train on exact `theonepiece` source using the 20M v5 practice set
  (`practice249`) plus fixtures, following the teammate's `regen-pgo.sh` flow.
- Check in `rust/pgo/merged.profdata` (~9 MB; repo limit 50 MB).
- Pin Dockerfile to the digest above; add
  `-C profile-use=/src/rust/pgo/merged.profdata -Cllvm-args=-no-pgo-warn-mismatch`
  alongside the existing `-C target-cpu=x86-64-v3`.
- Local A/B measured PGO faster in 14/20 pairs (~6%); expect ~6-15% officially.
- If the profile is rejected or does not help, drop the commit.

### Stage 2 — wider SIMD in the event parser
- The old `lanes8` patch targets the express-lane code, not this lineage.
  Investigate whether the Nova parser's vector decode can be widened.
- Medium confidence; only proceed after Stage 1 result is known.

### Stage 3 — direct Parquet writer
- Replace `arrow`/`parquet` crates (~13% of the Mac profile) with a minimal
  writer. High effort/risk; only if earlier stages leave output as the clear
  bottleneck.

### Stage 4 — image/runtime tuning
- `FROM scratch` base; partition-count and read-ahead sweeps beyond 32.

## Correctness gate (every stage, before any attempt)

1. 36/36 fixtures via `scripts/run_fixtures.py`.
2. Official v5 dev answer MATCH via `scripts/compare.py`.
3. Independent Python reference on two extra small seeds.
4. 20M v5 output MATCH against the exact fallback (`FORCE_SLOW=1` output).

## Measurement constraint

The development Mac is swapping heavily (multi-GB swap, load 5-8), so local
wall times are unreliable for absolute ranking. Alternating paired A/B on the
dev set is used only for directional confirmation. The scorer is the sole
authoritative measurement.

## Testing

- Unit tests (`cargo test --release --locked`), including the forced-collision
  router tests.
- The four-item correctness gate above.
- An official practice attempt per stage; record the receipt and score.
