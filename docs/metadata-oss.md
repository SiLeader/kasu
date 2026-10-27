# Metadata Differential Verification Against a Pinned OSS Repository

English | [日本語](metadata-oss_ja.md)

This imports the [7 pinned refs](../crates/kasu-ingest/tests/fixtures/oss-refs.txt) of
[libgit2/TestGitRepository](https://github.com/libgit2/TestGitRepository) and compares the Metadata backend API against Git after the refs are published.
The input is 21 commits and 70 objects. In addition to comparisons on generated fixtures, this test verifies history created externally.

On 2026-10-26, everything below matched on both macOS arm64 (Git 2.54.0) and Linux arm64 with pinned Rust 1.90 (Git 2.39.5).

| Query | Count per environment | Compared against |
|---|---:|---|
| Path history | 189 | All 21 commits × all 8 paths plus 1 nonexistent path. `git log --first-parent --format=%H -- <path>` |
| Blame | 77 | Every regular-file snapshot of every commit. Origin OID, original line number, and origin path from `git blame --line-porcelain` |
| MergeBase | 441 | All 21 × 21 ordered tip pairs. The set from `git merge-base --all` |

Log fetches pages of 2 and also checks termination and that cursors do not repeat.
Log's first-parent semantics follow the [API plan](plan/03-protocols-and-api.md).
MergeBase covers every pair, including identical tips, tips that are their own ancestors, divergence, unrelated roots, and octopus merges.
Git's exit code 1 for no common ancestor is accepted only when stdout / stderr are empty.
Before import, all refs are checked against pinned values and strict fsck; after import, commit / object counts are checked.

```sh
KASU_OSS_REPO=/path/to/pinned-TestGitRepository.git \
  cargo test -p kasu-server --test oss_metadata --locked -- --ignored --nocapture
```

The input is shared with the [pinned Git import fixture](git-import.md#verification).
Preparation steps from the pinned OIDs are in the [nightly workflow](../.github/workflows/oss-metadata.yml).
A normal `cargo test` does not run this test, which requires external input.

On Linux, use the following commands. The input is passed read-only and copied to a temporary directory in the container before running.
The Rust image is pinned by digest and the network is disabled while tests run.

```sh
docker build -f tools/oss-fixtures/metadata.Dockerfile -t kasu:oss-metadata-test .
docker run --rm --network none --user "$(id -u):$(id -g)" \
  --mount type=bind,source=/absolute/path/to/pinned-TestGitRepository.git,target=/fixtures/TestGitRepository.git,readonly \
  kasu:oss-metadata-test
```

Counts and source SHA-256 hashes are stored in [summary.json](metadata-oss/summary.json), the [macOS log](metadata-oss/macos.log),
the [Linux log](metadata-oss/linux.log), and [Linux build information](metadata-oss/linux-build.log).
`tools/oss-fixtures/record-metadata.py` cross-checks test names, counts, and build-time source hashes when recording.
Results of the workflow running on GitHub have not been confirmed.

This input is small and is not representative of real OSS repositories in general or of every Git rename / copy heuristic.
It contains no non-UTF-8 paths. Line moves / copies and transport comparisons are covered by [a separate generated-history test](blame-lineage.md).
