# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
the roadmap phases until the first release.

## [Unreleased]

### irontide adoption

Ideas adopted from a review of the irontide engine, reimplemented for
Dravity's sans-IO design. Status per item: [`docs/irontide-adoption.md`](docs/irontide-adoption.md).

- **Security (item 1):** tracker HTTP announces are SSRF-guarded
  (`dravity_tracker::guard`, `ClientConfig::ssrf_mitigation`, default on):
  a `localhost`/loopback tracker must use an `/announce` path; redirects
  stay on `http(s)`, never downgrade `https`, are capped at 5, and a
  redirect from a non-local tracker may not reach a non-public address,
  literal or resolved (a resolver drops non-public addresses for every
  host that is not itself an announce origin). New
  `TrackerError::Blocked`. The address classifier moved to
  `dravity_core::net::is_public` (`dravity_webseed::is_public` re-exports it).
- **Smart ban (item 2):** a failed piece with blocks from more than one
  address is quarantined while its blocks are hashed
  (`Effect::HashBlocks` → `Event::BlockHashes`, SHA-256 per 16 KiB block
  via `dravity_storage::block_hashes`); once a copy verifies, only the
  senders of the blocks that differ are banned. On v2/hybrid torrents
  the piece's leaf hashes are requested from a BEP 52 peer that did not
  send it (verified against the piece layer) and convict at once.
  `Config::smart_ban` (default on); strikes remain the fallback when the
  blocks cannot be read or the tracking table (256 pieces) is full. Bans
  are session-wide: a torrent's ban is shared with every other torrent
  and the listener (`Session::ban_ip`, `unban_ip`, `banned_ips`,
  `Event::BanIp`, `TorrentState::with_banned_ips`).
- **Session connection cap (item 3):** `SessionConfig::max_connections`
  (default 500, `--max-connections`, 0 for none) caps open peer
  connections across all torrents, TCP and uTP, dialed and accepted. One
  lock-free counter (`dravity_core::admit::{ConnectionLimit,
  ConnectionSlot, admit_permitted}`) hands out RAII slots: the listener
  closes connections over the cap before the handshake, and a dial over
  the cap is reported as the new `Event::PeerConnectDeferred` (no blame;
  dialed again after `DEFERRED_DIAL_RETRY`, 5 s). Changeable at runtime
  (`Session::set_max_connections`, `max_connections`, `connection_count`).
  A loom model (`dravity-core/tests/loom_admit.rs`, CI job `loom`) checks
  the counter under every interleaving.
- **Session-wide hash pool (item 4):** `SessionConfig::hash_workers` now
  bounds piece hashing across the whole session instead of per torrent.
  Checks, rechecks and smart-ban digests of every torrent share one FIFO
  pool, so N torrents rechecking at once no longer run N × cores
  blocking jobs. A panicking job reports a storage error and frees its
  slot. `Session::hash_jobs_running` shows the pool's load.
- **Sans-IO purity tripwires (item 5):** `tests/sans_io_purity.rs` in
  `dravity-utp` (`conn`, `packet`), `dravity-mse` (`handshake`,
  `crypto`), `dravity-webseed` (`plan`, `addr`), `dravity-tracker`
  (`http`, `udp`, `guard`, `types`) and `dravity-portmap` (`natpmp`), and
  for every file of `dravity-core`, `-engine`, `-wire` and `-bencode`,
  fail the build when a sans-IO module mentions tokio, `.await`, sockets,
  `std::fs`, threads or the clock (comments and unit tests excepted).
- **Hash on arrival (item 6):** downloaded pieces are verified from their
  blocks as they are written: `dravity_core::verify::PieceHasher` feeds
  SHA-1 in order (holding early blocks) and hashes each block's SHA-256
  leaf at once, then checks with the same rules as `Info::check_piece`
  (now built on a shared digest check). The session feeds it on the write
  threads and answers `Effect::HashPiece` without reading the piece back
  (`SessionConfig::hash_on_arrival`, default on). Rechecks, duplicate
  blocks and pieces whose early blocks exceed 32 MiB per torrent still
  read back. Hybrids no longer hash a piece twice from disk.
- **Read cache (item 7):** `dravity_storage::{CachedStorage, ReadCache,
  CacheStats}`, a `Storage` middleware with a segmented-LRU cache of
  16 KiB chunks under one byte budget shared by all torrents (scan
  resistant: a chunk is protected on its second hit). Writes go through
  and invalidate; a read that raced a write is not cached. The session
  wraps the storages it creates (`SessionConfig::read_cache`, default
  64 MiB, `--read-cache`, `Session::read_cache_stats`).
- **Verify-before-download prescan (item 8):** before the start check,
  one `stat` per file (`Storage::stamps`) finds pieces that touch a
  missing or short file (`dravity_storage::absent_pieces`); the engine
  skips hashing them (`TorrentState::with_absent_pieces` /
  `set_absent_pieces`). A new or partial download is checked in O(files)
  plus the pieces that may be present.
- **BDP request depth and torrent budget (item 9):** each peer's request
  depth is the larger of its AIMD window and its bandwidth-delay product
  (EWMA delivery rate × minimum recent request round trip, +25 % and two
  blocks), still capped by `max_requests_per_peer` and `reqq`; a request
  timeout hands control back to AIMD for 10 s. New
  `Config::request_budget` (1024 blocks) bounds outstanding requests
  across all peers of a torrent. Rate and RTT are measured from the
  engine's own events, so it stays sans-IO.
- **Piece reservation (item 10):** `Picker::pick_for(owner, ..)` reserves
  each started piece for the peer that started it. Peers continue their
  own pieces, take over unowned ones (owner left or choked us), start new
  ones, and only then help with another peer's piece, so fewer pieces
  sit half done and a bad piece usually has a single sender (which helps
  smart banning). `Picker::release_owner`, `Picker::owner`.
- **Preallocation and durability (item 11):** `PreallocateMode`
  (`none`/`sparse`/`full`) sizes or reserves each file when
  `FileStorage` creates it (`FileStorage::with_preallocate`; `full` uses
  `fallocate` and falls back to sparse). `Durability`
  (`never`/`per-piece`/`on-complete`) controls when data is synced:
  `per-piece` syncs a verified piece's files before the engine counts it.
  New `Storage::sync_range` (default: `flush`). `SessionConfig::preallocate`,
  `SessionConfig::durability`, and the CLI flags `--preallocate` and `--durability`.
- **Seed choking modes and slot tuner (item 12):** `Config::seed_choking`
  (`SeedChoking::FastestUpload`, the default, `RoundRobin` with
  `Config::round_robin_quota`, or `AntiLeech`) ranks regular slots while
  seeding. `Config::upload_slots_auto` re-derives the slot count every
  round from measured upload rates. `TorrentState::upload_slots`. The
  CLI flags are `--seed-choking` and `--auto-upload-slots`.
- **Torrent creation (item 13):** `dravity_core::create::TorrentCreator`
  is a sans-IO creator for v1, v2 and hybrid torrents. It handles BEP 47
  padding (always for hybrids, optional for v1), automatic piece length
  (`auto_piece_length`), trackers in tiers, web seeds, `private`,
  `source`, comments and dates. `dravity_storage::create_torrent` walks a
  file or directory. The new `dravity create` command writes the
  `.torrent` and prints its info hashes.
- **Tracker scrape (item 14):** `TrackerClient::scrape` covers HTTP (BEP 48:
  `announce` becomes `scrape`, `http::scrape_url` and
  `http::parse_scrape_response`) and UDP (BEP 15 action 2:
  `udp::scrape_request` and `udp::parse_scrape_response`, reusing the
  cached connection id). The new type is `ScrapeStats`. In the session,
  `TorrentHandle::scrape()` and `SessionConfig::scrape_interval` (off by
  default) fill `TrackerStatus::scrape` / `scraped`. The TUI tracker pane
  shows scraped counts, and the CLI flag is `--scrape-interval MINUTES`.
- **Local Service Discovery (item 15):** the new `dravity-lsd` crate has a
  sans-IO BEP 14 codec (`codec::encode` / `codec::parse`) and an `Lsd`
  service on `239.192.152.143:6771` and `[ff15::efc0:988f]:6771`, with
  a per-source rate limit and a cookie that drops our own announces.
  `SessionConfig::lsd` (off by default in the library) announces running
  public torrents and adds discovered peers. Private torrents and
  magnets without metadata are never announced. The CLI enables LSD when
  it listens; `--no-lsd` turns it off.

### Phase 6 — v2/hybrid torrents, web seeds, io_uring and encryption

See [`docs/phase-6.md`](docs/phase-6.md) for design notes.

#### Added

- BEP 52: `dravity_core::merkle` (SHA-256 trees over 16 KiB leaves, piece
  layers, padded subtrees); v2-only and hybrid `.torrent` parsing
  (`file tree`, `piece layers`) with `MetaVersion`, `Metainfo::info_hash_v2`,
  `swarm_hashes`, `set_piece_layer`, `Info::version`, `check_piece`,
  `pieces_root`, `piece_layers`, `missing_piece_layers`, and
  `InfoHashV2::from_info_bytes`. New `MetainfoError` variants
  `MissingFileTree`, `InvalidFileTree`, `HybridMismatch`,
  `MissingPieceLayer`, `InvalidPieceLayer`.
- `Magnet::swarm_hash`; `MetadataState::new_v2` and `with_info_hash_v2`
  check magnet metadata with SHA-256 (both hashes for hybrid links).
- The session downloads and seeds v2 and hybrid torrents and `btmh`
  magnet links. A hybrid torrent accepts peers, and looks up and
  announces on the DHT, under both swarm ids. Saved torrents keep their
  piece layers and web seeds.
- Web seeds (BEP 19): `Metainfo::web_seeds` / `with_web_seeds` (`url-list`,
  magnet `ws`), the new `dravity-webseed` crate (sans-IO range planning,
  HTTP(S) client), and web seeds as virtual peers in the session
  (`SessionConfig::web_seed`, `web_seed_retry`, `Transport::Web`,
  re-exported `WebSeedConfig`, `SessionError::WebSeed`).
- CLI: `info` prints the metainfo version, v2 hash and web seeds;
  `--no-web-seeds`; the TUI peer flags mark web seeds with `W`.
- BEP 52 hash requests: `Message::HashRequest`, `Hashes` and `HashReject`
  (`dravity_wire::HashRequest`), `Reserved::v2` / `with_v2`;
  `merkle::layer_proof`, `verify_proof`, `layer_requests`, `HashProof`,
  `LayerRequest`, `MAX_HASH_REQUEST`; `Info::piece_layer`. v2-only
  magnets with files larger than one piece fetch and verify their piece
  layers from peers (`Effect::MetadataReceived` now carries them);
  torrents answer hash requests from their piece layers
  (`TorrentState::with_piece_layers`). Dravity sets the v2 reserved bit.
- Hybrid torrents announce their second id to the tracker that answered
  and merge its peers; a hybrid added from a magnet accepts peers on its
  second id as soon as the metadata arrives.
- Optional io_uring file I/O: `dravity_storage::FileIo`, `BlockingIo`,
  `FileStorage::with_io`, and with the `uring` feature (Linux) `UringIo`
  (`tokio-uring` on a dedicated thread). `SessionConfig::storage_backend`
  (`StorageBackend::{Blocking, IoUring}`) with a logged fallback to
  blocking I/O, `Session::storage_backend`; CLI `--storage-backend` and
  `uring` features on `dravity-session` and `dravity-cli`.
  `FileIo::sync_data`, so io_uring also flushes through the ring.
- Message Stream Encryption: the new `dravity-mse` crate (sans-IO
  `Initiator` / `Responder`, async `initiate` / `respond`, `MseStream`,
  `Policy`); `SessionConfig::encryption` (`EncryptionPolicy`, default
  `Enabled`) for TCP and uTP, incoming and outgoing;
  `PeerStatus::encrypted`; CLI `--encryption` and the `E` peer flag.
- `dravity_tracker::CryptoSupport` and `AnnounceRequest::crypto`: HTTP
  announces send `supportcrypto=1` (and `requirecrypto=1` under
  `EncryptionPolicy::Required`).
- Fuzz targets `webseed_plan` and `mse_handshake`; `metainfo_parse` checks
  v2 hashes and `check_piece`.
- Hybrid magnets fetch their piece layers with BEP 52 hash requests (from
  peers with the v2 reserved bit) before downloading, so every piece is
  checked with SHA-1 and Merkle and the layers are saved and served;
  `Config::piece_layer_timeout` bounds the wait, then files without a
  layer verify with SHA-1 only.
- PEX flags (BEP 11) describe each peer: `0x01` once it completed MSE,
  `0x02` seed, `0x04` uTP, `0x10` only for peers we dialed. Received flags
  and our own connections steer dialing (`dravity_engine::DialHints`,
  `PeerLink`, `TorrentState::with_dial_hints`, `Handover::hints`):
  `Required` skips peers known not to encrypt, `Enabled` tries plain first
  for them, and uTP-flagged peers are dialed over uTP first.
- `Storage::write_bytes` and `read_vec` (owned buffers),
  `dravity_storage::OpenFile`, and a `file_io` Criterion bench comparing
  the storage backends.
- A hybrid that started on SHA-1 alone keeps fetching its missing piece
  layers from BEP 52 peers while it downloads or seeds
  (`TorrentState::with_missing_piece_layers`, `Effect::PieceLayerReceived`);
  a completed layer is served, saved, and the file's pieces are checked
  again (`Event::RecheckPieces`); refuted pieces are downloaded again.
  `Info::pieces_with_root`.
- `Alert::PieceLayersStalled` (`Config::piece_layer_stall_alert`, 2 min):
  a v2-only magnet whose piece layers nobody sends says so, repeatedly,
  in the CLI, TUI and HTTP event stream (`piece_layers_stalled`).
- Web seed network safety: `WebSeedConfig::allow_private_addresses` (CLI
  `--web-seeds-allow-private`, off by default), `dravity_webseed::is_public`,
  `WebSeedError::ForbiddenAddress`, `ContentRange` and `FileLength`.
- Batched block I/O: `Storage::write_many` / `read_many`,
  `OpenFile::write_batch` / `read_batch`, `write_blocks` / `read_blocks`;
  io_uring merges adjacent blocks into one `writev` / `readv` and submits
  a batch in one ring round trip (about 5× faster block writes and reads
  than one call per block in the bench). The session hands storage the
  blocks queued since its last turn as one batch.
- `dravity_engine::web_seed_addr` / `is_web_seed_addr`.
- `TorrentState::with_padded_last_piece`.
- The `dravity-interop` crate: opt-in tests (`DRAVITY_INTEROP=1`,
  `#[ignore]`d) against `qbittorrent-nox` and `transmission-daemon` for
  v1, hybrid and v2 torrents both ways, MSE disabled, enabled and
  required, over TCP and uTP. Results are in `docs/phase-6.md`.

#### Changed

- **Breaking:** `SessionError::MagnetWithoutV1Hash` is now
  `MagnetWithoutInfoHash` (raised only when a magnet has neither a `btih`
  nor a `btmh` topic).
- **Breaking:** `MetainfoError::UnsupportedV2Only` is removed: v2-only
  torrents load.
- Storage verifies pieces through `Info::check_piece` (SHA-1, Merkle or
  both) and no longer depends on `sha1` directly.
- A v2-only torrent's session key (`info_hash()`) is its v2 hash
  truncated to 20 bytes, as in the handshake, trackers and the DHT.
- Outgoing peer connections try MSE first by default and fall back to a
  plain handshake; set `SessionConfig::encryption` to
  `EncryptionPolicy::Disabled` (CLI `--encryption disabled`) for the old
  behaviour.
- **Breaking:** `AnnounceRequest` has a new `crypto` field.
- **Breaking:** a v2-only magnet with files larger than one piece no longer
  stops with a missing piece layers error; it fetches the layers.
- The io_uring backend keeps each file open on the ring (no `dup` per
  operation) and moves owned buffers through it (no copy per block):
  about 15 % faster writes and piece checks and 25 % faster reads in the
  bench. **Breaking:** `FileIo` is now `name` + `adopt`, returning an
  `OpenFile`; `write_block` takes `Bytes`.
- **Breaking:** `PexPeer::flags` is `Option<u8>`; `Event::PeerConnected`
  and `PeerAccepted` have a `link` field, `Effect::Connect` a `hints`
  field, `HandoverPeer` a `link` field and `Handover` a `hints` field.
- Incoming peers are no longer flagged reachable (`0x10`) in PEX.
- **Breaking:** web seeds are reported to the engine at synthetic
  addresses in `100::/64` (RFC 6666) instead of their server's address;
  the engine never dials, retries or shares an address in that prefix.
- **Breaking:** `Segment::Fetch` carries the file's length; web seed
  redirects stay on the original host (at most five) and non-public
  addresses are refused by default.
- **Breaking:** `MetainfoError::NonUtf8Name`: names and path components
  must be valid UTF-8 (`name.utf-8` / `path.utf-8` are used when valid).
- The v2 reserved bit is set only for torrents with a v2 info-hash (and
  `btmh` magnets), in handshakes and listener replies, as libtorrent does.

#### Fixed

- BEP 47 padding files may share a name (e.g. several `.pad/16384`), as
  real hybrid torrents have; they were rejected as duplicate paths.
- Two web seeds on the same host and port both run; a web seed no longer
  hides a real peer at its server's address, and a dropped web seed is
  no longer dialed as a BitTorrent peer on port 80 or 443.
- A web seed `206` whose `Content-Range` does not match the request is
  rejected before its bytes are delivered.
- A web seed banned for bad data is not started again, and a URL that
  can never be fetched (forbidden address, bad scheme, or a name that
  resolves only to non-public addresses) never opens a connection.
- `file tree` keys that differ only in invalid UTF-8 no longer collapse
  into one path.
- uTP: selective ACK masks of any length are accepted; libtorrent's 3- or
  6-byte masks were dropped with their data, stalling uploads to
  qBittorrent until the connection timed out.
- v2: whole blocks of a padded last piece are served; libtorrent asked for
  them forever and a v2 download from Dravity never finished.
- libtorrent dropped plaintext connections to v1-only torrents because
  Dravity always set the v2 reserved bit.
- Simultaneous connections between two peers (both dialing over uTP) are
  resolved as libtorrent does (the lower listen port's outgoing
  connection stays), instead of each side possibly closing a different
  one and leaving none.

### Phase 5 — Full terminal UI

See [`docs/phase-5.md`](docs/phase-5.md) for design notes.

#### Added

- `dravity_engine::PeerInfo`, `TorrentState::peers`, `MetadataState::peers`:
  a snapshot of each connected peer (address, direction, pieces, payload
  totals, choke and interest flags).
- `TorrentHandle::peers` (`PeerStatus`, `Transport`) and
  `TorrentHandle::trackers` (`TrackerStatus`, `TrackerOutcome`): the last
  announce outcome per tracker URL, in tier order.
- `--tui` is a full client: add a magnet or a `.torrent` file while it
  runs (`a`), remove a torrent or remove it and delete its files after a
  confirm prompt (`d` / `D`), detail panes for files, peers and trackers
  (`1` `2` `3`, add a peer with `a`), a per-torrent alert log, ratio and
  ETA columns, a header with listen port, DHT, mapped port and totals,
  sort (`o`), name filter (`/`), a key list (`?`), mouse row selection.

#### Changed

- The TUI refreshes every torrent at once under one 500 ms deadline; a
  torrent that does not answer keeps its previous numbers instead of
  stalling the frame.
- `download --tui` keeps running once a second torrent was added from the
  screen, until every torrent is finished (or `q`).
- A magnet still fetching metadata shows no size or progress in the TUI
  (it showed 100 %).

#### Fixed

- Torrents added from a `.torrent` file now serve their metadata over
  BEP 9, so magnet-link peers can start from a Dravity seed.

### Toolchain and dependencies

#### Changed

- **MSRV raised from Rust 1.85 to 1.99** (latest stable; edition 2024 is
  unchanged as the newest edition). CI's MSRV job runs 1.99 and uses
  `actions/checkout@v7`.
- Every dependency upgraded to its latest release, including majors:
  ratatui 0.30, crossterm 0.29, sha1 0.11, criterion 0.8; fuzz: bytes 1.12,
  serde 1.0.229.
- Code adopts what the new MSRV allows: let-chains instead of nested
  `if`/`if let`, `slice::as_chunks` for fixed-size records (compact peers,
  hex, piece hashes, allowed fast set), `is_multiple_of`. No behavior
  change.

#### Removed

- The `deny.toml` advisory exception for `paste` (ratatui 0.30 no longer
  depends on it) and the criterion MSRV pin.

### Phase 4 — Product surface

See [`docs/phase-4.md`](docs/phase-4.md) for design notes.

#### Added

- File priorities: `dravity_engine::Priority`, per-piece priorities and
  urgent pieces in the picker (`Event::SetPriorities`, `Event::SetUrgent`,
  `Stats::wanted_left`); `AddTorrentOptions::with_file_priorities`,
  `Session::set_file_priorities`, `Session::set_file_priority`.
- `TorrentHandle::progress` (`TorrentProgress`, `FileProgress`).
- Streaming: `TorrentHandle::open_file` returns a seekable `FileStream`
  that waits for pieces and fetches a readahead window first.
- `Session::list`, `pause`, `resume`, `remove` (optionally deleting the
  files), `metainfo`; `AddTorrentOptions::paused`.
- Session persistence: `SessionConfig::state_dir`, `Session::restore`
  (`RestoreReport`), `dravity_session::persist`.
- Watch folder: `Session::watch` (`WatchConfig`, `WatchHandle`).
- `dravity-http`: JSON API over the session (torrents, files, priorities,
  peers, pause/resume/delete), file streaming with `Range`, alerts as
  server-sent events, Prometheus `/metrics`, optional bearer token.
- CLI: `dravity serve` (restore, seed, `--watch`, `--http`, `--token` /
  `DRAVITY_TOKEN`), `download --files` and `--file-priority`, `--tui`;
  `info` numbers the files.
- BEP 6 `suggest` messages steer piece selection.
- Announces use the router's external port when NAT-PMP mapped a
  different one (`dravity_portmap::Status::external_port`).
- Fuzz targets `session_state`, `http_range`.

#### Changed

- `TorrentStatus::Finished` / `Alert::Finished` mean every *wanted* piece
  is verified. Trackers get `completed` once, only when every piece is in;
  a partial download that does not seed announces `stopped`.
- `TorrentHandle::stop` also removes the torrent from the session (and its
  saved state); `Session::shutdown` keeps the saved state.
- CLI networking options are shared by `download` and `serve`.
- `/metrics` queries every torrent at once and stops after 2 seconds. A
  torrent that does not answer is still listed, without live counters.
- `HEAD` on a file's content returns the same status and headers as `GET`
  and an empty body.

### Phase 3 — Good citizen

See [`docs/phase-3.md`](docs/phase-3.md) for design notes.

#### Added

- Seeding: the engine serves blocks (`Effect::ReadBlock`,
  `Event::BlockRead`), keeps finished torrents in the swarm
  (`Config::seed`) and paces uploads (`upload_rate_limit`,
  `--upload-limit`).
- Choker: tit-for-tat by rate, anti-snubbing and a rotating optimistic
  unchoke (`upload_slots`, `--upload-slots`).
- BEP 6 fast extension: `have all`/`have none`, `reject`, allowed fast set
  (`dravity_wire::allowed_fast_set`).
- Per-IP bans for peers whose pieces fail the hash check
  (`hash_fail_ban`, `Alert::PeerBanned`).
- Incoming connections: `SessionConfig::listen` (`ListenConfig`), TCP and
  uTP on IPv4 and IPv6, routed by info-hash; `Session::listen_port`. CLI
  `--port` is now the listen port; `--no-listen`, `--seed`.
- `dravity-utp`: uTP (BEP 29) with LEDBAT. The session dials over uTP when
  TCP fails (`SessionConfig::prefer_utp`, `--prefer-utp`, `--no-utp`).
- `dravity-portmap`: NAT-PMP and UPnP IGD port mapping, on by default
  (`SessionConfig::port_mapping`, `Session::port_mapping`,
  `--no-port-mapping`); mappings are removed on shutdown.
- DHT `announce_peer` with the listen port after each lookup.
- `dravity_storage` `read_block`.
- Fuzz targets `utp_packet`, `portmap_parse`.

#### Changed

- Paths from metainfo are refused when they are invalid on the running
  platform (Windows device names, `<>:"|?*`, trailing dots or spaces) or
  have components over 255 bytes (`dravity_core::path::Platform`).
- On Unix, content files are opened beneath the download directory with
  `O_NOFOLLOW` at every level (`StorageError::UnsafePath`).
- The handshake advertises the fast extension; the extension handshake
  sends `p`, `reqq` and `yourip`.
- `SessionConfig::announce_port` is only used when the session does not
  listen.

### Phase 2 — Magnets and swarm

See [`docs/phase-2.md`](docs/phase-2.md) for design notes.

#### Added

- Magnet links: `Session::add_magnet` and `dravity download 'magnet:…'`.
  Metadata is fetched from peers (BEP 9/10), verified against the
  info-hash and handed over to the download without reconnecting
  (`dravity_engine::MetadataState`).
- Metadata serving: torrents answer `ut_metadata` requests.
- `dravity-dht`: mainline DHT (BEP 5) lookups and announces, wrapping
  `mainline`; the session runs a DHT node by default (`SessionConfig::dht`,
  `--no-dht`).
- Peer exchange (BEP 11), disabled for private torrents (as is the DHT).
- Endgame mode and block stealing from slow peers, with `cancel` for
  duplicates.
- AIMD per-peer request window capped by the peer's `reqq`.
- Download rate limit (`Config::download_rate_limit`, `--download-limit`).
- Fastresume: `dravity_storage::resume` (verified pieces + file stamps,
  atomic save); `SessionConfig::resume_dir`, CLI `--resume-dir` /
  `--no-resume` (default `<output>/.dravity`).
- UDP tracker connection-id cache (BEP 15).
- BEP 12 tier ordering: tiers shuffled and announced concurrently, trackers
  within a tier tried in order, the answering one promoted.
- Codecs: `dravity_wire::{MetadataMessage, PexMessage}`,
  `dravity_core::compact`, `Metainfo::from_info_bytes`,
  `dravity_bencode::Value::decode_prefix`.
- Fuzz targets `wire_metadata`, `wire_pex`, `storage_resume`.

#### Changed

- Default `max_requests_per_peer` is now 128 (the AIMD window's ceiling);
  new peers start at `initial_requests_per_peer` = 4.
- The handshake advertises the BEP 10 extension protocol.
- `Event::PeerConnected` carries the peer's reserved bytes.
- Peers may connect while the initial data check runs; they get a bitfield
  and interest once it finishes.

## Phase 1 — MVP downloader

`.torrent` downloads from HTTP/UDP trackers and TCP peers; see
[`docs/phase-1.md`](docs/phase-1.md).

## Phase 0 — Foundations

Bencode, metainfo, magnet and wire codecs with fuzz targets; see
[`docs/phase-0.md`](docs/phase-0.md).
