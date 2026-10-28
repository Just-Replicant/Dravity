# Dravity

A portable BitTorrent engine in Rust, built around a **sans-IO core**.
`dravity-session` (Phase 1) is the only I/O driver: poll → `update` →
dispatch effects. CLI and HTTP send commands and read alerts. Tokio is the
default runtime. See [`docs/rust-torrent-plan.md`](docs/rust-torrent-plan.md)
for the full architecture, comparison with existing clients, and roadmap.

> **Status: Phase 3 (good citizen) complete.** Dravity downloads and
> seeds v1 `.torrent` files and magnet links (metadata over BEP 9/10),
> finding peers through HTTP/UDP trackers (BEP 12 tiers), the mainline DHT
> and PEX. It accepts incoming peers over TCP and uTP (BEP 29, LEDBAT) on
> IPv4 and IPv6, maps its port with NAT-PMP or UPnP, chokes tit-for-tat
> with an optimistic unchoke, speaks the fast extension (BEP 6), bans peers
> that send corrupt data and refuses hostile paths. See
> [`docs/phase-1.md`](docs/phase-1.md), [`docs/phase-2.md`](docs/phase-2.md)
> and [`docs/phase-3.md`](docs/phase-3.md).

## Workspace

| Crate | Status | What it does |
|---|---|---|
| [`dravity-bencode`](crates/dravity-bencode) | ✅ Phase 0 | Zero-copy bencode `Value`, serde `Deserializer`/`Serializer`, `RawValue` for exact byte capture, depth and item caps |
| [`dravity-core`](crates/dravity-core) | ✅ Phase 3 | `InfoHash` (v1, v2 type), `PeerId`, `.torrent` parsing (`Metainfo`, also from fetched `info` bytes), magnet links, compact peers, piece/block/file math, per-platform path-safety checks |
| [`dravity-wire`](crates/dravity-wire) | ✅ Phase 3 | Handshake + reserved bits, BEP 3/5/6 messages and allowed fast set, BEP 10 extended handshake, BEP 9 `ut_metadata`, BEP 11 `ut_pex`, streaming zero-copy decoder |
| [`dravity-engine`](crates/dravity-engine) | ✅ Phase 3 | Sans-IO `TorrentState::update(now, Event) -> Vec<Effect>`: rarest-first picker, AIMD request window, endgame + block stealing, rate limits, choker (tit-for-tat + optimistic unchoke), seeding, BEP 6, bans, PEX, metadata serving, data check, tracker/DHT scheduling; `MetadataState` for magnets |
| [`dravity-storage`](crates/dravity-storage) | ✅ Phase 3 | `Storage` trait; filesystem backend (LRU FD cache, positional I/O, files opened without following symlinks) and memory backend; block reads; piece verification; fastresume data with file stamps |
| [`dravity-tracker`](crates/dravity-tracker) | ✅ Phase 2 | HTTP (BEP 3/7/23) and UDP (BEP 15, cached connection ids) announce: sans-IO codecs + async client (rustls) |
| [`dravity-dht`](crates/dravity-dht) | ✅ Phase 2 | Mainline DHT (BEP 5) peer lookups and announces, wrapping [`mainline`](https://crates.io/crates/mainline) |
| [`dravity-session`](crates/dravity-session) | ✅ Phase 3 | The only I/O driver: per-torrent owner task on Tokio, peer tasks, TCP/uTP listener (IPv4 + IPv6), disk/hash offload, uploads, magnets, DHT, port mapping, fastresume, BEP 12 tiers, alerts |
| [`dravity-cli`](crates/dravity-cli) | ✅ Phase 3 | `dravity download` / `dravity info` for files and magnet links, seeding with `--seed`; renders alerts, never drives I/O |
| [`dravity-utp`](crates/dravity-utp) | ✅ Phase 3 | uTP (BEP 29): packet codec, sans-IO connection with LEDBAT congestion control, Tokio socket and streams |
| [`dravity-portmap`](crates/dravity-portmap) | ✅ Phase 3 | Router port mapping: NAT-PMP (RFC 6886) and UPnP IGD codecs, `PortMapper` that maps, renews and removes |
| `dravity-http`, … | ⏳ Phase 4+ | See the roadmap |

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

## Phase 2 checklist

- [x] Magnet links: BEP 10 extension handshake and BEP 9 metadata download, verified against the info-hash, then handed over to the download without reconnecting
- [x] Serving metadata to other peers (`ut_metadata` data/reject)
- [x] Mainline DHT (BEP 5) peer lookups via `dravity-dht` (wrapping `mainline`)
- [x] PEX (BEP 11): learn peers and send add/drop diffs; off for private torrents (BEP 27), as is the DHT
- [x] Endgame duplicate requests with `cancel`, and block stealing from slow peers
- [x] AIMD per-peer request window capped by the peer's `reqq`; download rate limit (token bucket)
- [x] Fastresume: verified pieces + file length/mtime stamps, atomic save, full check when anything changed
- [x] UDP tracker connection-id cache; BEP 12 tier ordering
- [x] Tests: engine scripts + proptest, metadata state machine, codecs (proptest + fuzz), resume fuzz target, loopback DHT testnet, loopback swarm (magnet via tracker and DHT, PEX, resume, tier fallback), CLI

## Phase 3 checklist

- [x] Seeding: serve blocks from disk, keep finished torrents in the swarm, upload rate limit
- [x] Choker: tit-for-tat, anti-snubbing, optimistic unchoke
- [x] BEP 6 fast extension: `have all`/`have none`, `reject`, allowed fast set
- [x] uTP (BEP 29) with LEDBAT in `dravity-utp`; dial fallback between TCP and uTP
- [x] Incoming connections over TCP and uTP, IPv4 and IPv6, routed by info-hash; DHT `announce_peer`
- [x] Port mapping with NAT-PMP and UPnP IGD in `dravity-portmap`
- [x] Per-IP bans for peers sending corrupt pieces
- [x] Path hardening: per-platform name rules, symlink-free opens on Unix
- [x] Tests: seeding/choker scripts + proptest, simulated lossy network for uTP, fake NAT-PMP/UPnP routers, loopback swarms (TCP, uTP, IPv6, seeding), CLI seed; fuzz targets `utp_packet`, `portmap_parse`

## Command line

```sh
cargo run --release -p dravity-cli -- info example.torrent
cargo run --release -p dravity-cli -- download example.torrent --output downloads
# magnet links (quote them: `&` is special in shells)
cargo run --release -p dravity-cli -- download 'magnet:?xt=urn:btih:…&tr=…' --output downloads
# extra peers, e.g. for a LAN seeder
cargo run --release -p dravity-cli -- download example.torrent --peer 192.168.1.20:6881
# no DHT, at most 2 MiB/s
cargo run --release -p dravity-cli -- download example.torrent --no-dht --download-limit 2M
# keep seeding after the download (Ctrl-C to stop), uploads capped at 500 KiB/s
cargo run --release -p dravity-cli -- download example.torrent --seed --upload-limit 500K
# listen on another port, TCP only, without asking the router to forward it
cargo run --release -p dravity-cli -- download example.torrent --port 51413 --no-utp --no-port-mapping
```

Peers are accepted on `--port` (default 6881, TCP and uTP; `--no-listen`
to only dial out). The router is asked to forward that port with NAT-PMP or
UPnP unless `--no-port-mapping` is given. `--prefer-utp` dials over uTP
first.

Fastresume data goes to `<output>/.dravity` (`--resume-dir DIR` to move it,
`--no-resume` to always check existing files).

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
// or: session.add_magnet(&magnet, AddTorrentOptions::new("downloads"))?
while let Ok(a) = alerts.recv().await {
    if a.info_hash == torrent.info_hash() && a.alert == Alert::Finished {
        break;
    }
}
session.shutdown().await;
```

## Development

Requires Rust **1.85+** (edition 2024). `rust-toolchain.toml` selects stable.

Point `origin` at this repository over SSH:

```sh
git remote set-url origin git@github.com:Just-Replicant/Dravity.git
```

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
