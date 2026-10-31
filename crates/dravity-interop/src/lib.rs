//! Interoperability tests of Dravity against other BitTorrent clients.
//!
//! The tests live in `tests/interop.rs` and are `#[ignore]`d, so the
//! default `cargo test` stays hermetic. They drive `qbittorrent-nox`
//! (through its Web API) and `transmission-daemon` (through its RPC) on
//! loopback, with DHT, PEX and local discovery off and a tracker of their
//! own, and move data both ways for v1, hybrid and v2-only torrents, with
//! MSE required and merely enabled, and over uTP.
//!
//! Run them with the clients on `PATH`:
//!
//! ```text
//! DRAVITY_INTEROP=1 cargo test -p dravity-interop -- --ignored --test-threads=1
//! ```
//!
//! A test whose client is not installed (or without `DRAVITY_INTEROP=1`)
//! says so on stderr and passes. Every case prints one `interop:` line with
//! what it saw (transport, encryption, time). See the interop section of `docs/phase-6.md`.
