# Contributing to Dravity

See the [design plan](docs/rust-torrent-plan.md).

## Ground rules

1. **Sans-IO libraries.** `dravity-bencode`, `dravity-core`, `dravity-wire`,
   `dravity-engine`, and the tracker, port-map, and uTP codecs do no I/O,
   spawn no tasks, and read no clock. The engine takes `now`.
   `dravity-session` owns the only production loop. Binaries and
   `dravity-http` send commands and read alerts. I/O crates keep
   third-party types out of their public API.
2. **Bounded queues.** The torrent owner never awaits a send its own loop
   must drain. Use `try_send` or a spawned task.
3. **Errors.** `thiserror` in libraries. `anyhow` only in binaries.
4. **No `unsafe`.** The workspace forbids it.
5. **Untrusted input never panics.** Parsers get a cargo-fuzz target and
   property tests. Use `get()` and checked arithmetic.
6. **Metainfo paths are hostile.** Go through `dravity_core::path`.

## Before a PR

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo +nightly fuzz build
cargo deny check
```

Public items need rustdoc. Keep the MSRV in the root `Cargo.toml` unless
the PR says why. Add dependencies under `[workspace.dependencies]`.
Note user-visible changes in [CHANGELOG.md](CHANGELOG.md). Commit subjects
are `feat|fix|refactor|chore|docs(scope): …`.

Crashes from `cargo +nightly fuzz run <target>` belong in a regression
test before the fix.

## License

Contributions use the GNU Affero General Public License, version 3.0 or
later, the same terms as the rest of Dravity. See [LICENSE](LICENSE).
