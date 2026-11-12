# BEP coverage

Implemented [BitTorrent Enhancement Proposals](https://www.bittorrent.org/beps/bep_0000.html).
**Full** means the BEP as it applies to Dravity. **Partial** names the gap.
`crates/dravity/tests/bep_coverage.rs` fails if code cites a BEP with no row.

| BEP | Title | Status | Where | Notes |
|---|---|---|---|---|
| 3 | The BitTorrent Protocol | Full | `dravity-wire`, `dravity-engine`, `dravity-core`, `dravity-tracker` | Metainfo, handshake, peer messages, HTTP announce, rarest-first, tit-for-tat. |
| 5 | DHT Protocol | Full | `dravity-dht`, `dravity-session` | Default node is `mainline`. `--native-dht` runs `NativeDht` until public checks pass. |
| 6 | Fast Extension | Full | `dravity-wire`, `dravity-engine` | Have All/None, Reject, Suggest, Allowed Fast. |
| 7 | IPv6 Tracker Extension | Full | `dravity-tracker`, `dravity-core` | `peers6` and IPv6 announces. |
| 9 | Metadata exchange | Full | `dravity-wire`, `dravity-engine` | Fetch and serve `info`. |
| 10 | Extension Protocol | Full | `dravity-wire`, `dravity-engine` | `m`, `p`, `v`, `yourip`, `reqq`, `metadata_size`, `upload_only`. |
| 11 | Peer Exchange | Full | `dravity-wire`, `dravity-engine` | Added/dropped with flags. Off for private torrents. |
| 12 | Multitracker | Full | `dravity-session`, `dravity-core` | Tiers shuffled; a success is promoted. |
| 14 | Local Service Discovery | Full | `dravity-lsd` | IPv4 and IPv6 multicast. Off for private torrents. Library default is off. |
| 15 | UDP Tracker Protocol | Full | `dravity-tracker` | Connect, announce, scrape, retries, connection-id cache. |
| 16 | Superseeding | Full | `dravity-engine` | One piece at a time from the least shared. |
| 19 | WebSeed | Partial | `dravity-webseed`, `dravity-session` | HTTP(S) only, no FTP. Back-off survives restarts. |
| 20 | Peer ID Conventions | Full | `dravity-core` | Azureus-style `-DV0001-`. |
| 21 | Partial seeds | Full | `dravity-engine` | `upload_only` while seeding. |
| 23 | Compact peer lists | Full | `dravity-tracker`, `dravity-core` | |
| 27 | Private Torrents | Full | `dravity-core`, `dravity-session` | Disables DHT, PEX, LSD, tracker exchange, and holepunch. |
| 28 | Tracker exchange | Full | `dravity-wire`, `dravity-engine`, `dravity-session` | Learned trackers are a new tier. Off for private torrents. |
| 29 | uTP | Full | `dravity-utp` | LEDBAT, selective ACK, shared listen port. |
| 32 | DHT IPv6 | Partial | `dravity-dht` | Native node only (`--dht-ipv6`). `mainline` is IPv4. |
| 33 | DHT scrapes | Partial | `dravity-dht` | Native node only. |
| 40 | Canonical peer priority | Full | `dravity-core`, `dravity-engine` | Decides evictions at the peer cap. |
| 42 | DHT security | Partial | `dravity-dht` | Secure ids in the native node only. |
| 43 | Read-only DHT nodes | Partial | `dravity-dht` | Native node only. |
| 44 | DHT mutable items | Partial | `dravity-dht` | Native node only. |
| 47 | Padding files | Full | `dravity-core`, `dravity-storage` | Padding is never written. Hybrids always pad. |
| 48 | Tracker scrape | Full | `dravity-tracker`, `dravity-session` | HTTP and UDP, on `scrape_interval`. |
| 51 | DHT infohash index | Partial | `dravity-dht` | Native node only. |
| 52 | Protocol v2 | Full | `dravity-core`, `dravity-engine`, `dravity-wire` | v2 and hybrid, piece layers, hash requests, `btmh`. |
| 53 | Magnet file selection | Full | `dravity-core`, `dravity-session` | `so=` selects files. The rest start skipped. |
| 54 | `lt_donthave` | Full | `dravity-wire`, `dravity-engine` | Sent when a recheck loses a piece. |
| 55 | Holepunch | Full | `dravity-wire`, `dravity-engine` | Rendezvous after a failed dial. Dials over uTP. |

## Not supported

| BEP | Title | Why |
|---|---|---|
| 17 | HTTP seeding (Hoffman) | BEP 19 covers HTTP sources. |
| 24 | Tracker external IP | The external address comes from `yourip`. |
| 30 | Merkle hash torrent extension | Superseded by BEP 52. |
| 35 | Torrent signing | No deployed signers. |
| 36 | Torrent RSS | The watch folder adds files. |
| 38 | Local data via torrent hints | Not implemented. |
| 39 | Updating torrents via feed | Not implemented. |
| 41 | UDP tracker URL data | Not sent. |
| 46 | Updating torrents via DHT items | Not implemented. |

Also: MSE/PE in `dravity-mse`; PCP, NAT-PMP, and UPnP in `dravity-portmap`;
SOCKS4/5 and HTTP CONNECT in `dravity-session`.
