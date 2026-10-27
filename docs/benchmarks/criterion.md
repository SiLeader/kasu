# Criterion Benchmarks

English | [日本語](criterion_ja.md)

Corresponding to the test strategy in plan 05, these measure throughput and size for Changeset creation, Derived Data computation, and pack generation.
They use [Criterion 0.7.0](https://docs.rs/criterion/0.7.0/criterion/) with 20 samples and 95% confidence intervals, and also store raw samples, estimates, and outlier bounds.
All 11 cases were measured on 2026-10-26 on macOS arm64 / Rust 1.98.1 and on Linux arm64 with digest-pinned Rust 1.90.

| Case | macOS mean µs | Linux mean µs |
|---|---:|---:|
| `changeset_create/100` | 185.892 | 89.281 |
| `changeset_create/1000` | 184.978 | 93.254 |
| `changeset_create/10000` | 187.966 | 100.569 |
| `pack_encode/cold/262144` | 5,304.184 | 3,907.389 |
| `pack_encode/cold/4096` | 3,120.932 | 1,657.885 |
| `pack_encode/warm/262144` | 485.006 | 498.287 |
| `pack_encode/warm/4096` | 132.266 | 134.557 |
| `path_history/cold/100` | 14,286.035 | 4,042.626 |
| `path_history/cold/1000` | 120,877.387 | 39,528.348 |
| `path_history/warm/100` | 2.643 | 2.771 |
| `path_history/warm/1000` | 2.668 | 2.673 |

## Inputs and Measurement Scope

- `changeset_create/100,1000,10000`: Prepares histories with varying first-parent depth and a second parent every 10 generations. Each iteration creates a new child of the same tip, changing the content of one file. Messages are unique, so it is never a resend of an existing Changeset. Building the Changeset is outside the measurement; parent / blob / tree validation, eager computation, and storing immutable records are inside.
- `path_history/cold/100,1000`: Prepares a new store per iteration and computes an unstored PathHistory to completion. Fixture creation and teardown are outside the measurement. Cold means the derived records are not stored; it does not flush the OS page cache.
- `path_history/warm/100,1000`: A query returning the same stored result. Cold throughput is measured in commits in the history; warm in queries.
- `pack_encode/cold,warm/4096,262144`: 101 commits and 109 objects with 2 kinds of 4 KiB / 256 KiB blobs from a fixed seed. Cold has no compression cache; warm has every object precompressed. OID enumeration and Git verification are outside the measurement; index reads, needed body fetches / compression, pack checksum, and stream writes are inside. The writer consumes the actual bytes with black_box and does no allocation accumulating the whole pack.

All cases use the SQLite memory store and the real FS / zstd BlobStore. This is not PostgreSQL / S3 / network performance.
Stored records grow while Changesets are measured. Verifying lookup counts against history length is covered by the [100,000-Changeset FirstParentSkip v2 measurement](first-parent-skip-v2.md).
Real time including fixture preparation for cold Derived is longer than Criterion's configured measurement time.
Pack throughput is uncompressed body bytes of the selected Git objects per second. For network transfer volume, use the pack bytes in the next table.

| blob bytes | commit | object | raw bytes | pack bytes |
|---:|---:|---:|---:|---:|
| 4,096 | 101 | 109 | 38,099 | 28,605 |
| 262,144 | 101 | 109 | 554,195 | 545,088 |

Measurement starts only after cold / warm pack bodies match exactly and Git's `index-pack --strict` and `fsck --strict` after ref creation pass.
[pack-sizes.csv](criterion/pack-sizes.csv) matched in both environments. Production bandwidth / latency SLAs are not judged from this small fixed synthetic input.

## Rerunning

```sh
KASU_CRITERION_OUTPUT=/tmp/kasu-criterion-run CRITERION_HOME=/tmp/kasu-criterion-run/criterion \
  cargo bench -p kasu-ingest --bench throughput --locked
```

Criterion JSON is saved to `CRITERION_HOME`, and the pack size CSV to `KASU_CRITERION_OUTPUT`.
For an independent run, specify a new output directory. When using existing output, Criterion compares against the baseline.
Adding `-- --test` runs a smoke test that executes each case once.
`cargo test --workspace --all-targets --all-features --locked` also checks these 11 cases without running the measurement loop.

```sh
docker build -f tools/criterion/Dockerfile -t kasu:criterion .
mkdir -p /tmp/kasu-criterion-linux
docker run --rm --network none --user "$(id -u):$(id -g)" \
  -e KASU_CRITERION_OUTPUT=/reports \
  --mount type=bind,source=/tmp/kasu-criterion-linux,target=/reports \
  kasu:criterion
```

Source hashes are stored in [summary.json](criterion/summary.json), every sample and estimate in [macOS](criterion/macos.json) / [Linux](criterion/linux.json),
and the [run log](criterion/macos.log), [Linux log](criterion/linux.log), and [build information](criterion/linux-build.log).
`tools/criterion/record.py` recomputes means from the 20 samples and cross-checks all 11 cases, confidence intervals, strict pack results, and source hashes from both environments.
212 workspace tests across all targets / features and 11 benchmark smoke tests passed.
21 tests that require external services or pinned inputs are ignored in normal runs. clippy across all targets / features also passes.
The [nightly CI](../../.github/workflows/criterion.yml) measures in the same pinned Linux environment and saves JSON / CSV / logs as artifacts. Results of runs on GitHub are unconfirmed.
