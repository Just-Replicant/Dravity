# Contributing to Dravity

Thanks for helping! A few rules keep the codebase consistent with the
[design plan](docs/rust-torrent-plan.md).

## Ground rules

1. **Sans-IO libraries.** `dravity-bencode`, `dravity-core`, `dravity-wire`,
   and later `dravity-engine` never perform I/O or spawn tasks.
   `dravity-session` (Phase 1+) owns the only production loop: poll I/O →
   `update(Event)` → dispatch `Effect`s. Binaries send session commands and
   read alerts; they never write that loop.
2. **Errors:** `thiserror` enums in libraries; `anyhow` only in binaries.
3. **No `unsafe`.** The workspace forbids it. If a future backend (e.g.
   io_uring) truly needs it, isolate it in its own crate with a justification.
4. **Untrusted input never panics.** Every parser gets a cargo-fuzz target and
   property tests from day one. Use `get()`/checked arithmetic, not indexing.
5. **Paths from metainfo are hostile.** Go through `dravity_core::path`.

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

## License

Contributions are licensed under the GNU Affero General Public License,
version 3.0 or later, the same terms as the rest of Dravity. See [LICENSE](LICENSE).
