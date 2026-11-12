# Phase 3 — Good citizen: notes and decisions

Scope (plan §8): tit-for-tat and optimistic unchoke, seeding, BEP 6, uTP,
IPv6, NAT-PMP and UPnP, peer banning, and path hardening. Also listening,
the upload rate limit, and DHT `announce_peer` from Phase 2.

Phase 4 announces a mapped port when it differs from the listen port and
uses BEP 6 `suggest` when picking. IronTide made hash-failure bans
session-wide; see [`irontide-adoption.md`](irontide-adoption.md#2-smart-ban).

## Decisions

- Every `unchoke_interval` (10 s) the `upload_slots` (4) interested peers
  that upload fastest to us are unchoked. While seeding, the slots go to
  the peers we upload to fastest. A peer that holds requests and sends
  nothing for `snub_timeout` (60 s) loses its regular slot and can still
  hold the optimistic one. One optimistic slot rotates every 30 s to the
  choked, interested peer that waited longest.
- A request from an unchoked peer becomes `Effect::ReadBlock`. The session
  reads on the blocking pool and answers `Event::BlockRead` or
  `ReadFailed`. At most `max_pending_reads` (64) reads are in flight,
  peers are served round-robin, and `upload_rate_limit` paces them.
  Tokens taken for a read that is not sent are returned. A peer queues at
  most `max_upload_queue` (250) requests. With BEP 6, extras get
  `reject`; without it they are dropped.
- `have all` and `have none` replace empty or full bitfields. Choked
  requests are rejected. IPv4 peers get the canonical allowed-fast set
  (10 pieces). A piece finished after the handshake is offered to a fast
  peer whose set includes it. `suggest` is accepted and ignored.
- With `Config::seed` a finished torrent keeps uploading. Seed-to-seed
  connections are closed. The CLI exits after the download unless
  `--seed`.
- Hash-failure bans are per IP. Every block of a failed piece from one
  peer is two strikes; a share of one is one strike. `hash_fail_ban` (4)
  strikes ban the address for the torrent. Strikes are forgotten after
  `hash_fail_quiet` (one hour) with no new failure. An IPv4-mapped IPv6
  address is the same key as its IPv4 address. Bans filter dials, accepts,
  and PEX.
- One listen port serves TCP and uTP, on IPv4 and IPv6 (`IPV6_V6ONLY`).
  A port in use falls back to a free one; `Session::listen_port` reports
  it. The accept task reads the handshake, routes by info-hash, refuses
  self-connections, and bounds pending handshakes (`max_pending`, 32).
  DHT `announce_peer` runs after each successful lookup, with that port.
- `dravity-utp` is a sans-IO `Connection` plus a Tokio socket. LEDBAT
  (target delay 100 ms) controls the window; loss halves it. A
  retransmission timeout (RFC 6298, back-off up to 30 s) resends
  everything in flight from one MSS. The acceptor sends no data until the
  initiator's first packet (`SynRecv`). FIN is acknowledged only when all
  data before it has arrived. A closed connection lingers 60 s.
- Dials try TCP first and uTP after a retryable TCP failure. `prefer_utp`
  reverses that.
- Port mapping tries NAT-PMP on the IPv4 default route, then UPnP IGD
  (`WANIPConnection:2`, then `:1`, then `WANPPPConnection:1`). Mappings
  last two hours and renew at half that. UPnP error 725 retries with a
  permanent lease. Shutdown removes them, bounded to five seconds.
  Failure only shows in `Session::port_mapping()`. This phase requested
  the external port to equal the listen port and did not announce a
  different one.
- Peers may be IPv6 from trackers (BEP 7), PEX (`added6`), and dials over
  TCP and uTP. The allowed-fast set stays IPv4-only.
- Paths are checked against the strictest rules of the platform we run
  on (Windows device names, `<>:"|?*`, trailing dots and spaces;
  components over 255 bytes everywhere). On Unix every directory below
  the download root is opened with `O_NOFOLLOW` relative to its parent.

## Left for later

IPv6 DHT and BEP 5 `port` waited on the DHT backend. PCP, a uTP tail-loss
probe, path MTU discovery, the Windows directory-symlink race, and bans
for evidence other than bad pieces were left for later phases.
