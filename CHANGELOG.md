# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
the roadmap phases until the first release.

## [Unreleased]

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
