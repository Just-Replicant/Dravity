# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
the roadmap phases until the first release.

## [Unreleased]

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
