# Shopping Sessions submission candidate

This branch packages the implementation from
[hansliu-dev/676767](https://github.com/hansliu-dev/676767), commit
`efa72c0635b90efdafe80941621f109789c27bd9`, with its original Git history.
The implementation and root Dockerfile are unchanged from that revision.

## Submission

Use repository `https://github.com/arv20/databricksTrack-Joinbuilders`
and the full commit SHA of branch `submission/team67-candidate`.
Specify the SHA explicitly: this branch is not the default branch.

## Validation

On November 21, 2026, the release build and all 36 supplied fixtures passed
locally on macOS ARM. This does not establish Linux container performance
or an official competition score. The Docker build was not run locally.

The Dockerfile builds a static Rust binary with profile-guided optimization.
Its automatic CPU selection uses build-host features; ensure the scoring
runtime supports the selected instruction set.

Generated datasets, output tables, and compiled binaries are excluded.
