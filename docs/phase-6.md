# Phase 6 — v2/hybrid torrents and web seeds: notes and decisions

Scope from the roadmap (plan §8): "BEP 52 v2/hybrid + Merkle; web seeds
(BEP 19); optional io_uring backend; protocol encryption if required."
This phase covers the two protocol items. Dravity now downloads and seeds
v2-only and hybrid torrents, fetches their metadata from `btmh` magnet
links, joins both swarms of a hybrid torrent, and downloads from HTTP(S)
web seeds. The engine stays sans-IO; the session is still the only I/O
driver (plan §4).

**Not in this phase:** the optional io_uring backend and MSE/PE protocol
encryption (see [Deferred](#deferred) for the reasons), and BEP 52's
`hash request` / `hashes` / `hash reject` messages.

## Where Phase 5 left off

`dravity-core` knew the `InfoHashV2` type and parsed `btmh` magnet
topics, but `Metainfo::from_bytes` refused a torrent without v1 `pieces`
(`UnsupportedV2Only`), ignored a hybrid's `file tree`, and the session
refused a magnet without a `btih` topic (`MagnetWithoutV1Hash`).
`url-list` and the magnet `ws` parameter were dropped.

## Crates

| Crate | I/O? | Phase 6 additions |
|---|---|---|
| `dravity-core` | no | `merkle` (SHA-256 trees over 16 KiB leaves, piece layers, pad hashes); v2 `file tree` parsing with the same path checks as v1; `MetaVersion` (`V1`, `V2`, `Hybrid`); `Metainfo::info_hash_v2`, `swarm_hashes`, `web_seeds`, `with_web_seeds`, `set_piece_layer`; `Info::version`, `check_piece`, `pieces_root`, `piece_layers`, `missing_piece_layers`; `Magnet::swarm_hash`; `InfoHashV2::from_info_bytes`. Fix: BEP 47 padding files may share a name |
| `dravity-storage` | yes | `verify_piece` goes through `Info::check_piece` (SHA-1, Merkle, or both) |
| `dravity-engine` | no | `MetadataState::new_v2` / `with_info_hash_v2`: magnet metadata is checked with SHA-256 for `btmh`, with both hashes for a hybrid link |
| `dravity-webseed` (new) | yes (feature `client`) | `plan` (sans-IO): a file's URL under a web seed and the HTTP ranges, plus zero-filled padding, that make up a byte range of the torrent. `WebSeedClient` (reqwest + rustls, on by default): strict ranged `GET`s |
| `dravity-session` | async | v2/hybrid torrents and `btmh` magnets; a hybrid answers to, announces on and looks up both swarm ids; piece layers and web seeds are saved with the torrent; web seeds as virtual peers (`SessionConfig::web_seed`, `web_seed_retry`, `Transport::Web`) |
| `dravity-cli` | via session | `info` prints the version, the v2 hash and web seeds; `--no-web-seeds`; the peer table flags web seeds with `W` |

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
  The DHT is searched and announced for both. Trackers get the primary
  id only (see Deferred).
- **Magnets.** `Magnet::swarm_hash` picks the v1 hash, or the truncated
  v2 hash for a `btmh`-only link. The metadata state checks the received
  `info` bytes with SHA-1, SHA-256 or both, according to the topics the
  link carried. `MagnetWithoutV1Hash` is now `MagnetWithoutInfoHash`.
- **v2 magnets and piece layers.** BEP 9 carries only `info`, not
  `piece layers`. Files no larger than one piece need nothing more (their
  pieces root is the piece hash). For bigger files a v2-only magnet needs
  the BEP 52 hash request messages, which Dravity does not speak yet: the
  torrent stops with an error naming the missing piece layers instead of
  downloading data it cannot verify. Hybrid magnets fall back to SHA-1.
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
- Engine: `tests/metadata.rs` checks v2 magnets with SHA-256 and hybrid
  magnets against both hashes.
- Web seed: `tests/plan.rs` (URLs, ranges split at files, padding as
  zeros, proptest that a plan covers the range exactly), `tests/client.rs`
  against a loopback HTTP server (ranges, servers without `Range`
  support, short/long bodies and error statuses).
- Session, `tests/v2.rs` over loopback: v2-only and hybrid torrents moving
  between two sessions; a hybrid answering handshakes for both ids; a v2
  magnet with small files fetching metadata and downloading (and keeping
  its `ws`); a v2 magnet without piece layers stopping with an error;
  piece layers surviving a restart; v2-only and hybrid torrents
  downloaded from a web seed alone; a broken web seed retried without
  stopping a download from a peer; web seeds turned off.
- CLI: `info` on a v1 and a v2 torrent with a web seed; peer flags `W`.
- Fuzz: `metainfo_parse` also checks pieces through `check_piece` and the
  v2 hashes; new `webseed_plan`.

Not covered: interop against other clients' v2/hybrid torrents and real
HTTP servers (plan §9, Docker interop) — checked by construction
against BEP 52's reference layout only.

## Deferred

| Item | When |
|---|---|
| BEP 52 `hash request` / `hashes` / `hash reject`, and the v2 reserved bit | next: lets v2-only magnets with files larger than one piece download, and serves layers to v2 peers |
| Announce a hybrid's second id to trackers | with tracker requests per swarm id |
| Hybrid added from a magnet: listener alias for its v2 id before a restart | registered on the next start (from the saved torrent) |
| Optional io_uring backend | optional in the plan; `unsafe_code` is forbidden and the container/kernel support is uncertain — needs its own crate and decision |
| MSE/PE protocol encryption | "if required": peers accept plaintext by default; adds an encrypted peer flag then |
| Create empty files (v1 and v2) | pre-existing: storage creates files when data arrives |
| BEP 17 (`httpseeds`, Hoffman-style) | not planned; BEP 19 covers the common case |
