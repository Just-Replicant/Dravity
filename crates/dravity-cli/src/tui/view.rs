//! The view model: everything the screen shows, and the state changes keys
//! make before anything is sent to the session. Drawing reads a [`View`];
//! tests fill one by hand and never open a terminal.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

use dravity_core::InfoHash;
use dravity_engine::{Alert, Priority, Stats, TorrentStatus};
use dravity_session::{FileProgress, PeerStatus, TrackerOutcome, TrackerStatus, Transport};

use crate::format;

/// Alerts kept per torrent; the oldest is dropped first.
pub(crate) const ALERT_LOG: usize = 32;

/// Session facts for the header line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Header {
    /// Port the session accepts peers on.
    pub(crate) listen_port: Option<u16>,
    /// The session runs a DHT node.
    pub(crate) dht: bool,
    /// The router's public TCP port, when port mapping got one.
    pub(crate) mapped_port: Option<u16>,
}

/// One torrent as shown.
#[derive(Debug, Clone)]
pub(crate) struct TorrentRow {
    pub(crate) info_hash: InfoHash,
    pub(crate) name: String,
    pub(crate) paused: bool,
    pub(crate) running: bool,
    /// Live numbers, while running (the last ones that arrived).
    pub(crate) stats: Option<StatsRow>,
    /// Bytes per second.
    pub(crate) down_rate: u64,
    /// Bytes per second.
    pub(crate) up_rate: u64,
}

impl TorrentRow {
    /// A display state.
    pub(crate) fn state(&self) -> &'static str {
        if self.paused {
            "paused"
        } else if let Some(stats) = &self.stats {
            format::status(stats.status)
        } else if self.running {
            "starting"
        } else {
            "stopped"
        }
    }

    /// Completion in tenths of a percent (0 without numbers, or while a
    /// magnet's size is unknown).
    pub(crate) fn permille(&self) -> u64 {
        self.stats
            .filter(StatsRow::size_known)
            .map_or(0, |s| format::permille(s.bytes_have, s.total_length))
    }

    /// Time to finish the wanted pieces at the measured rate; `—` when the
    /// rate is 0 or the torrent is paused or finished.
    pub(crate) fn eta(&self) -> String {
        match self.stats {
            Some(s) if !self.paused && s.status == TorrentStatus::Downloading => {
                format::eta(s.wanted_left, self.down_rate)
            }
            _ => "—".to_owned(),
        }
    }

    /// Whether every wanted piece is in.
    pub(crate) fn is_finished(&self) -> bool {
        self.stats
            .is_some_and(|s| s.status == TorrentStatus::Finished)
    }
}

/// The numbers shown for a running torrent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StatsRow {
    pub(crate) status: TorrentStatus,
    pub(crate) total_length: u64,
    pub(crate) bytes_have: u64,
    pub(crate) wanted_left: u64,
    pub(crate) downloaded: u64,
    pub(crate) uploaded: u64,
    pub(crate) peers: usize,
}

impl StatsRow {
    /// Sizes mean something: not a magnet still fetching its metadata.
    pub(crate) fn size_known(&self) -> bool {
        self.status != TorrentStatus::FetchingMetadata
    }
}

impl From<&Stats> for StatsRow {
    fn from(s: &Stats) -> Self {
        Self {
            status: s.status,
            total_length: s.total_length,
            bytes_have: s.bytes_have,
            wanted_left: s.wanted_left,
            downloaded: s.downloaded,
            uploaded: s.uploaded,
            peers: s.peers,
        }
    }
}

/// One file as shown (padding files are left out).
#[derive(Debug, Clone)]
pub(crate) struct FileRow {
    pub(crate) index: usize,
    pub(crate) path: String,
    pub(crate) length: u64,
    pub(crate) have: u64,
    pub(crate) priority: Priority,
}

impl From<&FileProgress> for FileRow {
    fn from(f: &FileProgress) -> Self {
        Self {
            index: f.index,
            path: f.path.display().to_string(),
            length: f.length,
            have: f.have,
            priority: f.priority,
        }
    }
}

/// One connected peer as shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeerRow {
    pub(crate) addr: SocketAddr,
    /// Pieces the peer has and the torrent's piece count (0 and 0 while a
    /// magnet's metadata is unknown).
    pub(crate) pieces_have: u32,
    pub(crate) piece_count: u32,
    /// Payload byte totals (for the next rate sample).
    pub(crate) downloaded: u64,
    pub(crate) uploaded: u64,
    /// Bytes per second since the previous snapshot.
    pub(crate) down_rate: u64,
    pub(crate) up_rate: u64,
    pub(crate) am_choking: bool,
    pub(crate) peer_choking: bool,
    pub(crate) am_interested: bool,
    pub(crate) peer_interested: bool,
    pub(crate) incoming: bool,
    pub(crate) utp: bool,
}

impl From<&PeerStatus> for PeerRow {
    fn from(p: &PeerStatus) -> Self {
        let i = &p.info;
        Self {
            addr: i.addr,
            pieces_have: i.pieces_have,
            piece_count: i.piece_count,
            downloaded: i.downloaded,
            uploaded: i.uploaded,
            down_rate: 0,
            up_rate: 0,
            am_choking: i.am_choking,
            peer_choking: i.peer_choking,
            am_interested: i.am_interested,
            peer_interested: i.peer_interested,
            incoming: i.incoming,
            utp: p.transport == Transport::Utp,
        }
    }
}

impl PeerRow {
    /// Fixed-position flags: `C` we choke them, `c` they choke us, `I` we
    /// are interested, `i` they are interested, `<` incoming, `U` uTP;
    /// `.` when unset.
    pub(crate) fn flags(&self) -> String {
        [
            (self.am_choking, 'C'),
            (self.peer_choking, 'c'),
            (self.am_interested, 'I'),
            (self.peer_interested, 'i'),
            (self.incoming, '<'),
            (self.utp, 'U'),
        ]
        .iter()
        .map(|&(on, c)| if on { c } else { '.' })
        .collect()
    }

    /// How much the peer has, as a percentage (`?` before the metadata).
    pub(crate) fn have(&self) -> String {
        if self.piece_count == 0 {
            return "?".to_owned();
        }
        let p = format::permille(u64::from(self.pieces_have), u64::from(self.piece_count));
        format!("{}.{}%", p / 10, p % 10)
    }
}

/// What a tracker last said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrackerState {
    /// Not asked yet.
    Waiting,
    Ok {
        interval: Option<Duration>,
        seeders: Option<u32>,
        leechers: Option<u32>,
        peers: usize,
    },
    Failed(String),
}

/// One tracker as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackerRow {
    pub(crate) tier: usize,
    pub(crate) url: String,
    pub(crate) state: TrackerState,
}

impl From<&TrackerStatus> for TrackerRow {
    fn from(t: &TrackerStatus) -> Self {
        let state = match &t.last {
            None => TrackerState::Waiting,
            Some(TrackerOutcome::Ok {
                interval,
                seeders,
                leechers,
                peers,
            }) => TrackerState::Ok {
                interval: *interval,
                seeders: *seeders,
                leechers: *leechers,
                peers: *peers,
            },
            Some(TrackerOutcome::Failed { error }) => TrackerState::Failed(error.clone()),
            Some(_) => TrackerState::Failed("unknown outcome".to_owned()),
        };
        Self {
            tier: t.tier,
            url: t.url.clone(),
            state,
        }
    }
}

/// Which part of the screen has the keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Focus {
    #[default]
    Torrents,
    /// The detail pane of the selected torrent.
    Detail,
}

/// The detail panes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Pane {
    #[default]
    Files,
    Peers,
    Trackers,
}

impl Pane {
    /// Every pane, in key order (`1`, `2`, `3`).
    pub(crate) const ALL: [Self; 3] = [Self::Files, Self::Peers, Self::Trackers];

    const fn next(self) -> Self {
        match self {
            Self::Files => Self::Peers,
            Self::Peers => Self::Trackers,
            Self::Trackers => Self::Files,
        }
    }

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Peers => "Peers",
            Self::Trackers => "Trackers",
        }
    }
}

/// Torrent table order. Changing it never reorders the session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Sort {
    #[default]
    Added,
    Name,
    Progress,
    DownRate,
}

impl Sort {
    const fn next(self) -> Self {
        match self {
            Self::Added => Self::Name,
            Self::Name => Self::Progress,
            Self::Progress => Self::DownRate,
            Self::DownRate => Self::Added,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Name => "name",
            Self::Progress => "progress",
            Self::DownRate => "down rate",
        }
    }
}

/// An open input or question; it takes every key until it closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Prompt {
    /// A magnet link or a `.torrent` path.
    Add(String),
    /// `host:port` of a peer for the selected torrent.
    AddPeer { info_hash: InfoHash, text: String },
    /// Editing [`View::filter`].
    Filter,
    /// Remove a torrent (and maybe its files) once Enter confirms.
    Confirm {
        info_hash: InfoHash,
        name: String,
        delete_files: bool,
    },
}

/// What a key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Quit,
    /// Leave the detail pane.
    Back,
    Up,
    Down,
    /// Tab: torrents → files → torrents; peers → trackers → files.
    Tab,
    /// Jump to a detail pane (`1`, `2`, `3`).
    Pane(Pane),
    /// Enter on the torrent table.
    OpenDetail,
    TogglePause,
    SetPriority(Priority),
    OpenAdd,
    OpenAddPeer,
    ConfirmRemove {
        delete_files: bool,
    },
    CycleSort,
    OpenFilter,
    Help,
    /// Any key while the key list is shown.
    DismissHelp,
    /// Text entry: a printable key.
    Input(char),
    Backspace,
    Submit,
    Cancel,
    None,
}

/// What the session has to do for an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    Quit,
    Pause(InfoHash),
    Resume(InfoHash),
    SetPriority {
        info_hash: InfoHash,
        file: usize,
        priority: Priority,
    },
    /// The add prompt's line; the prompt stays open until it succeeds.
    Add(String),
    /// The peer prompt's line; the prompt stays open until it succeeds.
    AddPeer {
        info_hash: InfoHash,
        text: String,
    },
    Remove {
        info_hash: InfoHash,
        name: String,
        delete_files: bool,
    },
}

/// What the screen shows.
#[derive(Debug, Default)]
pub(crate) struct View {
    pub(crate) header: Header,
    /// Every torrent, in the order the session added them.
    pub(crate) torrents: Vec<TorrentRow>,
    /// The selected torrent. Kept while a filter hides it, so clearing the
    /// filter restores it; `None` selects the first visible row.
    pub(crate) selected: Option<InfoHash>,
    pub(crate) sort: Sort,
    /// Case-insensitive name filter (empty: every row).
    pub(crate) filter: String,
    pub(crate) focus: Focus,
    pub(crate) pane: Pane,
    /// Detail of the torrent in `detail_of` (empty when unknown).
    pub(crate) detail_of: Option<InfoHash>,
    pub(crate) files: Vec<FileRow>,
    pub(crate) selected_file: usize,
    pub(crate) peers: Vec<PeerRow>,
    pub(crate) selected_peer: usize,
    pub(crate) trackers: Vec<TrackerRow>,
    pub(crate) selected_tracker: usize,
    /// Recent alerts per torrent, oldest first.
    pub(crate) alerts: HashMap<InfoHash, VecDeque<String>>,
    pub(crate) prompt: Option<Prompt>,
    /// The footer shows the full key list until the next key.
    pub(crate) help: bool,
    /// Last action's outcome.
    pub(crate) message: String,
}

impl View {
    /// Rows that pass the filter, in the chosen order.
    pub(crate) fn visible(&self) -> Vec<&TorrentRow> {
        let needle = self.filter.to_lowercase();
        let mut rows: Vec<&TorrentRow> = self
            .torrents
            .iter()
            .filter(|t| needle.is_empty() || t.name.to_lowercase().contains(&needle))
            .collect();
        match self.sort {
            Sort::Added => {}
            Sort::Name => rows.sort_by_cached_key(|t| t.name.to_lowercase()),
            Sort::Progress => rows.sort_by_key(|t| std::cmp::Reverse(t.permille())),
            Sort::DownRate => rows.sort_by_key(|t| std::cmp::Reverse(t.down_rate)),
        }
        rows
    }

    /// Index of the selection among the visible rows (the first row when
    /// the selected torrent is hidden or gone).
    pub(crate) fn selected_index(&self) -> usize {
        self.selected
            .and_then(|ih| self.visible().iter().position(|t| t.info_hash == ih))
            .unwrap_or(0)
    }

    /// The selected torrent, if a visible row is selected.
    pub(crate) fn selected_row(&self) -> Option<&TorrentRow> {
        self.visible().get(self.selected_index()).copied()
    }

    /// Selects the visible row at `index` (clamped).
    pub(crate) fn select_index(&mut self, index: usize) {
        let visible = self.visible();
        let index = index.min(visible.len().saturating_sub(1));
        self.selected = visible.get(index).map(|t| t.info_hash);
    }

    /// Torrents across all rows: (down, up) bytes per second.
    pub(crate) fn totals(&self) -> (u64, u64) {
        self.torrents.iter().fold((0, 0), |(d, u), t| {
            (d.saturating_add(t.down_rate), u.saturating_add(t.up_rate))
        })
    }

    /// A prompt or the key list is taking keys, so the UI should stay up.
    pub(crate) fn holds_input(&self) -> bool {
        self.prompt.is_some() || self.help
    }

    /// Recent alerts of the selected torrent.
    pub(crate) fn selected_alerts(&self) -> Option<&VecDeque<String>> {
        self.selected_row()
            .and_then(|row| self.alerts.get(&row.info_hash))
    }

    /// Appends an alert to its torrent's log; returns whether it is one the
    /// log shows (progress snapshots and piece completions are not).
    pub(crate) fn record_alert(&mut self, info_hash: InfoHash, alert: &Alert) -> bool {
        let Some(text) = alert_text(alert) else {
            return false;
        };
        let log = self.alerts.entry(info_hash).or_default();
        if log.len() >= ALERT_LOG {
            log.pop_front();
        }
        log.push_back(text);
        true
    }

    /// Moves the selection of the focused list.
    fn step(&mut self, down: bool) {
        let move_by = |i: usize, len: usize| {
            if down {
                i.saturating_add(1).min(len.saturating_sub(1))
            } else {
                i.saturating_sub(1)
            }
        };
        match (self.focus, self.pane) {
            (Focus::Torrents, _) => {
                let len = self.visible().len();
                let next = move_by(self.selected_index(), len);
                self.select_index(next);
            }
            (Focus::Detail, Pane::Files) => {
                self.selected_file = move_by(self.selected_file, self.files.len());
            }
            (Focus::Detail, Pane::Peers) => {
                self.selected_peer = move_by(self.selected_peer, self.peers.len());
            }
            (Focus::Detail, Pane::Trackers) => {
                self.selected_tracker = move_by(self.selected_tracker, self.trackers.len());
            }
        }
    }

    /// Applies an action to the view; returns what the session must do.
    pub(crate) fn update(&mut self, action: Action) -> Option<Request> {
        match action {
            Action::Quit => return Some(Request::Quit),
            Action::None => {}
            Action::DismissHelp => self.help = false,
            Action::Help => self.help = true,
            Action::Back => self.focus = Focus::Torrents,
            Action::Up => self.step(false),
            Action::Down => self.step(true),
            Action::Tab => match (self.focus, self.pane) {
                (Focus::Torrents, _) => {
                    self.focus = Focus::Detail;
                    self.pane = Pane::Files;
                }
                (Focus::Detail, Pane::Files) => self.focus = Focus::Torrents,
                (Focus::Detail, pane) => self.pane = pane.next(),
            },
            Action::Pane(pane) => {
                self.focus = Focus::Detail;
                self.pane = pane;
            }
            Action::OpenDetail => {
                if self.selected_row().is_some() {
                    self.focus = Focus::Detail;
                }
            }
            Action::TogglePause => {
                let row = self.selected_row()?;
                let ih = row.info_hash;
                return Some(if row.paused || !row.running {
                    Request::Resume(ih)
                } else {
                    Request::Pause(ih)
                });
            }
            Action::SetPriority(priority) => {
                if (self.focus, self.pane) != (Focus::Detail, Pane::Files) {
                    return None;
                }
                let info_hash = self.detail_of?;
                let file = self.files.get(self.selected_file)?.index;
                return Some(Request::SetPriority {
                    info_hash,
                    file,
                    priority,
                });
            }
            Action::OpenAdd => {
                self.prompt = Some(Prompt::Add(String::new()));
                self.message.clear();
            }
            Action::OpenAddPeer => {
                if (self.focus, self.pane) != (Focus::Detail, Pane::Peers) {
                    return None;
                }
                let info_hash = self.selected_row()?.info_hash;
                self.prompt = Some(Prompt::AddPeer {
                    info_hash,
                    text: String::new(),
                });
                self.message.clear();
            }
            Action::ConfirmRemove { delete_files } => {
                let row = self.selected_row()?;
                self.prompt = Some(Prompt::Confirm {
                    info_hash: row.info_hash,
                    name: row.name.clone(),
                    delete_files,
                });
            }
            Action::CycleSort => self.sort = self.sort.next(),
            Action::OpenFilter => self.prompt = Some(Prompt::Filter),
            Action::Input(c) => match &mut self.prompt {
                Some(Prompt::Add(text) | Prompt::AddPeer { text, .. }) => text.push(c),
                Some(Prompt::Filter) => self.filter.push(c),
                Some(Prompt::Confirm { .. }) | None => {}
            },
            Action::Backspace => match &mut self.prompt {
                Some(Prompt::Add(text) | Prompt::AddPeer { text, .. }) => {
                    text.pop();
                }
                Some(Prompt::Filter) => {
                    self.filter.pop();
                }
                Some(Prompt::Confirm { .. }) | None => {}
            },
            Action::Cancel => {
                if self.prompt.take() == Some(Prompt::Filter) {
                    self.filter.clear();
                }
                self.message.clear();
            }
            Action::Submit => match self.prompt.clone()? {
                Prompt::Add(text) => return Some(Request::Add(text)),
                Prompt::AddPeer { info_hash, text } => {
                    return Some(Request::AddPeer { info_hash, text });
                }
                Prompt::Filter => self.prompt = None,
                Prompt::Confirm {
                    info_hash,
                    name,
                    delete_files,
                } => {
                    self.prompt = None;
                    return Some(Request::Remove {
                        info_hash,
                        name,
                        delete_files,
                    });
                }
            },
        }
        None
    }
}

/// The log line for an alert, or `None` for alerts the log leaves out.
pub(crate) fn alert_text(alert: &Alert) -> Option<String> {
    Some(match alert {
        Alert::StateChanged(status) => format!("state: {}", format::status(*status)),
        Alert::MetadataReceived => "metadata received".to_owned(),
        Alert::Finished => "finished".to_owned(),
        Alert::HashFailed { piece } => format!("piece {piece} failed the hash check"),
        Alert::TrackerError { message } => format!("tracker: {message}"),
        Alert::PeerBanned { ip } => format!("banned {ip}"),
        Alert::Error { message } => format!("error: {message}"),
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use dravity_engine::{Alert, Priority, TorrentStatus};

    use super::{
        ALERT_LOG, Action, FileRow, Focus, Pane, PeerRow, Prompt, Request, Sort, StatsRow,
        TorrentRow, View,
    };

    pub(crate) fn row(n: u8, name: &str, permille: u64, down_rate: u64) -> TorrentRow {
        let total = 4 * 1024 * 1024;
        TorrentRow {
            info_hash: format!("{n:02x}").repeat(20).parse().unwrap(),
            name: name.to_owned(),
            paused: false,
            running: true,
            stats: Some(StatsRow {
                status: TorrentStatus::Downloading,
                total_length: total,
                bytes_have: total * permille / 1000,
                wanted_left: total - total * permille / 1000,
                downloaded: total * permille / 1000,
                uploaded: 0,
                peers: 3,
            }),
            down_rate,
            up_rate: 0,
        }
    }

    /// Two torrents, the first one's files shown.
    pub(crate) fn view() -> View {
        let first = row(0xab, "debian.iso", 250, 512 * 1024);
        View {
            detail_of: Some(first.info_hash),
            torrents: vec![first, row(0xcd, "Arch.iso", 900, 0)],
            files: vec![
                FileRow {
                    index: 0,
                    path: "debian/disk.iso".to_owned(),
                    length: 4 * 1024 * 1024 - 100,
                    have: 1024 * 1024,
                    priority: Priority::High,
                },
                FileRow {
                    index: 1,
                    path: "debian/README".to_owned(),
                    length: 100,
                    have: 0,
                    priority: Priority::Skip,
                },
            ],
            ..View::default()
        }
    }

    #[test]
    fn add_opens_takes_text_and_submits_without_closing() {
        let mut v = view();
        assert_eq!(v.update(Action::OpenAdd), None);
        for c in "magnet:?xt=x".chars() {
            v.update(Action::Input(c));
        }
        v.update(Action::Backspace);
        assert_eq!(v.prompt, Some(Prompt::Add("magnet:?xt=".to_owned())));
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::Add("magnet:?xt=".to_owned()))
        );
        assert!(
            v.prompt.is_some(),
            "stays open until the session accepts it"
        );
    }

    #[test]
    fn esc_cancels_the_add_prompt_and_changes_nothing() {
        let mut v = view();
        v.update(Action::OpenAdd);
        v.update(Action::Input('x'));
        assert_eq!(v.update(Action::Cancel), None);
        assert_eq!(v.prompt, None);
        assert_eq!(v.torrents.len(), 2);
    }

    #[test]
    fn d_arms_confirm_and_only_enter_removes() {
        let mut v = view();
        assert_eq!(v.update(Action::ConfirmRemove { delete_files: true }), None);
        assert!(matches!(
            &v.prompt,
            Some(Prompt::Confirm { name, delete_files: true, .. }) if name == "debian.iso"
        ));
        // Other keys do nothing while confirming.
        assert_eq!(v.update(Action::None), None);
        assert_eq!(v.update(Action::Input('y')), None);
        let ih = v.torrents[0].info_hash;
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::Remove {
                info_hash: ih,
                name: "debian.iso".to_owned(),
                delete_files: true,
            })
        );
        assert_eq!(v.prompt, None);

        v.update(Action::ConfirmRemove {
            delete_files: false,
        });
        assert_eq!(v.update(Action::Cancel), None);
        assert_eq!(v.prompt, None);
    }

    #[test]
    fn priority_keys_only_work_on_the_files_pane() {
        let mut v = view();
        assert_eq!(
            v.update(Action::SetPriority(Priority::Low)),
            None,
            "torrents"
        );
        v.update(Action::Pane(Pane::Peers));
        assert_eq!(v.focus, Focus::Detail);
        assert_eq!(v.update(Action::SetPriority(Priority::Low)), None, "peers");
        v.update(Action::Pane(Pane::Files));
        v.update(Action::Down);
        assert_eq!(
            v.update(Action::SetPriority(Priority::Low)),
            Some(Request::SetPriority {
                info_hash: v.torrents[0].info_hash,
                file: 1,
                priority: Priority::Low,
            })
        );
    }

    #[test]
    fn the_filter_hides_rows_and_clearing_restores_the_selection() {
        let mut v = view();
        v.update(Action::Down);
        let arch = v.torrents[1].info_hash;
        assert_eq!(v.selected, Some(arch));
        v.update(Action::OpenFilter);
        for c in "DEB".chars() {
            v.update(Action::Input(c));
        }
        let names: Vec<_> = v.visible().iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, ["debian.iso"]);
        // The hidden row is not selectable: the visible one is.
        assert_eq!(v.selected_row().unwrap().name, "debian.iso");
        v.update(Action::Submit);
        assert_eq!(v.prompt, None);
        assert_eq!(v.visible().len(), 1, "Enter keeps the filter");
        v.update(Action::OpenFilter);
        v.update(Action::Cancel);
        assert!(v.filter.is_empty());
        assert_eq!(v.selected_row().unwrap().info_hash, arch);
    }

    #[test]
    fn sort_cycles_without_touching_the_rows() {
        let mut v = view();
        v.update(Action::CycleSort);
        assert_eq!(v.sort, Sort::Name);
        assert_eq!(v.visible()[0].name, "Arch.iso", "case-insensitive");
        v.update(Action::CycleSort);
        assert_eq!(v.sort, Sort::Progress);
        assert_eq!(v.visible()[0].name, "Arch.iso");
        v.update(Action::CycleSort);
        assert_eq!(v.sort, Sort::DownRate);
        assert_eq!(v.visible()[0].name, "debian.iso");
        v.update(Action::CycleSort);
        assert_eq!(v.sort, Sort::Added);
        assert_eq!(v.torrents[0].name, "debian.iso", "session order kept");
    }

    #[test]
    fn tab_and_panes_move_the_focus() {
        let mut v = view();
        v.update(Action::Tab);
        assert_eq!((v.focus, v.pane), (Focus::Detail, Pane::Files));
        v.update(Action::Tab);
        assert_eq!(v.focus, Focus::Torrents);
        v.update(Action::Pane(Pane::Peers));
        v.update(Action::Tab);
        assert_eq!((v.focus, v.pane), (Focus::Detail, Pane::Trackers));
        v.update(Action::Tab);
        assert_eq!(v.pane, Pane::Files);
        v.update(Action::Back);
        assert_eq!(v.focus, Focus::Torrents);
        v.update(Action::OpenDetail);
        assert_eq!((v.focus, v.pane), (Focus::Detail, Pane::Files));
    }

    #[test]
    fn add_peer_needs_the_peers_pane() {
        let mut v = view();
        v.update(Action::OpenAddPeer);
        assert_eq!(v.prompt, None);
        v.update(Action::Pane(Pane::Peers));
        v.update(Action::OpenAddPeer);
        for c in "10.0.0.2:6881".chars() {
            v.update(Action::Input(c));
        }
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::AddPeer {
                info_hash: v.torrents[0].info_hash,
                text: "10.0.0.2:6881".to_owned(),
            })
        );
    }

    #[test]
    fn a_prompt_or_the_key_list_holds_input() {
        let mut v = View::default();
        assert!(!v.holds_input());
        v.update(Action::OpenAdd);
        assert!(v.holds_input());
        v.update(Action::Cancel);
        assert!(!v.holds_input());
        v.update(Action::Help);
        assert!(v.holds_input());
        v.update(Action::DismissHelp);
        assert!(!v.holds_input());
    }

    #[test]
    fn pause_toggles_by_state() {
        let mut v = view();
        let ih = v.torrents[0].info_hash;
        assert_eq!(v.update(Action::TogglePause), Some(Request::Pause(ih)));
        v.torrents[0].paused = true;
        assert_eq!(v.update(Action::TogglePause), Some(Request::Resume(ih)));
        assert_eq!(View::default().update(Action::TogglePause), None);
    }

    #[test]
    fn the_alert_log_keeps_the_newest_and_skips_stats() {
        let mut v = view();
        let ih = v.torrents[0].info_hash;
        assert!(!v.record_alert(ih, &Alert::PieceCompleted { piece: 1 }));
        for piece in 0..40 {
            assert!(v.record_alert(ih, &Alert::HashFailed { piece }));
        }
        let log = v.selected_alerts().unwrap();
        assert_eq!(log.len(), ALERT_LOG);
        assert_eq!(log.back().unwrap(), "piece 39 failed the hash check");
        assert_eq!(log.front().unwrap(), "piece 8 failed the hash check");
    }

    #[test]
    fn rows_report_ratio_inputs_eta_and_flags() {
        let mut r = row(1, "x", 500, 1024 * 1024);
        assert_eq!(r.eta(), "2s");
        r.paused = true;
        assert_eq!(r.eta(), "—");
        let peer = PeerRow {
            addr: "10.0.0.2:6881".parse().unwrap(),
            pieces_have: 1,
            piece_count: 4,
            downloaded: 0,
            uploaded: 0,
            down_rate: 0,
            up_rate: 0,
            am_choking: true,
            peer_choking: false,
            am_interested: true,
            peer_interested: false,
            incoming: true,
            utp: true,
        };
        assert_eq!(peer.flags(), "C.I.<U");
        assert_eq!(peer.have(), "25.0%");
        assert_eq!(
            PeerRow {
                piece_count: 0,
                ..peer
            }
            .have(),
            "?"
        );
    }
}
