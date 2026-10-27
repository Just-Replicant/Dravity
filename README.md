# Dravity

A portable BitTorrent engine in Rust, built around a **sans-IO core**.
`dravity-session` (Phase 1) is the only I/O driver: poll → `update` →
dispatch effects. CLI and HTTP send commands and read alerts. Tokio is the
default runtime. See [`docs/rust-torrent-plan.md`](docs/rust-torrent-plan.md)
for the full architecture, comparison with existing clients, and roadmap.

> **Status: Phase 0 (Foundations) complete.** The parsing and protocol crates
> are in place, tested and fuzzed. Dravity cannot download anything yet;
> that is Phase 1.

## Workspace

| Crate | Status | What it does |
|---|---|---|
| [`dravity-bencode`](crates/dravity-bencode) | ✅ Phase 0 | Zero-copy bencode `Value`, serde `Deserializer`/`Serializer`, `RawValue` for exact byte capture, depth and item caps |
| [`dravity-core`](crates/dravity-core) | ✅ Phase 0 | `InfoHash` (v1, v2 type), `PeerId`, `.torrent` parsing (`Metainfo`), magnet links, piece/block/file math, path-safety checks |
| [`dravity-wire`](crates/dravity-wire) | ✅ Phase 0 | Handshake + reserved bits, BEP 3/5/6 messages, BEP 10 extended handshake, streaming zero-copy decoder |
| `dravity-engine`, `-storage`, `-tracker`, `-session`, `-cli`, … | ⏳ Phase 1+ | See the roadmap |

## Phase 0 checklist

- [x] Cargo workspace with shared package metadata, dependencies and lints (`unsafe_code = "forbid"`)
- [x] `dravity-bencode`: canonical, depth- and item-limited, zero-copy decoding; canonical (sorted) encoding; serde both ways
- [x] `dravity-core`: v1 info-hash over the *original* `info` bytes; metainfo validation; magnets (`btih` hex/base32, `btmh`); piece math; rejection of `..`, absolute, drive-prefixed and separator-containing paths
- [x] `dravity-wire`: handshake, BEP 3/5/6 messages, BEP 10 extended handshake, bitfield check
- [x] Unit tests, proptest roundtrips, generated metainfo fixtures
- [x] cargo-fuzz targets for every parser plus roundtrips (`fuzz/`)
- [x] CI: fmt, clippy `-D warnings`, tests on Linux/macOS/Windows, MSRV, docs, cargo-deny, fuzz build + smoke run

## Usage

```rust
use dravity_core::{Magnet, Metainfo};

let bytes = std::fs::read("example.torrent")?;
let torrent = Metainfo::from_bytes(&bytes)?;
println!("{} — {} pieces, info-hash {}", torrent.info().name(),
         torrent.info().piece_count(), torrent.info_hash());

let magnet: Magnet = "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a".parse()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Development

Requires Rust **1.85+** (edition 2024). `rust-toolchain.toml` selects stable.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps

# Fuzzing (nightly + cargo-fuzz)
cargo install cargo-fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run metainfo_parse
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for conventions.

## License

Licensed under the GNU Affero General Public License, version 3.0 or later.
See [LICENSE](LICENSE).
