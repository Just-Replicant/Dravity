# Configuration reference

<!-- Generated from dravity_session::schema. Do not edit by hand: run
`DRAVITY_BLESS=1 cargo test -p dravity-session --lib schema` to refresh. -->

Every field of `SessionConfig` and of the per-torrent engine `Config`
(`SessionConfig::engine`). **live** settings change on a running session
through `Session::apply_settings` or `PATCH /api/v1/settings` (the field
name is the API name); **restart** settings are read when the session or
torrent starts.

## SessionConfig

| Setting | Applies | CLI | Description |
|---|---|---|---|
| `peer_id_prefix` | restart | — | Peer id prefix in Azureus style (`-DV0001-`); the rest is random. |
| `listen` | restart | `--port`, `--no-listen`, `--no-utp` | Accept incoming peer connections (TCP, and uTP unless `ListenConfig::utp` is `None`); `None` only dials out over TCP. |
| `prefer_utp` | restart | `--prefer-utp` | Dial peers over uTP first and fall back to TCP. |
| `port_mapping` | restart | `--no-port-mapping` | Ask the router (NAT-PMP, then UPnP IGD) to forward the listening port, TCP and (with uTP) UDP, so peers behind no NAT of their own can reach us. |
| `announce_port` | restart | `--port` | Port reported to trackers when the session does not listen (for example behind a proxy that forwards to us). |
| `engine` | restart | — | Per-torrent engine tunables. |
| `tracker` | restart | — | Tracker client settings. |
| `num_want` | restart | — | Peers requested per announce. |
| `connect_timeout` | restart | — | Timeout for TCP connect plus handshake. |
| `tick_interval` | restart | — | Engine timer period. |
| `peer_queue` | restart | — | Messages queued towards one peer before it is dropped as too slow. |
| `event_queue` | restart | — | Capacity of each torrent's event queue (peer, disk, hash, tracker). |
| `max_connections` | live | `--max-connections` | Open peer connections across every torrent, dialed or accepted, TCP or uTP (each torrent also keeps its own `max_peers`). |
| `hash_workers` | restart | — | Concurrent piece-hash jobs across the whole session (piece checks, rechecks and smart-ban block digests of every torrent share them). |
| `hash_on_arrival` | restart | — | Verify downloaded pieces from their blocks as they are written (incremental SHA-1, per-block SHA-256 leaves) instead of reading each finished piece back from storage. |
| `read_cache` | restart | `--read-cache` | Bytes of recently read torrent data kept in memory, shared by every torrent whose storage the session creates, so seeding a popular piece to many peers reads it from disk once (`dravity_storage::CachedStorage`). |
| `preallocate` | restart | `--preallocate` | How files the session creates reserve their space. |
| `durability` | restart | `--durability` | When written data is forced to disk. |
| `download_rate_limit` | live | `--global-download-limit` | Session-wide download limit in bytes per second, shared by every torrent on top of each torrent's own limit. |
| `upload_rate_limit` | live | `--global-upload-limit` | Session-wide upload limit in bytes per second. |
| `max_active_downloads` | live | `--max-active-downloads` | Torrents downloading at once; later ones wait in the queue. |
| `max_active_seeds` | live | `--max-active-seeds` | Torrents seeding at once; later ones wait in the queue. |
| `ip_filter` | restart | `--ip-filter` | Address ranges whose peers are refused and never dialed (`crate::IpFilter`); replace it at runtime with `crate::Session::set_ip_filter`. |
| `proxy` | restart | `--proxy` | Dial peers through this proxy (TCP only: uTP is skipped while a proxy is set). |
| `peer_dscp` | restart | `--peer-dscp` | DSCP value (0 to `crate::MAX_DSCP`) marked on peer TCP connections. |
| `geoip` | restart | `--geoip` | Country database for `crate::PeerStatus::country` (`crate::GeoIp`). |
| `network` | restart | — | Carry peer connections over this `crate::transport::Network` instead of TCP and uTP sockets. |
| `lsd` | restart | `--no-lsd` | Local Service Discovery (BEP 14): find peers of the same torrents on the local network by multicast. |
| `scrape_interval` | restart | `--scrape-interval` | Scrape every tracker this often (BEP 48 / BEP 15) and show the counts in `crate::TrackerStatus::scrape`. |
| `stop_announce_timeout` | restart | — | Time allowed for the `stopped` announce when a torrent stops. |
| `alert_buffer` | restart | — | Broadcast buffer for alerts; slow subscribers miss older alerts. |
| `dht` | restart | `--no-dht` | Run a DHT node (BEP 5) for peer discovery; `None` disables the DHT. |
| `dht_lookup_timeout` | restart | — | Time allowed for one DHT lookup. |
| `resume_dir` | restart | — | Directory for fastresume files (`<info-hash>.resume`). |
| `state_dir` | restart | — | Directory for session state (`crate::persist`): the torrents of the session, their directories, file priorities and paused flags, so `crate::Session::restore` can bring them back after a restart. |
| `web_seed` | restart | `--no-web-seeds`, `--web-seeds-allow-private` | HTTP settings for web seeds (BEP 19: a torrent's `url-list`, a magnet link's `ws`). |
| `web_seed_retry` | restart | — | Wait this long before trying a web seed again after it failed; doubles with every failure in a row, up to 32 times as long. |
| `storage_backend` | restart | `--storage-backend` | How file data is read and written for torrents whose storage the session creates (not for `AddTorrentOptions::with_storage`). |
| `encryption` | restart | `--encryption` | Message Stream Encryption (MSE/PE) for peer connections over TCP and uTP. |

## Engine Config (`SessionConfig::engine`)

| Setting | Applies | CLI | Description |
|---|---|---|---|
| `max_peers` | restart | `--max-peers` | Maximum simultaneously connected (or connecting) peers. |
| `max_requests_per_peer` | restart | — | Upper bound of a peer's AIMD request window (outstanding block requests). |
| `initial_requests_per_peer` | restart | — | Request window a new peer starts with; it grows while blocks arrive in time and halves when a request times out. |
| `request_budget` | restart | — | Block requests outstanding across all peers of the torrent. |
| `steal_after` | restart | — | Request a block from a second peer when the first has not delivered it after this long and the second has nothing else to do (piece stealing from slow peers). |
| `endgame` | restart | — | Once every remaining block is requested, request blocks from more than one peer and cancel the duplicates as copies arrive (endgame). |
| `download_rate_limit` | restart | `--download-limit` | Download rate limit in bytes per second (`None`: unlimited). |
| `pex` | restart | — | Exchange peers with other peers (BEP 11). |
| `tracker_exchange` | restart | `--no-tracker-exchange` | Exchange tracker URLs with peers (`lt_tex`). |
| `holepunch` | restart | `--no-holepunch` | BEP 55 `ut_holepunch`: relay rendezvous between two connected peers, and ask a PEX source to relay one when dialing a peer it told us about fails. |
| `super_seed` | restart | `--super-seed` | BEP 16 super-seeding while the torrent is complete: peers see no bitfield, only one piece at a time chosen from the least shared, and get the next once they announce it. |
| `pex_interval` | restart | — | Minimum time between two PEX messages to the same peer (BEP 11: one minute). |
| `dht` | restart | — | Ask the driver to look up peers in the DHT (`Effect::DhtLookup`). |
| `dht_interval` | restart | — | Time between DHT lookups for one torrent after a lookup finishes. |
| `dht_retry` | restart | — | Time before another DHT lookup when the previous one timed out or the node was not ready. |
| `max_metadata_size` | restart | — | Largest `info` dictionary accepted from peers for a magnet link. |
| `piece_layer_timeout` | restart | — | How long a hybrid magnet waits for its v2 piece layers without any layer block arriving before the download starts on SHA-1 alone for the files still missing one (a v2-only magnet always waits: it has no other way to check its pieces). |
| `piece_layer_stall_alert` | restart | — | A v2-only magnet whose piece layers have not progressed for this long raises `Alert::PieceLayersStalled` (and again after each further period of silence). |
| `request_timeout` | restart | — | A request without an answer after this long is given to other peers. |
| `peer_timeout` | restart | — | Drop a peer that sent nothing for this long. |
| `peer_retry` | restart | — | Wait this long before connecting again to a peer that failed or dropped. |
| `keepalive_interval` | restart | — | Send a keep-alive after this long without sending anything. |
| `default_announce_interval` | restart | — | Re-announce interval when the tracker does not give one. |
| `min_announce_interval` | restart | — | Lower bound applied to tracker-provided intervals. |
| `announce_retry` | restart | — | Delay before retrying a failed announce. |
| `max_pending_writes` | restart | — | Blocks handed to storage but not yet written; no new requests are sent while this many writes are pending (disk backpressure). |
| `check_existing` | restart | — | Hash existing data on start to resume a partial download. |
| `check_concurrency` | restart | — | Maximum concurrent hash jobs while checking. |
| `seed` | restart | — | Keep the torrent in the swarm once it is complete: announce, accept connections and upload (seeding). |
| `upload_slots` | restart | `--upload-slots` | Peers unchoked for their upload to us (tit-for-tat) while downloading, or for the fastest uploads while seeding. |
| `optimistic_unchoke` | restart | — | Give one extra slot to a choked peer in turn, so newcomers get data and better partners are discovered. |
| `seed_choking` | restart | `--seed-choking` | How regular slots are ranked while seeding (see `SeedChoking`). |
| `round_robin_quota` | restart | — | Bytes a peer receives per turn under `SeedChoking::RoundRobin` before its slot passes on. |
| `upload_slots_auto` | restart | `--auto-upload-slots` | Tune `Self::upload_slots` every round from the measured upload rates: one more slot for each peer that takes more than an increasing threshold (1 KiB/s, 2 KiB/s, …), so slots grow while extra peers still add upload and stop when they would only split it. |
| `unchoke_interval` | restart | — | Time between two choker rounds (BEP 3 suggests ten seconds). |
| `optimistic_unchoke_interval` | restart | — | Time between two changes of the optimistic unchoke. |
| `snub_timeout` | restart | — | A peer that holds our requests and sends no block for this long is snubbing us: it loses its regular slot (anti-snubbing) and can still receive the optimistic unchoke. |
| `upload_rate_limit` | restart | `--upload-limit` | Upload rate limit in bytes per second (`None`: unlimited). |
| `max_upload_queue` | restart | — | Requests from one peer we queue before refusing more. |
| `max_pending_reads` | restart | — | Block reads for uploads in flight at once across all peers (disk backpressure for seeding). |
| `fast_extension` | restart | — | Advertise and use the BEP 6 fast extension (`have all`/`have none`, `reject`, `allowed fast`). |
| `allowed_fast` | restart | — | Size of the BEP 6 allowed fast set offered to fast-extension peers (0: offer none). |
| `hash_fail_ban` | restart | — | Ban a peer's IP address once it has this many strikes for pieces that failed the hash check (0: never ban). |
| `hash_fail_quiet` | restart | — | Forget an address's hash-failure strikes after this long with no new failure, so a shared NAT is not banned for mistakes spread over the whole download. |
| `smart_ban` | restart | — | Smart banning (default `true`): when a piece with several contributors fails, find out which block was bad instead of striking everyone. |
| `listen_port` | restart | — | Port we accept connections on, advertised in the extension handshake (`p`) so peers that we dialed can tell others how to reach us. |
