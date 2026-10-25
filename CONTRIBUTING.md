# Contributing to Dravity

Thanks for helping! A few rules keep the codebase consistent with the
[design plan](docs/rust-torrent-plan.md).

## Ground rules

1. **No GPL code.** Superseedr (GPL-3.0) may inspire ideas, but never copy or
   closely translate its source. Dravity is `MIT OR Apache-2.0`; `cargo deny`
   enforces a permissive-only dependency tree.
2. **Sans-IO libraries.** Protocol and parsing crates never perform I/O or
   spawn tasks. Runtime glue lives in `dravity-session` (Phase 1+).
3. **Errors:** `thiserror` enums in libraries; `anyhow` only in binaries.
4. **No `unsafe`.** The workspace forbids it. If a future backend (e.g.
   io_uring) truly needs it, isolate it in its own crate with a justification.
5. **Untrusted input never panics.** Every parser gets a cargo-fuzz target and
   property tests from day one. Use `get()`/checked arithmetic, not indexing.
6. **Paths from metainfo are hostile.** Go through `dravity_core::path`.

## Before opening a PR

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo +nightly fuzz build          # if you touched a parser
cargo deny check                   # if you touched dependencies
```

- Public items need rustdoc (`missing_docs` is on).
- Keep the MSRV (`rust-version` in `Cargo.toml`) unless the PR says why.
- Add workspace dependencies in the root `Cargo.toml` `[workspace.dependencies]`.
- Use [Conventional Commits](https://www.conventionalcommits.org/) style
  messages (`feat(wire): …`, `fix(core): …`).

## Fuzzing

```sh
cargo +nightly fuzz list
cargo +nightly fuzz run bencode_decode -- -max_total_time=300
```

Crashes land in `fuzz/artifacts/<target>/`; turn each into a regression unit
test before fixing it.
