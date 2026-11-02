# Releases and Container Images

English | [日本語](release_ja.md)

Releases are cut by the `Release` GitHub Actions workflow (`.github/workflows/release.yml`), and the container images are built by the `Container` workflow (`.github/workflows/container.yml`).

## Cutting a release

1. Set the new version everywhere it is declared, then commit it to the default branch:
   - `version` in `[workspace.package]` of the root `Cargo.toml`
   - the `version` of every path dependency: the `kasu-*` entries of `[workspace.dependencies]` and `kasu-daemon` in `crates/kasu-cli/Cargo.toml`
   - `Cargo.lock` (run `cargo update --workspace`)
2. Check it locally: `python3 tools/release/check_version.py <version>`.
3. In the Actions tab, run **Release** from the default branch with `version` set to the version without a leading `v`, for example `0.2.0`.

The workflow fails before building anything if the version has a leading `v`, any of the places above disagrees, the tag `v<version>` already exists, or it was started from another branch.
Otherwise it runs these jobs:

| Job | Result |
|---|---|
| `cli` | `kasu` CLI archives built with `--release --locked`: `kasu-v<version>-linux-x86_64.tar.gz` (x86_64-unknown-linux-gnu on `ubuntu-latest`, so glibc 2.39 or newer) and `kasu-v<version>-macos-arm64.tar.gz` (aarch64-apple-darwin). Each has a `.sha256` file and contains `kasu`, `LICENSE`, and `README.md` |
| `containers` | The server and web images, tagged `<version>` and `latest` |
| `release` | The GitHub release `v<version>`, created at the commit the workflow ran on, with the archives and checksums as assets and generated notes |

A version with a suffix, such as `0.2.0-rc.1`, becomes a pre-release and its images are not tagged `latest`.

## Container images

`docker/Dockerfile` has one build target per component. Both images are multi-architecture (`linux/amd64` and `linux/arm64`, each built natively) and run as UID 10001.

| Image | Contents | Ports |
|---|---|---|
| `ghcr.io/sileader/kasu/server:<version>` | `kasu-server` (built with the `production` feature: PostgreSQL and S3) and `kasu-ingest`. Stores data in `/data` | 50051 (gRPC), 50080 (HTTP) |
| `ghcr.io/sileader/kasu/web:<version>` | `kasu-web`, configured with `KASU_WEB_*` variables ([Web UI](web-ui.md)) | 50090 |

The server image starts `kasu-server /data 0.0.0.0:50051 0.0.0.0:50080`; arguments after the image name replace those defaults.

```sh
docker run --rm -p 50051:50051 -p 50080:50080 -v kasu-data:/data \
  -e KASU_TOKEN=change-me ghcr.io/sileader/kasu/server:0.1.0
```

### Development builds

Running **Container** by hand (workflow_dispatch) on any branch pushes `<Cargo.toml version>-<suffix>` and `<Cargo.toml version>-<suffix>-<short sha>` (suffix `dev` unless set), for example `0.1.0-dev` and `0.1.0-dev-1a2b3c4`. These runs never push a release version or `latest`.
