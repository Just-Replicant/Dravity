# Dravity — Rust BitTorrent client plan

Design for a portable Tokio engine with a sans-IO core. Ideas come from
rqbit, Superseedr, cratetorrent, and Vortex. Their code is not copied.
Superseedr is GPL-3.0. Dravity is AGPL-3.0-or-later.

Compared from public sources on 25 Oct 2026. The projects were not built
or benchmarked for this plan.

## 1. Verdict

Build a portable Tokio engine around a sans-IO core. Take rqbit's crate
split and storage trait, Superseedr's `update` → effects loop and fuzzing,
and Vortex's choking, off-thread hashing, and optional io_uring. Treat
cratetorrent as a design document.

## 2. What each project is for

| Project | Take | Leave |
|---|---|---|
| rqbit | Storage trait, piece stealing, streaming, HTTP API | Shared locks, unchoke-everyone, no fuzz |
| Superseedr | Reducer, v2, uTP, web seeds, interop tests | One huge crate, open/seek per write, GPL |
| cratetorrent | Small actor design, vectored I/O | Sequential picker, unbounded channels, abandoned |
| Vortex | Choker, dir-fd opens, io_uring as an option | Linux-only loop, full rehash on start, `unsafe` |

## 4. Architecture

```text
CLI / HTTP / TUI ──commands──► dravity-session ──alerts──► CLI / HTTP / TUI
                                    │
                     poll → update(Event) → dispatch Effects
                                    │
                                    ▼
                             dravity-engine
                                    │
              peer I/O          storage           hash pool
```

Library, CLI, HTTP, and TUI never poll sockets or call `update`.

1. **Sans-IO core.** `TorrentState::update(now, Event) -> Vec<Effect>`.
   No I/O, no tasks, no clock.
2. **One driver.** `dravity-session` polls and dispatches. Apps do not.
3. **One owner per torrent.** That task is the only caller of `update`.
   Every channel is bounded.
4. **Dispatch, don't inline.** The owner queues sends, writes, hashes,
   and announces. Completions come back as events.
5. **Wire is a second sans-IO layer.** Peer tasks feed bytes. The engine
   sees messages.
6. **Disk.** FD cache and positional writes. No open-and-seek per block.
7. **Hash off the I/O path.**
8. **`Storage` trait.** Filesystem first. Middleware for cache and faults.
9. **Tokio is the default poll loop.** io_uring replaces file I/O only,
   behind a feature.
10. **Paths.** Reject `..`, absolute paths, and symlinks.
11. **Errors.** `thiserror` in libraries. `anyhow` only in binaries.

Peers are handles, not strings. The picker keeps rarity buckets.

## 5. Crates

`dravity-bencode`, `dravity-core`, `dravity-wire`, `dravity-engine`,
`dravity-storage`, `dravity-tracker`, `dravity-dht`, `dravity-utp`,
`dravity-session`, `dravity-cli`, `dravity-http`. The session is the
library entry. The `dravity` facade is the supported dependency.

Runtime: tokio, bytes. Hashing: sha1, sha2. Errors: thiserror, anyhow.
Interface: clap, axum, ratatui. Tests: proptest, cargo-fuzz, criterion.

## 8. Roadmap

1. **Phase 0 — Foundations.** Bencode, v1 metainfo, wire codec, fuzz, CI.
2. **Phase 1 — MVP downloader.** `.torrent`, HTTP and UDP trackers, TCP
   peers, rarest-first, filesystem storage, piece hashes. The session
   drives I/O. The CLI only shows alerts.
3. **Phase 2 — Magnets and swarm.** BEP 9/10, DHT, PEX, endgame, fastresume,
   AIMD, rate limits.
4. **Phase 3 — Good citizen.** Tit-for-tat, seeding, BEP 6, uTP, IPv6,
   port mapping, bans, path hardening.
5. **Phase 4 — Product surface.** Persistence, HTTP API, a first TUI,
   streaming, watch folder, metrics.
6. **Phase 5 — Full terminal UI.** Add, remove, peers, trackers, alerts.
   Still commands and alerts only.
7. **Phase 6 — Advanced.** BEP 52, web seeds, optional io_uring, MSE.

Each phase ships something usable. Later phases do not rewrite the core.
Notes: [`phase-0.md`](phase-0.md) through [`phase-6.md`](phase-6.md).

## 9. Testing

Scripted `update` sequences and proptest for the engine. Fuzz every
parser. Loopback swarms. Opt-in interop with qBittorrent and Transmission.
Storage faults through middleware. Criterion on the codec and picker.

## 10. Pitfalls

| Pitfall | Rule |
|---|---|
| Unchoke everyone | Tit-for-tat plus one optimistic slot |
| Open and seek per block | FD cache and positional I/O |
| Unbounded channels | Bounded queues |
| Shared torrent locks | One owner task |
| String peer ids | Handles |
| Rehash every start | Fastresume stamps |
| Hash on the async worker | Blocking pool |
| Trust metainfo paths | `dravity_core::path` |
| Require io_uring | Optional feature |
| Copy GPL sources | Reimplement the behavior |
| A read loop in every app | One driver |
