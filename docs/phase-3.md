# Phase 3 — Good citizen: notes and decisions

Scope from the roadmap (plan §8): tit-for-tat + optimistic unchoke;
seeding; BEP 6; uTP (`dravity-utp`); IPv6; UPnP/NAT-PMP; peer banning; path
hardening. Also picks up the items Phase 2 deferred to Phase 3: listening
for incoming peers, the upload rate limit and DHT `announce_peer`.

## Crates

| Crate | I/O? | Phase 3 additions |
|---|---|---|
| `dravity-core` | no | `path::Platform`, `validate_component_for`, `SafePath::new_for`: Windows device names, forbidden characters, trailing dots/spaces, 255-byte components |
| `dravity-wire` | no | `allowed_fast_set` (BEP 6 canonical generator, checked against the BEP's test vector) |
| `dravity-engine` | no | Uploads (`Effect::ReadBlock` / `Event::BlockRead`), choker (tit-for-tat, anti-snubbing, optimistic unchoke), upload rate limit, BEP 6, hash-failure bans, `Event::PeerAccepted`, seeding |
| `dravity-storage` | blocking | `read_block`; files opened beneath the download directory without following symlinks (`openat` + `O_NOFOLLOW` on Unix) |
| `dravity-utp` (new) | async | uTP (BEP 29): packet codec, sans-IO `Connection` with LEDBAT, `UtpSocket`/`UtpStream` on Tokio |
| `dravity-portmap` (new) | async | NAT-PMP (RFC 6886) and UPnP IGD codecs, `PortMapper` (map, renew, remove) |
| `dravity-session` | async | Listener (TCP + uTP, IPv4 + IPv6), handshake routing by info-hash, block reads, DHT `announce_peer`, uTP dialing, port mapping |
| `dravity-cli` | via session | `--port` (listen), `--no-listen`, `--seed`, `--upload-limit`, `--upload-slots`, `--no-utp`, `--prefer-utp`, `--no-port-mapping` |

## Decisions

- **Choker.** Every `unchoke_interval` (10 s) the `upload_slots` (4)
  interested peers that upload fastest to us are unchoked (while seeding:
  those we upload to fastest). A peer that holds our requests and sends
  nothing for `snub_timeout` (60 s) loses its regular slot and can still
  hold the optimistic one. One extra optimistic slot rotates every 30 s
  to the choked, interested peer that waited longest, so newcomers get
  their first pieces.
- **Uploads are sans-IO too.** A request from an unchoked peer becomes
  `Effect::ReadBlock`; the session reads on the blocking pool and answers
  `Event::BlockRead` (or `ReadFailed`). At most `max_pending_reads` (64)
  reads are in flight, peers are served round-robin, and a token bucket
  paces them (`upload_rate_limit`). Tokens taken when a read is queued are
  returned when that block is not sent. A peer queues at most
  `max_upload_queue` (250, our `reqq`) requests; with BEP 6 extra requests
  get `reject`, without it they are dropped.
- **BEP 6.** `have all`/`have none` replace empty or full bitfields, choked
  requests are rejected, and IPv4 peers get the canonical allowed fast set
  (10 pieces). A piece finished after the handshake is offered to a fast
  peer whose set includes it, so a choked peer can request it. `suggest`
  is accepted and ignored.
- **Seeding.** With `Config::seed` (default) a finished torrent keeps
  accepting and uploading; seed-to-seed connections are closed. The CLI
  exits after the download unless `--seed` is given (then Ctrl-C ends it,
  successfully).
- **Bans are per IP**, from hash failures: every block of a failed piece
  from one peer is two strikes, a share of one is one strike, and
  `hash_fail_ban` (4) strikes ban the address. The default forgives one
  corrupt piece (a transient error on the sender, or a shared piece with a
  bad peer). Strikes are forgotten after `hash_fail_quiet` (one hour) with
  no new failure, so a shared NAT is not banned for mistakes spread over
  the download; a ban lasts for the torrent. An IPv4-mapped IPv6 address
  is the same key as its IPv4 address. Bans filter dials, accepts and PEX.
- **Listener.** One port for TCP and UDP (uTP), on IPv4 and IPv6
  (`IPV6_V6ONLY`, so both families can use the same port). A port in use
  falls back to a free one; `Session::listen_port` tells which. The accept
  task reads the handshake first, routes by info-hash, refuses
  self-connections, and bounds pending handshakes (`max_pending`, 32).
- **uTP is in-house.** `dravity-utp` is a sans-IO `Connection` plus a
  Tokio socket driver. LEDBAT (target delay 100 ms) controls the window;
  loss halves it; a retransmission timeout (RFC 6298, back-off up to 30 s)
  resends everything in flight from one MSS in slow start, like TCP. The
  accepting side sends no data until the initiator's first packet arrives
  (`SynRecv`), FIN is acknowledged only when all data before it has
  arrived, and a closed connection lingers 60 s to re-acknowledge a
  retransmitted FIN. Writing it here keeps the state machine sans-IO and
  testable on a simulated network, and adds no dependencies.
- **Dial order.** TCP first, uTP when TCP fails with a retryable error
  (refused, reset, timeout); `prefer_utp` reverses it.
- **Port mapping is in-house.** NAT-PMP first (the next hop of the IPv4
  default route: `/proc/net/route` on Linux, `route print -4` on Windows,
  `route -n get default` on macOS — not the `.1` address of a guessed
  `/24`, and not skipped on CGNAT), then UPnP IGD via SSDP
  (`WANIPConnection:2` > `:1` > `WANPPPConnection:1`). A minimal HTTP/1.1
  client and a tolerant tag scanner replace an HTTP and an XML crate.
  Mappings (TCP, plus UDP with uTP) last two hours and are renewed at half
  that; UPnP error 725 retries with a permanent lease. Shutdown removes
  them, bounded to five seconds. Failure is normal and only shows in
  `Session::port_mapping()`. The mapped external port is requested to
  equal the listen port; we do not announce a different one. When NAT-PMP
  returns another port, a warning is logged. UPnP does not read the
  external port back, so that mismatch stays invisible. Announcing a
  mapped port that differs is phase 4.
- **IPv6.** Dual listeners, IPv6 peers from trackers (BEP 7), PEX
  (`added6`) and dials over TCP and uTP. The BEP 6 allowed fast set is
  IPv4-only, as in the BEP.
- **Path hardening.** Metainfo paths are checked against the strictest
  rules of the platform we run on (Windows: `CON`, `NUL`, `COM1`… also
  with an extension, `<>:"|?*`, trailing dots/spaces; everywhere:
  components over 255 bytes). On Unix every directory below the download
  root is opened with `O_NOFOLLOW` relative to its parent, so a symlink
  swapped in after a check cannot redirect writes.

## Phase 2 deferrals handled here

| Item | Where |
|---|---|
| Listening for incoming peers, seeding, choking, upload rate limit | `dravity-engine` (`choker`, `state`), `dravity-session` (`listen`) |
| DHT `announce_peer` | `dravity-session` (after each successful lookup, with the listen port) |
| BEP 5 `port` message → DHT node | Still not done: `mainline` has no add-node API |
| IPv6 DHT (BEP 32) | Not done: `mainline` speaks IPv4 only |

## Tests

- Core: Windows names and characters on every host via `Platform`.
- Storage: symlinked directories and files refused, `read_block`.
- Wire: BEP 6 test vector.
- Engine: `tests/seeding.rs` (choker rounds, optimistic rotation,
  anti-snubbing, upload limit, rejects, allowed fast, bans, seed-to-seed);
  the engine proptest now sends requests, cancels, rejects and fast
  messages and checks the pending-read invariants.
- uTP: codec unit tests and fuzz target `utp_packet`; a simulated lossy,
  reordering network with a proptest; loopback socket tests.
- Port mapping: codec unit tests, fuzz target `portmap_parse`, a fake
  NAT-PMP gateway and a fake UPnP device (725 fallback, removal on stop).
- Session (loopback): session seeding to session over TCP and uTP, uTP
  fallback when TCP is refused, routing by info-hash, IPv6 over TCP and uTP,
  port mapping against a fake gateway.
- CLI: `--seed` serving a second CLI download over uTP.

Not covered: real routers and public swarms. The sandbox has no NAT
gateway and drops BitTorrent UDP traffic, so uTP and port mapping were
verified on loopback and against fakes.

## Deferred

| Item | Phase |
|---|---|
| IPv6 DHT (BEP 32), BEP 5 `port` messages | when the DHT backend supports them |
| PCP (RFC 6887), IPv6 pinholes | 5 (moved from 4) |
| Announcing a mapped port that differs from the listen port | done in 4 ([phase-4.md](phase-4.md)) |
| `suggest` (BEP 6) used for picking | done in 4 |
| uTP tail-loss probe; path MTU discovery (fixed 1382-byte payload) | 5 (moved from 4) |
| Directory symlink race on Windows (no `openat`; checked, then opened without following reparse points) | 5, with the io_uring/platform backends |
| Peer banning by other evidence (bad messages, ban lists) | 5 (moved from 4) |
