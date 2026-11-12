# Phase 4 — Product surface: notes and decisions

Scope (plan §8): session persistence, `dravity-http`, a first TUI, file
priorities, streaming, a watch folder, and Prometheus. Also BEP 6
`suggest` for picking, and announcing a mapped port that differs from
the listen port.

`dravity-http` is a JSON API. It serves no static files. Routes are
documented on [`dravity-http`](../crates/dravity-http/src/lib.rs).

## Decisions

- The engine stores priorities per piece (`skip` < `low` < `normal` <
  `high`); the session sets them per file. The picker orders by priority
  rank, then availability, then piece index. A piece takes the highest
  priority of the non-padding files it overlaps, so a skipped file beside
  a wanted one still gets its shared boundary pieces. A piece already
  started is finished before a higher-priority piece begins. Partly
  downloaded pieces of a skipped file are not continued.
- `Finished` means every wanted piece is present. Trackers are told
  `completed` only when every piece is in, once. A partial download that
  does not seed announces `stopped`. Raising a priority or marking a
  piece urgent returns the torrent to `Downloading`.
- `FileStream` reads verified data. A read of a missing piece registers a
  waiter and marks `readahead` (8) pieces from that position urgent.
  Urgent pieces are picked before everything else, including skipped
  ones. Seeking moves the window. Several streams combine theirs.
  Dropping a stream clears it.
- `state_dir` keeps two files per torrent: a `.torrent` rebuilt from the
  verified `info` bytes, and a bencoded `.state` (directory, priorities,
  paused, added-at, and the magnet while metadata is unknown). Piece
  progress stays in fastresume. One writer applies saves and removals in
  order with atomic renames. Restored files are capped (1 MiB state, 64
  MiB torrent). A broken pair is reported in `RestoreReport::failed` and
  skipped.
- The watch folder polls (default every 2 s). A file is read only after
  it looked the same twice. Handled files are renamed `.added` or
  `.invalid`.
- `remove(…, delete_files: true)` deletes the torrent's files under the
  download directory, refuses to descend through symlinked directories,
  and removes directories it emptied, deepest first.
- `dravity-http` uses only the public session API. It binds
  `127.0.0.1:9091` by default. The bearer token is compared in constant
  time. Bodies are capped at 16 MiB. Unknown JSON fields are rejected.
  A non-loopback bind without a token is refused unless
  `allow_insecure_unauthenticated` is set. There is no TLS.
- One byte range is honored (`206`, or `416` with
  `Content-Range: bytes */len`). Several ranges or a malformed `Range`
  send the whole file. `HEAD` returns the same status and headers as
  `GET` and no body.
- `/metrics` reads the session when scraped and writes the text format
  directly. Every torrent is asked at once. The scrape stops after 2
  seconds; a torrent that has not answered is listed without its live
  counters.
- Server-sent events carry alerts as JSON (`event:` is the alert type).
  A slow client gets a `lagged` event instead of stalling the session.
  Streams end on shutdown.
- The TUI (ratatui) lists torrents and the selected torrent's files, and
  can pause, resume, and set priorities. Rendering is a pure function of
  a view model. Phase 5 turned that screen into a client.
- When NAT-PMP maps a different public TCP port, trackers and the DHT are
  told `Status::external_port`. Otherwise they are told the listen port.
- Each peer keeps its 16 most recent `suggest` pieces. The picker prefers
  those among the best priority class.

## Left for later

Phase 5 took the full TUI. Later work took PCP, the uTP tail-loss probe,
bans beyond bad pieces, UPnP external-port readback, and session-wide
rate limits. API TLS stays out of scope; use a reverse proxy.
