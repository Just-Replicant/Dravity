# Phase 5 — Full terminal UI: notes and decisions

Scope (plan §8): the Phase 4 `--tui` screen becomes a client. It adds a
magnet or a `.torrent`, removes a torrent, and shows peers, trackers,
and alerts. Drawing is a pure function of a view model. The TUI sends
session commands and reads alerts. It does not poll sockets or call
`update`.

Phase 6 added the encrypted-peer flag. Keys for the current screen are
in the README.

## Decisions

- Add uses the same `load` as `dravity download` (on the blocking pool),
  then `Session::add_magnet` or `Session::add_torrent`. The UI has no
  bencode parser. A bad URI, an unreadable file, or a session error stays
  under the input and leaves the prompt open.
- `download --tui` with one torrent still exits at `Finished` unless
  `--seed`. Once another torrent exists, it exits only when every torrent
  is finished, or on `q`. `serve --tui` never auto-exits; `q` shuts the
  session down.
- Peers are a snapshot. The engine copies each connected peer into
  `PeerInfo` (address, direction, pieces, payload totals, choke and
  interest). The TUI derives rates from two snapshots. The session adds
  the transport seen at connect time. Columns: address, availability
  (`?` before a magnet's metadata), rates, and flags `C`/`c` (choke),
  `I`/`i` (interest), `<` (incoming), `U` (uTP).
- Trackers show the last outcome the owner saw: error text, or interval,
  seeders, leechers, and peer count. The list follows announce order. A
  tracker never asked shows as `waiting`. The TUI does not call a
  tracker. The final `stopped` announce is not recorded.
- `Session::subscribe` fills a ring of 32 alerts per torrent. A shown
  alert redraws at once. Logged: state changes, metadata, finished, hash
  failures, tracker errors, bans, fatal errors. The table is rebuilt from
  `list` plus `stats`, because paused torrents emit no `Alert::Stats`. A
  lagging receiver reports how many alerts it missed.
- One refresh asks every running torrent at once, plus the selected
  torrent's progress, peers, and trackers. The frame waits at most 500 ms,
  draws what answered, and keeps the previous numbers for the rest.
  Unanswered requests are aborted. Rates are the difference of
  `downloaded` and `uploaded` between two samples that arrived.
- Sort and filter are view state. `o` cycles added, name, progress, and
  down rate. `/` filters names. The selection is an info-hash, so a
  hidden row is not selectable and clearing the filter restores it.
- `d` confirms removal. `D` confirms removal and file deletion. Pause,
  resume, and priorities stay immediate. On the peers pane, `a` takes one
  `host:port` and calls `TorrentHandle::add_peers`.
- Printable keys, Backspace, Enter, and Esc are `Action`s. `View::update`
  applies an action and returns the `Request` the session must perform,
  so the key path is tested without a session. A left click selects a
  torrent row, or a file row while the files pane shows. Mouse capture
  is switched off on exit.
- Torrents added from a `.torrent` file pass their verified `info` bytes
  to the engine, so a Dravity seed answers BEP 9.

## Left for later

A download directory in the add prompt, live rate limits, a piece map,
and peer client strings. HTTP routes for peers and trackers were left
until after these session reads. Mouse buttons and wheel scrolling stay
out of scope.
