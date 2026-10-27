# Phase 2 — Magnets and swarm: notes and decisions

Scope from the roadmap (plan §8): BEP 9/10 metadata; DHT via
`dravity-dht`/`mainline`; PEX; endgame; piece stealing; fastresume; AIMD
window + rate limits. Also picks up the items Phase 1 deferred to Phase 2:
UDP tracker connection-id caching and BEP 12 tier ordering.

## Crates

| Crate | I/O? | Phase 2 additions |
|---|---|---|
| `dravity-core` | no | `compact` peer codec (BEP 23/7 compact v4/v6), `Metainfo::from_info_bytes` for metadata fetched from peers |
| `dravity-bencode` | no | `Value::decode_prefix` (a value followed by raw bytes, as in `ut_metadata` data messages) |
| `dravity-wire` | no | `MetadataMessage` (BEP 9 `ut_metadata`), `PexMessage` (BEP 11 `ut_pex`) |
| `dravity-engine` | no | `MetadataState` (magnet metadata fetch + handover), extension handshake, metadata serving, PEX, AIMD window, endgame, block stealing, download rate limit, DHT scheduling |
| `dravity-storage` | blocking | `resume`: `ResumeData`, `FileStamp`, `Storage::stamps`, atomic `save_resume` / `load_resume` |
| `dravity-tracker` | async | UDP connection-id cache |
| `dravity-dht` (new) | async (own thread) | `Dht` (`get_peers`, `announce`) and a loopback `Testnet`, wrapping `mainline` |
| `dravity-session` | async | `Session::add_magnet`, DHT lookups, fastresume load/save, BEP 12 tiers |
| `dravity-cli` | via session | magnet links, `--no-dht`, `--download-limit`, `--resume-dir`, `--no-resume` |

## Decisions

- **Magnets run two state machines.** A magnet starts as a sans-IO
  `MetadataState`: it announces (with `left` set to one metadata piece so
  trackers return peers), dials peers, exchanges BEP 10 handshakes and asks
  for 16 KiB metadata pieces (two in flight per peer). The assembled `info`
  dictionary is hashed against the info-hash; a mismatch disconnects every
  peer that contributed and starts over, a `reject` or a timeout moves on to
  other peers, and sizes above `max_metadata_size` (8 MiB) are refused.
  `into_handover()` then returns the open connections with the messages to
  replay (extension handshake, availability, `unchoke`), the known
  addresses, and whether `started` was announced. The session builds the
  storage and a `TorrentState` and replays all of that as ordinary events,
  so no connection is dropped and no second `started` is sent.
- **`TorrentState` accepts peers while checking** (needed for the handover)
  but sends no bitfield or interest until the check is done.
- **Metadata serving.** A `TorrentState` built `with_info_bytes` advertises
  `metadata_size` and answers `ut_metadata` requests; otherwise it rejects
  them.
- **PEX (BEP 11).** Peers from `ut_pex` are added to the pool (capped at
  200 per message on decode); every `pex_interval` (60 s) each peer gets
  the added/dropped diff since its last message (at most 50 entries each).
  Private torrents (BEP 27) disable PEX and DHT.
- **AIMD request window.** Each peer starts at `initial_requests_per_peer`
  (4). Blocks that arrive in time grow it — doubling in slow start, then +1
  per window — up to `max_requests_per_peer` (128) or the peer's `reqq`,
  whichever is smaller. A request timeout halves it.
- **Endgame and stealing share one mechanism.** A block may have at most two
  owners. Once every missing block is requested (endgame) an idle peer
  duplicates outstanding requests; before that, an idle peer may "steal" a
  block whose holder has not delivered after `steal_after` (10 s). When a
  copy arrives the other owners get `cancel`. A block goes back to the
  picker only when no owner is left.
- **Rate limit.** `download_rate_limit` is a token bucket (one second of
  burst, at least one block) that paces *requests*, so the engine stays
  clock-free and nothing is buffered beyond what was asked for. Upload
  limits come with seeding in Phase 3.
- **DHT through `mainline`.** `dravity-dht` wraps `mainline` 8 (MIT,
  MSRV 1.85), which runs the node on its own thread; its types stay out of
  the public API. The engine emits `Effect::DhtLookup` on start and every
  `dht_interval` (5 min); the session runs a lookup bounded by
  `dht_lookup_timeout` (30 s) and returns `Event::DhtPeers`. The session's
  DHT is on by default (`SessionConfig::dht`) and off in tests unless they
  bring a loopback `Testnet`. `mainline` brings a second
  `sha1`/`digest` generation; cargo-deny reports the duplicates as
  warnings (`multiple-versions = "warn"`).
- **No DHT `announce_peer` yet.** We do not accept incoming connections
  until Phase 3, so announcing ourselves would only send peers to a closed
  port. `Dht::announce` exists (and is tested) for when we listen. For the
  same reason we do not set the BEP 5 DHT reserved bit, and `port` messages
  are ignored (`mainline` has no API to add a node by address).
- **Fastresume.** On stop, after the final flush, the session saves
  `<resume_dir>/<info-hash>.resume`: the verified-piece bitfield and a
  length + mtime (ns) stamp per file. On the next start the data is trusted —
  `check_existing` is skipped — only if the info-hash, piece count and every
  stamp still match; anything else (a touched, truncated, created or deleted
  file) falls back to the full check. Saving is atomic (synced temporary file
  + rename). Resume is off by default in the library
  (`SessionConfig::resume_dir = None`); the CLI uses `<output>/.dravity`.
- **BEP 12 tiers.** Tiers are shuffled once, announced concurrently, and the
  trackers within a tier are tried one after another until one answers; that
  tracker moves to the front of its tier. Asking every tier at once trades
  some tracker load for faster peer discovery (libtorrent's
  `announce_to_all_tiers`). Magnet `tr` entries each form their own tier.
- **UDP connection ids** are cached per tracker address for 60 s (BEP 15)
  and shared between `TrackerClient` clones. A failure with a cached id (an
  error response, or no answer to a single attempt) drops it and retries
  once with a fresh `connect`.
- **We advertise BEP 10** in the handshake reserved bits so peers send their
  extension handshakes.

## Phase 1 deferrals handled here

| Item | Where |
|---|---|
| Magnets, BEP 9/10 metadata, DHT, PEX | `dravity-engine` (`MetadataState`, `ext`), `dravity-wire`, `dravity-dht`, `dravity-session` |
| Endgame, piece stealing, AIMD request window, rate limits | `dravity-engine` (`state`, `window`, `rate`) |
| Fastresume | `dravity-storage::resume`, `dravity-session` |
| UDP tracker connection-id caching; BEP 12 tier ordering | `dravity-tracker`, `dravity-session` |
| Random tie-breaking among equally rare pieces | Not done: endgame and stealing already cover the slow-tail problem; ties still go to the lowest index |

## Tests

- Wire: unit tests and proptest roundtrips for `ut_metadata` and `ut_pex`;
  fuzz targets `wire_metadata`, `wire_pex`.
- Engine: scripted sequences for the extension handshake and metadata
  serving, PEX (including private torrents), window growth and `reqq`,
  endgame duplicates and cancels, stealing from slow peers, rate pacing, DHT
  scheduling, peers handed over while checking; `MetadataState` fetch,
  verification and handover, bad metadata, rejects, timeouts, oversize; the
  proptest now includes extended messages and checks no peer gets the same
  block twice.
- Storage: resume roundtrip, bitfield validation, stamp invalidation, atomic
  save; fuzz target `storage_resume`.
- Tracker: a counting UDP tracker shows ids are reused, shared by clones and
  refreshed after the tracker forgets them.
- DHT: announce + lookup on a loopback testnet; an isolated node.
- Session (loopback): magnet via a tracker, magnet via a DHT testnet, peers
  learned only through PEX, fastresume (skip, then invalidation), tier
  fallback; existing Phase 1 swarm tests.
- CLI: download with `--no-dht --download-limit`, resume file written,
  `info` for magnets, bad rate rejected.

Not covered: real public swarms. The sandbox this was built in drops
BitTorrent UDP traffic (DHT routers and UDP trackers never answer), so the
DHT and magnet paths were verified on loopback only.

## Deferred

| Item | Phase |
|---|---|
| Listening for incoming peers, seeding, choking, upload rate limit, DHT `announce_peer` | 3 |
| BEP 5 `port` message → DHT node | 3 (needs an add-node API or another DHT backend) |
| IPv6 DHT (BEP 32) | 3, with IPv6 |
| v2-only magnets (`btmh` without `btih`) | 5, with BEP 52 |
| Custom storage backends for magnets (size unknown up front) | 4 (storage factory) |
| Resume data written periodically during a download (now: on stop only) | 4, with session persistence |
