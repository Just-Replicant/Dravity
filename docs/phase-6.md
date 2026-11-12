# Phase 6 — v2, web seeds, io_uring, and encryption: notes and decisions

Scope (plan §8): BEP 52 v2 and hybrid torrents, BEP 19 web seeds, an
optional io_uring backend, and MSE/PE. The engine stays sans-IO. The
session is still the only I/O driver.

Before this phase, `Metainfo::from_bytes` refused a torrent without v1
`pieces`, ignored a hybrid `file tree`, and the session refused a magnet
without `btih`. `url-list` and the magnet `ws` parameter were dropped.

For a v2-only torrent the session's info-hash is the v2 hash truncated
to 20 bytes. `dravity-http` carries that id unchanged.

## BEP 52

- One content stream for every version. A v2 file starts on a piece
  boundary. A hybrid says so with BEP 47 padding. A v2-only torrent gets
  the same `.pad/<n>` entries synthesized (never written). Only
  verification differs.
- `Info::check_piece` uses SHA-1 (v1), SHA-256 against the piece layer or
  pieces root (v2), or both (hybrid: both must agree). A short last piece
  is padded with zero leaves up to its subtree. A hash failure still
  re-downloads and can ban.
- A file longer than one piece needs its layer, checked against `pieces
  root`. A v2-only torrent without layers is `MissingPieceLayer`. A
  hybrid without them still verifies with SHA-1. A layer whose root does
  not match is `InvalidPieceLayer`.
- A hybrid's v1 list (padding removed) and v2 tree must list the same
  paths and lengths in the same order, and every file must start a piece
  (`HybridMismatch`).
- The session keys torrents with a 20-byte `InfoHash`: the v1 hash for v1
  and hybrid, the truncated v2 hash for v2-only. `info_hash_v2` is the
  full SHA-256. `swarm_hashes` lists every id (two for a hybrid).
- A hybrid listener answers with the id the peer asked for. DHT search
  and announce use both ids. The tracker URL that answered the primary
  id is then asked for the second id; those peers are merged, and a
  failure there is only logged. A hybrid magnet registers the second id
  when metadata arrives.
- `Magnet::swarm_hash` is the v1 hash, or the truncated v2 hash for a
  `btmh`-only link. Metadata is checked with SHA-1, SHA-256, or both,
  matching the topics. `MagnetWithoutV1Hash` is now
  `MagnetWithoutInfoHash`.
- BEP 9 carries `info`, not `piece layers`. Files no larger than one
  piece need nothing more. Bigger files are fetched with `hash request`
  (at most 512 hashes, uncle hashes up to the pieces root, two
  outstanding per peer) after `info` is verified. A bad proof disconnects
  and bans. A `hash reject` or a timeout moves the request. A v2-only
  magnet is handed over only with every layer complete.
- A hybrid magnet fetches layers the same way before downloading, and
  only from peers that set the v2 bit. `piece_layer_timeout` (20 s,
  restarted by each accepted `hashes`) hands the torrent over with the
  layers that are complete. Files still missing a layer verify with
  SHA-1. The fetch continues in `TorrentState` while downloading or
  seeding, including for a hybrid `.torrent` that lacked layers. A
  completed layer is saved (`PieceLayerReceived`) and pieces of that file
  already accepted on SHA-1 are rechecked (`RecheckPieces`). A piece the
  Merkle tree refutes is forgotten, reported as `HashFailed`, and
  downloaded again. Nobody is banned for that recheck.
- A v2-only magnet cannot fall back to SHA-1. After
  `piece_layer_stall_alert` (2 min) without layer data it raises
  `Alert::PieceLayersStalled`, and again after each further silent period.
- File names must be UTF-8 (`MetainfoError::NonUtf8Name`). A v1 torrent
  may supply `name.utf-8` / `path.utf-8`. Comment, creator, and URLs stay
  lenient.
- A torrent with piece layers answers `hash request` for those layers.
  Leaf layers, malformed requests, and unknown roots get `hash reject`.
  During metadata exchange every request is rejected.
- The v2 reserved bit is set only for a torrent with a v2 info-hash, or
  for a magnet that carried `btmh` before its metadata arrives.
  libtorrent drops a plaintext handshake that sets the bit on a v1-only
  torrent.
- v2-only torrents accept a request that starts in the last piece and
  ends within its piece length. Storage reads the content and the engine
  appends zeros. Our own requests never ask for that padding.
- When both sides connect at once, the connection opened by the lower
  listen port is kept.
- Saved `.torrent` files include `piece layers` and `url-list` next to
  `info`.

## Web seeds (BEP 19)

- Each `url-list` or magnet `ws` URL is a virtual incoming peer: full
  bitfield, unchoked, never dialed, never sent over PEX. Block requests
  become HTTP range reads. The picker, window, endgame, rate limits, hash
  checks, and bans apply unchanged.
- Web seed N is `[100::N]:0` (RFC 6666 discard prefix). The engine does
  not pool, dial, retry, learn hints for, or share an address in
  `100::/64`. The session keeps its own back-off.
- `plan_range` maps a stream range onto file URLs and HTTP ranges, and
  zeros for padding. A single-file URL is used as given (plus the name
  when it ends in `/`). Multi-file paths are `<url>/<name>/<path…>`.
- Consecutive requests merge up to 1 MiB. `Cancel` drops queued blocks
  that have not been fetched.
- A body must be exactly the requested range (`206`), and `Content-Range`
  must name those bytes and, unless `*`, the file length. A `200` is
  accepted only for a range starting at offset 0, with `Content-Length`
  equal to the file length, and only the requested prefix is read.
  Anything else ends the virtual peer. Piece verification still applies.
- Redirects (at most five) stay on the original host and port and may
  upgrade `http` to `https`. Unless `allow_private_addresses` is set, only
  public addresses are used, including after DNS, and the connection uses
  the address that was checked. Environment proxies are not used.
- Web seeds run only while the torrent is downloading. A failure retries
  after `web_seed_retry` (30 s), doubling up to 32×. Received data resets
  the count. A seed banned for a bad hash, or a URL that can never be
  fetched, is not retried. A DNS failure is retried. `web_seed = None`
  never contacts them.

## io_uring

- `FileStorage` reads and writes through `Arc<dyn FileIo>`. `BlockingIo`
  is the default, so path safety, the descriptor cache, `sync`, and
  fastresume are shared.
- `UringIo` (feature `uring`, Linux) runs `tokio-uring` on a
  `dravity-uring` thread. `start` probes the ring and returns an error
  when the kernel refuses it. An opened file is adopted once on that
  thread. Writes take the block's `Bytes`; reads return a `Vec<u8>`.
  There is no `unsafe` and no per-operation copy. Registered buffers and
  `SQPOLL` are not used.
- `write_many` / `read_many` split a batch by file. Adjacent blocks become
  one `writev` / `readv` (at most 256 buffers). The session queues up to
  64 blocks, or flushes when no more input is ready. A failed batch is
  retried block by block.
- `SessionConfig::storage_backend` selects `Blocking` or `IoUring`. A
  missing feature or a ring that will not start logs a warning and uses
  blocking I/O. `Session::storage_backend` reports the backend in use.

## MSE/PE

- `dravity-mse` follows the frozen spec: 768-bit DH, generator 2, 160-bit
  private keys, SHA-1 for `req1`/`req2`/`req3` and `keyA`/`keyB`, RC4 with
  1024 keystream bytes dropped, PadA/PadB of 0–512 bytes, plaintext
  `0x01` and RC4 `0x02`. Public keys outside `1 < Y < P - 1` are refused.
  PadC, PadD, and IA are sent empty and accepted up to their spec limits.
- `Initiator` and `Responder` are byte-in/byte-out. `MseStream` then
  encrypts the connection. A plain BitTorrent handshake returned to an
  initiator is `PlaintextPeer`.
- `Enabled` (default): incoming may be plain or MSE; outgoing offers both
  and falls back to plain on the same transport if MSE fails after the
  peer accepts. `Required`: RC4 only. `Disabled`: plain only.
- The listener treats a leading `\x13BitTorrent protocol` as plain and
  anything else as an MSE public key. The responder matches
  `HASH('req2', SKEY)` against every running torrent, including both ids
  of a hybrid. The inner handshake must name the same torrent. TCP and
  uTP are handled alike.
- MSE hides the handshake from passive shaping. It is not authenticated
  security. `PeerStatus::encrypted` (TUI `E`) means RC4, not plaintext MSE.
- HTTP announces send `supportcrypto=1` under `Enabled` and also
  `requirecrypto=1` under `Required`. UDP announces have no such field.
- PEX `added.f`: `0x01` encryption, `0x02` seed, `0x04` uTP, `0x10` we
  dialed them. A short flag string leaves the rest unknown. Our own
  connections override PEX. Under `Required`, a peer flagged without
  `0x01` is not dialed. Under `Enabled`, known-plain peers get a plain
  handshake first. uTP is tried first when the peer is flagged `0x04`.
  Hints survive a magnet handover.

## Interop

`dravity-interop` is opt-in (`#[ignore]`, `DRAVITY_INTEROP=1`) against
`qbittorrent-nox` and `transmission-daemon`. The run command is in the
README. Transfers are compared byte for byte. `required` must be RC4,
`disabled` must be plain.

Fixes those runs required:

- Selective ACK masks of any length. libtorrent sends 3- or 6-byte masks.
- v2 last pieces are served as whole padded blocks.
- The v2 reserved bit is per torrent, not set on every handshake.
- Simultaneous connections keep the one the lower listen port opened.

Transmission 4.1 has no v2 (hybrids run as v1), checks BEP 47 padding as
real files, unchokes on a roughly 10 s pulse, and does not dial loopback
peers from a tracker.

## Left open

Leaf hash layers are not served. io_uring stays opt-in, without
registered buffers. A PEX peer flagged without `0x01` is skipped under
`Required`, because BEP 11 has no "unknown" bit. Empty files are still
created when their data arrives. BEP 17 `httpseeds` is not implemented.
