# Repository Guidelines

## Project Structure & Module Organization

kasu is a Rust 2024 workspace for a server-centric version control system. Workspace members live in `crates/`: `kasu-core` defines the model, `kasu-store` handles persistence, `kasu-derived` computes derived data, and `kasu-ops` implements repository operations. Client and service entry points include `kasu-cli`, `kasu-server`, `kasu-cache`, and `kasu-daemon`; Git compatibility lives in `kasu-git` and `kasu-ingest`.

Each crate keeps implementation in `src/` and integration tests in `tests/`. Protocol definitions are in `crates/kasu-proto/proto/kasu.proto`. Consult `docs/` for architecture and operational guides, `tools/` for integration tooling, and `fuzz/` for fuzz targets and seed corpora.

## Build, Test, and Development Commands

Install Rust stable, a C compiler, `protoc`, and Git. Run commands from the repository root:

- `RUSTC_WRAPPER= cargo build --workspace --locked`: build workspace crates.
- `RUSTC_WRAPPER= cargo fmt --all -- --check`: verify formatting.
- `RUSTC_WRAPPER= cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: run CI lint checks.
- `RUSTC_WRAPPER= cargo test --workspace --all-targets --all-features --locked`: run the workspace test suite.
- `RUSTC_WRAPPER= cargo deny check`: audit dependencies; requires `cargo-deny`.

For local use, follow the import and server startup commands in `README.md`.

## Coding Style & Naming Conventions

Use rustfmt's standard formatting and four-space indentation. Use `snake_case` for modules, functions, and tests; `UpperCamelCase` for types; and `SCREAMING_SNAKE_CASE` for constants. Keep changes within the responsible crate and follow surrounding error-handling patterns. Apply formatting with `RUSTC_WRAPPER= cargo fmt --all`.

## Testing Guidelines

Use Rust's test harness, Tokio tests for asynchronous behavior, and existing `proptest` patterns for invariants. Name tests descriptively by behavior. Add regression coverage for fixes. Run a focused suite with `RUSTC_WRAPPER= cargo test -p kasu-server --test metadata --locked`. External-service and ignored tests require setup documented in the relevant guides. No numeric coverage threshold is specified.

## Commit & Pull Request Guidelines

Existing commits use short, plain subjects such as `update readme`; no enforced prefix convention is evident. Write specific, action-oriented subjects. PRs should explain the problem, resulting behavior, and validation performed; link relevant issues and update affected documentation.

## Documentation Languages

Write documentation in both English and Japanese. The English version uses the plain file name (for example, `docs/land.md`), and the Japanese version adds a `_ja` suffix (`docs/land_ja.md`). Keep the two in sync when editing either one, start each with a language switcher line (`English | [日本語](land_ja.md)` / `[English](land.md) | 日本語`), and point links in Japanese documents at the `_ja` versions. The design plan in `docs/plan/` is Japanese only and is not translated.

## Agent & Configuration Instructions

Always clear `RUSTC_WRAPPER` for sandboxed Cargo commands to disable inherited sccache settings. Do not modify or remove `.cargo/config.toml` to bypass sandbox restrictions. Keep credentials out of commits; README tokens are local examples.
