# Phase 5 — Full terminal UI: plan

Scope: turn the Phase 4 `--tui` viewer into a client for one session.
The screen can add a magnet or a `.torrent`, remove a torrent, and show
peers, trackers and alerts. Drawing stays a pure function of a view
model. The TUI sends session commands and reads alerts; it never polls
sockets or calls `update` (plan §4).

**Not in this phase:** playing a file inside the terminal, BEP 52, web
seeds, an io_uring backend, or changing rate limits on a torrent that
is already running. The protocol items stay on the roadmap as phase 6.

## What Phase 4 already does

`--tui` (ratatui + crossterm, in `dravity-cli`) draws one screen:
torrent table, a gauge for the selection, and that torrent's files.
Keys move the selection, pause or resume, and set a file's priority.
`download --tui` exits when that torrent finishes; `serve --tui` stays
until `q`, then shuts the session down. An empty session tells you to
use the API or the watch folder. There is no way to hand the running
UI a magnet.

Refresh asks each torrent for `stats()` one after another, each with a
500 ms cap, so a stuck owner stretches the whole frame.

## Crates

| Crate | I/O? | Phase 5 additions |
|---|---|---|
| `dravity-engine` | no | A peer snapshot the owner can copy out (address, progress, rates, choke and interest flags). No new effects |
| `dravity-session` | async | `TorrentHandle::peers` and `TorrentHandle::trackers`; the owner remembers the last announce outcome per tracker URL |
| `dravity-cli` | via session | Screens and keys below. `download --tui` and `serve --tui` both accept a magnet or a path while running |

`dravity-http` is unchanged. Anything the TUI can do, the API can
already do, except the two new reads (peers, trackers). Those can be
added to the API later; this phase does not grow routes.

## Screens

One full-screen UI, four modes. The view model holds which mode is up,
so tests draw a mode by filling a `View` and never open a terminal.

1. **Torrents.** The Phase 4 table, plus ratio (`uploaded / downloaded`,
   `—` when nothing has been downloaded) and ETA (`wanted_left` over
   the measured down rate, `—` when the rate is 0 or the torrent is
   paused or finished). The header is one line of session facts already
   on `Session`: torrent count, listen port, DHT on or off, and the
   mapped external port when `port_mapping()` has one. Totals for down
   and up are the sum of the rows.
2. **Add.** An input over the torrents screen. The line is a `magnet:`
   URI or a path to a `.torrent` file. Enter adds it into the session's
   download directory (`--output` for `download`, `--download-dir` for
   `serve`) via `add_magnet` or `add_torrent`. Esc closes the input and
   changes nothing. A bad URI, an unreadable file, or a session error
   stays in the footer and leaves the input open.
3. **Detail.** The selected torrent. Three panes, switched by keys, not
   by a new screen stack: **Files** (the Phase 4 list), **Peers**,
   **Trackers**. A short alert log for that info-hash sits under the
   panes (the last few `SessionAlert`s: state, metadata, finished,
   hash failure, tracker error, ban, fatal error).
4. **Confirm.** Remove from the session, or remove and delete files.
   The prompt names the torrent. Enter confirms, Esc cancels. Nothing
   is deleted on the first key.

Help is the footer, which already follows focus. `?` replaces the
footer with the full key list until the next key.

## Decisions

- **Add is a CLI parse plus a session call.** A line starting with
  `magnet:` goes to `Session::add_magnet`. Anything else is a filesystem
  path: the CLI reads and parses the `.torrent` the way `download`
  already does, then calls `Session::add_torrent`. The UI does not grow
  a bencode parser of its own. The download directory is the one the
  process was started with; picking another directory is a later
  change to `AddTorrentOptions` from the prompt, not a second default.
- **`download --tui` stays up once a second torrent exists.** With a
  single torrent it still exits at `Finished` unless `--seed`. After
  the user adds another, it exits only when every torrent is finished
  (or `q`). `serve --tui` never auto-exits; `q` still shuts the session
  down, as it does today.
- **Peers are a snapshot, not a subscription.** The engine copies the
  fields it already keeps per peer into a small owned struct.
  `TorrentHandle::peers` is a oneshot command, the same shape as
  `stats`. Columns: address, percent they have, down and up rates since
  the previous snapshot, and flags (we choke them, they choke us,
  interested, they are interested, incoming, uTP, encrypted if the
  session knows). No client-identification string until the engine
  stores the peer id for a reason of its own.
- **Trackers are the last outcome the owner saw.** When an announce
  task finishes, the owner records the URL, whether it failed, the
  error text or the interval, and seeders and leechers when the
  response carries them. `TorrentHandle::trackers` returns that record.
  The TUI does not call a tracker.
- **Alerts update the log; numbers still come from a refresh.**
  `Session::subscribe` appends to a ring (32 per torrent, dropped
  oldest) as events arrive, so a tracker error shows up before the
  next poll. `Alert::Stats` is not a substitute for the table: paused
  torrents emit nothing. The table is rebuilt from `list` plus `stats`.
- **One deadline for the whole refresh.** Every running torrent is
  asked at once. The frame waits at most 500 ms, then draws what
  answered and keeps the previous numbers for the rest. Same rule as
  `/metrics`: one stuck owner cannot hide the others or freeze the UI.
  Rates stay the difference of `downloaded` and `uploaded` between two
  samples that actually arrived.
- **Sort and filter are view state.** `o` cycles the torrent sort
  (added, name, progress, down rate) and does not reorder the session.
  `/` filters rows by a case-insensitive substring of the name; a
  hidden row is not selectable. Clearing the filter restores the
  selection by info-hash.
- **Destructive keys ask first.** `d` confirms removal
  (`Session::remove` with `delete_files: false`). `D` confirms removal
  and deletion (`delete_files: true`), which already refuses to follow
  symlinked directories. Pause, resume, and priorities stay immediate,
  as in Phase 4.
- **Adding a peer is the existing call.** On the peers pane, `a` asks
  for `host:port` and calls `TorrentHandle::add_peers`. One address;
  a list can wait.
- **Text entry is part of `Action`.** Printable keys append, Backspace
  deletes, Enter submits, Esc cancels. The key thread and the
  `action` function stay the only input path. Resize redraws on the
  next loop without waiting for the refresh tick.
- **Mouse selects a row and does not add actions.** A click on a
  torrent or file row moves the selection. Buttons, dragging, and
  scrolling by wheel are out of scope; every command has a key.
- **The empty state tells you the key.** "No torrents. `a` adds a
  magnet or a .torrent file." The watch folder and the API still work
  beside the UI; they are no longer the only way in.

## Keys

| Key | Torrents | Files | Peers | Trackers | Add / confirm |
|---|---|---|---|---|---|
| `q`, Esc, Ctrl-C | quit the UI | quit | quit | quit | Esc cancels; Ctrl-C still quits |
| `a` | open add | — | add one peer | — | — |
| Enter | open detail | — | — | — | submit |
| Esc | back from detail | back | back | back | cancel |
| ↑↓, `j`/`k` | move | move | move | move | — |
| Tab | files pane | torrents | next pane | next pane | — |
| `1` `2` `3` | — | files, peers, trackers | same | same | — |
| Space, `p` | pause / resume | — | — | — | — |
| `s` `l` `n` `h` | — | that file's priority | — | — | — |
| `d` / `D` | confirm remove / remove and delete | — | — | — | — |
| `o` | cycle sort | — | — | — | — |
| `/` | filter by name | — | — | — | — |
| `?` | key list | key list | key list | key list | — |

## Tests

- Draw tests stay on `TestBackend`: empty session names `a`; a magnet
  in the add buffer is visible; confirm names the torrent; peers and
  trackers render from the view model; the footer matches the pane.
- `action` tests: add opens and submits; Esc cancels; `d` arms confirm
  and Enter is the only key that removes; priority keys do nothing
  while the peers pane is focused; filter hides a row.
- One loopback session test through the public API the UI calls:
  paste a magnet (metadata from a local peer, as the session tests
  already do), see it listed, pause it, remove it. Peer and tracker
  rows are unit-tested from a filled `View`; a live swarm is not
  required to prove the widgets.

## Deferred

| Item | When |
|---|---|
| Choose a download directory in the add prompt | later, still `AddTorrentOptions` |
| Change rate limits on a running torrent | when the session can retune one; the TUI only displays the limits the process was started with |
| Piece map, peer client strings | when the session already stores them |
| HTTP routes for peers and trackers | after the session reads exist |
| Mouse buttons and wheel scrolling | out of scope |
