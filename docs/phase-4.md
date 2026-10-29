# Phase 4 — Product surface: notes and decisions

Scope from the roadmap (plan §8): session persistence; `dravity-http`;
TUI/Web UI; streaming seek; file priorities; watch folder; Prometheus.
Also picks up two Phase 3 deferrals: BEP 6 `suggest` used for picking, and
announcing a mapped port that differs from the listen port.

**Not in this phase: the Web UI.** `dravity-http` is a JSON API only; it
serves no static files. A browser UI can be built on the API later without
changing it (see [Deferred](#deferred)).

## Crates

| Crate | I/O? | Phase 4 additions |
|---|---|---|
| `dravity-engine` | no | `Priority` (`skip` < `low` < `normal` < `high`), per-piece priorities in the picker, urgent pieces, `Event::SetPriorities`/`SetUrgent`, `Stats::wanted_left`, BEP 6 `suggest` hints |
| `dravity-portmap` | async | `Status::external_port` |
| `dravity-session` | async | File priorities, `TorrentHandle::progress`, `FileStream` (`open_file`), catalog of paused and running torrents (`list`, `pause`, `resume`, `remove`), `SessionConfig::state_dir` + `restore`, watch folder, mapped-port announces |
| `dravity-http` (new) | async | axum router over the session: torrents, files, streaming with `Range`, server-sent alerts, Prometheus `/metrics`, bearer token |
| `dravity-cli` | via session | `dravity serve`, `--files`/`--file-priority`, `--tui` |

## Decisions

- **Priorities are per piece in the engine, per file in the session.** The
  engine knows pieces, not files: `Picker` keys its queue by (priority
  rank, availability, piece), so high pieces go first and rarest-first
  still orders pieces of equal priority. A piece takes the highest priority
  of the non-padding files it overlaps, so a skipped file next to a wanted
  one still gets its shared boundary pieces (and those bytes are written,
  as in every client). Partly downloaded pieces of a skipped file are not
  continued.
- **"Finished" means every wanted piece.** `TorrentStatus::Finished` and
  `Alert::Finished` fire when nothing wanted is missing. Trackers are told
  `completed` only when *every* piece is in, once. A partial download that
  does not seed announces `stopped` and leaves the swarm. Wanting more
  (raising a priority, an urgent piece) returns it to `Downloading`.
- **Streaming = urgent window + waiters.** `FileStream` reads verified
  data through the storage backend. A read of a missing piece registers a
  waiter with the owner task and marks `readahead` (8) pieces from the
  position urgent; urgent pieces are picked before all others, even
  skipped ones. Seeking moves the window; several streams combine theirs;
  dropping a stream clears it. Reads never return unverified bytes.
- **Persistence keeps two files per torrent** in `state_dir`: a `.torrent`
  rebuilt from the verified `info` bytes (so the info-hash cannot change)
  and a small bencoded `.state` record (directory, priorities, paused,
  added-at, magnet while the metadata is unknown). Piece progress stays in
  fastresume. One writer task applies writes and removals in order with
  atomic renames, so no owner task waits for the disk. Restored files are
  read with size caps (1 MiB state, 64 MiB torrent) and decoded like any
  untrusted input; a broken pair is reported in `RestoreReport::failed`
  and skipped.
- **The watch folder polls** (default every 2 s) instead of using inotify
  and friends: portable, no dependency, and a file is read only after it
  looked the same twice, so half-copied files are not parsed. Handled
  files are renamed `.added` or `.invalid`, which also prevents re-adding.
- **Remove never follows symlinks.** `remove(…, delete_files: true)`
  deletes the torrent's files beneath the download directory, refuses to
  descend through symlinked directories, and removes directories it
  emptied, deepest first.
- **`dravity-http` is a library.** It drives only the public session API,
  like the CLI (plan §4), so `thiserror` applies and `dravity serve`
  embeds it. It binds `127.0.0.1:9091` by default; a bearer token
  (constant-time compared) is optional and recommended before exposing it;
  bodies are capped (16 MiB); unknown JSON fields are rejected. There is
  no TLS: put a reverse proxy in front for remote use.
- **HTTP streaming** honours one byte range (`206`, `416` with
  `Content-Range: bytes */len`); several ranges or malformed headers are
  ignored and the whole file is sent, as RFC 9110 allows. The `Range`
  parser is fuzzed (`http_range`).
- **Prometheus without a registry.** `/metrics` reads the session when
  scraped and writes the text format by hand (escaped labels, `HELP` and
  `TYPE` lines). This avoids a global recorder and an exporter dependency,
  and there is no state to keep in sync. The plan named `metrics` +
  exporter; that remains an option if in-engine counters are added.
- **Server-sent events** carry alerts as JSON (`event:` is the alert
  type). Slow clients get a `lagged` event with the number of alerts they
  missed instead of stalling the session (the broadcast channel is
  bounded). Streams end on server shutdown so graceful shutdown finishes.
- **TUI in the CLI** with ratatui 0.29 + crossterm: torrents, the selected
  torrent's files, pause/resume and priorities. Rendering is a pure
  function of a view model, tested with `TestBackend`. ratatui 0.30 needs
  Rust 1.86 (above the MSRV), and 0.29 pulls in the unmaintained `paste`
  macro crate; `deny.toml` documents that exception.
- **Mapped port.** When NAT-PMP maps a different public TCP port, trackers
  and the DHT are told that port (`Status::external_port`); otherwise the
  listen port, as before.
- **BEP 6 `suggest`** is kept per peer (16 most recent) and preferred by
  the picker among pieces of the best priority class.

## HTTP API

See the [`dravity-http` docs](../crates/dravity-http/src/lib.rs) for the
full table. Examples:

```sh
TOKEN=change-me dravity serve --download-dir ~/Downloads --watch ~/torrents --token "$TOKEN"
curl -H "Authorization: Bearer $TOKEN" localhost:9091/api/v1/torrents
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/x-bittorrent' \
     --data-binary @file.torrent 'localhost:9091/api/v1/torrents?paused=true'
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"magnet":"magnet:?xt=urn:btih:…"}' localhost:9091/api/v1/torrents
curl -H "Authorization: Bearer $TOKEN" -X PUT -H 'Content-Type: application/json' \
     -d '{"priorities":["high","skip"]}' localhost:9091/api/v1/torrents/<hash>/files
mpv --http-header-fields="Authorization: Bearer $TOKEN" \
     localhost:9091/api/v1/torrents/<hash>/files/0/content
curl -N -H "Authorization: Bearer $TOKEN" 'localhost:9091/api/v1/events?stats=false'
curl -H "Authorization: Bearer $TOKEN" localhost:9091/metrics
```

## Tests

- Engine: `tests/priorities.rs` (skip, order by priority, urgent before
  everything, finishing on wanted pieces, `completed` only when complete,
  re-wanting resumes, `suggest`), picker unit tests and a proptest.
- Session: priority mapping and file progress units (padding, boundary
  pieces, symlinked directories on delete), state-record roundtrip,
  rejection and proptests; loopback `tests/product.rs`: skipped files,
  streaming with seek, pause/resume/remove with file deletion, restart
  restore, watch folder. Mapped-port selection.
- HTTP: `tests/api.rs` with `tower::ServiceExt::oneshot` against real
  loopback sessions (add paused, priorities, resume, peers, ranged and
  whole streaming, `416`, metrics, pause, delete, errors, token, SSE,
  graceful shutdown with an open event stream); `Range` proptests.
- CLI: TUI rendering and key mapping, file selection parsing, `serve`
  (token, watch folder, metrics, restart restore over SIGINT).
- Fuzz: `session_state`, `http_range`.

Not covered: real media players against `/content` beyond HTTP semantics,
and terminals other than an xterm-compatible pseudo-terminal.

## Deferred

| Item | Phase |
|---|---|
| Web UI (static files served by `dravity-http`) | later, on top of the API |
| PCP (RFC 6887), IPv6 pinholes | 5 |
| uTP tail-loss probe; path MTU discovery | 5 |
| Peer banning by other evidence (bad messages, ban lists) | 5 |
| UPnP external-port readback (the device is not asked which port it chose) | 5 |
| Global (session-wide) rate limits and queueing (max active torrents) | 5 |
| TLS for the API | out of scope: use a reverse proxy |
