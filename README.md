# Dravity

A portable BitTorrent engine in Rust, built around a **sans-IO core**.
`dravity-session` (Phase 1) is the only I/O driver: poll → `update` →
dispatch effects. CLI and HTTP send commands and read alerts. Tokio is the
default runtime. See [`docs/rust-torrent-plan.md`](docs/rust-torrent-plan.md)
for the full architecture, comparison with existing clients, and roadmap.

> **Status: Phase 6 complete, plus the IronTide adoption work.**
> Dravity downloads and seeds v1, v2 and hybrid `.torrent` files (BEP 52,
> SHA-256 Merkle verification) and magnet links (`btih` and `btmh`,
> metadata over BEP 9/10), finding peers through HTTP/UDP trackers (BEP 12
> tiers), the DHT (the `mainline` crate by default, or Dravity's own node
> with `--native-dht`), PEX and LSD, and downloading from HTTP(S) web seeds
> (BEP 19). It accepts incoming peers over TCP and uTP (BEP 29, LEDBAT)
> on IPv4 and IPv6, maps its port with PCP, NAT-PMP or UPnP (including
> IPv6 PCP pinholes), chokes tit-for-tat, speaks the fast extension
> (BEP 6) and later wire extensions (`upload_only`, `lt_donthave`,
> `lt_tex`, BEP 55 holepunch), bans peers that send corrupt data
> (session-wide, with smart banning) and refuses hostile paths. Live
> settings (rate limits, queue, listen port) can change while the
> session runs and are persisted. SOCKS4/5 and HTTP CONNECT proxies
> carry peer, tracker, web-seed and (native) DHT traffic. See
> [`docs/phase-1.md`](docs/phase-1.md) to [`docs/phase-6.md`](docs/phase-6.md)
> for the original roadmap, [`docs/irontide-adoption.md`](docs/irontide-adoption.md)
> for the work adopted after Phase 6, and
> [`docs/bep-coverage.md`](docs/bep-coverage.md) for BEP status.

## Workspace

| Crate | Status | What it does |
|---|---|---|
| [`dravity-bencode`](crates/dravity-bencode) | ✅ Phase 0 | Zero-copy bencode `Value`, serde `Deserializer`/`Serializer`, `RawValue` for exact byte capture, depth and item caps |
| [`dravity-core`](crates/dravity-core) | ✅ Phase 6 | `InfoHash` (v1, v2), `PeerId`, `.torrent` parsing for v1, v2 and hybrid (`Metainfo`, also from fetched `info` bytes), BEP 52 Merkle trees, piece checks and hash proofs, web seed lists, magnet links, compact peers, piece/block/file math, per-platform path-safety checks |
| [`dravity-wire`](crates/dravity-wire) | ✅ Phase 6 | Handshake + reserved bits, BEP 3/5/6 messages and allowed fast set, BEP 10 extended handshake, BEP 9 `ut_metadata`, BEP 11 `ut_pex` (with peer flags), BEP 52 hash request / hashes / hash reject, streaming zero-copy decoder |
| [`dravity-engine`](crates/dravity-engine) | ✅ Phase 6 | Sans-IO `TorrentState::update(now, Event) -> Vec<Effect>`: rarest-first picker with piece priorities and urgent pieces, peer snapshots, AIMD request window, endgame + block stealing, rate limits, choker (tit-for-tat + optimistic unchoke), seeding, BEP 6, bans, PEX with peer flags and dial hints, metadata serving, data check, tracker/DHT scheduling; `MetadataState` for magnets (v1, v2 and hybrid metadata checks, v2 and hybrid piece layers over hash requests); serves hash requests |
| [`dravity-storage`](crates/dravity-storage) | ✅ Phase 6 | `Storage` trait; filesystem backend (LRU FD cache, positional I/O, files opened without following symlinks) and memory backend; block reads; piece verification (SHA-1, SHA-256 Merkle, or both); fastresume data with file stamps; optional io_uring file I/O (`tokio-uring`, feature `uring`, Linux; long-lived ring files, owned buffers) |
| [`dravity-tracker`](crates/dravity-tracker) | ✅ Phase 6 | HTTP (BEP 3/7/23) and UDP (BEP 15, cached connection ids) announce: sans-IO codecs + async client (rustls); MSE `supportcrypto` / `requirecrypto` |
| [`dravity-dht`](crates/dravity-dht) | ✅ IronTide | DHT (BEP 5): `mainline` by default, or Dravity's own `NativeDht` (`DhtConfig::native`, `--native-dht`) with BEP 32/33/42/43/44/51, optional IPv6 dual-stack and SOCKS5 UDP relay. Public-network validation is documented in [`docs/native-dht-validation.md`](docs/native-dht-validation.md). |
| [`dravity-session`](crates/dravity-session) | ✅ IronTide | The only I/O driver: per-torrent owner task on Tokio, peer tasks, TCP/uTP listener (IPv4 + IPv6, live rebind), disk/hash offload, uploads, magnets, DHT, port mapping, fastresume, BEP 12 tiers, alerts; file priorities, streaming `FileStream`, pause/resume/remove, persisted state + restore, watch folder; live settings (rate limits, queue, connection cap); IP filter, GeoIP, DSCP, SOCKS/HTTP proxies; peer and tracker reads; v2/hybrid torrents, web seeds, MSE/PE; pluggable `Network` transport; settings registry |
| [`dravity-http`](crates/dravity-http) | ✅ Phase 4 | axum JSON API over the session: torrents, files, priorities, streaming with `Range`, server-sent alerts, Prometheus `/metrics`, bearer token. A non-loopback bind without a token is refused unless `--allow-insecure-http` / `HttpConfig::allow_insecure_unauthenticated` is set. |
| [`dravity-cli`](crates/dravity-cli) | ✅ Phase 6 | `dravity download` / `info` / `create` / `serve` for files and magnet links, seeding, file selection, a full ratatui `--tui` client (add, remove, peers, trackers, alerts); renders alerts, never drives I/O |
| [`dravity-utp`](crates/dravity-utp) | ✅ Phase 6 | uTP (BEP 29): packet codec, sans-IO connection with LEDBAT congestion control, Tokio socket and streams |
| [`dravity-lsd`](crates/dravity-lsd) | ✅ irontide | Local Service Discovery (BEP 14): sans-IO `BT-SEARCH` codec and a multicast service; the session skips private torrents |
| [`dravity-portmap`](crates/dravity-portmap) | ✅ IronTide | Router port mapping: PCP (RFC 6887, including IPv6 pinholes) first, then NAT-PMP (RFC 6886) and UPnP IGD. PCP mappings keep their nonce and assigned address and renew at half the lifetime the router granted. |
| [`dravity-webseed`](crates/dravity-webseed) | ✅ Phase 6 | Web seeds (BEP 19): sans-IO plan of file URLs and HTTP ranges (padding as zeros), strict ranged HTTP(S) client (rustls) |
| [`dravity-interop`](crates/dravity-interop) | ✅ Phase 6 | Opt-in tests against qBittorrent (libtorrent) and Transmission: v1, hybrid and v2 both ways, MSE, TCP and uTP |
| [`dravity-mse`](crates/dravity-mse) | ✅ Phase 6 | Message Stream Encryption (MSE/PE): sans-IO initiator and responder (DH, RC4-drop1024, SHA-1 key derivation), async handshake and encrypted stream |
| [`dravity-i2p`](crates/dravity-i2p) | ⚙️ partial | I2P SAM v3 client (transient sessions, naming lookup, streams). Tested against a fake SAM bridge; not yet wired into the session for BitTorrent peers. |
| [`dravity`](crates/dravity) | ✅ IronTide | Facade crate: `prelude`, `ClientBuilder`, one `Error`. The intended public API when the library is consumed from outside this workspace. |

## After Phase 6: IronTide adoption

A review of the irontide engine produced a 31-item plan, reimplemented
for Dravity's sans-IO design. The status of every item, and the design
notes, are in [`docs/irontide-adoption.md`](docs/irontide-adoption.md).
Highlights that are not in the Phase 6 write-up:

- Session-wide rate limits, a torrent queue, live listen-port changes
  and persisted live settings; concurrent settings writes are
  serialized, and a live rebind of an occupied port fails instead of
  silently picking another.
- Tracker SSRF policy: private and loopback HTTP trackers need an
  explicit opt-in or origin allowlist (`--allow-private-trackers`);
  trusted-origin state is request-scoped.
- Configurable torrent piece-length ceiling (32 MiB by default,
  `--max-piece-length`) applied before torrent state is allocated.
- Native DHT with BEP 32/33/42/43/44/51, opt-in via `--native-dht` and
  `--dht-ipv6`. A shared UDP demultiplexer (`dravity_dht::shared`) is
  ready; the session still binds DHT, uTP and UDP trackers on separate
  sockets. See [`docs/native-dht-validation.md`](docs/native-dht-validation.md)
  before making the native node the default.
- The `dravity` facade, a settings registry that generates
  [`docs/configuration.md`](docs/configuration.md), and a BEP coverage
  table with a drift test.

Still partial: I2P peer integration (the SAM client exists, the session
does not use it), shared UDP demultiplexing on the listen port, and
running the native DHT by default.

## Consuming Dravity as a library

All workspace crates set `publish = false`. Dravity is not on crates.io.
That is a deliberate decision, not an omission:

- The version is `0.1.0`; the public API is still evolving.
- A few features remain incomplete or opt-in, as listed above.
- The license is AGPL-3.0-or-later.

The intended public surface is the [`dravity`](crates/dravity) facade
(`prelude`, `ClientBuilder`, a single `Error`). Internal crates are not
a supported API. To depend on Dravity today, use a git dependency:

```toml
dravity = { git = "https://github.com/Just-Replicant/Dravity", package = "dravity" }
```

`publish = true` will be considered once the facade is stable enough
for a versioned release and the native DHT has passed the public-network
checks in [`docs/native-dht-validation.md`](docs/native-dht-validation.md).
Crate metadata (description, license, repository, Rust version) is
already consistent across the workspace, so flipping the flag is a
release decision rather than a cleanup.

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
- [x] Local Service Discovery (BEP 14) in `dravity-lsd`
- [x] Per-IP bans for peers sending corrupt pieces
- [x] Path hardening: per-platform name rules, symlink-free opens on Unix
- [x] Tests: seeding/choker scripts + proptest, simulated lossy network for uTP, fake NAT-PMP/UPnP routers, loopback swarms (TCP, uTP, IPv6, seeding), CLI seed; fuzz targets `utp_packet`, `portmap_parse`

## Phase 4 checklist

- [x] File priorities (`skip`/`low`/`normal`/`high`) mapped to piece priorities; `Finished` means every wanted piece
- [x] Streaming seek: `TorrentHandle::open_file` → `FileStream` reads while downloading, urgent readahead window
- [x] Pause, resume and remove (optionally deleting files, never through symlinks); `Session::list`
- [x] Session persistence: `SessionConfig::state_dir`, atomic writes off the owner tasks, `Session::restore`
- [x] Watch folder for `.torrent` and `.magnet` files
- [x] `dravity-http`: JSON API, file streaming with `Range`, server-sent alerts, bearer token, loopback by default
- [x] Prometheus `/metrics`
- [x] TUI (`--tui`, ratatui) and `dravity serve`; `--files` / `--file-priority`
- [x] Phase 3 leftovers: BEP 6 `suggest` used for picking; announce the router's mapped port
- [x] Tests: engine priority scripts + proptest, loopback session product tests, HTTP API via `tower` oneshot, TUI via `TestBackend`, CLI `serve`; fuzz targets `session_state`, `http_range`

## Phase 5 checklist

- [x] Add a magnet or a `.torrent` from the screen into the session's download directory; errors keep the prompt open
- [x] Remove, or remove and delete files, behind a confirm prompt that names the torrent
- [x] Detail panes: files, peers (`TorrentHandle::peers`: flags, rates, TCP/uTP), trackers (`TorrentHandle::trackers`: last outcome per URL); add one peer
- [x] Alert log per torrent from `Session::subscribe`; ratio, ETA and a session header
- [x] Sort and name filter as view state; `?` key list; mouse selects rows; resize redraws at once
- [x] One 500 ms deadline for the whole refresh; unanswered torrents keep their last numbers
- [x] `download --tui` keeps running while added torrents are unfinished
- [x] Fix: sessions serve BEP 9 metadata for torrents added from a file
- [x] Tests: engine peer snapshots, loopback peer transports and tracker outcomes, TUI draw/key/update tests on `TestBackend`, a loopback paste-a-magnet → pause → remove test

## Phase 6 checklist

- [x] BEP 52 Merkle trees (`dravity_core::merkle`): 16 KiB SHA-256 leaves, piece layers, padded subtrees
- [x] v2-only and hybrid `.torrent` parsing: `file tree` with v1 path checks, `piece layers` checked against pieces roots, hybrid halves must agree; v2 files mapped onto one piece-aligned stream
- [x] Piece verification by version (`Info::check_piece`): SHA-1, Merkle, or both for hybrids
- [x] `btmh` magnets: SHA-256 (or both) metadata checks; v2-only swarms keyed by the truncated v2 hash
- [x] Hybrid torrents join both swarms: listener, DHT lookups and announces under both ids
- [x] Piece layers and web seeds saved with the session's torrents
- [x] Web seeds (BEP 19, `url-list` and magnet `ws`) as virtual peers with batched range requests, back-off and `--no-web-seeds`; new `dravity-webseed` crate
- [x] Fix: BEP 47 padding files may share a name (real hybrid torrents)
- [x] CLI `info` shows the version, v2 hash and web seeds; `W` peer flag
- [x] BEP 52 hash request / hashes / hash reject and the v2 reserved bit: v2 magnets fetch and verify piece layers (multi-piece files download), torrents serve them
- [x] Hybrid torrents announce both ids to trackers; a hybrid magnet accepts peers on its second id once the metadata arrives
- [x] Optional io_uring file backend (`tokio-uring` on its own thread, no `unsafe`; files stay open on the ring and owned buffers move through it, no per-operation `dup` or copy), `--storage-backend`, blocking fallback, `file_io` bench
- [x] Hybrid magnets fetch their piece layers too (SHA-1 and Merkle on every piece, layers saved and served), bounded by `piece_layer_timeout`
- [x] PEX flags (BEP 11): encryption, seed, uTP and reachable sent from observed connections; received flags steer MSE and transport order, `Required` skips peers known not to encrypt
- [x] MSE/PE encryption (new `dravity-mse` crate) for TCP and uTP, incoming and outgoing; `--encryption disabled|enabled|required`; `E` peer flag
- [x] Web seeds get synthetic, unroutable addresses (`100::/64`): two seeds on one host both run, and none is ever dialed, retried as a peer or shared over PEX
- [x] Web seed responses: `Content-Range` must match the request; redirects stay on the original host; loopback, private and link-local targets refused (checked after DNS resolution, no rebinding) unless `--web-seeds-allow-private`; a banned or permanently unusable web seed is not retried
- [x] File names that are not valid UTF-8 are rejected (`NonUtf8Name`)
- [x] A hybrid keeps fetching late piece layers while it downloads or seeds, and rechecks the pieces it accepted on SHA-1 alone; a stalled v2 magnet raises `PieceLayersStalled`
- [x] io_uring batches block I/O (adjacent blocks merged into one `writev`/`readv`, one ring round trip per batch): about 5× faster block writes and reads than one call per block
- [x] Interop against qBittorrent 5.1 and Transmission 4.1 (opt-in `dravity-interop`): 44 transfers pass; fixes for uTP selective ACKs, v2 padded last pieces, the per-torrent v2 reserved bit and simultaneous connections
- [x] Tests: Merkle proptest against a naive tree, v2/hybrid fixtures and hostile trees, engine v2 metadata, loopback v2/hybrid sessions, v2 magnets, restart, loopback web seed downloads; hash proofs, v2 magnets with multi-piece files, hybrid tracker announces; io_uring storage and sessions; MSE against a reference implementation and between sessions under every policy pair; fuzz targets `metainfo_parse` (v2 checks), `webseed_plan` and `mse_handshake`; hybrid magnet layers and a SHA-1-pass/Merkle-fail peer over loopback; PEX flags across three sessions

## Command line

```sh
cargo run --release -p dravity-cli -- info example.torrent
cargo run --release -p dravity-cli -- download example.torrent --output downloads
# magnet links (quote them: `&` is special in shells)
cargo run --release -p dravity-cli -- download 'magnet:?xt=urn:btih:…&tr=…' --output downloads
# extra peers, e.g. for a LAN seeder
cargo run --release -p dravity-cli -- download example.torrent --peer 192.168.1.20:6881
# never use the torrent's HTTP web seeds
cargo run --release -p dravity-cli -- download example.torrent --no-web-seeds
# let web seeds reach LAN or loopback servers (refused by default)
cargo run --release -p dravity-cli -- download example.torrent --web-seeds-allow-private
# no DHT, at most 2 MiB/s
cargo run --release -p dravity-cli -- download example.torrent --no-dht --download-limit 2M
# Dravity's own DHT node (experimental until checked on the public DHT)
cargo run --release -p dravity-cli -- download example.torrent --native-dht
# …and a second IPv6 node (BEP 32); implies --native-dht
cargo run --release -p dravity-cli -- download example.torrent --native-dht --dht-ipv6
# keep seeding after the download (Ctrl-C to stop), uploads capped at 500 KiB/s
cargo run --release -p dravity-cli -- download example.torrent --seed --upload-limit 500K
# listen on another port, TCP only, without asking the router to forward it
cargo run --release -p dravity-cli -- download example.torrent --port 51413 --no-utp --no-port-mapping
# only RC4-encrypted peer connections (MSE/PE; the default `enabled` prefers them)
cargo run --release -p dravity-cli -- download example.torrent --encryption required
# file I/O through io_uring (Linux; falls back to blocking I/O where unavailable)
cargo run --release -p dravity-cli --features uring -- download example.torrent --storage-backend io-uring
```

Peers are accepted on `--port` (default 6881, TCP and uTP; `--no-listen`
to only dial out). The router is asked to forward that port with NAT-PMP or
UPnP unless `--no-port-mapping` is given. `--prefer-utp` dials over uTP
first.

Peer connections use Message Stream Encryption when the peer supports it
(`--encryption enabled`, the default): incoming connections may be plain or
encrypted, and outgoing ones try MSE first and redial in plaintext if the
peer does not speak it. `required` only makes and accepts RC4-encrypted
connections; `disabled` never encrypts. MSE hides BitTorrent traffic from
passive shaping; it is not authenticated security.

`--storage-backend io-uring` (in builds with the `uring` feature, Linux
only) reads and writes torrent data through io_uring on a dedicated
thread. Without the feature, or where the kernel or a container refuses
io_uring, a warning is logged and the default blocking backend is used.

Fastresume data goes to `<output>/.dravity` (`--resume-dir DIR` to move it,
`--no-resume` to always check existing files).

```sh
# only files 0 and 2-4 (indices from `dravity info`), file 3 first
cargo run --release -p dravity-cli -- download example.torrent --files 0,2-4 --file-priority 3=high
# full-screen terminal UI: a adds a magnet or .torrent, d/D remove, enter shows
# files/peers/trackers (1/2/3), space pauses, s/l/n/h set file priorities, ? lists keys
cargo run --release -p dravity-cli -- download example.torrent --tui
cargo run --release -p dravity-cli -- serve --download-dir downloads --tui
# a daemon: saved session, watch folder, HTTP API on 127.0.0.1:9091
DRAVITY_TOKEN=change-me cargo run --release -p dravity-cli -- serve \
    --download-dir downloads --watch torrents
curl -H 'Authorization: Bearer change-me' localhost:9091/api/v1/torrents
# 4 MiB/s down for all torrents together, at most 2 downloading at once
# (the rest queue); the same settings change live over the API or with L/U in the TUI
cargo run --release -p dravity-cli -- serve --download-dir downloads \
    --global-download-limit 4M --max-active-downloads 2
curl -X PATCH -H 'Authorization: Bearer change-me' -H 'Content-Type: application/json' \
    -d '{"upload_rate_limit": 1048576, "max_active_seeds": null}' localhost:9091/api/v1/settings
# make a hybrid (v1 + v2) torrent of a directory, with a tracker and a web seed
cargo run --release -p dravity-cli -- create ./share -t http://tracker/announce -w https://host/share/
```

`dravity serve` keeps its state in `<download-dir>/.dravity` (`--state-dir`
to move it), restores it on start, seeds (unless `--no-seed`) and stops on
Ctrl-C. The API is described in [`docs/phase-4.md`](docs/phase-4.md) and the
[`dravity-http` docs](crates/dravity-http/src/lib.rs).

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

Requires Rust **1.99+** (edition 2024, the latest edition). `rust-toolchain.toml`
selects the latest stable toolchain (`rustup update stable`); CI also tests
the MSRV, 1.99.

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
cargo bench -p dravity-storage --features uring --bench file_io

# Interop against qbittorrent-nox and transmission-daemon (opt-in, needs both on PATH)
DRAVITY_INTEROP=1 cargo test -p dravity-interop -- --ignored --test-threads=1 --nocapture

# Fuzzing (nightly + cargo-fuzz)
cargo install cargo-fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run metainfo_parse
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for conventions.

## License

Licensed under the GNU Affero General Public License, version 3.0 or later.
See [LICENSE](LICENSE).
