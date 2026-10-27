# Dravity

A portable BitTorrent engine in Rust, built around a **sans-IO core**.
`dravity-session` (Phase 1) is the only I/O driver: poll → `update` →
dispatch effects. CLI and HTTP send commands and read alerts. Tokio is the
default runtime. See [`docs/rust-torrent-plan.md`](docs/rust-torrent-plan.md)
for the full architecture, comparison with existing clients, and roadmap.

> **Status: Phase 1 (MVP downloader) complete.** Dravity downloads v1
> `.torrent` files from HTTP/UDP trackers and TCP peers with rarest-first,
> pipelined requests and SHA-1 verification. It does not seed or accept
> incoming connections yet (Phase 3). See [`docs/phase-1.md`](docs/phase-1.md).

## Workspace

| Crate | Status | What it does |
|---|---|---|
| [`dravity-bencode`](crates/dravity-bencode) | ✅ Phase 0 | Zero-copy bencode `Value`, serde `Deserializer`/`Serializer`, `RawValue` for exact byte capture, depth and item caps |
| [`dravity-core`](crates/dravity-core) | ✅ Phase 0 | `InfoHash` (v1, v2 type), `PeerId`, `.torrent` parsing (`Metainfo`), magnet links, piece/block/file math, path-safety checks |
| [`dravity-wire`](crates/dravity-wire) | ✅ Phase 0 | Handshake + reserved bits, BEP 3/5/6 messages, BEP 10 extended handshake, streaming zero-copy decoder |
| [`dravity-engine`](crates/dravity-engine) | ✅ Phase 1 | Sans-IO `TorrentState::update(now, Event) -> Vec<Effect>`: rarest-first picker, peer table, pipelining, timeouts, data check, tracker scheduling |
| [`dravity-storage`](crates/dravity-storage) | ✅ Phase 1 | `Storage` trait; filesystem backend (LRU FD cache, positional I/O, symlink refusal) and memory backend; piece verification |
| [`dravity-tracker`](crates/dravity-tracker) | ✅ Phase 1 | HTTP (BEP 3/7/23) and UDP (BEP 15) announce: sans-IO codecs + async client (rustls) |
| [`dravity-session`](crates/dravity-session) | ✅ Phase 1 | The only I/O driver: per-torrent owner task on Tokio, peer tasks, disk/hash offload, alerts |
| [`dravity-cli`](crates/dravity-cli) | ✅ Phase 1 | `dravity download` / `dravity info`; renders alerts, never drives I/O |
| `dravity-dht`, `-utp`, `-http`, … | ⏳ Phase 2+ | See the roadmap |

## Phase 0 checklist

- [x] Cargo workspace with shared package metadata, dependencies and lints (`unsafe_code = "forbid"`)
- [x] `dravity-bencode`: canonical, depth- and item-limited, zero-copy decoding; canonical (sorted) encoding; serde both ways
- [x] `dravity-core`: v1 info-hash over the *original* `info` bytes; metainfo validation; magnets (`btih` hex/base32, `btmh`); piece math; rejection of `..`, absolute, drive-prefixed and separator-containing paths
- [x] `dravity-wire`: handshake, BEP 3/5/6 messages, BEP 10 extended handshake, bitfield check
- [x] Unit tests, proptest roundtrips, generated metainfo fixtures
- [x] cargo-fuzz targets for every parser plus roundtrips (`fuzz/`)
- [x] CI: fmt, clippy `-D warnings`, tests on Linux/macOS/Windows, MSRV, docs, cargo-deny, fuzz build + smoke run

## Phase 1 checklist

- [x] Load `.torrent` files (`dravity info` shows them)
- [x] HTTP and UDP trackers, announced concurrently; `started`/`completed`/`stopped` events, re-announce on interval, retry on failure
- [x] TCP peers: handshake with info-hash and self-connection checks, `tokio_util` codec, batched writes, bounded queues
- [x] Rarest-first picking with partial pieces finished first, pipelined requests, choke/disconnect/timeout re-requests
- [x] `dravity-storage` filesystem backend: lazy files, LRU FD cache, positional I/O, padding files skipped, symlinks refused
- [x] SHA-1 piece verification off the async workers; failed pieces are downloaded again; existing data is checked on start (resume)
- [x] `dravity-session` ships the driver; `dravity-cli` shows progress through alerts only
- [x] Tests: scripted engine sequences + proptest, storage, tracker mocks, loopback swarm end-to-end, CLI; fuzz targets for tracker responses; criterion benches

## Command line

```sh
cargo run --release -p dravity-cli -- info example.torrent
cargo run --release -p dravity-cli -- download example.torrent --output downloads
# extra peers, e.g. for a LAN seeder
cargo run --release -p dravity-cli -- download example.torrent --peer 192.168.1.20:6881
```

`DRAVITY_LOG=dravity_session=debug` enables logging (tracing `EnvFilter` syntax).

## Library usage

```rust
use dravity_core::{Magnet, Metainfo};

let bytes = std::fs::read("example.torrent")?;
let torrent = Metainfo::from_bytes(&bytes)?;
println!("{} — {} pieces, info-hash {}", torrent.info().name(),
         torrent.info().piece_count(), torrent.info_hash());

let magnet: Magnet = "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a".parse()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Downloading goes through the session; see the
[`dravity-session` docs](crates/dravity-session/src/lib.rs):

```rust
use dravity_engine::Alert;
use dravity_session::{AddTorrentOptions, Session, SessionConfig};

let session = Session::new(SessionConfig::default())?;
let mut alerts = session.subscribe();
let torrent = session.add_torrent(torrent, AddTorrentOptions::new("downloads"))?;
while let Ok(a) = alerts.recv().await {
    if a.info_hash == torrent.info_hash() && a.alert == Alert::Finished {
        break;
    }
}
session.shutdown().await;
```

## Development

Requires Rust **1.85+** (edition 2024). `rust-toolchain.toml` selects stable.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
cargo bench -p dravity-wire -p dravity-engine

# Fuzzing (nightly + cargo-fuzz)
cargo install cargo-fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run metainfo_parse
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for conventions.

## License

Licensed under the GNU Affero General Public License, version 3.0 or later.
See [LICENSE](LICENSE).
