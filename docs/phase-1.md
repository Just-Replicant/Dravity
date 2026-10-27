# Phase 1 — MVP downloader: notes and decisions

Scope from the roadmap (plan §8): load `.torrent`; HTTP + UDP trackers; TCP
peers; rarest-first + pipelined requests; `dravity-storage` filesystem
backend; piece hash verify. `dravity-session` ships the driver;
`dravity-cli` shows progress through alerts and never polls I/O itself.
Also picks up the items Phase 0 deferred to Phase 1 (see below).

## Crates

| Crate | I/O? | Role |
|---|---|---|
| `dravity-engine` | no | `TorrentState::update(now, Event) -> Vec<Effect>`, `Picker`, `Bitfield` |
| `dravity-storage` | blocking | `Storage` trait, `FileStorage`, `MemoryStorage`, `write_block`, `verify_piece` |
| `dravity-tracker` | async (feature `client`) | HTTP/UDP codecs (sans-IO) + `TrackerClient` |
| `dravity-session` | async | `Session`, `TorrentHandle`, `SessionAlert`, `PeerCodec` |
| `dravity-cli` | via session | `dravity download`, `dravity info` |

## Decisions

- **The engine reads no clock.** Every `update` call takes `now: Instant`,
  so timeouts, keep-alives and announce scheduling are tested with scripted
  time. Peers are addressed by driver-allocated `PeerKey(u64)` handles.
- **One owner task per torrent** is the only caller of `update`. It never
  awaits a bounded queue it must itself drain: peer queues use `try_send`
  (a full queue drops the slow peer), disk/hash/tracker work runs in spawned
  tasks that report back through the bounded event queue. All channels are
  bounded.
- **Disk backpressure lives in the engine.** It stops requesting while
  `max_pending_writes` blocks wait for storage and resumes as writes finish,
  so a slow disk cannot grow memory without bound.
- **Rarest-first with rarity buckets.** Wanted, unstarted pieces sit in an
  ordered set keyed by `(availability, index)`; partial pieces are finished
  before new ones start. Ties go to the lowest index (deterministic; random
  tie-breaking can come with endgame in Phase 2).
- **Block lifecycle** is missing → requested → received → written; a piece is
  hashed only when all blocks are on storage. Late blocks for timed-out
  requests are still accepted. A failed hash resets the whole piece.
- **Hashing and disk I/O** run on Tokio's blocking pool; hashing is limited by
  a per-torrent semaphore (`hash_workers`, default = CPU count).
- **Resume = check on start.** `check_existing` hashes existing data (bounded
  by `check_concurrency`) before announcing; missing or short files count as
  "not there" rather than errors. Fastresume without rehash is Phase 2.
- **Trackers are announced concurrently** (all tiers) and results merged; the
  announce succeeds if any tracker answers. The `stopped` announce is awaited
  with a timeout when a torrent stops. UDP retries with exponential back-off
  (BEP 15) and ignores datagrams with another transaction id; connection ids
  are not cached yet.
- **TLS:** reqwest with rustls on the `ring` provider and the platform
  certificate verifier. aws-lc-rs is avoided because its OpenSSL-licensed
  parts are not on the cargo-deny allow-list. `CDLA-Permissive-2.0` was added
  to the allow-list for Mozilla's root certificate data.
- **Storage safety:** files are created lazily below the download directory
  from `Info::file_path` (already validated by `dravity_core::path`); any
  existing symlink on the way is refused; BEP 47 padding files are never
  written to disk and read as zeros. Positional I/O avoids per-block seeks and
  an LRU cache bounds open descriptors.
- **Handshake leftovers:** bytes that arrive in the same segment as the peer's
  handshake are handed to the framed codec as a readable buffer
  (`Framed::from_parts`), so a `bitfield` coalesced with the handshake is not
  stuck until the next read. (Regression test in `dravity-session`.)
- **Download-only:** we never unchoke peers, so `request`/`cancel` are
  ignored; the announced port is informational because Phase 1 does not
  listen. `anyhow` is allowed only for `dravity-cli` (cargo-deny wrapper).
- **MSRV 1.85 is kept.** `criterion` is pinned to 0.7 (0.8 needs 1.86).

## Phase 0 deferrals handled here

| Item | Where |
|---|---|
| Applying `check_bitfield` when the piece count is known | `dravity-engine` (`Bitfield::from_wire`) |
| `tokio_util::codec` adapter for the wire decoder | `dravity_session::PeerCodec` |
| Session driver (one owner task per torrent) | `dravity-session` |
| Criterion benchmarks for codecs | `dravity-wire/benches/codec.rs`, `dravity-engine/benches/picker.rs` |
| Symlink-safe file opening | Partly: components are checked with `symlink_metadata` before opening. Dir-fd + `O_NOFOLLOW` (race-free) and Windows reserved names stay in Phase 3 path hardening |

## Tests

- Engine: scripted event sequences for each behavior, plus a proptest that
  feeds random event sequences and checks invariants.
- Storage: multi-file spans, corruption detection, padding, FD-cache bound,
  symlink refusal, bounds checks.
- Tracker: codec unit tests, in-process HTTP and UDP trackers (stray
  datagrams, retries, timeouts), fuzz targets `tracker_http_response` and
  `tracker_udp_response`.
- Session: loopback swarm end-to-end (tracker → seeder → files on disk),
  resume from disk without downloading, recovery from a corrupt block, and
  the handshake-leftover regression test.
- CLI: `info`, `download` against a loopback seeder, error reporting.

Not covered on CI: real public swarms (the sandbox this was built in blocks
outbound traffic to non-standard ports, so trackers such as
`bttracker.debian.org:6969` were unreachable).

## Deferred

| Item | Phase |
|---|---|
| Magnets, BEP 9/10 metadata, DHT, PEX | 2 |
| Endgame mode, piece stealing from slow peers, AIMD request window, rate limits | 2 |
| Fastresume (persisted have-bitfield + mtime/size, no rehash) | 2 |
| UDP tracker connection-id caching; BEP 12 tier ordering (we announce to all) | 2 |
| Listening for incoming peers, seeding, choking algorithm, BEP 6 behavior | 3 |
| Banning peers that send bad pieces | 3 |
| Race-free path hardening (dir fds, `O_NOFOLLOW`, Windows reserved names) | 3 |
| Session persistence, HTTP API, TUI, file priorities | 4 |
