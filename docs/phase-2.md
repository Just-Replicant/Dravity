# Phase 2 — Magnets and swarm: notes and decisions

Scope (plan §8): BEP 9/10 metadata, DHT via `mainline`, PEX, endgame,
piece stealing, fastresume, an AIMD window, and rate limits. Also the
UDP connection-id cache and BEP 12 tiers deferred from Phase 1.

Phase 3 added listening, seeding, the upload limit, and DHT
`announce_peer`. This phase did not announce or set the BEP 5 reserved bit.

## Decisions

- A magnet starts as `MetadataState`. It announces with `left` set to one
  metadata piece, dials peers, and fetches 16 KiB metadata pieces (two in
  flight per peer). The assembled `info` is hashed against the info-hash;
  a mismatch disconnects every peer that contributed, bans those
  addresses, and starts over. A `reject` or a timeout moves that piece;
  the peer is skipped until `request_timeout`. The first in-range
  `metadata_size` is kept only while it makes progress, and is abandoned
  when every request for it has failed and another peer advertises a
  different size. Sizes above `max_metadata_size` (8 MiB) are refused.
  `into_handover()` returns the open connections and the messages to
  replay, so the session builds `TorrentState` without dropping peers or
  sending a second `started`.
- `TorrentState` accepts peers during the initial check and sends no
  bitfield or interest until the check finishes.
- `with_info_bytes` advertises `metadata_size` and answers `ut_metadata`.
  Without those bytes, requests are rejected.
- PEX adds at most 200 peers per message. Every `pex_interval` (60 s) a
  peer gets the added and dropped diff since its last message, at most
  50 each. Private torrents disable PEX and DHT. A magnet learns
  `private` only with the info-dict; addresses learned from DHT or PEX
  during the fetch are then dropped.
- Each peer starts at `initial_requests_per_peer` (4) and grows to
  `max_requests_per_peer` (128) or the peer's `reqq`, whichever is
  smaller: doubling in slow start, then +1 per window. A timeout halves
  the effective limit and cancels requests that no longer fit. A `piece`
  is accepted only from a peer that was asked for that block.
- A block has at most two owners. In endgame an idle peer duplicates
  outstanding requests. Before that, an idle peer may steal a block held
  for `steal_after` (10 s). When one copy arrives the other owners get
  `cancel`. The block returns to the picker only when no owner is left.
- `download_rate_limit` is a token bucket (one second of burst, at least
  one block) that paces requests.
- `dravity-dht` wraps `mainline` on its own thread. The engine emits
  `Effect::DhtLookup` on start and every `dht_interval` (5 min). A lookup
  is bounded by `dht_lookup_timeout` (30 s). A timeout, or an empty result
  when `Dht::has_nodes` is false, retries after `dht_retry` (15 s). DHT is
  on by default and off in tests unless they bring a loopback `Testnet`.
- On stop, after the final flush, the session saves
  `<resume_dir>/<info-hash>.resume`: the verified-piece bitfield and a
  length plus mtime stamp per file. The next start skips `check_existing`
  only when the info-hash, piece count, and every stamp match. Saving is
  atomic. In-flight writes are drained and every file is synced first,
  including files already evicted from the descriptor cache. The library
  leaves `resume_dir` unset; the CLI uses `<output>/.dravity`.
- BEP 12: tiers are shuffled once and announced concurrently. Trackers in
  a tier are tried in order until one answers, and that tracker moves to
  the front. Each magnet `tr` entry is its own tier.
- UDP connection ids are cached per tracker address for 60 s and shared
  by `TrackerClient` clones, with the UDP socket that performed
  `connect`. Announces to one tracker take turns on that socket. A
  failure with a cached id drops that id and retries once with a fresh
  `connect`. An I/O error drops the socket and retries once from a new port.
- The handshake advertises BEP 10.

## Left for later

Phase 3: incoming peers, seeding, choking, the upload limit, DHT
`announce_peer`. IPv6 DHT and BEP 5 `port` waited on a DHT backend that
could do them. v2-only magnets waited on BEP 52. Periodic resume writes
and a storage factory for magnets waited on Phase 4. Piece ties still
use the lowest index.
