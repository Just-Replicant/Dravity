# Phase 1 — MVP downloader: notes and decisions

Scope (plan §8): load `.torrent`; HTTP and UDP trackers; TCP peers;
rarest-first with pipelined requests; filesystem storage; piece hash
verify. `dravity-session` drives I/O; `dravity-cli` only renders alerts.
Phase 0 items deferred here are included.

Later phases replaced the download-only limits (no unchoke, no listen),
announce-all-tiers, and the missing UDP connection-id cache.

## Decisions

- Every `update` takes `now: Instant`. Peers are `PeerKey(u64)`.
- One owner task per torrent calls `update`. Peer queues use `try_send`
  (a full queue drops the slow peer). Disk, hash, and tracker work is
  spawned and reports on a bounded event queue.
- The engine stops requesting while `max_pending_writes` blocks are in flight.
- Wanted pieces are ordered by `(availability, index)`. Partial pieces
  finish before new ones start. Ties take the lowest index.
- Blocks go missing → requested → received → written. A piece is hashed
  only when every block is stored. Late blocks for timed-out requests are
  kept. A bad hash resets the piece.
- Hash and disk I/O use Tokio's blocking pool. Hashing is capped by one
  session-wide pool (`dravity-session/src/hash_pool.rs`).
- Resume hashes existing data (`check_concurrency`) before announce.
  Missing or short files count as absent.
- Tracker tiers are announced together; any answer succeeds. `stopped` is
  awaited with a timeout. UDP retries with exponential back-off (BEP 15)
  and drops datagrams with another transaction id.
- TLS is reqwest with rustls on the `ring` provider and the platform
  verifier. `CDLA-Permissive-2.0` is allowed for Mozilla's root data.
- Files are created lazily under the download directory from validated
  `Info::file_path`. An existing symlink is refused. BEP 47 padding is
  not written and reads as zeros. Positional I/O; an LRU caps open files.
- Bytes after the handshake are passed to `Framed::from_parts`, so a
  coalesced `bitfield` is not stuck until the next read.
- Download-only: peers stay choked, so `request` and `cancel` are ignored.
  `anyhow` is allowed only in `dravity-cli`.
- A failed dial or a dropped connection retries after `peer_retry`
  (default 30s). A wrong info-hash, our own peer id, or a malformed
  handshake does not.
- `bitfield`, `have all`, and `have none` stay legal after keep-alives,
  DHT `port`, and BEP 10 extended messages. A bitfield after any other
  message disconnects.
- Re-announce waits at least each answering tracker's `min interval`.
  With no trackers, explicit peers are still used.
- A hash mismatch or missing data is downloaded again. Any other storage
  error while hashing stops the torrent.

## From Phase 0

`Bitfield::from_wire`, `PeerCodec`, the per-torrent owner task, and codec
benches. Symlink checks use `symlink_metadata` before open; dir-fd and
`O_NOFOLLOW` stayed for Phase 3.

## Left for later

Phase 2: magnets, DHT, PEX, endgame, fastresume, rate limits, UDP
connection-id cache, BEP 12 tiers. Phase 3: listening, seeding, choking,
peer bans, race-free paths. Phase 4: persistence, HTTP API, TUI, file
priorities.
