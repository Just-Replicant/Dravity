# Phase 5 — Full terminal UI: notes and decisions

Scope from the roadmap (plan §8): turn the Phase 4 `--tui` viewer into a
client for one session. The screen adds a magnet or a `.torrent`, removes
a torrent, and shows peers, trackers and alerts. Drawing stays a pure
function of a view model. The TUI sends session commands and reads
alerts; it never polls sockets or calls `update` (plan §4).

**Not in this phase:** playing a file inside the terminal, BEP 52, web
seeds, an io_uring backend, or changing rate limits on a torrent that
is already running. The protocol items stay on the roadmap as phase 6.

## Where Phase 4 left off

`--tui` (ratatui + crossterm, in `dravity-cli`) drew one screen: torrent
table, a gauge for the selection, and that torrent's files. Keys moved
the selection, paused or resumed, and set a file's priority. There was
no way to hand the running UI a magnet, and a refresh asked each torrent
for `stats()` one after another, each with a 500 ms cap, so a stuck owner
stretched the whole frame.

## Crates

| Crate | I/O? | Phase 5 additions |
|---|---|---|
| `dravity-engine` | no | `PeerInfo`, `TorrentState::peers`, `MetadataState::peers`: a copy of each connected peer (address, direction, pieces, payload totals, choke and interest flags). No new events or effects |
| `dravity-session` | async | `TorrentHandle::peers` (`PeerStatus`, `Transport`) and `TorrentHandle::trackers` (`TrackerStatus`, `TrackerOutcome`); the owner remembers the last announce outcome per tracker URL. Fix: torrents added from a file now serve their metadata over BEP 9 |
| `dravity-cli` | via session | The TUI is a module tree: `tui::view` (model and the state changes keys make), `tui::keys` (key → `Action`), `tui::draw` (rendering), `tui` (loop). `download --tui` and `serve --tui` both accept a magnet or a path while running |

`dravity-http` is unchanged. Anything the TUI can do, the API can
already do, except the two new reads (peers, trackers). Those can be
added to the API later; this phase does not grow routes.

## Screen

One full-screen UI (abridged below). The view model holds the focus, the detail pane, an
open prompt and the help flag, so tests draw any state by filling a
`View` and never open a terminal.

```text
 dravity — 2 torrents · port 6881 · DHT on · mapped 40000 · ↓ 512.0 KiB/s ↑ 0 B/s
┌ Torrents · sort: added ─────────────────────────────────────────────────┐
│Name        State        Done   Size     Down        Up      Ratio ETA  Peers│
│debian.iso  downloading  25.0%  4.0 MiB  512.0 KiB/s 0 B/s   0.00  6s   3    │
└─────────────────────────────────────────────────────────────────────────┘
┌ Selected ───────────────────────────────────────────────────────────────┐
│███████ 1.0 MiB of 4.0 MiB · 3.0 MiB left wanted · up 0 B · ratio 0.00 … │
└─────────────────────────────────────────────────────────────────────────┘
┌ 1 Files │ 2 Peers │ 3 Trackers ───────────────────────────────────────┐
│Priority Done    Size       Path                                         │
└─────────────────────────────────────────────────────────────────────────┘
┌ Alerts ─────────────────────────────────────────────────────────────────┐
│state: downloading                                                       │
└─────────────────────────────────────────────────────────────────────────┘
q quit · a add · enter detail · space pause/resume · d/D remove · o sort · / filter · ? keys
```

1. **Torrents.** The Phase 4 table plus ratio (`uploaded / downloaded`,
   `—` when nothing has been downloaded) and ETA (`wanted_left` over the
   measured down rate, `—` when the rate is 0 or the torrent is paused or
   not downloading). A magnet still fetching its metadata shows `-` for
   done and size rather than a full bar. The header is one line of session
   facts: torrent count, listen port, DHT on or off, the mapped external
   port when `port_mapping()` has one, and the down and up totals (the sum
   of the rows).
2. **Add.** A popup over the torrents screen. The line is a `magnet:`
   URI or a path to a `.torrent` file. Enter adds it into the session's
   download directory (`--output` for `download`, `--download-dir` for
   `serve`). Esc closes it and changes nothing. A bad URI, an unreadable
   file, or a session error (e.g. a duplicate) stays under the input and
   leaves it open.
3. **Detail.** The selected torrent's **Files** (the Phase 4 list),
   **Peers** or **Trackers**, switched by keys in one block whose title
   names the panes. A short alert log for the selected torrent sits under
   it.
4. **Confirm.** Remove from the session, or remove and delete files.
   The prompt names the torrent. Enter confirms, Esc cancels, every other
   key is ignored: nothing is deleted on the first key.

`?` replaces the footer with the full key list (including the peer flag
legend) until the next key, which only closes it.

## Decisions

- **Add is a CLI parse plus a session call.** The line goes through the
  same `load` as `dravity download` (on the blocking pool, since it may
  read a file), then `Session::add_magnet` or `Session::add_torrent`. The
  UI has no bencode parser of its own. The download directory is the one
  the process started with; choosing another one is a later change to
  `AddTorrentOptions` from the prompt.
- **`download --tui` stays up once a second torrent exists.** With a
  single torrent it still exits at `Finished` unless `--seed`. Once the
  user has added another, it exits only when every torrent is finished
  (or `q`). `serve --tui` never auto-exits; `q` shuts the session down.
- **Peers are a snapshot, not a subscription.** The engine copies what
  it already keeps per peer into `PeerInfo`. Byte counters are payload
  totals since the peer connected (two new fields beside the per-round
  counters the choker resets), so the TUI derives down and up rates from
  two snapshots, exactly as it does for torrents. The session adds the
  transport it saw at connect time (TCP or uTP). `TorrentHandle::peers`
  is a oneshot command shaped like `stats`. Columns: address, how much
  the peer has (`?` before a magnet's metadata), rates, and flags
  `C` we choke them, `c` they choke us, `I` we are interested, `i` they
  are interested, `<` incoming, `U` uTP. There is no "encrypted" flag:
  the session does not speak MSE/PE. No client strings until the engine
  stores the peer id for a reason of its own.
- **Trackers are the last outcome the owner saw.** Announce tasks now
  hand the owner, besides the merged engine event, what each tracker they
  asked said: the error text, or interval, seeders, leechers and peer
  count. `TorrentHandle::trackers` lists every tracker in announce order
  (BEP 12 tiers, the one that last answered first in its tier) with that
  record; one never asked (a tier stops at its first answer) shows as
  `waiting`. The TUI does not call a tracker. The final `stopped`
  announce is not recorded: nobody can read it once the task ends.
- **Alerts update the log; numbers still come from a refresh.**
  `Session::subscribe` feeds a ring (32 per torrent, oldest dropped) as
  alerts arrive, and a shown alert redraws at once, so a tracker error
  appears before the next poll. Logged: state changes, metadata, finished,
  hash failures, tracker errors, bans, fatal errors. `Alert::Stats` is not
  a substitute for the table (paused torrents emit nothing); the table is
  rebuilt from `list` plus `stats`. A lagging receiver says how many
  alerts it missed in the footer.
- **One deadline for the whole refresh.** Every running torrent is asked
  at once (a `JoinSet`), plus the selected torrent's progress, peers and
  trackers. The frame waits at most 500 ms, then draws what answered and
  keeps the previous numbers for the rest; unanswered requests are
  aborted. Same rule as `/metrics`. Rates stay the difference of
  `downloaded` and `uploaded` between two samples that actually arrived.
- **Sort and filter are view state.** `o` cycles the torrent sort
  (added, name, progress, down rate) and does not reorder the session.
  `/` filters rows by a case-insensitive substring of the name as you
  type; Enter keeps the filter, Esc clears it. The selection is an
  info-hash: a hidden row is not selectable (the first visible row acts
  as selected), and clearing the filter restores the selection.
- **Destructive keys ask first.** `d` confirms removal
  (`Session::remove` with `delete_files: false`). `D` confirms removal
  and deletion (`delete_files: true`), which already refuses to follow
  symlinked directories. Pause, resume, and priorities stay immediate,
  as in Phase 4.
- **Adding a peer is the existing call.** On the peers pane, `a` asks
  for `host:port` (an IP literal, or a name resolved with the system
  resolver) and calls `TorrentHandle::add_peers`. One address; a list
  can wait.
- **Text entry is part of `Action`.** Printable keys append, Backspace
  deletes, Enter submits, Esc cancels; Ctrl-C quits from anywhere. Keys
  map to an `Action` (`tui::keys`), and `View::update` applies it and
  returns the `Request` the session must perform, so the key path is
  testable without a session. Resize redraws on the next loop without
  waiting for the refresh tick.
- **Mouse selects a row and does not add actions.** A left click on a
  torrent row, or on a file row while the files pane shows, moves the
  selection. Clicks map to rows through the same layout function the
  drawing uses. Buttons, dragging, and wheel scrolling are out of scope;
  every command has a key. Mouse capture is switched off again on exit.
- **The empty state tells you the key.** "No torrents. `a` adds a
  magnet or a .torrent file." The watch folder and the API still work
  beside the UI.
- **A seed serves its metadata.** Testing the paste-a-magnet path against
  a session seed showed that torrents added from a `.torrent` file never
  handed their `info` dictionary to the engine, so they rejected every
  BEP 9 request: magnet users could not start from a Dravity seed. The
  owner now passes the verified `info` bytes for every torrent.

## Keys

| Key | Torrents | Files | Peers | Trackers | Add / filter / confirm |
|---|---|---|---|---|---|
| `q` | quit the UI | quit | quit | quit | typed (confirm: ignored) |
| Ctrl-C | quit | quit | quit | quit | quit |
| Esc | quit the UI | back | back | back | cancel (filter: clear) |
| `a` | open add | — | add one peer | — | — |
| Enter | open detail | — | — | — | submit |
| ↑↓, `j`/`k` | move | move | move | move | — |
| Tab | files pane | torrents | trackers | files | — |
| `1` `2` `3` | files, peers, trackers | same | same | same | — |
| Space, `p` | pause / resume | — | — | — | — |
| `s` `l` `n` `h` | — | that file's priority | — | — | — |
| `d` / `D` | confirm remove / remove and delete | — | — | — | — |
| `o` | cycle sort | — | — | — | — |
| `/` | filter by name | — | — | — | — |
| `?` | key list | key list | key list | key list | — |

## Tests

- Engine: `tests/peers.rs` (flags, totals and ordering of a torrent's
  peers; a magnet's peers before the size is known).
- Session: tracker log ordering and replacement (unit); loopback
  `peers_are_listed_with_their_transport` (an incoming TCP and an
  incoming uTP peer), tracker outcomes in `tier_falls_back_to_the_next_tracker`,
  and `a_session_serves_metadata_to_a_magnet_link` (the BEP 9 fix).
- CLI, drawing on `TestBackend`: the empty session names `a`; a magnet in
  the add buffer is visible with the last error; confirm names the
  torrent; peers and trackers render from the view model; a magnet
  without metadata shows no sizes; the footer follows the pane; help;
  filter; alert log; click-to-row mapping.
- CLI, `action` and `View::update`: add opens, takes text and submits
  without closing; Esc cancels; `d` arms confirm and Enter is the only key
  that removes; priority keys do nothing on the peers pane; the filter
  hides a row and clearing it restores the selection; sort, Tab and pane
  keys; add-peer needs the peers pane.
- CLI, one loopback session test through the calls the UI makes: a bad
  magnet and a missing file keep the prompt open; paste a magnet whose
  metadata comes from a local session seed, see it listed and finished
  with its file, add a peer from the peers pane, see the alert log, pause
  it, remove it with its files.

Not covered: terminals other than an xterm-compatible pseudo-terminal
(checked by hand under a PTY: add, panes, help, confirm, remove, quit,
mouse capture on and off).

## Deferred

| Item | When |
|---|---|
| Choose a download directory in the add prompt | later, still `AddTorrentOptions` |
| Change rate limits on a running torrent | when the session can retune one; the TUI only displays the limits the process was started with |
| Piece map, peer client strings | when the session already stores them |
| Encryption flag on peers | with protocol encryption (phase 6, if required) |
| HTTP routes for peers and trackers | after the session reads exist (they do now) |
| Mouse buttons and wheel scrolling | out of scope |
