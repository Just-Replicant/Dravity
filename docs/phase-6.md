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
| `dravity-wire` | no | BEP 52 `HashRequest` (21), `Hashes` (22), `HashReject` (23); `Reserved::v2` (reserved bit `0x10` of byte 7); `PexPeer::flags` is `Option<u8>` (unknown when `added.f` is short) with `PexPeer::has` |
| `dravity-core` | no | `merkle` (SHA-256 trees over 16 KiB leaves, piece layers, pad hashes; `layer_proof`, `verify_proof` and `layer_requests` for hash requests); v2 `file tree` parsing with the same path checks as v1; `MetaVersion` (`V1`, `V2`, `Hybrid`); `Metainfo::info_hash_v2`, `swarm_hashes`, `web_seeds`, `with_web_seeds`, `set_piece_layer`; `Info::version`, `check_piece`, `pieces_root`, `piece_layers`, `missing_piece_layers`; `Magnet::swarm_hash`; `InfoHashV2::from_info_bytes`; `Info::pieces_with_root`. Fixes: BEP 47 padding files may share a name; names that are not valid UTF-8 are refused (`MetainfoError::NonUtf8Name`) |
| `dravity-storage` | yes | `verify_piece` goes through `Info::check_piece` (SHA-1, Merkle, or both); `FileIo` / `OpenFile` under `FileStorage` (`BlockingIo` by default); `UringIo` (feature `uring`, Linux); `Storage::write_bytes` / `read_vec` move owned buffers; `Storage::write_many` / `read_many` and `OpenFile::write_batch` / `read_batch` (one io_uring round trip per batch); `benches/file_io.rs` |
| `dravity-tracker` | yes | `CryptoSupport` / `AnnounceRequest::crypto`: `supportcrypto` and `requirecrypto` on HTTP announces |
| `dravity-engine` | no | `MetadataState::new_v2` / `with_info_hash_v2`: magnet metadata is checked with SHA-256 for `btmh`, with both hashes for a hybrid link; v2 and hybrid magnets then fetch missing piece layers with hash requests (`Effect::MetadataReceived` carries them, `Config::piece_layer_timeout` bounds a hybrid's wait); `TorrentState::with_piece_layers` answers hash requests; PEX flags from what connections showed (`PeerLink` on `Event::PeerConnected` / `PeerAccepted`, `DialHints` on `Effect::Connect`, `TorrentState::with_dial_hints`, `Handover::hints`); web seeds at synthetic `100::/64` addresses (`web_seed_addr`, `is_web_seed_addr`); a hybrid keeps fetching missing layers while it downloads or seeds (`TorrentState::with_missing_piece_layers`, `Effect::PieceLayerReceived`, `Event::RecheckPieces`); `Alert::PieceLayersStalled` (`Config::piece_layer_stall_alert`); `TorrentState::with_padded_last_piece`; simultaneous connections resolved as libtorrent does |
| `dravity-mse` (new) | sans-IO core, async with feature `stream` | MSE/PE: `Initiator` / `Responder` state machines, `initiate` / `respond` and `MseStream` over Tokio I/O, `Policy` |
| `dravity-webseed` (new) | yes (feature `client`) | `plan` (sans-IO): a file's URL under a web seed and the HTTP ranges, plus zero-filled padding, that make up a byte range of the torrent. `WebSeedClient` (reqwest + rustls, on by default): strict ranged `GET`s (`Content-Range` checked), same-host redirects, public addresses only unless `WebSeedConfig::allow_private_addresses` (`is_public`, a filtering resolver) |
| `dravity-session` | async | v2/hybrid torrents and `btmh` magnets; a hybrid answers to, announces on (DHT and trackers) and looks up both swarm ids, also when added from a magnet; piece layers and web seeds are saved with the torrent; web seeds as virtual peers (`SessionConfig::web_seed`, `web_seed_retry`, `Transport::Web`); `SessionConfig::storage_backend` and `Session::storage_backend`; `SessionConfig::encryption`, `PeerStatus::encrypted`; dials follow PEX flags and earlier connections (MSE or plain first, uTP or TCP first, peers that cannot encrypt skipped under `Required`) |
| `dravity-utp` | yes | Fix: selective ACKs of any length are accepted (libtorrent sends 3- or 6-byte masks) |
| `dravity-interop` (new, tests only) | runs other clients | opt-in tests against `qbittorrent-nox` and `transmission-daemon` (see Interop) |
| `dravity-cli` | via session | `info` prints the version, the v2 hash and web seeds; `--no-web-seeds`; `--storage-backend blocking\|io-uring` (feature `uring`); `--encryption disabled\|enabled\|required`; `--web-seeds-allow-private`; the peer table flags web seeds with `W` and encrypted peers with `E`; stalled piece layers are printed and shown in the alert log |

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
  complete, so no data is ever downloaded unverifiable.
- **Hybrid magnets fetch their layers too, before downloading.** A
  hybrid `info` has SHA-1 piece hashes, so it could start at once and
  fetch the layers alongside. It does not: pieces accepted on SHA-1
  alone would have to be read back and re-checked against the Merkle
  roots once the layers arrived (and dropped if they failed), resume
  data would mix the two, and until then the torrent could not answer
  hash requests. The layers are small (32 bytes per piece, a few round
  trips), so the metadata state asks for them right after `info` is
  verified, exactly as for a v2-only magnet, and every piece is then
  checked with both SHA-1 and Merkle from the first block. Differences
  from v2-only: only peers that set the v2 reserved bit are asked (a
  v1-only peer of a hybrid swarm does not know the messages), and the
  wait is bounded: `Config::piece_layer_timeout` (20 s by default,
  restarted by every accepted `hashes` reply) without progress hands the
  torrent over with the layers that are complete, and files whose layer
  is still missing verify with SHA-1 only (`check_piece` already allows
  that for hybrids). The fetch then continues in the background (see
  the next point).
- **A hybrid keeps fetching late layers.** After the timeout, or for a
  hybrid `.torrent` that lacks some layers, `TorrentState` keeps asking
  every BEP 52 peer (also ones that connect later) for the missing
  layers while it downloads or seeds, with the same proof checks and
  bans (shared `layers` module). A completed layer is served at once,
  saved with the torrent (`Effect::PieceLayerReceived`), and every piece
  of its file already accepted on SHA-1 alone is read back and checked
  again (`Event::RecheckPieces`, `check_concurrency` at a time). A piece
  the Merkle tree refutes is forgotten, reported as `HashFailed` and
  downloaded again; a finished torrent goes back to downloading. Nobody
  is banned for it, since the sender is no longer known.
- **A stuck v2 magnet says so.** A v2-only magnet cannot fall back to
  SHA-1, so it keeps waiting for its layers, but after
  `Config::piece_layer_stall_alert` (2 min) without layer data it raises
  `Alert::PieceLayersStalled { missing_layers, silent }`, and again
  after each further period of silence (CLI line, TUI alert log, HTTP
  event `piece_layers_stalled`).
- **File names must be UTF-8.** `file tree` keys, v1 `name` and `path`
  components were decoded with `from_utf8_lossy`, so two different keys
  could collapse into one path, or a file be saved under a name that is
  not its key. Names that are not valid UTF-8 now fail with
  `MetainfoError::NonUtf8Name`; a v1 torrent may supply valid
  `name.utf-8` / `path.utf-8` instead. Free text (comment, created by,
  URLs) is still decoded leniently.
- **SHA-1 pass, Merkle fail.** `Info::check_piece` requires both hashes
  for a hybrid piece whose layer is known, so data that passes SHA-1 and
  fails Merkle (a SHA-1 collision, or a peer serving another v2 file) is
  a plain hash failure: the piece is discarded and re-downloaded, and the
  peers that sent it collect strikes and are banned
  (`Config::hash_fail_ban`), as for any bad piece. The fetched layers are
  stored with the torrent (see Persistence), so a restart verifies and
  serves with them.
- **Serving hash requests.** A torrent with piece layers answers
  `hash request`s from them (layers at or above the piece layer, with
  the uncles asked for). Requests for leaf layers below the piece layer
  (Dravity keeps only piece layers), malformed requests and unknown
  roots get a `hash reject`, which BEP 52 allows; serving leaf layers
  would mean keeping every file's 16 KiB leaf hashes, which no v2 client
  needs to download. During metadata exchange every request is rejected.
- **The v2 reserved bit is per torrent.** Dravity sets it, as libtorrent
  does, only for a torrent with a v2 info-hash (v2 or hybrid metadata, or
  before the metadata a magnet with a `btmh`), in its own handshakes and
  in the listener's replies (`listen::Route::v2`; a `btih` magnet turns
  it on once its metadata shows a hybrid). libtorrent 2.x drops a
  plaintext connection whose handshake sets the bit for a v1-only
  torrent (found by the interop tests).
- **v2 last pieces are padded for requests.** libtorrent treats a v2-only
  torrent's last file as padded to a whole piece and asks for whole 16
  KiB blocks of a short last piece. `TorrentState::with_padded_last_piece`
  (on for v2-only torrents) accepts a request that starts in the last
  piece's content and ends within its piece length; storage reads the
  content and the engine appends the zeros. Our own requests never ask
  for padding.
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
- **Synthetic, unroutable addresses.** The engine allows one connection
  per address, so a web seed is not reported at its server's address
  (two seeds on one CDN host and port would collide, a real peer there
  would be skipped, and a dropped seed would be redialed as a
  BitTorrent peer on port 80 or 443). Web seed number N of a torrent is
  `[100::N]:0`, in the IPv6 discard-only prefix (RFC 6666), which never
  routes and never belongs to a real peer. The engine never pools,
  dials, retries, learns hints for or shares over PEX an address in
  `100::/64`, whoever names it; the session keeps its own back-off for
  web seeds.
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
  (`206`), and its `Content-Range` must name the requested first and
  last byte and (unless `*`) the file's length, or the response is
  `WebSeedError::ContentRange` before a byte is delivered. A `200` is
  accepted only for a range starting at file offset 0, with the file's
  length as `Content-Length`, and only its first bytes are read. Anything else, or a timeout, ends
  the virtual peer. Data still goes through piece verification, so wrong
  content from a server is rejected and handled by the engine's usual
  hash-failure path, as for any peer.
- **Network safety.** A torrent's `url-list` is untrusted input that
  could aim the client at its own network. Redirects (at most five) stay
  on the original host and port, may upgrade `http` to `https` but never
  downgrade. Unless `WebSeedConfig::allow_private_addresses` (CLI
  `--web-seeds-allow-private`) is set, an IP literal must be public
  (`dravity_webseed::is_public`: no loopback, RFC 1918, CGNAT,
  link-local including `169.254.169.254`, ULA, unspecified, multicast,
  reserved or documentation ranges, nor IPv4-mapped forms of them), and
  host names go through a resolver that keeps only public addresses.
  The connection is made to the address that was checked, so DNS
  rebinding between the check and the connect is not possible.
  Environment proxies are not used (a proxy would resolve the name
  itself).
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
- **Long-lived files, owned buffers, no `unsafe`.** `FileIo::adopt` turns
  an opened `std::fs::File` into an `OpenFile` that the descriptor cache
  keeps. For io_uring, the ring thread converts it once
  (`tokio_uring::fs::File::from_std`, no `dup`) and keeps it in a map;
  callers hold a handle whose drop closes it on the ring (after any
  operation still in flight). Writes take the block's `Bytes` as
  received from the wire (`Storage::write_bytes`, `write_block`), reads
  fill a `Vec<u8>` that is returned to the caller (`Storage::read_vec`,
  used by `read_block`, piece checks and file streaming), both moved
  through the ring as tokio-uring's owned buffers, so there is no `dup`
  and no copy per operation. Reads, writes and `fdatasync`
  (`OpenFile::sync_data`) all go through the ring. The borrowed
  `write_at` / `read_at` remain (copy-free on the blocking backend). The
  workspace's `unsafe_code = "forbid"` holds; the backend has no
  `unsafe` code. Registered (fixed) buffers are not used: a block arrives
  as `Bytes` from the wire and would have to be copied into the
  registered pool, which is the copy this change removes.
- **Measured.** `cargo bench -p dravity-storage --features uring --bench
  file_io` writes, reads and verifies a 16 MiB torrent in 16 KiB blocks
  and 256 KiB pieces. On the development box (8 vCPUs, Linux 6.12, ext4,
  page cache warm), removing the `dup` and copy took io_uring block writes from
  6.78 to 5.24 ms (−15 %), block reads from 4.14 to 3.04 ms (−26 %) and
  piece verification from 3.07 to 2.65 ms (−15 %); blocking I/O did not
  change (0.57, 0.39, 2.41 ms). Per small operation, io_uring is still
  slower than `pread`/`pwrite` here: each one is a channel round trip to
  the ring thread, which batching across blocks amortizes (next
  points).
- **Batched submissions.** Each io_uring call was a round trip to the
  ring thread, paid once per 16 KiB block. `Storage::write_many` /
  `read_many` (defaults: one call per block) let `FileStorage` split a
  batch by file and hand each file its share at once
  (`OpenFile::write_batch` / `read_batch`). `UringIo` sends a batch as
  one job: blocks adjacent in the file become one `writev` / `readv` (at
  most 256 buffers, continued after a short transfer), all runs are
  submitted together and the caller wakes once, still without `unsafe`
  or copies. The session queues the blocks the engine asks to write or
  read and hands them over once no more input is ready (or 64 are
  queued), so an idle task still sends a single block at once; a batch
  that fails is retried block by block so only the failed blocks are
  reported.
- **Measured, batched.** Same bench and box, 16 MiB in 16 KiB blocks, one
  piece (16 blocks) per batched call, median of 20 samples:

  | Backend | Operation | One call per block | Batched | Change |
  |---|---|---|---|---|
  | io_uring | write | 4.88 ms | 0.87 ms | −82 % (5.6×) |
  | io_uring | read | 3.10 ms | 0.63 ms | −80 % (4.9×) |
  | blocking | write | 0.56 ms | 0.54 ms | about the same |
  | blocking | read | 0.37 ms | 0.38 ms | about the same |

  Piece verification is 2.87 ms on io_uring and 2.31 ms blocking (it
  is dominated by hashing). With a warm page cache on this box, batched
  io_uring is still about 1.6× slower than `pread` / `pwrite` on the
  blocking pool; io_uring's advantage is expected on cold storage at high
  queue depth, which this bench does not measure. The backend stays
  opt-in.
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
    ones offer RC4 and plaintext (Dravity's responder picks RC4;
    libtorrent's default picks plaintext when both are offered) and are redialed
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
  trackers have no such field.
- **PEX flags (BEP 11).** `added.f` now says what this client knows:
  `0x01` when the peer completed an MSE handshake with us (or PEX told us
  so and nothing contradicted it), `0x02` for seeds, `0x04` for uTP
  connections, `0x10` only for peers we dialed (an incoming peer's
  address is not known to accept connections). Received flags are kept
  per address in a bounded book (`2 × MAX_POOL` entries) as tri-state
  hints: what our own connections observed overrides PEX, PEX only fills
  gaps. A short `added.f` leaves the remaining peers unknown rather than
  "no features". Dialing follows the hints:
  - `Required`: a peer flagged without `0x01` (or that refused MSE
    before) is not dialed; flagged or unknown peers are. BEP 11 has no
    "unknown" bit, so a peer whose PEX entry has flags but not `0x01`
    counts as not encrypting; this can skip a peer whose sender simply
    never tried MSE, which is the price of not failing a handshake per
    PEX peer.
  - `Enabled`: MSE first, plain as the fallback, except for peers known
    not to encrypt, which get a plain handshake first with MSE as the
    fallback.
  - uTP first for peers flagged `0x04` (or last reached over uTP) when a
    uTP socket exists, TCP first for peers flagged without it, otherwise
    `prefer_utp`; the other transport stays the fallback.
  - What a dial learns is recorded: MSE completed (`Some(true)`), MSE
    failed and plain worked (`Some(false)`), or not tried (unknown). The
    hints survive a magnet's hand-over.

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
  forged uncle disconnects the peer); hybrid magnets asking only peers
  with the v2 bit, banning forged proofs, falling back to SHA-1 after
  `piece_layer_timeout`, while v2-only magnets keep waiting.
  `tests/seeding.rs` serves hash requests from piece layers.
  `tests/engine.rs`: PEX flags for each kind of connection, and dial
  hints learned from PEX and overridden by connections; `tests/metadata.rs`:
  hints and links surviving the hand-over; `hints.rs` unit tests (bounds,
  precedence).
- Wire: PEX flags unknown past a short `added.f`, encoded as zero before
  known ones. Fuzz `wire_pex` checks that only a suffix is unknown.
- Core: a hybrid piece that passes SHA-1 but not Merkle is rejected.
- Storage: `tests/uring.rs` (feature `uring`) writes, reads and verifies
  through io_uring from many threads and checks that an unusable ring is
  an error, not a panic, and flushes through the ring, closes files the
  descriptor cache evicts (counted in `/proc/self/fd`), and moves owned
  buffers across file boundaries; each test prints a note and passes
  when the kernel refuses io_uring. `benches/file_io.rs` compares both
  backends.
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
  magnet answering its v2 id once the metadata arrives; a hybrid `btih`
  magnet with multi-piece files fetching its piece layers, keeping them
  across a restart and serving them to a v2-only magnet; a peer whose
  data passes SHA-1 but not Merkle banned;
  piece layers surviving a restart; v2-only and hybrid torrents
  downloaded from a web seed alone; a broken web seed retried without
  stopping a download from a peer; web seeds turned off.
  `tests/storage_backend.rs`: io_uring used when available (blocking
  otherwise), torrents moving between io_uring and blocking sessions.
  `tests/encryption.rs`: two sessions under all nine policy pairs over
  TCP (who connects, encrypted or not, data intact) and MSE over uTP.
  `tests/pex.rs`: three sessions where the middle one shares the seed
  over PEX: `Required` dials it encrypted when flagged `0x01` and skips
  it when flagged plain; `Enabled` dials a plain peer in plaintext.
  `peer.rs` unit tests for the dial order.
- CLI: `info` on a v1 and a v2 torrent with a web seed; peer flags `W`
  and `E`; a download with `--storage-backend io-uring`; an unknown
  backend refused; two CLI processes (default `--encryption enabled`).
- Fuzz: `metainfo_parse` also checks pieces through `check_piece` and the
  v2 hashes, and that no accepted name was decoded lossily; new
  `webseed_plan` and `mse_handshake`.
- Late fixes (this round): core rejects non-UTF-8 `file tree` keys and v1
  names, and accepts valid `name.utf-8` / `path.utf-8`. Web seed
  `tests/client.rs`: wrong, missing and shifted `Content-Range`, `200`
  handling, same-host redirects (a redirect to `localhost` refused) and
  refused addresses (loopback, `169.254.169.254`, `::1`, RFC 1918,
  CGNAT, ULA, `0.0.0.0`, and a name resolving to loopback); `addr.rs`
  unit tests for `is_public`. Engine: web seeds are never dialed,
  retried or shared, and a real peer at a CDN's address still connects
  (`tests/engine.rs`); a hybrid keeps fetching its layers after the
  timeout and rechecks its pieces once one arrives, a stalled v2 magnet
  alerts and keeps asking (`tests/metadata.rs`); simultaneous
  connections keep the one the lower listen port opened, in both phases
  (`tests/peers.rs`); a padded v2 last piece is served in whole blocks.
  Session (`tests/v2.rs`): two web seeds on one host and port serve at
  once; a failing web seed is never dialed as a peer; pieces a late layer
  refutes are downloaded again; a v2 swarm is dialed with the v2 bit,
  a v1 swarm without it (`tests/swarm.rs`, also the listener's reply).
  Storage: batched writes and reads against single-block ones, across
  files, on both backends (`tests/common/batch.rs`). uTP: selective ACK
  masks of 3 and 6 bytes.

## Interop

`crates/dravity-interop` runs Dravity against `qbittorrent-nox`
(libtorrent 2.x, through its Web API) and `transmission-daemon` (through
its RPC). It is opt-in, so the default `cargo test` stays hermetic: every
test is `#[ignore]`d, also needs `DRAVITY_INTEROP=1`, and passes with a
note when its client is not on `PATH`.

```text
DRAVITY_INTEROP=1 cargo test -p dravity-interop -- --ignored --test-threads=1 --nocapture
```

Each case starts a fresh client in a temporary profile on loopback, with
DHT, PEX, local discovery, UPnP and GeoIP lookups off and a tracker of
the test's own, and moves a three-file torrent (about 3.7 MiB, rate
limited to 2.5 MB/s so the connection can be sampled mid-transfer) one
way. The received files are compared byte for byte, and Dravity's view
of the connection (TCP or uTP, RC4 or plain) is checked against the
case: `required` must be RC4, `disabled` must be plain, a single-transport
client must be reached over that transport. A transfer is bounded to 90
s, client start-up and checks to 20–30 s. `DRAVITY_INTEROP_ONLY=<text>`
runs the matching cases, `DRAVITY_INTEROP_KEEP=1` keeps the profiles and
logs, `RUST_LOG` turns on Dravity's logs.

Results on the development box (Linux 6.12, qBittorrent 5.1.0,
Transmission 4.1.0-beta.2), 44 transfers, all passing. "plain" under
`enabled` is expected: libtorrent picks the plaintext method when both
are offered, and either side may dial first.

| Client | Torrent | MSE (both sides) | Client transports | Client seeds → Dravity | Dravity seeds → client |
|---|---|---|---|---|---|
| qBittorrent 5.1.0 | v1 | disabled | TCP+uTP | ok, 1.9 s, uTP, plain | ok, 1.4 s, uTP, plain |
| qBittorrent 5.1.0 | v2 | disabled | TCP+uTP | ok, 2.1 s, TCP, plain | ok, 1.4 s, uTP, plain |
| qBittorrent 5.1.0 | v1 | enabled | TCP+uTP | ok, 2.0 s, TCP, RC4 | ok, 1.4 s, uTP, plain |
| qBittorrent 5.1.0 | hybrid | enabled | TCP+uTP | ok, 2.1 s, TCP, RC4 | ok, 1.3 s, uTP, plain |
| qBittorrent 5.1.0 | v2 | enabled | TCP+uTP | ok, 3.0 s, uTP, plain | ok, 1.4 s, uTP, RC4 |
| qBittorrent 5.1.0 | v1 | required | TCP+uTP | ok, 3.0 s, uTP, RC4 | ok, 1.4 s, uTP, RC4 |
| qBittorrent 5.1.0 | hybrid | required | TCP+uTP | ok, 2.1 s, TCP, RC4 | ok, 2.7 s, uTP, RC4 |
| qBittorrent 5.1.0 | v2 | required | TCP+uTP | ok, 2.1 s, TCP, RC4 | ok, 1.3 s, uTP, RC4 |
| qBittorrent 5.1.0 | v1 | enabled | TCP | ok, 2.9 s, TCP, plain | ok, 1.3 s, TCP, plain |
| qBittorrent 5.1.0 | v2 | enabled | TCP | ok, 2.1 s, TCP, RC4 | ok, 1.3 s, TCP, RC4 |
| qBittorrent 5.1.0 | v1 | enabled | uTP | ok, 2.0 s, uTP, RC4 | ok, 1.3 s, uTP, plain |
| qBittorrent 5.1.0 | hybrid | enabled | uTP | ok, 2.0 s, uTP, RC4 | ok, 1.4 s, uTP, RC4 |
| qBittorrent 5.1.0 | v2 | enabled | uTP | ok, 2.0 s, uTP, RC4 | ok, 1.4 s, uTP, plain |
| Transmission 4.1.0-beta.2 | v1 | disabled | TCP+uTP | ok, 10.7 s, TCP, plain | ok, 11.4 s, TCP, plain |
| Transmission 4.1.0-beta.2 | v1 | enabled | TCP+uTP | ok, 10.7 s, TCP, RC4 | ok, 11.4 s, TCP, RC4 |
| Transmission 4.1.0-beta.2 | hybrid | enabled | TCP+uTP | ok, 10.7 s, TCP, RC4 | ok, 11.4 s, TCP, RC4 |
| Transmission 4.1.0-beta.2 | v1 | required | TCP+uTP | ok, 10.7 s, TCP, RC4 | ok, 11.4 s, TCP, RC4 |
| Transmission 4.1.0-beta.2 | hybrid | required | TCP+uTP | ok, 10.8 s, TCP, RC4 | ok, 11.4 s, TCP, RC4 |
| Transmission 4.1.0-beta.2 | v1 | enabled | TCP | ok, 10.7 s, TCP, RC4 | ok, 11.4 s, TCP, RC4 |
| Transmission 4.1.0-beta.2 | hybrid | enabled | TCP | ok, 10.8 s, TCP, RC4 | ok, 11.4 s, TCP, RC4 |
| Transmission 4.1.0-beta.2 | v1 | enabled | uTP | ok, 10.7 s, uTP, RC4 | ok, 1.4 s, uTP, RC4 |
| Transmission 4.1.0-beta.2 | hybrid | enabled | uTP | ok, 10.7 s, uTP, RC4 | ok, 1.4 s, uTP, RC4 |

What the tests found, each fixed with a regression test of its own:

| Bug | Symptom against the client | Fix |
|---|---|---|
| Selective ACK masks had to be a multiple of 4 bytes | libtorrent sends 3- or 6-byte masks; those packets were dropped with their data, so an upload to qBittorrent over uTP stalled until the 60 s timeout | accept any length (`f8125b1`) |
| A v2 torrent's short last piece was sized by its content | libtorrent asks for the whole 16 KiB block of the padded last piece; Dravity rejected it and the v2 download from Dravity never finished | serve whole blocks of the padded last piece (`6ff5e87`) |
| The v2 reserved bit was set for every torrent | libtorrent closes a plaintext connection that sets it for a v1-only torrent, so Dravity could not dial a v1 qBittorrent seed with encryption disabled | set the bit per torrent (`ddce83f`) |
| Simultaneous connections kept whichever joined first | over uTP both connections share one address; libtorrent keeps the one the lower listen port opened, so each side could close a different one and leave none (about one uTP run in ten stalled for 60 s) | apply libtorrent's rule (`c2fb652`) |

Client limitations, not Dravity bugs:

- **Transmission has no v2.** Transmission 4.1 refuses v2-only torrents
  ("unrecognized info") and runs hybrids as v1, so it has no v2 cases;
  its hybrid cases exercise Dravity's hybrid support from the v1 side.
- **Transmission reads pad files from disk.** Seeding a hybrid, it
  checks BEP 47 padding files as real files and never completes its
  check without them, so the harness writes them (zeros) for it.
- **Transmission's 10 s pulse.** Over TCP, and as a seed over uTP,
  Transmission sends `interested` or `unchoke` only at its next
  rechoke pulse, about 10 s after the handshake; a packet capture shows
  Dravity answering within a millisecond. Those cases take about 11 s.
- **Transmission does not dial loopback peers from a tracker** and has
  no RPC to add a peer, so Dravity dials it in the "Dravity seeds" cases.

Not covered: real swarms and trackers on the Internet, DHT and PEX
between clients (both off for determinism), and web seeds against real
HTTP servers or CDNs (a loopback mock server covers the protocol cases).

## Deferred

| Item | Why |
|---|---|
| io_uring registered (fixed) buffers and `SQPOLL` | blocks arrive as `Bytes` from the wire; registered buffers would need the copy that was just removed, and batching already removed the per-block round trip |
| Serving leaf layers (below the piece layer) in hash requests | no v2 client asks for them; it would mean keeping every file's 16 KiB leaf hashes |
| A PEX "unknown encryption" state under `Required` | BEP 11 has no such bit; a peer flagged without `0x01` is skipped (see PEX flags) |
| Interop with DHT and PEX on, on real swarms and trackers, and web seeds against real CDNs | the loopback harness keeps runs deterministic and offline; these need a network and are left to manual testing |
| v2 interop with Transmission | Transmission 4.1 does not support v2-only torrents |

Out of scope for Phase 6 and unchanged from earlier phases: creating
empty files up front (storage creates a file when its data arrives) and
BEP 17 `httpseeds` (BEP 19 covers web seeds in practice).
