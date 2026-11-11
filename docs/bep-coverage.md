# BEP coverage

What Dravity implements from the [BitTorrent Enhancement Proposals](https://www.bittorrent.org/beps/bep_0000.html),
where the implementation lives, and what is missing. **Full** means the BEP
is implemented as written for the roles Dravity plays (downloader, seeder,
relay). **Partial** gives the gap. A test (`crates/dravity/tests/bep_coverage.rs`)
fails if the code cites a BEP that has no row here.

| BEP | Title | Status | Where | Notes |
|---|---|---|---|---|
| 3 | The BitTorrent Protocol | Full | `dravity-wire`, `dravity-engine`, `dravity-core`, `dravity-tracker` | Metainfo, handshake, peer messages, HTTP tracker announces, rarest-first, tit-for-tat. |
| 5 | DHT Protocol | Full (via `mainline`) | `dravity-session` (`Dht`), `dravity-dht` | The running node is the `mainline` crate on its own UDP socket. `dravity_dht::krpc` is a native sans-IO codec that is not yet wired in (see item 22 in `irontide-adoption.md`). |
| 6 | Fast Extension | Full | `dravity-wire::fast`, `dravity-engine` | Have All/None, Reject, Suggest Piece (remembered and preferred when picking), Allowed Fast (BEP 6 set, offered and used). |
| 7 | IPv6 Tracker Extension | Full | `dravity-tracker`, `dravity-core::compact` | `peers6`, IPv6 announces. |
| 9 | Extension for Peers to Send Metadata Files | Full | `dravity-wire::metadata`, `dravity-engine::metadata` | Magnet links fetch and serve the `info` dictionary. |
| 10 | Extension Protocol | Full | `dravity-wire::extended`, `dravity-engine::ext` | `m`, `p`, `v`, `yourip`, `reqq`, `metadata_size`, `upload_only`. |
| 11 | Peer Exchange (PEX) | Full | `dravity-wire::pex`, `dravity-engine` | Added/dropped with flags (encryption, seed, uTP, holepunch, reachable); off for private torrents. |
| 12 | Multitracker Metadata Extension | Full | `dravity-session` (tiers), `dravity-core` | Tiers shuffled and promoted on success. |
| 14 | Local Service Discovery | Full | `dravity-lsd` | IPv4 and IPv6 multicast; off for private torrents and by default in the library. |
| 15 | UDP Tracker Protocol | Full | `dravity-tracker::udp` | Connect, announce, scrape, retransmission, connection id cache. |
| 16 | Superseeding | Full | `dravity-engine` (`Config::super_seed`) | One piece at a time from the least shared, revealed as peers announce it. |
| 19 | WebSeed (HTTP/FTP Seeding, GetRight style) | Partial | `dravity-webseed`, `dravity-session::web_seed` | HTTP(S) only, no FTP. Back-off and bans persist across restarts. |
| 20 | Peer ID Conventions | Full | `dravity-core::PeerId` | Azureus style `-DV0001-` prefix. |
| 21 | Extension for Partial Seeds | Full | `dravity-engine` | `upload_only` sent while seeding; partial seeds kept and flagged in PEX. |
| 23 | Tracker Returns Compact Peer Lists | Full | `dravity-tracker`, `dravity-core::compact` | |
| 27 | Private Torrents | Full | `dravity-core`, `dravity-session` | Disables DHT, PEX, LSD, tracker exchange and holepunch. |
| 28 | Tracker exchange extension | Full | `dravity-wire::ext_msgs` (`lt_tex`), `dravity-engine`, `dravity-session` | Learned trackers form a new tier; off for private torrents. |
| 29 | uTorrent transport protocol (uTP) | Full | `dravity-utp` | LEDBAT congestion control, selective ACK, shared listen port with TCP. |
| 32 | BitTorrent DHT Extensions for IPv6 | Partial | `dravity-dht::krpc` | `nodes6` encoded and decoded by the native codec. The running `mainline` DHT is IPv4 only. |
| 40 | Canonical Peer Priority | Full | `dravity-core::priority`, `dravity-engine` | CRC-32C ranking verified against the BEP's vectors; decides evictions at the peer cap. |
| 42 | DHT Security Extension | Partial | `dravity-dht::secure_id` | Secure node ids and checks verified against the BEP's vectors. They are not applied by the `mainline` runtime. |
| 47 | Padding files and extended file attributes | Full | `dravity-core`, `dravity-storage` | Padding files are never written; `attr` `p` honoured; hybrids always pad. |
| 48 | Tracker Protocol Extension: Scrape | Full | `dravity-tracker`, `dravity-session` | Periodic scrape (`scrape_interval`) for HTTP and UDP trackers. |
| 52 | The BitTorrent Protocol Specification v2 | Full | `dravity-core::{merkle,v2}`, `dravity-engine::layers`, `dravity-wire` | v2-only and hybrid torrents, piece layers, hash requests, v2 magnets; checked against independent vectors (`dravity-core/tests/bep52_conformance.rs`). |
| 53 | Magnet URI extension - Select specific file indices for download | Full | `dravity-core::magnet`, `dravity-session` | `so=` ranges select files; others start skipped. |
| 54 | The lt_donthave extension | Full | `dravity-wire::ext_msgs`, `dravity-engine` | Sent when a recheck loses a piece; received pieces are withdrawn. |
| 55 | Holepunch extension | Full | `dravity-wire::holepunch`, `dravity-engine` | Relays rendezvous, requests one from the PEX source after a failed dial, dials over uTP. |

## Not supported

| BEP | Title | Why |
|---|---|---|
| 17 | HTTP Seeding (Hoffman style) | Rare in practice; BEP 19 web seeds cover HTTP sources. |
| 24 | Tracker Returning External IP | The external address comes from peers' `yourip` instead. |
| 30 | Merkle hash torrent extension | Superseded by BEP 52. |
| 33 | DHT Scrapes | Not offered by the `mainline` runtime. |
| 35 | Torrent Signing | Draft, no deployed signers. |
| 36 | Torrent RSS feeds | Out of scope for the library; the CLI's watch folder covers automatic adds. |
| 38 | Finding Local Data Via Torrent File Hints | Not implemented. |
| 39 | Updating Torrents Via Feed URL | Not implemented. |
| 41 | UDP Tracker Protocol Extensions | URL data is not sent to UDP trackers. |
| 43 | Read-only DHT Nodes | Not exposed by the `mainline` runtime. |
| 44 | Storing arbitrary data in the DHT | Not used. |
| 46 | Updating Torrents Via DHT Mutable Items | Not implemented. |
| 51 | DHT Infohash Indexing | Not implemented. |

Non-BEP protocols: Message Stream Encryption (MSE/PE) in `dravity-mse`; NAT-PMP
(RFC 6886), PCP (RFC 6887) and UPnP IGD in `dravity-portmap`; SOCKS4/5 and
HTTP CONNECT proxies in `dravity-session::proxy`.
