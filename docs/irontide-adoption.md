# irontide adoption

Dravity reviewed the irontide BitTorrent engine and listed 31 ideas worth
adopting (fixes, improvements and features). This page tracks each one.
Every item is **reimplemented** for Dravity's sans-IO architecture; no
irontide code is copied (irontide is GPL, Dravity is MIT/Apache-2.0).

Status: **done** (implemented as described), **partial** (the closest
sound version, with the difference explained), **open** (not started).

| # | Item | Status | Commits | Notes |
|---|------|--------|---------|-------|
| 1 | SSRF guard for HTTP tracker announces | done | `fix(tracker): guard HTTP announces against SSRF` | [notes](#1-ssrf-guard) |
| 2 | Session-wide smart ban | done | `feat(engine,session): session-wide smart ban` | [notes](#2-smart-ban) |
| 3 | Global connection cap with one admit function and a loom model | done | `feat(core,session): session-wide connection cap` | [notes](#3-connection-cap) |
| 4 | Session-wide hash pool | done | `perf(session): share one hash pool across torrents` | [notes](#4-hash-pool) |

## Design notes

### 1. SSRF guard

`dravity_tracker::guard` checks announce URLs and every redirect hop. A
tracker on `localhost`/loopback must use an `/announce` path (so a
torrent cannot aim requests at arbitrary local services). Redirects must
stay on `http(s)`, may not downgrade `https` to `http`, and are limited
to `MAX_REDIRECTS` (5). A redirect from a non-local tracker may not
reach a non-public address. This applies to literal addresses and to
resolved ones. The filter runs inside the resolver the connection
uses, so DNS rebinding cannot get around it. Host names that the
torrent itself names as announce origins are trusted, so LAN trackers
keep working. `ClientConfig::ssrf_mitigation = false` turns the guard
off.

### 2. Smart ban

irontide paroles a peer after a failed piece and blames whoever fails
the next attempt. Dravity attributes blame directly instead:

- When a piece fails and its blocks came from more than one address, the
  engine quarantines the piece. The picker does not release it yet, so
  new blocks cannot overwrite the evidence. The engine emits
  `Effect::HashBlocks`. The session reads the piece from storage on the
  shared hashing permits and answers with `Event::BlockHashes`, which
  carries one SHA-256 digest per 16 KiB block.
- The digests are stored per (block, sender). After that the piece is
  downloaded again. When a copy passes, its blocks are hashed too, and
  any address whose recorded block differs from the verified block is
  banned. An honest peer that shared the piece is never struck.
- v2/hybrid torrents need no good copy. The engine asks a BEP 52 peer
  that has the piece (preferring one that did not send it) for the
  piece's leaf hashes. It checks them against the piece-layer hash it
  already trusts, then convicts at once.
- Strikes (`hash_fail_ban`) remain the fallback in three cases: the
  piece had a single sender, the blocks could not be read, or the
  tracking table (256 pieces) is full. `Config::smart_ban = false`
  restores pure strikes.
- Bans are session-wide. `Alert::PeerBanned` puts the address in the
  session's ban list and sends `Command::BanIp` to every other torrent.
  New torrents start with the list (`TorrentState::with_banned_ips`).
  The listener refuses banned addresses before the handshake.
  `Session::ban_ip`, `unban_ip` and `banned_ips` give applications the
  same control.

### 3. Connection cap

`dravity_core::admit` holds the single decision (`admit_permitted(max,
live)`) and a lock-free `ConnectionLimit` that hands out RAII
`ConnectionSlot`s. It lives in `dravity-core` (no tokio), so it can be
built on loom's atomics under `--cfg loom`. `tests/loom_admit.rs`
explores every interleaving of contending admits and releases (CI job
`loom`). The session owns one limit (`SessionConfig::max_connections`,
default 500). The TCP and uTP accept paths take a slot before the
handshake and close the connection when none is free. The slot then
moves with the connection (`Accepted::slot`) for its lifetime. Outgoing
dials take a slot before connecting. When none is free the session
answers the engine's `Effect::Connect` with
`Event::PeerConnectDeferred`. The engine keeps the address without blame
and retries after `DEFERRED_DIAL_RETRY`. It does not start the next dial
in the same step, which would only be deferred as well. The cap can be
changed at runtime (`Session::set_max_connections`). Lowering it never
closes a connection.

### 4. Hash pool

`dravity-session/src/hash_pool.rs` replaces the per-torrent semaphore
with a single `HashPool` that the session owns and hands to each torrent
through `TorrentContext`. It bounds jobs with a FIFO `tokio::Semaphore`,
so torrents take turns. The jobs run on Tokio's blocking pool. irontide
runs its own threads with `catch_unwind`. Dravity gets the same
isolation from `spawn_blocking`'s `JoinError`: a panicking job reports
`StorageFailed` and frees its slot. Dedicated threads would duplicate
the blocking pool that storage I/O already uses.
