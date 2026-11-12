# Changelog

Notable changes. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow the roadmap until the first release.

## [Unreleased]

### TUI

- Queued torrents draw as `queued`. A user pause still wins.
- `Q` sets queue caps, `C` the connection cap, `P` the listen port
  (`0` picks a free port). `b` / `B` / `U` ban and unban. `i` loads an
  IP filter. `s` on trackers scrapes now. `t` on a file opens it and,
  under `serve --tui`, shows the HTTP Range URL.
- Add is a form: start paused, extra peers, a download directory, and
  file selection.
- The header shows limits, queue caps, peer id, connections, hash jobs,
  read-cache stats, and port mapping. The selected torrent shows piece
  and peer counts. Trackers show announce and scrape times.
  `Alert::PieceCompleted` and `Alert::UploadFailed` appear in the log.

### Audit remediation

- The token bucket returns the full wait. Admissions are FIFO. A live
  rate change reprices debt without charging the bytes again.
- PCP renewals keep the nonce and assigned address and use half the
  granted lifetime.
- A live rebind of a taken port fails and leaves the old listener.
- Private tracker origins need `--allow-private-trackers`. Trust is
  request-scoped. Proxy DNS is refused unless allowed.
- Ban and IP-filter updates use a generation watch, so a full command
  queue cannot drop them.
- Piece length is capped at 32 MiB (`--max-piece-length`) before state
  is allocated.
- A non-loopback HTTP bind without a token is refused unless
  `--allow-insecure-http` is set.
- `apply_settings` and `set_listen_port` are serialized.
- Native DHT token secrets come from the OS CSPRNG. The default node
  stays `mainline`. See [`docs/native-dht-validation.md`](docs/native-dht-validation.md).
- Queue status queries run in parallel. Concurrent read-cache misses of
  one span share one disk read.
- GitHub Actions are pinned to commit SHAs.
- `SharedUdp` can demultiplex DHT, uTP, and tracker datagrams. The
  session does not use it yet.
- Crates stay `publish = false`. The `dravity` facade is the public API.

### irontide adoption

Reimplemented for the sans-IO design. Per-item notes:
[`docs/irontide-adoption.md`](docs/irontide-adoption.md).

- HTTP tracker SSRF guard, on by default.
- Session-wide smart ban, with strikes as the fallback.
- One connection cap (default 500) for every torrent.
- One hash pool for the session.
- Sans-IO modules fail CI if they touch tokio, sockets, files, or the clock.
- Pieces hash as their blocks are written.
- Shared read cache (default 64 MiB). No shared write pool.
- Files shorter than the torrent skip hashing the missing tail.
- Request depth follows the bandwidth-delay product, plus a torrent budget.
- A started piece stays with the peer that started it.
- Sparse or full preallocation, and sync per piece, on finish, or never.
- Seed choking: fastest, round-robin, or anti-leech, with an optional slot tuner.
- `dravity create` writes v1, v2, and hybrid torrents.
- HTTP and UDP tracker scrape.
- Local service discovery (BEP 14). Off in the library. On in the CLI.
- Live session rate limits, queue, and listen port, saved with the session.
- Magnet `so=` selects files.
- `upload_only`, `lt_donthave`, super-seeding, and tracker exchange.
- BEP 40 decides who is dropped at the peer cap.
- IP filter (eMule, P2P, CIDR).
- PCP first, including IPv6 pinholes, then NAT-PMP, then UPnP.
- Native DHT (BEP 5/32/33/42/43/44/51), opt-in with `--native-dht`.
- SOCKS4/5 and HTTP CONNECT for peers, trackers, web seeds, and native DHT.
  I2P SAM exists and is not used for peers.
- GeoIP country, DSCP on peer TCP, web-seed back-off saved across restarts.
- BEP 55 holepunch.
- BEP 52 vectors from an independent reference.
- Loom models for the rate bucket and the connection cap.
- Pluggable `Network`, with an in-memory swarm.
- Settings registry. It generates [`docs/configuration.md`](docs/configuration.md).
- `dravity` facade: `ClientBuilder` and one `Error`.
- BEP table with a drift test.

### Phase 6 — v2, web seeds, io_uring, encryption

Design: [`docs/phase-6.md`](docs/phase-6.md).

#### Added

- v2 and hybrid torrents, `btmh` magnets, piece layers, and hash requests.
- A hybrid joins both swarms (listener, DHT, trackers).
- HTTP(S) web seeds as virtual peers, with back-off and `--no-web-seeds`.
- Optional io_uring storage (`--storage-backend io-uring`), with blocking fallback.
- MSE/PE (`--encryption disabled|enabled|required`). HTTP announces send
  `supportcrypto` / `requirecrypto`.
- PEX flags steer encryption and TCP versus uTP.
- Opt-in interop tests against qBittorrent and Transmission.

#### Changed

- **Breaking:** `MagnetWithoutV1Hash` is `MagnetWithoutInfoHash`.
- **Breaking:** `UnsupportedV2Only` is removed. v2-only torrents load.
- **Breaking:** `AnnounceRequest` has a `crypto` field.
- **Breaking:** `FileIo` is `name` + `adopt`. `write_block` takes `Bytes`.
- **Breaking:** `PexPeer::flags` is `Option<u8>`. Connect and handover
  events carry link and dial hints.
- **Breaking:** web seeds use synthetic `100::/64` addresses.
- **Breaking:** web-seed redirects stay on the original host. Non-public
  addresses are refused unless allowed.
- **Breaking:** names that are not UTF-8 are `NonUtf8Name`.
- A v2-only session id is the v2 hash truncated to 20 bytes.
- Outgoing peers try MSE first. `Disabled` restores plain handshakes.
- The v2 reserved bit is set only for v2 and hybrid torrents.
- A v2 magnet fetches piece layers instead of failing when they are missing.
- Hybrid magnets wait up to `piece_layer_timeout`, then verify missing
  files with SHA-1 and keep fetching layers.
- A stalled v2 magnet raises `PieceLayersStalled`.

#### Fixed

- BEP 47 padding files may share a name.
- Two web seeds on one host both run, and a web seed is never dialed as a peer.
- A `206` whose `Content-Range` does not match is rejected.
- A banned or permanently unusable web seed is not retried.
- uTP selective ACK masks of any length are accepted.
- v2 padded last pieces are served as whole blocks.
- Simultaneous connections keep the one the lower listen port opened.

### Phase 5 — Full terminal UI

Design: [`docs/phase-5.md`](docs/phase-5.md).

#### Added

- Peer and tracker snapshots on `TorrentHandle`.
- `--tui` adds, removes, and shows files, peers, trackers, and alerts.
  Sort, filter, help, and mouse row selection.

#### Changed

- One 500 ms refresh for every torrent. Unanswered rows keep their last numbers.
- `download --tui` stays up once a second torrent exists.
- A magnet with no metadata shows no size.

#### Fixed

- Torrents added from a file serve their metadata over BEP 9.

### Toolchain

- **MSRV is Rust 1.99** (edition 2024). It was 1.85.
- Dependencies moved to current releases, including ratatui 0.30,
  crossterm 0.29, sha1 0.11, and criterion 0.8.
- The `paste` advisory exception and the criterion MSRV pin are gone.

### Phase 4 — Product surface

Design: [`docs/phase-4.md`](docs/phase-4.md).

#### Added

- File priorities, streaming `FileStream`, pause, resume, and remove.
- Session persistence, restore, and a watch folder.
- `dravity-http`: JSON API, ranged streaming, server-sent alerts,
  Prometheus `/metrics`, bearer token.
- `dravity serve`, `--files`, `--file-priority`, `--tui`.
- BEP 6 `suggest` affects picking. Announces use a mapped port when it differs.

#### Changed

- `Finished` means every wanted piece. Trackers get `completed` only when
  the torrent is complete.
- `TorrentHandle::stop` removes the torrent. `shutdown` keeps saved state.
- `/metrics` stops after 2 seconds. `HEAD` matches `GET` with an empty body.

### Phase 3 — Good citizen

Design: [`docs/phase-3.md`](docs/phase-3.md).

#### Added

- Seeding, upload rate limit, tit-for-tat, anti-snubbing, optimistic unchoke.
- BEP 6, per-IP hash-failure bans, incoming TCP and uTP on IPv4 and IPv6.
- `dravity-utp` and `dravity-portmap` (NAT-PMP and UPnP).
- DHT `announce_peer`.

#### Changed

- Metainfo paths must be valid on this platform, including Windows device names.
- Unix opens use `O_NOFOLLOW` at every directory.
- The handshake advertises the fast extension. `announce_port` applies
  only when the session does not listen.

### Phase 2 — Magnets and swarm

Design: [`docs/phase-2.md`](docs/phase-2.md).

#### Added

- Magnet links, metadata fetch and serving, mainline DHT, PEX.
- Endgame, block stealing, AIMD window, download rate limit, fastresume.
- UDP connection-id cache and BEP 12 tiers.

#### Changed

- New peers start at 4 requests and grow to at most 128.
- The handshake advertises BEP 10.
- Peers may connect during the initial check. Interest waits until it finishes.

## Phase 1 — MVP downloader

`.torrent` downloads from HTTP and UDP trackers and TCP peers.
See [`docs/phase-1.md`](docs/phase-1.md).

## Phase 0 — Foundations

Bencode, metainfo, magnets, and the wire codec, with fuzz targets.
See [`docs/phase-0.md`](docs/phase-0.md).
