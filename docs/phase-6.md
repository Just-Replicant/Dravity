# Phase 6 — v2/hybrid torrents, web seeds, io_uring and encryption: notes and decisions

Scope from the roadmap (plan §8): "BEP 52 v2/hybrid + Merkle; web seeds
(BEP 19); optional io_uring backend; protocol encryption if required."
This phase covers all four. Dravity now downloads and seeds v2-only and
hybrid torrents, fetches their metadata and piece layers from `btmh`
magnet links (BEP 52 hash requests), joins both swarms of a hybrid
torrent (DHT, trackers and listener), downloads from HTTP(S) web seeds,
can do its file I/O through io_uring, and encrypts peer connections with
MSE/PE. The engine stays sans-IO; the session is still the only I/O
driver (plan §4).

## Where Phase 5 left off

`dravity-core` knew the `InfoHashV2` type and parsed `btmh` magnet
topics, but `Metainfo::from_bytes` refused a torrent without v1 `pieces`
(`UnsupportedV2Only`), ignored a hybrid's `file tree`, and the session
refused a magnet without a `btih` topic (`MagnetWithoutV1Hash`).
`url-list` and the magnet `ws` parameter were dropped.

## Crates

| Crate | I/O? | Phase 6 additions |
|---|---|---|
| `dravity-wire` | no | BEP 52 `HashRequest` (21), `Hashes` (22), `HashReject` (23); `Reserved::v2` (reserved bit `0x10` of byte 7) |
| `dravity-core` | no | `merkle` (SHA-256 trees over 16 KiB leaves, piece layers, pad hashes; `layer_proof`, `verify_proof` and `layer_requests` for hash requests); v2 `file tree` parsing with the same path checks as v1; `MetaVersion` (`V1`, `V2`, `Hybrid`); `Metainfo::info_hash_v2`, `swarm_hashes`, `web_seeds`, `with_web_seeds`, `set_piece_layer`; `Info::version`, `check_piece`, `pieces_root`, `piece_layers`, `missing_piece_layers`; `Magnet::swarm_hash`; `InfoHashV2::from_info_bytes`. Fix: BEP 47 padding files may share a name |
| `dravity-storage` | yes | `verify_piece` goes through `Info::check_piece` (SHA-1, Merkle, or both); `FileIo` under `FileStorage` (`BlockingIo` by default); `UringIo` (feature `uring`, Linux) |
| `dravity-tracker` | yes | `CryptoSupport` / `AnnounceRequest::crypto`: `supportcrypto` and `requirecrypto` on HTTP announces |
| `dravity-engine` | no | `MetadataState::new_v2` / `with_info_hash_v2`: magnet metadata is checked with SHA-256 for `btmh`, with both hashes for a hybrid link; v2 magnets then fetch missing piece layers with hash requests (`Effect::MetadataReceived` carries them); `TorrentState::with_piece_layers` answers hash requests |
| `dravity-mse` (new) | sans-IO core, async with feature `stream` | MSE/PE: `Initiator` / `Responder` state machines, `initiate` / `respond` and `MseStream` over Tokio I/O, `Policy` |
| `dravity-webseed` (new) | yes (feature `client`) | `plan` (sans-IO): a file's URL under a web seed and the HTTP ranges, plus zero-filled padding, that make up a byte range of the torrent. `WebSeedClient` (reqwest + rustls, on by default): strict ranged `GET`s |
| `dravity-session` | async | v2/hybrid torrents and `btmh` magnets; a hybrid answers to, announces on (DHT and trackers) and looks up both swarm ids, also when added from a magnet; piece layers and web seeds are saved with the torrent; web seeds as virtual peers (`SessionConfig::web_seed`, `web_seed_retry`, `Transport::Web`); `SessionConfig::storage_backend` and `Session::storage_backend`; `SessionConfig::encryption`, `PeerStatus::encrypted` |
| `dravity-cli` | via session | `info` prints the version, the v2 hash and web seeds; `--no-web-seeds`; `--storage-backend blocking\|io-uring` (feature `uring`); `--encryption disabled\|enabled\|required`; the peer table flags web seeds with `W` and encrypted peers with `E` |

`dravity-http` is unchanged: its DTOs carry the session's info-hash,
which for a v2-only torrent is the truncated v2 hash (see below).

## Decisions

### BEP 52

- **One content stream for every version.** A v2 file starts on a piece
  boundary. A hybrid torrent says so with BEP 47 padding files in its v1
  list. For a v2-only torrent the parser synthesizes the same `.pad/<n>`
  entries (marked padding, never written). Picking, storage, file
  priorities, streaming and fastresume therefore work unchanged on piece
  indices over one stream. Only verification differs.
- **Verification by version.** `Info::check_piece(index, data)` hashes
  with SHA-1 (v1), with SHA-256 against the file's piece layer or pieces
  root (v2), or both (hybrid: a piece passes only if both agree). A piece
  that covers only the end of a file is padded with zero leaves up to the
  piece's subtree, as BEP 52 specifies. Storage verifies through this
  call, so the hashing pool and the "hash failed → download again / ban"
  path are the same for every version.
- **Piece layers are mandatory for v2-only, optional for hybrid.** A file
  longer than one piece needs its layer from the torrent's `piece layers`
  (checked against the file's `pieces root` on load). A v2-only torrent
  without them cannot be verified and is refused (`MissingPieceLayer`).
  A hybrid without them still verifies with SHA-1. A layer whose root
  does not match is `InvalidPieceLayer`.
- **Hybrid halves must agree.** The v1 file list (padding removed) and
  the v2 tree must list the same paths and lengths in the same order,
  and every file must start a piece (`HybridMismatch`).
  Otherwise the two swarms could be serving different data under one
  torrent.
- **Which id is "the" info-hash.** The session keys torrents with an
  `InfoHash` (20 bytes). For v1 and hybrid torrents that is the v1 hash.
  For v2-only it is the v2 hash truncated to 20 bytes, which is what v2
  peers put in the handshake and what trackers and the DHT use. The full
  SHA-256 is `Metainfo::info_hash_v2`. `Metainfo::swarm_hashes` lists
  every id the torrent is known by (two for a hybrid).
- **Hybrid swarms.** The listener registers both ids for a hybrid
  torrent, and answers each handshake with the id the peer asked for.
  The DHT is searched and announced for both. Each tracker announce for
  the primary id is followed, on the tracker URL that answered, by one
  for the second id; its peers are merged, and its failure is only
  logged (the primary announce decides the tier order and the alert). A
  hybrid added from a magnet registers its second id with the listener
  as soon as the metadata arrives, without a restart.
- **Magnets.** `Magnet::swarm_hash` picks the v1 hash, or the truncated
  v2 hash for a `btmh`-only link. The metadata state checks the received
  `info` bytes with SHA-1, SHA-256 or both, according to the topics the
  link carried. `MagnetWithoutV1Hash` is now `MagnetWithoutInfoHash`.
- **v2 magnets and piece layers.** BEP 9 carries only `info`, not
  `piece layers`. Files no larger than one piece need nothing more (their
  pieces root is the piece hash). For bigger files the metadata state,
  once `info` is verified, asks peers for each missing layer with BEP 52
  `hash request`s (at most 512 hashes each, with the uncle hashes up to
  the pieces root, two outstanding per peer, any peer that set the v2
  reserved bit or not). Each `hashes` reply is checked against the
  file's `pieces root` before it is accepted; a bad proof disconnects and
  bans the peer, a `hash reject` or a timeout moves the request to
  another peer. The torrent is handed over only with every layer
  complete, so no data is ever downloaded unverifiable. Hybrid magnets
  still verify with SHA-1 and do not fetch layers.
- **Serving hash requests.** A torrent with piece layers answers
  `hash request`s from them (layers at or above the piece layer, with
  the uncles asked for). Requests for leaf layers below the piece layer
  (Dravity keeps only piece layers), malformed requests and unknown
  roots get a `hash reject`, which BEP 52 allows; serving leaf layers
  would mean keeping every file's 16 KiB leaf hashes, which no v2 client
  needs to download. During metadata exchange every request is rejected.
  Dravity sets the v2 reserved bit.
- **Persistence.** The session writes `piece layers` and `url-list` next
  to `info` in its saved `.torrent`, so a v2 torrent restores without a
  peer, and keeps its web seeds.

### Web seeds (BEP 19)

- **A web seed is a peer.** Rather than a second download path through
  the engine, each web seed URL runs as a virtual *incoming* peer: it
  reports a full bitfield and unchokes, and turns each block `Request`
  into an HTTP range read. The engine's picker, request window, endgame,
  rate limits, hash checks and bans then apply unchanged, and nothing in
  the sans-IO engine knows about HTTP. Being "incoming", it is never
  dialed back and never shared over PEX.
- **Planning is sans-IO.** `dravity_webseed::plan_range` maps a stream
  range onto files: a URL and an HTTP range per file touched, zeros for
  padding (never requested). As BEP 19 specifies, a single-file torrent
  uses the URL as given (plus the name when it ends in `/`), and a
  multi-file torrent's files live under `<url>/<name>/<path…>`
  (percent-encoded). It is fuzzed (`webseed_plan`).
- **Batched requests.** Consecutive block requests are merged up to
  1 MiB, so a web seed costs one HTTP request per batch, not per 16 KiB
  block. `Cancel` drops queued blocks that have not been fetched yet.
- **Untrusted responses.** A body must be exactly the requested range
  (`206`). A `200` is accepted only for a range starting at file offset
  0, and only its first bytes are read. Anything else, or a timeout, ends
  the virtual peer. Data still goes through piece verification, so wrong
  content from a server is rejected and handled by the engine's usual
  hash-failure path, as for any peer.
- **Only while downloading.** Web seeds start when the torrent is
  downloading and close when it finishes, pauses or checks; a seed
  never needs them. A failed seed is retried after
  `SessionConfig::web_seed_retry` (30 s), doubling with each failure in a
  row up to 32×; data received resets the count.
  `SessionConfig::web_seed = None` (CLI `--no-web-seeds`) never contacts
  them.
- **Sources.** The torrent's `url-list` (a string or a list) and a
  magnet's `ws` parameters, which are kept once the metadata arrives.

### io_uring storage backend

- **Behind a trait, off by default.** `FileStorage` does its positional
  reads and writes through `Arc<dyn FileIo>`. `BlockingIo` (`pread` /
  `pwrite` on the hashing and storage thread pool) is the default, so path
  safety, the descriptor cache, `sync` and fastresume are shared by both
  backends and the default build does not change.
- **tokio-uring on its own thread.** `UringIo` (crate feature `uring`,
  Linux only) starts a `tokio-uring` runtime on a dedicated `dravity-uring`
  thread; callers on any thread send jobs over a channel and wait for the
  reply. `UringIo::start` probes the ring with a no-op and returns an
  error instead of panicking when the kernel, seccomp or a container
  refuses io_uring.
- **No `unsafe`.** Each job owns a `dup` of the cached descriptor and an
  owned buffer (tokio-uring's ownership model), at the cost of one `dup`
  and one copy per operation. Reads, writes and `fdatasync`
  (`FileIo::sync_data`) all go through the ring. The workspace's
  `unsafe_code = "forbid"` holds: tokio-uring's safe, owned-buffer API is
  enough, so the backend has no `unsafe` code at all.
- **Selection and fallback.** `SessionConfig::storage_backend`
  (`Blocking`, `IoUring`; CLI `--storage-backend`) applies to storage the
  session creates. Asking for io_uring in a build without the feature, or
  where the ring cannot start, logs a warning and uses blocking I/O;
  `Session::storage_backend` reports the backend in use. The feature
  chains `dravity-cli/uring` → `dravity-session/uring` →
  `dravity-storage/uring`.

### Message Stream Encryption (MSE/PE)

- **The specification, with maintained crypto.** `dravity-mse` follows the
  frozen MSE spec: Diffie-Hellman over its 768-bit prime with generator 2
  and 160-bit private keys from `getrandom` (`num-bigint`), SHA-1
  (`sha1`) for `req1`/`req2`/`req3` and `keyA`/`keyB`, RC4 (`rc4`) with
  the first 1024 bytes of keystream dropped, random PadA/PadB of 0–512
  bytes, `crypto_provide` / `crypto_select` (plaintext `0x01`, RC4
  `0x02`). Public keys outside `1 < Y < P - 1` are refused. PadC and PadD
  are sent empty (as the spec suggests) but parsed up to 512 bytes; IA is
  sent empty and accepted up to 65535 bytes.
- **Sans-IO first.** `Initiator` and `Responder` are byte-in/byte-out state
  machines (fuzzed by `mse_handshake`). `initiate` / `respond` drive them
  over any Tokio stream, writing and reading at once so neither side
  stalls on a full send buffer; `MseStream` then encrypts and decrypts
  transparently. An initiator that gets a plain BitTorrent handshake back
  fails at once (`PlaintextPeer`).
- **Policies** (`SessionConfig::encryption`, CLI `--encryption`), the
  spec's three client modes:
  - `Enabled` (default): incoming connections may be plain or MSE; outgoing
    ones offer RC4 and plaintext (the responder picks RC4) and are redialed
    with a plain handshake on the same transport if the peer accepts the
    connection but fails MSE. A dial that cannot connect at all is not
    redialed.
  - `Required`: RC4-encrypted MSE only, both ways; plain incoming
    handshakes are dropped.
  - `Disabled`: plain handshakes only; an MSE key exchange arriving is
    simply not a BitTorrent handshake.
- **Incoming detection and SKEY.** The listener reads the first bytes: a
  plain handshake starts with `\x13BitTorrent protocol`, anything else is
  an MSE public key. The responder finds the torrent by comparing the
  de-obfuscated `HASH('req2', SKEY)` with every running torrent's id
  (both ids of a hybrid), and the BitTorrent handshake inside must name
  the same torrent. TCP and uTP are handled alike.
- **What it is not.** MSE is obfuscation against passive traffic shaping:
  RC4 and 768-bit DH are weak by current standards and the peer is not
  authenticated. `PeerStatus::encrypted` (TUI flag `E`) marks
  RC4-encrypted connections; MSE with the plaintext method only hides the
  handshake.
- **Trackers.** HTTP announces carry `supportcrypto=1` under `Enabled`
  and also `requirecrypto=1` under `Required`
  (`dravity_tracker::CryptoSupport`), as the MSE spec describes; UDP
  trackers have no such field. PEX encryption flags are not sent.

## Tests

- Core: `merkle` known values, padding and invalid lengths, a proptest
  against a naive tree. `tests/v2.rs`: v2-only single and multi-file
  torrents (padding layout, every piece checks), hybrids checking both
  hashes, hybrid mismatch cases, required and verified piece layers (a
  flipped byte fails `check_piece`), a v2 magnet's `info` needing its
  layers, hostile file trees (traversal, missing or bad roots, bad
  lengths), `url-list`; a proptest that generated v2 and hybrid torrents
  verify every piece and cover the whole stream. `tests/metainfo.rs`: padding
  files sharing a name, `file tree` without `meta version` ignored.
- Storage: the existing tests, plus v2/hybrid verification through the
  session tests below (storage only delegates to `check_piece`).
- Wire: hash request / hashes / hash reject round trips and their size
  rules; the v2 reserved bit.
- Core: `layer_proof` / `verify_proof` for every base layer, index and
  proof depth, including a proptest; `layer_requests` covering a layer.
- Tracker: `supportcrypto` / `requirecrypto` in announce URLs.
- Engine: `tests/metadata.rs` checks v2 magnets with SHA-256 and hybrid
  magnets against both hashes; a v2 magnet fetching its piece layers with
  hash requests (bad proofs ban, rejects move on), small files needing
  none, requests rejected during metadata exchange, and a 600-piece
  layer fetched as two 512-hash blocks whose uncle hash is checked (a
  forged uncle disconnects the peer).
  `tests/seeding.rs` serves hash requests from piece layers.
- Storage: `tests/uring.rs` (feature `uring`) writes, reads and verifies
  through io_uring from many threads and checks that an unusable ring is
  an error, not a panic, and flushes through the ring; each test prints
  a note and passes when the kernel refuses io_uring.
- MSE: RFC 6229 RC4 output, the 1024-byte drop, the prime (768 bits,
  Fermat-probable, safe), DH agreement and weak keys; a reference
  implementation of both sides written from the spec text (with PadC,
  PadD and IA, which Dravity never sends) talking to Dravity's responder
  and initiator; method negotiation, unknown torrents, lost sync, plain
  replies; async streams over 64-byte pipes in both directions.
- Web seed: `tests/plan.rs` (URLs, ranges split at files, padding as
  zeros, proptest that a plan covers the range exactly), `tests/client.rs`
  against a loopback HTTP server (ranges, servers without `Range`
  support, short/long bodies and error statuses).
- Session, `tests/v2.rs` over loopback: v2-only and hybrid torrents moving
  between two sessions; a hybrid answering handshakes for both ids; a v2
  magnet with small files fetching metadata and downloading (and keeping
  its `ws`); a v2 magnet with multi-piece files fetching its piece layers
  and downloading; a v2 magnet whose 520-piece layer takes two hash
  requests with uncle proofs; a hybrid announcing both ids to a tracker; a hybrid
  magnet answering its v2 id once the metadata arrives;
  piece layers surviving a restart; v2-only and hybrid torrents
  downloaded from a web seed alone; a broken web seed retried without
  stopping a download from a peer; web seeds turned off.
  `tests/storage_backend.rs`: io_uring used when available (blocking
  otherwise), torrents moving between io_uring and blocking sessions.
  `tests/encryption.rs`: two sessions under all nine policy pairs over
  TCP (who connects, encrypted or not, data intact) and MSE over uTP.
- CLI: `info` on a v1 and a v2 torrent with a web seed; peer flags `W`
  and `E`; a download with `--storage-backend io-uring`; an unknown
  backend refused; two CLI processes (default `--encryption enabled`).
- Fuzz: `metainfo_parse` also checks pieces through `check_piece` and the
  v2 hashes; new `webseed_plan` and `mse_handshake`.

Not covered: interop against other clients' v2/hybrid torrents and real
HTTP servers (plan §9, Docker interop) — checked by construction
against BEP 52's reference layout only.

## Deferred

| Item | Why |
|---|---|
| Piece layers for hybrid magnets | they verify with SHA-1; fetching the layers too would only let them answer v2-only peers' hash requests |
| io_uring without the per-operation `dup` and copy | needs registered buffers and files; one 16 KiB copy is cheap next to the hashing |
| PEX encryption flags (`added.f` bit `0x01`) | peers learned over PEX are dialed with MSE first anyway under `Enabled` |

Out of scope for Phase 6 and unchanged from earlier phases: creating
empty files up front (storage creates a file when its data arrives) and
BEP 17 `httpseeds` (BEP 19 covers web seeds in practice).
