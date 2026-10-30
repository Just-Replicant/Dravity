# Dravity — Rust BitTorrent client plan

Source-level comparison of **rqbit**, **Superseedr**, **cratetorrent**, and **Vortex**, plus a full design for **Dravity**: a portable Tokio torrent engine with a sans-IO core, borrowing the best idea from each (not their GPL code).

*Verified against shallow clones on 25 Oct 2026. Paths are relative to each upstream repo.*

## Contents

1. Verdict
2. Side-by-side comparison
3. Strengths and weaknesses
4. Recommended architecture
5. Workspace layout and dependencies
6. What to borrow from each
7. Protocol and product surface
8. Roadmap
9. Testing and benchmarking
10. Pitfalls to avoid
11. Repos analysed

---

## 1. Verdict

> **Build Dravity as a portable Tokio engine around a sans-IO core.** Use rqbit’s workspace split and storage trait, Superseedr’s Action→Effect state machine and fuzzing, and Vortex’s choking, buffer pooling, and off-thread hashing. Keep io_uring as an optional Linux backend added later. Treat cratetorrent mainly as a design document: no commits since 2021.

| Project | Best for |
|---|---|
| rqbit | Feature completeness, library/API, storage trait |
| Superseedr | Protocol breadth (v2, uTP, web seeds), deepest tests |
| Vortex | Performance-focused I/O path and choking |
| cratetorrent | Smallest readable design (stale) |

License stance: **AGPL-3.0-or-later** for Dravity. Superseedr is GPL-3.0 — take *ideas* only, never copy code.

---

## 2. Side-by-side comparison

### 2.1 Architecture

| Aspect | rqbit | Superseedr | cratetorrent | Vortex |
|---|---|---|---|---|
| Layout | Workspace (~12 crates: `librqbit`, `dht`, `tracker_comms`, `peer_binary_protocol`, `bencode`, `upnp`…) + Tauri | Single large `src/` + `fuzz` + wasm demo | `cratetorrent` + `cratetorrent-cli` | `vortex-bittorrent` + `vortex-cli` |
| Runtime | Tokio, multi-threaded | Tokio, multi-threaded | Tokio, multi-threaded | Custom single-threaded io_uring loop |
| Concurrency | Tasks per peer; `parking_lot` RwLock + DashMap (`torrent_state/live/mod.rs`) | One actor per torrent; pure `update(Action) -> Vec<Effect>` (`torrent_manager/state.rs`); reader/writer per peer | Actors over unbounded mpsc: engine, torrent, disk, peer | One I/O thread; hashing on rayon with borrowed buffers; no locks/`Arc` (`event_loop.rs`) |
| Interface | CLI, axum HTTP API, Web UI, desktop, library | Ratatui TUI, file-based CLI control, RSS, cluster | Library + alert channel, small TUI CLI | Library + Ratatui TUI CLI |

### 2.2 Protocol

| Aspect | rqbit | Superseedr | cratetorrent | Vortex |
|---|---|---|---|---|
| Wire | Own `peer_binary_protocol`; ring reader spanning slices (`read_buf.rs`, `double_buf.rs`) | Own codec (`networking/protocol.rs`); batched writer | tokio-util `Codec` (`peer/codec.rs`) | Own decoder (`peer_protocol.rs`) + io_uring multishot recv into buffer ring (`buf_ring.rs`) |
| Piece picking | File-priority + steal from 10×/3× slower peers (`piece_tracker.rs`); sequential-only in practice | Rarest-first + priorities; random in endgame (`piece_manager.rs`) | Lowest missing index = sequential (`piece_picker.rs`) | Rarest-first + endgame (`piece_selector.rs`) |
| Choking | Unchoke everyone after handshake (`peer_connection.rs`) | Tit-for-tat by rate + timed optimistic unchoke (tested) | Unconditional unchoke (TODO in `peer.rs`) | Max 8 unchoked, periodic recalc, optimistic unchoke, round-robin when seeding |
| Discovery | Own DHT, HTTP/UDP trackers, PEX, LSD, UPnP | Own DHT (BEP 42), HTTP/UDP, PEX, web seeds | HTTP trackers only | Library: none; CLI uses `mainline` DHT |
| BEPs (claimed/seen) | 3, 5, 7, 9, 10, 11, 12, 14, 15, 20, 23, 27, 29, 32, 47, 53 | 3, 5, 9, 10, 11, 19, 52 (v2/hybrid) | 3 only | 3, 6, 9, 10, 20 |
| uTP | Yes (`librqbit-utp`) | Own (`networking/utp.rs`) | No | No |

### 2.3 Disk and performance

| Aspect | rqbit | Superseedr | cratetorrent | Vortex |
|---|---|---|---|---|
| Storage | `TorrentStorage` + pread/pwrite; fs/mmap/memory + middleware | Open + seek + write per span (`persistence/payload.rs`) | Per-piece write buffer; `pwritev`/`preadv`; LRU read cache | `fallocate`; io_uring ops; dir fd + `O_NOFOLLOW` |
| Hashing | `block_in_place` + semaphore | Blocking tasks; v2 Merkle per 16 KiB | `spawn_blocking` per buffered piece | rayon; startup rehash via `par_iter` |
| Resume | Fastresume without rehash | Persisted state + integrity prober | None found | Full rehash at start (slow on large torrents) |
| Perf tricks | Chunk stealing, vectored writes, request window, rate limits | AIMD window, token buckets, FD semaphores | TCP slow-start-style request queue | Multishot accept/recv, buffer rings, SQ/CQ tuning, fat LTO |

### 2.4 Quality and project

| Aspect | rqbit | Superseedr | cratetorrent | Vortex |
|---|---|---|---|---|
| Tests | ~163 fns; e2e in `librqbit/src/tests`; criterion in protocol | ~2474 fns/proptest; Docker interop vs qBittorrent/Transmission | ~66 fns + shell vs Transmission | ~175 fns; large `peer_comm/tests.rs`; integration in `bittorrent/tests` |
| Fuzzing | None found | 10 cargo-fuzz targets | None | 2 targets (parse + roundtrip) |
| Errors | `anyhow` in engine; `thiserror` at edges | `thiserror` domain errors | Custom enums | `thiserror` |
| Rust size (raw `.rs`) | ~36k | ~226k (incl. tests/wasm) | ~11k | ~18k |
| Activity | Last commit 9 Oct 2026 | 20 Oct 2026 | 19 Dec 2021 | 14 Oct 2026 |
| Stars | ~1,951 | ~675 | ~493 | ~139 |
| License | Apache-2.0 | GPL-3.0-or-later | MIT OR Apache-2.0 | BSD-3-Clause |

---

## 3. Strengths and weaknesses

### rqbit

**Strengths:** Clean reusable crates; pluggable `TorrentStorage` + middleware; piece stealing; streaming/seek; HTTP API; Prometheus; UPnP media; fastresume.  
**Weaknesses:** Shared RwLocks (deadlock-order warnings); no real choking; sequential picker; no fuzz; `anyhow` inside the engine.

### Superseedr

**Strengths:** Deterministic reducer; widest protocol (BEP 52, uTP, DHT, web seeds); fuzz + proptest + Docker interop; proper tit-for-tat.  
**Weaknesses:** Huge single crate (`state.rs` ~13k lines); open/seek per block write; `String` peer ids and linear pick scans; **GPL-3.0**.

### cratetorrent

**Strengths:** Excellent `DESIGN.md`; clean actors; zero-copy `IoVecs`; per-piece buffer + LRU; slow-start request queue.  
**Weaknesses:** v1 + HTTP only; sequential picker; unconditional unchoke; unbounded channels; abandoned (edition 2018 / old tokio-util).

### Vortex

**Strengths:** io_uring multishot + buffer rings; lock-free single I/O thread; safe path handling; tuned choker; fuzz + integration tests.  
**Weaknesses:** Linux-only recent kernel; no trackers/uTP in library; rehashes all pieces on start; relatively high `unsafe` density.

---

## 4. Recommended architecture

```text
CLI / HTTP / TUI ──commands──► dravity-session ──alerts──► CLI / HTTP / TUI
                                    │
                     only production loop, per torrent:
                     poll → update(Event) → dispatch Effects
                                    │
                                    ▼
                          dravity-engine (sans-IO)
                          picker · choker · peer table
                                    │
                     effects dispatched, never inlined:
                                    │
           ┌────────────────────────┼────────────────────────┐
           ▼                        ▼                        ▼
     peer read/write          dravity-storage           hash pool
     (wire feed/encode,       (FD cache, pwritev)       (rayon /
      batched flush)                                    spawn_blocking)
           │                        │                        │
           └──────── completion events back to the owner ────┘
```

Library, CLI, HTTP, and TUI never poll sockets or call `update`. They send commands and read alerts.

### Design rules

1. **Sans-IO core** — `TorrentState::update(Event) -> Vec<Effect>` (Superseedr pattern). The engine does no I/O and spawns no tasks. Swapping Tokio for io_uring does not fork protocol logic.
2. **One driver** — `dravity-session` owns the only production loop: poll I/O, timers, disk, and hash → `update(Event)` → dispatch `Effect`s. App code never writes a read/feed/write loop.
3. **Single owner per torrent** — one task owns that torrent and is the only caller of `update`. Peer, disk, and hash tasks use **bounded** mpsc (backpressure). No shared torrent `RwLock`.
4. **Dispatch, don't inline** — the owner queues `Send`, `WriteBlock`, `HashPiece`, and `Announce`. It does not write the socket, the disk, or the hash. Completions return as events (`BlockStored`, `PieceHashed`).
5. **Two sans-IO layers** — `dravity-wire` feeds bytes and yields frames inside the session peer tasks. The engine sees `Event::Message`, not sockets. Peer tasks batch writes into one buffer per flush.
6. **Disk actor** — FD cache, positional/`pwritev` writes; never open+seek per block.
7. **Hash off the I/O path** — piece SHA-1 (and v2 Merkle per 16 KiB) on a blocking/rayon pool.
8. **`Storage` trait** — rqbit-style: filesystem first; mmap and memory backends; middleware for write-through cache, timing, fault injection.
9. **`Runtime` trait at the edge** — Tokio is the default *poll* implementation. The effect match stays in `dravity-session`, so an optional Linux io_uring backend (Vortex-style) replaces polling only.
10. **Path safety** — reject `..`, absolute paths, symlinks; prefer dir-fd + `O_NOFOLLOW` where available.
11. **Errors** — `thiserror` in libraries; `anyhow` only in binaries (`dravity-cli`, `dravity-http`).

### Driver (session)

```text
// one task per torrent, inside dravity-session
loop {
    let event = poll_sources().await;   // peers, disk, hash, timers, commands
    let effects = state.update(event);  // dravity-engine, pure
    for effect in effects {
        dispatch(effect);               // queue I/O; do not run it here
    }
}

// peer task, same crate
read bytes → wire.feed → Event::Message
Effect::Send → wire.encode → batched write
```

`update` stays public for scripted engine tests and a future runtime. It is not the integration API.

### Core types (sketch)

```text
Event:   PeerConnected | Message | BlockStored | PieceHashed { ok } | Tick | Cmd(...)
Effect:  Send { peer, msg } | WriteBlock | HashPiece | Announce | Alert | Disconnect
Cmd:     AddTorrent | SetPriority | Pause | Seek (streaming) | SetLimits
Alert:   StateChanged | Finished | Error | MetadataReceived
```

Peers addressed by arena handles (`slotmap`), not `String` ids. Picker keeps rarity buckets; endgame + steal from slow peers.

---

## 5. Workspace layout and dependencies

### Crates

| Crate | Responsibility |
|---|---|
| `dravity-bencode` | Zero-copy bencode + serde |
| `dravity-core` | Info-hash v1/v2, metainfo, magnet, piece/file length math |
| `dravity-wire` | Handshake, messages, BEP 6 Fast, BEP 10 extension protocol |
| `dravity-engine` | Sans-IO torrent state, picker, choker. `update` only; no driver |
| `dravity-storage` | `Storage` trait, filesystem/mmap/memory backends, middleware |
| `dravity-tracker` | HTTP and UDP announce/scrape |
| `dravity-dht` | Thin wrapper over `mainline` (own KRPC later if needed) |
| `dravity-utp` | uTP transport (own impl or wrap a permissive crate; not GPL) |
| `dravity-session` | The only driver: poll → `update` → dispatch effects. Limits, resume, metrics, torrent map |
| `dravity-cli` | clap CLI + optional ratatui TUI. Session commands and alerts only |
| `dravity-http` | axum session API. Session commands and alerts only |

Workspace root: `dravity` (meta) documenting the library entry via `dravity-session`.

### Key dependencies

| Area | Crates |
|---|---|
| Runtime | `tokio`, `tokio-util`, `bytes`, `socket2` |
| Hashing | `sha1`, `sha2` (optionally `aws-lc-rs`) |
| Bitfields | `bitvec` |
| Peer handles | `slotmap` |
| Network | `reqwest` + rustls, `mainline`, uTP crate TBD |
| Errors | `thiserror` (libs), `anyhow` (bins) |
| Observability | `tracing`, `metrics` + Prometheus exporter |
| Interface | `clap`, `axum`, `ratatui` |
| Testing | `proptest`, `cargo-fuzz`, `criterion`, `tempfile` |

---

## 6. What to borrow from each

| Idea | From | Where to look |
|---|---|---|
| Workspace of small reusable crates | rqbit | `Cargo.toml`, `crates/` |
| Storage trait + middleware | rqbit | `crates/librqbit/src/storage/` |
| Piece stealing from slow peers | rqbit | `crates/librqbit/src/piece_tracker.rs` |
| Streaming with prioritised pieces | rqbit | `torrent_state/streaming.rs` |
| Action→Effect reducer | Superseedr | `src/torrent_manager/state.rs` |
| AIMD request window + token buckets | Superseedr | `src/networking/session.rs`, `src/token_bucket.rs` |
| Fuzz targets + interop harness | Superseedr | `fuzz/fuzz_targets/`, `integration_tests/` |
| Design doc alongside code | cratetorrent | `DESIGN.md` |
| Vectored I/O helper | cratetorrent | `cratetorrent/src/iovecs.rs` |
| Choker (optimistic + seeding round-robin) | Vortex | `bittorrent/src/torrent.rs` |
| Buffer pools / io_uring buffer rings | Vortex | `buf_pool.rs`, `buf_ring.rs` |
| Safe path handling | Vortex | `bittorrent/src/file_store.rs` |

---

## 7. Protocol and product surface

### BEPs (target coverage)

| Priority | BEPs | Notes |
|---|---|---|
| MVP | 3 | v1 wire + `.torrent` |
| Swarm | 5, 9, 10, 11, 15 | DHT, metadata exchange, extensions, PEX, UDP tracker |
| Citizen | 6, 7, 14, 27 | Fast extension, IPv6, LSD (opt), private torrents |
| Product | 12, 21, 29 | Multitracker, uTP, holepunch as needed |
| Advanced | 19, 52 | Web seeds; v2 + hybrid + Merkle |
| Ops | 53 etc. | As required for magnet/DHT interoperability |

### Product features

- Library session API with alerts
- CLI with progress; Ratatui TUI (viewer in phase 4, full client in phase 5)
- HTTP API (axum)
- Fastresume (have-bitfield + mtime/size; no full rehash)
- File priorities, streaming seek, watch folder
- Rate limits, peer ban on bad hashes
- UPnP / NAT-PMP port mapping
- Prometheus metrics
- Optional Linux io_uring `Runtime` backend

---

## 8. Roadmap

1. **Phase 0 — Foundations**  
   `dravity-bencode`, `dravity-core` (v1 metainfo), `dravity-wire` codec with fuzz targets from day one; CI: `fmt`, `clippy`, tests.

2. **Phase 1 — MVP downloader**  
   Load `.torrent`; HTTP + UDP trackers; TCP peers; rarest-first + pipelined requests; `dravity-storage` filesystem backend; piece hash verify. `dravity-session` ships the driver; `dravity-cli` shows progress through alerts and never polls I/O itself.

3. **Phase 2 — Magnets and swarm**  
   BEP 9/10 metadata; DHT via `dravity-dht`/`mainline`; PEX; endgame; piece stealing; fastresume; AIMD window + rate limits.

4. **Phase 3 — Good citizen**  
   Tit-for-tat + optimistic unchoke; seeding; BEP 6; uTP (`dravity-utp`); IPv6; UPnP/NAT-PMP; peer banning; path hardening.

5. **Phase 4 — Product surface**  
   Session persistence; `dravity-http`; a first TUI (list, pause, file priorities); streaming seek; watch folder; Prometheus.

6. **Phase 5 — Full terminal UI**  
   The Phase 4 screen becomes a client: add a magnet or a `.torrent` while it is running, remove, peers, trackers, alerts, sort and filter. Still session commands and alerts only. See [`docs/phase-5.md`](phase-5.md).

7. **Phase 6 — Advanced**  
   BEP 52 v2/hybrid + Merkle; web seeds (BEP 19); optional io_uring backend; protocol encryption if required.

Each phase ships a usable artifact; later phases do not rewrite the sans-IO core.

---

## 9. Testing and benchmarking

1. **Unit** — scripted event sequences call `dravity-engine::update` directly; proptest state machines for picker/choker invariants. CLI and HTTP tests talk to the session API and do not reimplement the driver.
2. **Fuzz** — every parser: wire, bencode, torrent files, KRPC, UDP tracker, uTP; encode/decode roundtrips.
3. **In-process swarm** — loopback seed↔leech (see rqbit `tests/e2e.rs`, Vortex `tests/chained_seeding.rs`).
4. **Interop** — Docker against qBittorrent and Transmission for v1, v2, and hybrid.
5. **Fault injection** — storage middleware (slow, failing, partial writes) like rqbit `slow.rs`.
6. **Bench** — criterion on codec and picker; fixed-torrent loopback throughput; compare against the four projects on the same hardware before claiming speed.

---

## 10. Pitfalls to avoid

| Pitfall | Seen in | Dravity rule |
|---|---|---|
| Unchoke everyone | rqbit, cratetorrent | Real tit-for-tat + optimistic unchoke |
| Open/seek per block write | Superseedr | FD cache + positional/`pwritev` |
| Unbounded peer/disk channels | cratetorrent | Bounded mpsc everywhere |
| Dual shared locks + order rules | rqbit | Single owner per torrent |
| Linear pick + `String` peer ids | Superseedr | Rarity buckets + `slotmap` handles |
| Full rehash on start | Vortex | Persist have-bitfield + mtime/size |
| Hash/disk on async workers | common | Always offload |
| Trust metainfo paths | common | Reject `..`, absolutes, symlinks |
| Mandatory io_uring | Vortex | Optional feature only |
| Copying Superseedr sources | — | GPL; reimplement from behavior |
| Read/feed/write loop in each app | common sans-IO wrappers | One driver in `dravity-session`; binaries use commands and alerts |

---

## 11. Repos analysed

- rqbit: https://github.com/ikatson/rqbit  
- Superseedr: https://github.com/Jagalite/superseedr  
- cratetorrent: https://github.com/mandreyel/cratetorrent (redirects to vimpunk/cratetorrent)  
- Vortex: https://github.com/Nehliin/vortex  

*Notes: line counts are raw `.rs` totals; test counts are grep counts of test functions; stars and last-commit dates from GitHub API / git logs on 25 Oct 2026. Projects were not built or benchmarked under this plan.*
