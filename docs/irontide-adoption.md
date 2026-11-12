# irontide adoption

Thirty-one ideas from the irontide engine, reimplemented for Dravity's
sans-IO design. No irontide code is copied (AGPL here, GPL there).

**done** means implemented as described. **partial** names the gap.

| # | Item | Status | Notes |
|---|---|---|---|
| 1 | SSRF guard for HTTP tracker announces | done | [notes](#1-ssrf-guard) |
| 2 | Session-wide smart ban | done | [notes](#2-smart-ban) |
| 3 | Global connection cap | done | [notes](#3-connection-cap) |
| 4 | Session-wide hash pool | done | [notes](#4-hash-pool) |
| 5 | Sans-IO purity tripwires | done | [notes](#5-purity-tripwires) |
| 6 | Hash on arrival | done | [notes](#6-hash-on-arrival) |
| 7 | Byte-budgeted read cache | partial | Read cache only. No shared write pool. [notes](#7-read-cache) |
| 8 | Verify-before-download prescan | done | [notes](#8-prescan) |
| 9 | BDP-adaptive request budget | done | [notes](#9-bdp-request-budget) |
| 10 | Piece reservation | done | [notes](#10-piece-reservation) |
| 11 | Preallocation and durability | done | [notes](#11-preallocation-and-durability) |
| 12 | Seed choking and slot tuner | done | [notes](#12-seed-choking-and-slot-tuner) |
| 13 | Torrent creation | done | [notes](#13-torrent-creation) |
| 14 | Tracker scrape | done | [notes](#14-tracker-scrape) |
| 15 | Local Service Discovery | done | [notes](#15-local-service-discovery) |
| 16 | Live settings, rate limits, queue | done | [notes](#16-live-settings-rate-limits-and-queueing) |
| 17 | BEP 53 select-only magnets | done | [notes](#17-bep-53-select-only-magnets) |
| 18 | Wire extensions | done | [notes](#18-wire-extensions) |
| 19 | BEP 40 peer priority | done | [notes](#19-bep-40-peer-priority) |
| 20 | IP filter | done | [notes](#20-ip-filter) |
| 21 | PCP before NAT-PMP and UPnP | done | [notes](#21-pcp) |
| 22 | Native DHT | partial | Opt-in. `mainline` is still the default. [notes](#22-native-dht-building-blocks) |
| 23 | SOCKS and HTTP proxies | partial | Native DHT only. I2P is not wired in. [notes](#23-proxy-support) |
| 24 | GeoIP, DSCP, web-seed stats | done | [notes](#24-peer-extras) |
| 25 | BEP 55 holepunch | done | [notes](#25-bep-55-holepunch) |
| 26 | BEP 52 conformance vectors | done | [notes](#26-bep-52-conformance-vectors) |
| 27 | Loom models | done | [notes](#27-more-loom-models) |
| 28 | Pluggable network transport | done | [notes](#28-pluggable-network-transport) |
| 29 | Settings registry | done | Generates `docs/configuration.md`. [notes](#29-settings-single-source-of-truth) |
| 30 | Facade crate | done | [notes](#30-facade-crate) |
| 31 | BEP coverage table | done | [notes](#31-bep-coverage-table) |

## Design notes

### 1. SSRF guard

`dravity_tracker::guard` checks the announce URL and every redirect.
Loopback trackers must use an `/announce` path. Redirects stay on
`http(s)`, never downgrade TLS, and stop after 5. A redirect from a
public tracker cannot land on a private address, literal or resolved.
The filter runs in the resolver the connection uses. Origins the torrent
itself names are trusted. `allow_private_trackers` or
`private_tracker_origins` opts in. Trust is request-scoped, so one allowed
LAN tracker cannot pull a later announce into the LAN. Proxy DNS for
untrusted names needs `allow_proxy_dns`.

### 2. Smart ban

A failed piece whose blocks came from more than one address is
quarantined. The session hashes each 16 KiB block (`Effect::HashBlocks`).
When a later copy verifies, only addresses whose block digest differs are
banned. On v2, leaf hashes from a peer that did not send the piece convict
at once. Strikes remain when there was one sender, the blocks cannot be
read, or the 256-piece table is full. `smart_ban = false` uses strikes
only. Bans are session-wide. `BanList` bumps a generation on each ban and
filter change, so a full command queue cannot drop them.

### 3. Connection cap

`dravity_core::admit` hands out RAII slots (`max_connections`, default
500). Accept and dial take a slot first. A full cap defers the dial
(`PeerConnectDeferred`) and retries after `DEFERRED_DIAL_RETRY`. Lowering
the cap does not close connections. A loom model covers admit and release.

### 4. Hash pool

One session `HashPool` (`hash_workers`) bounds hashing for every torrent.
Jobs run on Tokio's blocking pool. A panic reports `StorageFailed` and
frees the slot.

### 5. Purity tripwires

`tests/sans_io_purity.rs` rejects tokio, `.await`, sockets, `std::fs`,
threads, and the clock in sans-IO modules. Comments and `mod tests` are
skipped. Crates that are entirely sans-IO scan every file under `src/`.

### 6. Hash on arrival

`PieceHasher` takes blocks in any order, holds early blocks until SHA-1
can continue, and hashes each 16 KiB SHA-256 leaf on arrival. The session
feeds it on the write threads, so `HashPiece` does not read the piece
back. A duplicate block, a piece that would hold more than 32 MiB of early
blocks, and checks of existing data still read from storage.

### 7. Read cache

`CachedStorage` shares one segmented LRU (`read_cache`, default 64 MiB).
A chunk is protected on its second hit. `read_many` keeps io_uring
batching. Concurrent misses of one span share one disk read. Writes drop
overlapping chunks. A read that races a write is not inserted. There is
no shared write pool: writes already use `Bytes` from the wire.

### 8. Prescan

`absent_pieces` lists pieces that extend past a file's length, using the
stamps fastresume already collects. The engine skips hashing those. A
preallocated full-length file is still hashed.

### 9. BDP request budget

Request depth is the larger of the AIMD window and the bandwidth-delay
product (EWMA rate times the minimum RTT from the last 10 s), still inside
`max_requests_per_peer` and `reqq`. A timeout leaves AIMD in charge for
10 s. `request_budget` caps outstanding requests for the torrent.

### 10. Piece reservation

`pick_for` prefers urgent pieces, then the peer's own, then unowned
started pieces, then a new rarest piece, and only then another peer's
piece. Disconnect, choke, or a failed hash releases the reservation.

### 11. Preallocation and durability

`Sparse` uses `set_len`. `Full` uses `fallocate` and falls back to
`set_len`. Files are created on first write. A longer file is never
truncated. `PerPiece` syncs a passing piece before the engine counts it.
`OnComplete` syncs on `Finished`. `Never` syncs on resume save, descriptor
eviction, and stop. A preallocated file cannot be ruled out by size, so
it is rechecked when resume data is missing.

### 12. Seed choking and slot tuner

While downloading, slots stay tit-for-tat. While seeding,
`FastestUpload` (default), `RoundRobin` (4 MiB quota), or `AntiLeech`
(closest to 50% progress) ranks peers. `upload_slots_auto` counts peers
above a threshold that starts at 1 KiB/s and rises by 1 KiB/s per peer,
then adds one probing slot, clamped to 1..=64. Ties stay deterministic.

### 13. Torrent creation

`TorrentCreator` streams SHA-1 for the v1 layout (zeros for BEP 47 pads)
and SHA-256 leaves per file. `finish` encodes `info` and the root.
Automatic piece length is the smallest power of two from 16 KiB to 16 MiB
that keeps about 1500 pieces. `create_torrent` skips symlinks, rejects
non-UTF-8 names, and fails if a file changes size while hashing.
`dravity create` writes the file.

### 14. Tracker scrape

HTTP scrape exists only when the last path segment starts with
`announce`. UDP scrape is action 2, at most 74 hashes. Scrapes share the
connection-id cache and the SSRF guard. The session scrapes only on
`TorrentHandle::scrape` or `scrape_interval`.

### 15. Local service discovery

`dravity-lsd` sends `BT-SEARCH` with at most 20 info-hashes and a cookie.
It binds with `SO_REUSEADDR`, drops its own announces, and accepts at most
16 messages per source per minute. The session announces public torrents
with known metadata at most once per 5 minutes. The library default is
off. The CLI turns it on.

### 16. Live settings, rate limits, and queueing

`SettingsUpdate` uses `Option<Option<_>>`: absent keeps a value,
`Some(None)` clears it. Validation builds the next value before anything
changes. Session rate limits are token buckets on top of each torrent's
limit. Rates below 16 KiB/s are refused. The queue starts the oldest
waiting torrents and stops the newest surplus. A live listen-port change
binds the new sockets first. An occupied requested port fails and leaves
the old listener. Connected peers get a new extension handshake.
`settings.bencode` and `listen.bencode` restore both. `apply_settings` and
`set_listen_port` share one mutex. The bucket returns the full wait, and
a live rate change reprices debt without charging the bytes again.

### 17. BEP 53 select-only magnets

`so=` is comma-separated indices and ranges, at most 1024 ranges. Files
outside the selection start at `Skip` unless priorities were set
explicitly. The selection is part of the saved magnet URI.

### 18. Wire extensions

`lt_donthave` is a piece index. `lt_tex` keeps at most 50 `http`, `https`,
or `udp` URLs. `upload_only` follows whether anything is still wanted and
does not disconnect the peer. Super-seeding reveals one piece at a time
and rejects requests for any other. Learned trackers become one new tier
and are not relayed. Private torrents skip the exchange.

### 19. BEP 40 peer priority

CRC-32C with the BEP's masks. At `max_peers`, a newcomer replaces the
lowest-ranked peer only when it ranks strictly higher. Until `yourip`
gives a public address, the newcomer is refused. Dial order stays FIFO.

### 20. IP filter

Lines are eMule, a bare range, or P2P text, including CIDR. Ranges merge.
The listener and dial path both consult the list. `set_ip_filter` closes
connections the new filter blocks, including during metadata fetch.
Trackers and web seeds are not filtered. Port filtering is not added.

### 21. PCP

MAP requests are 60 bytes. A response must echo the nonce, protocol, and
port. Failure falls back to NAT-PMP, then UPnP. Renewals reuse the nonce
and the assigned address at half the granted lifetime. IPv6 pinholes go
to the IPv6 default router and do not change the announced port. Other
operating systems need `gateway_v6` until their route lookup exists.

### 22. Native DHT building blocks

Sans-IO KRPC, a 160-bucket routing table, and a token store. `NativeDht`
answers the four BEP 5 queries, looks up iteratively, and checks announce
tokens against the source address. `--native-dht` runs it behind `Dht`.
The same node implements BEP 33, 42, 43, 44, and 51. `--dht-ipv6` starts
a second node. Each family keeps its own table. Token secrets come from
the OS CSPRNG. `SharedUdp` can classify DHT, uTP, and tracker datagrams,
but the session still binds them separately. Public checks:
[`native-dht-validation.md`](native-dht-validation.md).

### 23. Proxy support

Peers dial through SOCKS4, SOCKS5, or HTTP CONNECT. uTP is skipped while
a proxy is set. HTTP and SOCKS proxies also carry trackers and web seeds.
Names are resolved at the proxy (`socks5h`, `socks4a`, SOCKS5 domain
addresses). A SOCKS5 UDP relay carries UDP trackers and, when native DHT
is on, the DHT node. Nothing is sent until ASSOCIATE succeeds. `mainline`
is not proxied. LSD stays on the LAN. `dravity-i2p` speaks SAM v3 and is
not used for BitTorrent peers. Incoming TCP is not proxied.

### 24. Peer extras

GeoIP reads a `start,end,CC` CSV (256 MiB cap). Web-seed addresses get no
country. DSCP is set on peer TCP sockets (0–63). uTP sockets are shared
and not marked. Unhealthy web-seed back-off is stored in `.state` (at
most 256 URLs) and restored.

### 25. BEP 55 holepunch

A rendezvous is relayed to both peers, or answered with `NoSelf`,
`NotConnected`, or `NoSupport`. A failed dial of a holepunch-capable PEX
address asks that source once. The resulting connect prefers uTP.
Metadata-phase peers do not relay. Private torrents and `--no-holepunch`
turn it off.

### 26. BEP 52 conformance vectors

`tests/data/bep52_vectors.py` is a separate tree written from the BEP.
The Rust tests compare piece roots, layers, padding hashes, and created
torrents against that output.

### 27. More loom models

The session rate bucket is `dravity_core::rate::TokenBucket` with the
clock passed in. Loom checks two concurrent takes, a rate change racing
a take, and `set_max` racing admit. `last` only moves forward, so an
earlier timestamp cannot credit the same interval twice.

### 28. Pluggable network transport

`SessionConfig` holds `Arc<dyn Network>`. With one set, TCP and uTP
sockets, port mapping, LSD announce, and peer proxies are skipped.
Trackers, web seeds, DHT, and LSD keep their own sockets.
`MemoryNetwork` delivers duplex pipes and can mark an address unreachable.

### 29. Settings single source of truth

A `registry!` macro emits `const &[Setting]` and an exhaustive destructure
of the config struct, so a new field fails the build until it is listed.
`docs/configuration.md` must match `configuration_markdown()`. Live
registry names must match `LiveSettings` and `GET /api/v1/settings`.
Every registered flag must appear in `dravity download --help`. Refresh
with `DRAVITY_BLESS=1 cargo test -p dravity-session --lib schema`.

### 30. Facade crate

`dravity` re-exports session, core, and storage. `ClientBuilder` wraps
`SessionConfig`. `Client` derefs to `Session`. `Error` wraps the session,
metainfo, magnet, proxy, and file errors. The binary stays `dravity-cli`.

### 31. BEP coverage table

[`bep-coverage.md`](bep-coverage.md) lists implemented and rejected BEPs.
`crates/dravity/tests/bep_coverage.rs` requires a row for every BEP the
code cites.
