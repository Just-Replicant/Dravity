# irontide adoption

Dravity reviewed the irontide BitTorrent engine and listed 31 ideas worth
adopting (fixes, improvements and features). This page tracks each one.
Every item is **reimplemented** for Dravity's sans-IO architecture; no
irontide code is copied: Dravity keeps its own authorship and design
(it is AGPL-3.0-or-later; irontide is GPL), and only ideas carry over.

Status: **done** (implemented as described), **partial** (the closest
sound version, with the difference explained), **open** (not started).

| # | Item | Status | Commits | Notes |
|---|------|--------|---------|-------|
| 1 | SSRF guard for HTTP tracker announces | done | `fix(tracker): guard HTTP announces against SSRF` | [notes](#1-ssrf-guard) |
| 2 | Session-wide smart ban | done | `feat(engine,session): session-wide smart ban` | [notes](#2-smart-ban) |
| 3 | Global connection cap with one admit function and a loom model | done | `feat(core,session): session-wide connection cap` | [notes](#3-connection-cap) |
| 4 | Session-wide hash pool | done | `perf(session): share one hash pool across torrents` | [notes](#4-hash-pool) |
| 5 | Sans-IO purity tripwire tests | done | `test: sans-IO purity tripwires` | [notes](#5-purity-tripwires) |
| 6 | Hash on arrival | done | `perf(core,session): verify pieces as their blocks are written` | [notes](#6-hash-on-arrival) |
| 7 | Byte-budgeted read cache (Storage middleware) | partial | `feat(storage,session): shared read cache middleware` | [notes](#7-read-cache) |
| 8 | Verify-before-download prescan | done | `perf(storage,engine,session): prescan files before the start check` | [notes](#8-prescan) |
| 9 | BDP-adaptive request budget | done | `feat(engine): BDP request depth and a torrent request budget` | [notes](#9-bdp-request-budget) |
| 10 | Piece reservation / extent affinity | done | `feat(engine): reserve started pieces for the peer that started them` | [notes](#10-piece-reservation) |
| 11 | Preallocation and durability modes | done | `feat(storage,session): preallocation and durability modes` | [notes](#11-preallocation-and-durability) |
| 12 | Seed choking variants and an upload slot tuner | done | `feat(engine): seed choking modes and an upload slot tuner` | [notes](#12-seed-choking-and-slot-tuner) |
| 13 | Torrent creation (v1/v2/hybrid, BEP 47 pads, auto piece size) and dravity create | done | `feat(core,storage,cli): torrent creation and dravity create` | [notes](#13-torrent-creation) |
| 14 | Tracker scrape (BEP 48 HTTP, BEP 15 UDP) in TrackerStatus | done | `feat(tracker,session,cli): tracker scrape` | [notes](#14-tracker-scrape) |
| 15 | Local Service Discovery (BEP 14), off for private torrents | done | `feat(lsd,session,cli): local service discovery` | [notes](#15-local-service-discovery) |
| 16 | Live settings, session-wide rate limits and queueing | partial | `feat(session,http,cli): live settings, session rate limits and queueing` | [notes](#16-live-settings-rate-limits-and-queueing) |
| 17 | BEP 53 select-only magnets (so=) | done | `feat(core,session): BEP 53 select-only magnets` | [notes](#17-bep-53-select-only-magnets) |
| 18 | Wire extensions: upload_only, lt_donthave, super-seeding, lt_tex | done | `feat(wire,engine,session,cli): upload_only, lt_donthave, super-seeding and tracker exchange` | [notes](#18-wire-extensions) |
| 19 | BEP 40 canonical peer priority | done | `feat(core,engine): BEP 40 canonical peer priority at the connection cap` | [notes](#19-bep-40-peer-priority) |
| 20 | IP filter (eMule .dat, P2P, CIDR) and --ip-filter | done | `feat(session,cli): IP filter lists` | [notes](#20-ip-filter) |
| 21 | PCP (RFC 6887) before NAT-PMP and UPnP | partial | `feat(portmap): PCP port mapping` | [notes](#21-pcp) |
| 22 | Native DHT building blocks: KRPC codec, BEP 42 ids, UDP demux | partial | `feat(dht): KRPC codec, BEP 42 secure ids and a UDP datagram classifier` | [notes](#22-native-dht-building-blocks) |
| 23 | Proxy support: SOCKS4, SOCKS5, HTTP CONNECT | partial | `feat(session,tracker,webseed,cli): dial peers through SOCKS4/5 and HTTP CONNECT proxies` | [notes](#23-proxy-support) |
| 24 | Peer extras: GeoIP country, DSCP marking, persistent web-seed stats | done | `feat(session,cli): GeoIP peer countries, DSCP marking and persistent web seed back-off` | [notes](#24-peer-extras) |
| 25 | BEP 55 holepunch (ut_holepunch) | done | `feat(wire,engine,cli): BEP 55 ut_holepunch relaying and rendezvous` | [notes](#25-bep-55-holepunch) |
| 26 | BEP 52 conformance vector suite | done | `test(core): BEP 52 conformance vectors from an independent reference` | [notes](#26-bep-52-conformance-vectors) |
| 27 | More loom models: shared rate bucket, live cap changes | done | `test(core,session): loom models for the shared rate bucket and live cap changes` | [notes](#27-more-loom-models) |
| 28 | Pluggable network transport (Network, TransportListener, MemoryNetwork) | done | `feat(session): pluggable peer transport with an in-memory network` | [notes](#28-pluggable-network-transport) |

## Design notes

### 1. SSRF guard

`dravity_tracker::guard` checks announce URLs and every redirect hop. A
tracker on `localhost`/loopback must use an `/announce` path (so a
torrent cannot aim requests at arbitrary local services). Redirects must
stay on `http(s)`, may not downgrade `https` to `http`, and are limited
to `MAX_REDIRECTS` (5). A redirect from a non-local tracker may not
reach a non-public address. This applies to literal addresses and to
resolved ones. The filter runs inside the resolver the connection
uses, so DNS rebinding cannot get around it. Host names that the
torrent itself names as announce origins are trusted, so LAN trackers
keep working. `ClientConfig::ssrf_mitigation = false` turns the guard
off.

### 2. Smart ban

irontide paroles a peer after a failed piece and blames whoever fails
the next attempt. Dravity attributes blame directly instead:

- When a piece fails and its blocks came from more than one address, the
  engine quarantines the piece. The picker does not release it yet, so
  new blocks cannot overwrite the evidence. The engine emits
  `Effect::HashBlocks`. The session reads the piece from storage on the
  shared hashing permits and answers with `Event::BlockHashes`, which
  carries one SHA-256 digest per 16 KiB block.
- The digests are stored per (block, sender). After that the piece is
  downloaded again. When a copy passes, its blocks are hashed too, and
  any address whose recorded block differs from the verified block is
  banned. An honest peer that shared the piece is never struck.
- v2/hybrid torrents need no good copy. The engine asks a BEP 52 peer
  that has the piece (preferring one that did not send it) for the
  piece's leaf hashes. It checks them against the piece-layer hash it
  already trusts, then convicts at once.
- Strikes (`hash_fail_ban`) remain the fallback in three cases: the
  piece had a single sender, the blocks could not be read, or the
  tracking table (256 pieces) is full. `Config::smart_ban = false`
  restores pure strikes.
- Bans are session-wide. `Alert::PeerBanned` puts the address in the
  session's ban list and sends `Command::BanIp` to every other torrent.
  New torrents start with the list (`TorrentState::with_banned_ips`).
  The listener refuses banned addresses before the handshake.
  `Session::ban_ip`, `unban_ip` and `banned_ips` give applications the
  same control.

### 3. Connection cap

`dravity_core::admit` holds the single decision (`admit_permitted(max,
live)`) and a lock-free `ConnectionLimit` that hands out RAII
`ConnectionSlot`s. It lives in `dravity-core` (no tokio), so it can be
built on loom's atomics under `--cfg loom`. `tests/loom_admit.rs`
explores every interleaving of contending admits and releases (CI job
`loom`). The session owns one limit (`SessionConfig::max_connections`,
default 500). The TCP and uTP accept paths take a slot before the
handshake and close the connection when none is free. The slot then
moves with the connection (`Accepted::slot`) for its lifetime. Outgoing
dials take a slot before connecting. When none is free the session
answers the engine's `Effect::Connect` with
`Event::PeerConnectDeferred`. The engine keeps the address without blame
and retries after `DEFERRED_DIAL_RETRY`. It does not start the next dial
in the same step, which would only be deferred as well. The cap can be
changed at runtime (`Session::set_max_connections`). Lowering it never
closes a connection.

### 4. Hash pool

`dravity-session/src/hash_pool.rs` replaces the per-torrent semaphore
with a single `HashPool` that the session owns and hands to each torrent
through `TorrentContext`. It bounds jobs with a FIFO `tokio::Semaphore`,
so torrents take turns. The jobs run on Tokio's blocking pool. irontide
runs its own threads with `catch_unwind`. Dravity gets the same
isolation from `spawn_blocking`'s `JoinError`: a panicking job reports
`StorageFailed` and frees its slot. Dedicated threads would duplicate
the blocking pool that storage I/O already uses.

### 5. Purity tripwires

Each crate that mixes sans-IO modules with Tokio code lists those
modules in `tests/sans_io_purity.rs` (with `include_str!`, so the check
needs no features). The crates that are sans-IO as a whole
(`dravity-core`, `-engine`, `-wire`, `-bencode`) scan every file in
`src/` instead, so a new file is covered automatically. The scan rejects
these tokens: tokio, `.await`, `async fn`/`async move`, socket types,
`ToSocketAddrs`, `std::fs`, `std::thread`, `Instant::now`,
`SystemTime::now`, reqwest and rustix. It skips comments, because doc
examples may show a clock, and the trailing `mod tests`. A self-test
proves the checker catches a violation.

### 6. Hash on arrival

`dravity_core::verify::PieceHasher<B>` takes a piece's blocks in any
order. SHA-1 must see the bytes in order, so the hasher holds a block
that arrives early until the gap before it closes. A block is exactly
one 16 KiB v2 leaf, so the SHA-256 leaves are hashed when each block
arrives, and the final Merkle step only combines 32-byte hashes.
`verify` and `Info::check_piece` both go through a single digest check
(`Info::check_digests`), so the two paths cannot disagree. A proptest
compares them over random block orders, v2 and hybrid layouts, and
corruptions.

The session feeds the hashers on the blocking write threads, only for
blocks the storage reported as written, before the `BlockWritten`
events go out. The hasher is therefore complete by the time the engine
asks for `HashPiece`. A block added twice breaks the hasher, because
storage holds the later copy and the hasher cannot vouch for it. Those
pieces, pieces whose early blocks would push the torrent over 32 MiB,
and all checks of existing data fall back to reading the piece back.
A storage test counts the bytes read and shows that a download reads
back nothing (before this change it read back the whole torrent).

### 7. Read cache

This follows design rule 8 of the plan: a `Storage` middleware
(`dravity-storage/src/cache.rs`). `CachedStorage` wraps any storage.
`ReadCache` is shared by every storage the session creates, so all
torrents draw on one byte budget (`SessionConfig::read_cache`, default
64 MiB). The policy is a segmented LRU. A chunk enters on probation and
moves to the protected segment (at most 80 % of the budget) on its
second hit, so a recheck or a peer sweeping the torrent cannot flush the
pieces that peers keep asking for. `read_many` serves hits from memory
and sends all the misses to the wrapped storage as one batch, so
io_uring batching survives. Writes go through and drop the chunks they
overlap. Each write bumps an epoch, and a read that raced a write does
not insert what it read.

**Why partial:** irontide pairs its ARC cache with a unified buffer pool
that also backs in-flight writes. Dravity's writes already use
reference-counted `Bytes` straight from the wire, and the hash-on-arrival
hashers (item 6) hold only early blocks, so a shared write pool would add
copies, not remove them. The read side, which is where seeding gains, is
fully implemented. Segmented LRU replaces ARC because it gives the same
scan resistance with simpler and cheaper bookkeeping.

### 8. Prescan

`dravity_storage::absent_pieces(info, stamps)` is a pure function. It
uses the file stamps that fastresume already collects (one
`symlink_metadata` per file) and lists every piece that touches bytes
beyond a file's length on disk. Padding files read as zeros and never
count. When resume data cannot be used, the session runs the stamps on
a blocking thread before `Event::Start`. The engine then skips those
pieces in its `Checking` phase, and they simply stay missing. A
preallocated (full-length) file gives no information, so its pieces are
hashed as before. This is always correct, because the prescan only
removes pieces that could not possibly verify.

### 9. BDP request budget

`dravity-engine/src/window.rs` adds `Bdp` next to the AIMD `Window`. The
engine measures what it needs from its own events, so no new I/O-side
event is required. The round trip is request time to block arrival
(both are already in `inflight`). The rate is the bytes delivered
between ticks, smoothed with an EWMA. The base round trip is the
minimum sample from the last 10 s. A smoothed round trip would include
the queueing our own requests cause and would drive the depth up
without bound. The depth is `max(window, bdp)` within
`max_requests_per_peer` and `reqq`. A timeout clears the estimate and
leaves AIMD in charge for 10 s, so a congested peer is not refilled
from stale numbers. `Config::request_budget` caps outstanding requests
across the torrent. The scripted tests cover the budget split across
peers and the existing timeout and `reqq` behavior. Unit tests cover the
BDP arithmetic, minimum-RTT expiry and the hand-back after a loss.

### 10. Piece reservation

Each started piece in the picker records the peer that owns it.
`pick_for` picks in this order: urgent pieces (streaming), the peer's
own pieces, unowned started pieces, a new piece (rarest first, honoring
`suggest` hints), and only then another peer's piece. The last step is
what keeps the final pieces quick, much like endgame. A peer that
disconnects or chokes us releases its reservations. A failed piece
resets its owner. irontide tracks blocks with a 256-bit chunk mask.
Dravity's picker already keeps a per-block state vector, which serves
the same purpose without a fixed limit on blocks per piece, so the
mask was not adopted. Reservation also feeds item 2: with one sender
per piece, a hash failure needs no attribution at all.

### 11. Preallocation and durability

`dravity_storage::PreallocateMode` acts when `FileStorage` opens a file
for writing. A shorter file is grown to its torrent length: `Sparse` uses
`set_len`, and `Full` calls `fallocate` (rustix). If the filesystem
reports `EOPNOTSUPP`, `ENOSYS` or `EINVAL`, or the platform has no
`fallocate`, it falls back to `set_len`. A longer file is never
truncated. Only files that receive data are created, so skipped files
and BEP 47 padding stay off disk.

`Durability` adds `Storage::sync_range(offset, len)`. Its default calls
`flush()`. `FileStorage` syncs only the files the range touches and
skips files that do not exist. `CachedStorage` forwards the call. With
`PerPiece`, the hash job syncs a passing piece before it reports
`PieceHashed`. A crash therefore cannot lose a piece the engine (and
resume data) already counted. `OnComplete` syncs everything on
`Alert::Finished`. `Never` keeps the old behavior: a sync on resume
save, on descriptor eviction and on stop.

Trade-off: a fully or sparsely preallocated file has its final length.
The item 8 prescan cannot rule it out by size, so such a torrent is
rechecked by hashing on restart without resume data. irontide
preallocates eagerly at add time. Dravity preallocates lazily on first
write, so a torrent with skipped files never reserves their space.

### 12. Seed choking and slot tuner

The choker round now takes the slot count and whether the torrent seeds.
While downloading it stays tit-for-tat. While seeding,
`Config::seed_choking` ranks the interested, non-snubbed peers:

* `FastestUpload` (the default, as before): the most bytes uploaded last
  round.
* `RoundRobin`: a peer keeps its slot until it has received
  `round_robin_quota` bytes (default 4 MiB) since its unchoke. Then the
  slot goes to the peer that has been choked longest. Peers whose turn
  is used up rank last.
* `AntiLeech`: rank by distance from the middle of the download
  (`|progress - 50%|`), so new and nearly finished peers come first.
  Ties go to the faster peer.

The slot tuner (`upload_slots_auto`) follows libtorrent's rate-based
choker. Each round it sorts peers by upload rate. It counts how many
peers exceed a threshold that starts at 1 KiB/s and rises by 1 KiB/s per
counted peer, then adds one probing slot. The result is clamped to
1..=64. Between rounds, `fill_unchoke_slots` uses the tuned count.
Everything is computed from engine counters, so it stays sans-IO.
Unit tests cover each ranking and the tuner arithmetic. Scripted engine
tests cover the round-robin hand-over and tuner growth and shrinkage.
irontide's own variants use randomness for ties. Dravity keeps the
choker deterministic, as it already did for the optimistic unchoke.

### 13. Torrent creation

`TorrentCreator::new(name, files, single, options)` sorts files by path.
BEP 52 orders the file tree that way, and a hybrid's v1 list must
match it. The caller feeds the bytes in `files()` order, split
anywhere. The creator streams both hashes:

* SHA-1 over the v1 content stream. Zeros are fed for BEP 47 pads
  (`.pad/<n>`, `attr = "p"`), which follow every unaligned file except
  the last.
* SHA-256 leaves per 16 KiB block for each file. The piece layer comes
  from `merkle::root_padded`, and the pieces root from
  `merkle::root_from_layer`. A file of at most one piece has no layer.
  An empty file has no root.

`finish()` encodes the info dictionary and the root dictionary
(`announce`, `announce-list`, `url-list`, `piece layers`, comment,
creator and date) with `dravity_bencode::Value`. Paths are checked with
Dravity's own portable component rules, so every torrent it creates is
one it can download. The automatic piece length is the smallest power
of two from 16 KiB to 16 MiB that keeps the piece count at about 1500
or fewer.

The core does no I/O, so the sans-IO tripwire still covers it.
`dravity_storage::create_torrent` walks the tree. It skips symlinks,
which could point outside it, rejects non-UTF-8 names, reads in 1 MiB
chunks, and fails if a file changes size during hashing.

Tests check every piece of v1, v2 and hybrid output with
`Info::check_piece`. Roots are compared with `merkle::file_hashes`. A
proptest shows that any split of the input gives the same info
dictionary. A storage test verifies a created directory torrent with
`FileStorage`. A CLI test covers trackers, tiers, `--private`,
`--source`, `--no-date` and `--force`. Hashing is single-threaded.
irontide hashes in parallel; this can come later because the creator
is sans-IO and its v1 and v2 streams are independent.

### 14. Tracker scrape

The codec is sans-IO. `http::scrape_url` applies BEP 48's convention:
only a last path segment that starts with `announce` has a scrape URL,
and the query is kept (passkeys). `parse_scrape_response` reads the
`files` dictionary keyed by the 20-byte hash. UDP adds action 2, with
at most 74 hashes per packet and one `(seeders, completed, leechers)`
triple per hash.

The client's UDP path is now generic over `UdpRequest::{Announce,
Scrape}`. Scrapes share the endpoint cache, the gate, the connection-id
TTL and the retry rules with announces. HTTP scrapes go through the same
SSRF guard, judged on the announce URL. The scrape URL differs only in
`announce` becoming `scrape` on the same host, so a LAN tracker
announced at `/announce` can be scraped at `/scrape`.

The session never scrapes unless asked: `TorrentHandle::scrape()` (it
replies once every tracker has answered) or
`SessionConfig::scrape_interval`. All trackers are scraped concurrently
in the background. Results go into `TrackerLog` beside the announce
outcomes. Tests run in-process trackers: an HTTP scrape, a missing
torrent, a URL without `announce`, two UDP scrapes on one connection id,
and session scrapes on demand and on a schedule.

### 15. Local service discovery

`dravity-lsd` follows the crate pattern used for protocols. The codec
is pure and covered by a purity tripwire. Encoding writes a `BT-SEARCH *
HTTP/1.1` request with `Host`, `Port`, up to 20 `Infohash` headers (so a
message stays below 1400 bytes) and a `cookie`. Parsing ignores header
case, unknown headers and malformed or duplicate hashes, and rejects
messages without a nonzero port or any hash.

The `Lsd` service binds the multicast port with `SO_REUSEADDR` (and
`SO_REUSEPORT` on Unix), so several clients on one host coexist. It
joins the IPv4 group with TTL 1 and multicast loop on, and joins the
IPv6 group when available. It drops its own announces by cookie and
accepts at most 16 messages per source address per minute. Unicast mode
(`LsdConfig::unicast`) sends to fixed addresses, which keeps the tests
independent of multicast routing.

The session starts LSD with the listener, since it needs a port to
advertise. A task checks the catalog every 5 s. It announces running
torrents that are public and whose metadata is known, each at most once
per `interval` (default 5 min). It passes discovered peers to the
torrent as `Command::AddPeers`, again only for public torrents.
Magnets wait for their metadata, which tells whether they are private.
Tests cover two services exchanging announces (split over two
messages, own announces filtered), a leecher that finds a seed only
through LSD and completes, and a private torrent that is never
discovered. The library default is off, so embedders opt in. The CLI
turns LSD on.

### 16. Live settings, rate limits and queueing

- `dravity-session/src/settings.rs` holds the live subset (`LiveSettings`) and the update type. `SettingsUpdate` fields are `Option<Option<_>>`: absent leaves a value alone, `Some(None)` lifts it. `applied_to` validates every field and builds the next value before anything is touched; `Session::apply_settings` then swaps it in under one mutex and pushes the rates and connection cap. `SessionConfig` runs through the same validation in `Session::new` (`SessionError::Settings`). `max_connections = Some(0)` stays valid (it holds every dial until raised, as before).
- `rate.rs` is a debt token bucket per direction shared by all peer tasks (`RateLimits`). A peer task takes download tokens after forwarding a received `Piece` and upload tokens before writing one, so session caps apply across torrents on top of each torrent's engine limit. A wait is capped at 2 s per acquisition so a lowered limit takes effect quickly and keep-alives are not starved. Rates below 16 KiB/s are refused because one 16 KiB block per second is the slowest pace peers tolerate.
- `queue.rs`: `plan()` is a pure function from the catalog (running, paused, queued, complete, added order) and the limits to keep/start/queue decisions, unit tested on its own. A task (`run`, about once per second, holding only a `Weak<Inner>`) learns completion from each handle's stats, then stops the newest surplus torrents (marked `queued`) and starts the oldest queued ones. Paused and failed torrents take no slot; pausing or resuming clears `queued`. `add_*` queues a new torrent at once when its class is full.
- **Partial:** irontide also rebinds the listen port live with rollback on failure; Dravity's listen port stays a restart setting because the listeners, port mapping and LSD announces are all bound to it at start. Live settings and the queued flag are not persisted by `dravity serve` (the CLI flags apply at each start).
- Tests: settings and queue-plan unit tests, `rate.rs` pacing tests, `tests/settings.rs` (all-or-nothing update, a 96 KiB download paced by a 32 KiB/s session limit, one-at-a-time queue then live lift and seed limit), the HTTP `settings_are_read_and_patched_all_at_once`, CLI flag tests and the TUI limit-prompt test.

### 17. BEP 53 select-only magnets

- Parsing lives in `dravity-core/src/magnet.rs`: comma-separated decimal indices and `a-b` ranges (no signs, spaces, empty items or reversed ranges), stored as `RangeInclusive<usize>` so `so=0-18446744073709551615` costs one entry rather than an expansion. More than 1024 ranges are refused so selection checks stay cheap.
- The session copies the ranges into `MagnetStart`. When the magnet switches to downloading and no priorities were given, `files::selected` turns them into Normal/Skip per file (indices are the torrent's file list, the same ones `dravity info` prints, BEP 47 pads included). If no listed index exists, every file stays Normal rather than skipping the whole torrent.
- The selection is part of the magnet URI, so a saved session restores it.
- Tests: parser unit tests (valid lists, every malformed form, the range cap), the proptest round trip now includes `so`, a doc test for `selects_file`, `files::selected` unit test and `a_select_only_magnet_downloads_just_its_files` (a real download fetches only `b.bin`).

### 18. Wire extensions

- Wire (`dravity-wire/src/ext_msgs.rs`): `lt_donthave` is a 4-byte big-endian piece index; anything else drops the peer, as does an index past the end. `lt_tex` payloads are `{added: [url…]}`; decoding keeps at most 50 URLs and only `http`/`https`/`udp` ones of at most 512 bytes without whitespace or control characters, so a peer cannot plant `file:` URLs or giant strings. Tracker URLs still go through the tracker client's SSRF guard (item 1) when announced.
- Engine: our handshake now always registers `lt_donthave` (id 3) and `lt_tex` (id 4) when enabled; the extension list is sorted. `upload_only` follows the torrent status (`Finished` means nothing more is wanted) and a change re-sends the handshake to every extended peer (BEP 10 allows repeats). A received `upload_only` marks the peer as a partial seed in the PEX flags. It is deliberately *not* a reason to disconnect: a partial seed may want more after a priority change (the session's skip-then-unskip test caught this), and the seed would otherwise cost it a redial after `peer_retry`.
- Super-seeding is active only while the torrent is complete. Peers get `HaveNone` (fast extension) or nothing instead of a bitfield and no allowed-fast set, then one `Have` for a piece they lack, chosen by (times revealed, swarm availability, index). Requests for any other piece are rejected. When the peer announces its piece, it is shown the next one. irontide waits until a *different* peer announces the piece; Dravity reveals on the receiver's own `Have`, the simpler variant libtorrent also uses, which is safe because the reveal counter still spreads pieces.
- Our tracker list goes out once per peer after its handshake; received URLs not yet known (at most 100 in total) are reported once with `Effect::TrackersReceived`. Learned URLs are not relayed to other peers. The session appends them as one new tier, so the torrent's own tiers keep priority and they show in `TorrentHandle::trackers`.
- Tests: wire unit and doc tests, five engine tests in `tests/seeding.rs` (upload_only both ways, lt_donthave receive and send, lt_tex with filtering and the off switch, super-seeding reveal/reject/next) and `tests/exchange.rs` (a leecher learns the seed's tracker; private torrents do not).

### 19. BEP 40 peer priority

- `dravity-core/src/priority.rs` is a self-written CRC-32C (bitwise, Castagnoli polynomial) plus the BEP 40 masking: IPv4 keeps `ff.ff.55.55`, `ff.ff.ff.55` for the same /16, everything for the same /24; same host compares the ports. IPv4-mapped IPv6 counts as IPv4. For IPv6 the first 6, 7 or 8 bytes are kept whole depending on the shared /48 or /56 and the rest masked with `0x55`. Both BEP 40 IPv4 vectors (`0xec2d7224`, `0x99568189`) are unit and doc tests.
- Engine: `external_ip` comes from the latest `yourip` in an extension handshake (unspecified and loopback ignored, so loopback tests are unchanged). At the cap, `on_connected` compares the newcomer with the connected peer of lowest rank (using its reachable address) and evicts that peer only if the newcomer ranks strictly higher; the ranking is deterministic, so two peers cannot flap. While our address is unknown the old behaviour (refuse the newcomer) stays.
- Dial order stays FIFO: when below the cap every queued address is dialed anyway, and at the cap no dial is made, so ranking the queue would change nothing; the eviction path is where BEP 40 matters.
- Tests: vectors, symmetry, same-host ports, mapped addresses, IPv6 symmetry, and `at_the_cap_a_higher_bep40_priority_replaces_the_lowest` (unknown address refuses, lower rank refused, higher rank replaces).

### 20. IP filter

- `dravity-session/src/ip_filter.rs` is pure parsing and lookup: lines are tried as eMule (`range , level , description`), then as a bare range, then as P2P (`description:range`, split at the last colon so descriptions may contain colons or dashes). eMule octets may carry leading zeros, which `Ipv4Addr::from_str` refuses, so octets are parsed by hand. Ranges are kept as `(u32, u32)` and `(u128, u128)` intervals, sorted and merged (adjacent ranges too), and looked up with a binary search; IPv4-mapped IPv6 is checked as IPv4. Unreadable lines are counted (`skipped()`), and a file with no usable line at all is an error so a wrong path or format is not silently an empty filter. Files are read lossily so a description in a legacy encoding does not drop its range.
- The filter lives in the session's `BanList` next to the ban set, so the listener's existing pre-handshake check covers it with no new path. Dials check it in `TorrentTask::connect` and report `PeerConnectFailed { retry: false }`, so PEX, DHT, LSD and tracker peers in a blocked range are dropped from the pool. `set_ip_filter` swaps an `Arc` and sends every torrent `Command::Refilter`, which closes connections (including during a magnet's metadata phase) the new filter blocks. Trackers and web seeds are not filtered (they are URLs, guarded by the SSRF checks).
- Port filtering from irontide is not added: blocklists in the wild are address-only, and the session already refuses port 0.
- Tests: three parser unit tests (eMule levels, P2P, CIDR, IPv6, merging, malformed lines, reversed and mixed bounds, `0.0.0.0/0`), a doc test, `tests/ip_filter.rs` (a filtered peer is never dialed until the filter changes; the listener refuses filtered addresses; a live change closes an open connection) and a CLI test (unreadable file is an error; a loopback eMule range keeps the download from finishing).

### 21. PCP

- `pcp.rs` is a self-written codec of the 60-byte MAP request/response (24-byte header plus 36-byte MAP body). Responses must be version 2 with opcode `0x81` and result 0; version 0 means a NAT-PMP-only gateway (`PcpError::NatPmpOnly`), other result codes are reported with RFC 6887 names. A response must carry the request's nonce, protocol and port to count. The module is in the crate's purity tripwire.
- Mapper: `map_pcp` learns our address towards the gateway, sends one MAP per mapping with its own nonce (12 bytes from `RandomState` and the clock: unique and not guessable off-path, which is all RFC 6887 §11.1 asks), records the assigned port and public IPv4 address, and keeps the requests in `Active::Pcp` so renewals happen through the normal remap cycle and the delete on stop reuses the nonce with lifetime 0. Retransmissions follow the NAT-PMP schedule. Any PCP failure falls back to NAT-PMP, so old routers behave as before.
- **Partial:** irontide also requests IPv6 pinholes. The codec handles IPv6 client and external addresses, but the mapper only knows an IPv4 default gateway (`default_gateway`), and IPv6 needs no address translation, only a firewall pinhole on routers that block inbound; adding IPv6 gateway discovery is left for later.
- Tests: codec unit tests (layout, success, IPv6 external address, refusal, NAT-PMP-only, malformed and truncated input), a doc test, and mapper tests against a fake PCP gateway (map, report, delete with the same nonce, one nonce per mapping) and against a NAT-PMP-only gateway (PCP tried first, NAT-PMP used).

### 22. Native DHT building blocks

- `krpc.rs` decodes and encodes `ping`, `find_node`, `get_peers` and `announce_peer` queries, responses (`id`, `nodes`, `nodes6`, `values`, `token`) and errors with typed results. It keeps at most 100 nodes or peers per response and drops port-0 entries, so hostile responses stay cheap; ids must be exactly 20 bytes. The BEP 5 examples are decoded in tests and every message type round-trips.
- `secure_id.rs` implements BEP 42 with the CRC-32C from item 19: the IPv4 mask `03 0f 3f ff` (IPv6 `01 03 07 0f 1f 3f 7f ff` on the first 8 bytes) with `r` in the top bits, 21 checked bits and the random last byte. All five BEP 42 vectors pass; private, loopback and link-local addresses are exempt as the BEP says.
- `demux.rs` classifies by first byte: a bencoded dictionary is KRPC, a 20+ byte packet with version 1 and type 0 to 4 is uTP, and `00 00 00 0..3` with 8+ bytes is a BEP 15 tracker response. The three never overlap.
- All three are in a new purity tripwire test for the crate.
- **Partial, why:** the item asks to replace `mainline` with a native dual-stack DHT (routing table, lookups, BEP 43/44/51/33) on one socket shared with uTP and UDP trackers. That is several weeks of work (the comparison rated it L) and swapping a working DHT for a new one needs interop testing against the live network, which this offline environment cannot do. These modules are the tested foundation for that swap: the codec, the id rules and the socket classifier a shared socket needs.

### 23. Proxy support

- `dravity-session/src/proxy.rs` keeps the protocol bytes in plain functions (`socks4_request`/`socks4_reply`, `socks5_greeting`/`socks5_auth`/`socks5_request`/`socks5_reply_len`, `http_connect_request`/`http_connect_reply`, `udp_header`/`parse_udp_header`), tested without sockets, and `proxy::connect` drives them over a `TcpStream`. The HTTP reply is read byte by byte (capped at 8 KiB) so no peer bytes after the header are swallowed. `Proxy::parse` takes `socks4://`, `socks5://` and `http://` URLs with percent-encoded credentials; `Debug` never prints the password.
- Peer dials go through the proxy in `peer::connect` when it is set. uTP is skipped (a TCP proxy cannot carry it), and failures surface as ordinary connect failures so the engine's retry and back-off logic is unchanged.
- Tracker and web seed clients take an explicit `http_proxy` via `reqwest::Proxy::all`. The session fills both from an HTTP `SessionConfig::proxy` unless they already set their own. The web seed client keeps `no_proxy()` otherwise, and the tracker client keeps reqwest's default.
- Tests: five unit tests on the byte formats (SOCKS4/5 requests and replies, RFC 7617 Basic example, UDP header round trip, URL parsing and refusal), a doc test, `tests/proxy.rs` (a full download through an in-process SOCKS5-with-auth, SOCKS4 and HTTP CONNECT proxy, checking the proxy saw only the seed; a refused SOCKS5 password means no connection), and a CLI test for a bad `--proxy` URL.
- **Partial, why:** (1) SOCKS proxies do not carry HTTP trackers or web seeds, because reqwest's `socks` feature needs the `tokio-socks` crate, which is not available in this offline build. Use an HTTP proxy for those. (2) UDP is not proxied: DHT, UDP trackers, LSD and incoming uTP still go direct. The UDP ASSOCIATE codec is the groundwork for a relay. (3) No I2P/SAM, which the comparison rated L. Incoming connections are never proxied, which matches libtorrent's behaviour for SOCKS without BIND.

### 24. Peer extras

- **GeoIP** (`dravity-session/src/geoip.rs`): there is no MaxMind reader in the offline dependency set, and MMDB parsing is a large format, so the database is the free CSV layout both db-ip lite and IP2Location LITE ship: `start,end,CC`, with IP or decimal-integer bounds, optional quotes, and extra columns ignored. Ranges are sorted and same-country neighbours merged; overlaps keep the earlier range. Lookups are a binary search, IPv4-mapped IPv6 is looked up as IPv4, `-`/`ZZ` mean unassigned, and the file is capped at 256 MiB. `CountryCode` is a validated, upper-case `[u8; 2]`. Web seeds (synthetic `100::/64` addresses) never get a country.
- **DSCP** (`qos.rs`): `socket2::SockRef::set_tos_v4` / `set_tclass_v6` with the value shifted into the top six bits (ECN left clear). This is safe code; socket2's `all` feature is enabled for the v6 call, which is the same crate with no new dependency. Outgoing dials (direct and proxied) and accepted connections are marked. A failure is logged at debug level and never drops the peer. Values above 63 are refused by `Session::new` (as `SettingsError::Invalid`) and by clap. uTP is not marked because its sockets are shared and owned by `dravity-utp`.
- **Web seed stats**: the task keeps `failures`, `retry_at` and `stopped` per URL as before. It now also writes them to the catalog entry and the `.state` record whenever they change: on each failure, on a stop or ban, and when a failing seed delivers data again, but not on every piece. Only unhealthy seeds are stored, at most 256, with `retry_at` as Unix seconds. A restored task gets them through `TorrentContext` (the catalog is locked while a task is created, so reading it back would deadlock) and resumes the remaining back-off. Malformed entries are skipped, and older records without the key still load.
- Tests: GeoIP parser tests (formats, merging, overlaps, mapped IPv6, unassigned) and a doc test; DSCP byte layout plus a real socket checked with `IP_TOS` on Linux; record round trips (healthy seeds dropped, junk skipped); `tests/peer_extras.rs` (a live peer reports its country with DSCP set on both ends, DSCP 64 is refused, and web seed back-off is saved, survives a restart, and is not retried early); TUI render and row tests; CLI tests for `--peer-dscp 64` and a missing `--geoip` file.

### 25. BEP 55 holepunch

- Wire: `holepunch.rs` is the BEP's binary layout (`msg_type`, `addr_type`, 4 or 16 address bytes, port, `err_code`). Unknown types, wrong lengths and an error message with code 0 are refused; unknown non-zero codes decode as `HolepunchError::Other`. The wire crate's purity test covers it.
- Engine relay: a `Rendezvous(target)` is matched against connected peers by dialed or reachable address. The relay sends `Connect` to both sides, each carrying the other's reachable address, or `Error` with `NoSelf`, `NotConnected` or `NoSupport`. A malformed message drops the sender, like other malformed extension messages.
- Engine initiator: when a PEX message carries the holepunch flag (`0x08`) and the sender itself speaks `ut_holepunch`, the address is remembered with that sender as its relay (at most 1000). If dialing the address fails, a `Rendezvous` goes to that relay if it is still connected, at most once per address, so a peer that can never be reached does not cause a loop.
- Engine connect: a `Connect(addr)` is dialed at once when the torrent is active, under the peer cap, and the address is not banned or connected. The dial goes through `PeerPool::dial_now`, so the usual in-flight accounting and retry on failure apply, with `DialHints::utp = Some(true)` so the session tries uTP first. Both sides send a uTP SYN at the same moment; when both connections succeed, the existing duplicate-connection rule keeps one.
- PEX: we set flag `0x08` for connected peers whose handshake lists `ut_holepunch`.
- Scope: only the download/seed state relays. The metadata phase of a magnet does not, because its peers are short-lived. Whether a punch gets through a real NAT cannot be tested on loopback, so the tests check the protocol exchange, not NAT traversal.
- Tests: a codec round-trip and refusal test plus a doc test, and two engine tests: the relay pairs two peers and returns each error code, and a failed dial asks the PEX source once and dials over uTP on `Connect`, with the feature off when disabled. The CLI test now passes `--no-holepunch`.

### 26. BEP 52 conformance vectors

- The vectors are not produced by Dravity. `tests/data/bep52_vectors.py` is a separate, deliberately naive implementation written from the BEP text: it builds the full tree with 32 zero-byte padding leaves, and builds each piece-layer hash from the piece's leaves padded to a full piece. Its output is pasted in as `VECTORS`, and the script is kept so the vectors can be regenerated.
- Sizes: 1, 100, 16383, 16384, 16385, 32767, 32768, 32769, 65536, 65537, 3 leaves + 5 bytes, 7 leaves, and 9 leaves + 1 byte, each at three piece lengths. That covers files under one leaf, an exact leaf, one byte over, an exact piece, a short last piece, and leaf counts that are not a power of two.
- Checks: `merkle::file_hashes` (root and layer), `root_from_layer` agreeing with the root, `pad_hash(0)` being zeros and `pad_hash(1)` being SHA-256 of 64 zero bytes, and an empty file having no root. Real multi-file v2 torrents are built with `TorrentCreator` and their `pieces root`s and `piece layers` compared with the vectors. Single-piece files must have no layer, and the v2 info hash must equal SHA-256 of `info_bytes()`.
- All three tests passed on the first run apart from a test bug: v2 files are aligned to pieces, so the stream model shows padding entries, and the test now skips them. No library change was needed.

### 27. More loom models

- Dravity's shared mutable state is the connection counter (`admit`, modelled since item 4) and, from item 16, the session-wide rate buckets and the live connection cap. The buckets lived in `dravity-session` behind a `std` mutex, out of loom's reach. The arithmetic is now `dravity_core::rate::TokenBucket`: pure, with the clock passed in, so it fits the core purity test. `SharedBucket` is that bucket behind `Arc<Mutex<_>>`, using `loom::sync` under `--cfg loom` the same way `admit.rs` does. `dravity-session::rate::Limiter` is a thin wrapper on tokio's clock (`Instant::into_std`), so paused-time tests still drive it.
- Models: (1) two takes of 500 bytes at 1000 B/s at the same instant: in every interleaving the waits are exactly 0.5 s and 1 s and the debt is exactly 1000. (2) `set_rate` racing `take`: the new rate always wins, the debt is exactly the taken bytes, and the wait is priced at the rate that held the lock. (3) `set_max(Some(0))` racing `try_admit`: once the setter has joined, no admit succeeds, the count never exceeds the old cap, and every slot returns.
- Found while writing them: two callers can reach the lock with timestamps out of order. The old code set `last = now` regardless, so a later caller with an earlier timestamp moved the clock back and the next caller was credited the same interval twice. `last` now only moves forward, and a unit test covers it.
- dcheck's loom step (`--test 'loom_*'`) picks up the new file automatically.

### 28. Pluggable network transport

- The traits are object-safe (boxed futures), so a `SessionConfig` holds an `Arc<dyn Network>` and stays `Clone`. `listen` is synchronous, so `Session::new` can report `listen_port` straight away, and it is called once. Streams are `Box<dyn PeerIo>` (any `AsyncRead + AsyncWrite + Send + Unpin + Debug`) and enter the existing pipeline as `RawStream::Boxed`. MSE, the BitTorrent handshake, rate limits, bans and the connection cap all apply unchanged; status reports these connections as TCP.
- With a network set, `SessionConfig::listen` is ignored: no TCP or uTP sockets are bound, so there is no port mapping or LSD announce for them, and dials skip uTP and proxies. Trackers, web seeds, the DHT and LSD keep their own sockets, so in-memory tests turn them off (`dht = None`, `lsd = None`, `port_mapping = None`).
- `MemoryNetwork::host(ip)` gives each session a host address. Its listener gets a port from a shared counter, and a dial delivers one end of a 256 KiB duplex pipe to the listener together with the dialer's address. `set_unreachable` makes dials to an address fail like a NAT or firewall would, and dropping a listener unregisters it, so later dials are refused.
- Tests: a transport unit test (dial, accept, data, unreachable, listener gone), and `tests/memory_swarm.rs`, where one seed and three leechers download over memory only and the seed sees only memory addresses, and an unreachable seed blocks the download until the fault is lifted. Both run in about 0.6 s. The existing loopback swarm tests stay, since they cover the real TCP/uTP paths.
