//! The view model: everything the screen shows, and the state changes keys
//! make before anything is sent to the session. Drawing reads a [`View`];
//! tests fill one by hand and never open a terminal.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use dravity_core::{InfoHash, PeerId};
use dravity_engine::{Alert, Priority, Stats, TorrentStatus};
use dravity_session::{
    FileProgress, PeerStatus, PortMapMethod, ScrapeOutcome, TrackerOutcome, TrackerStatus,
    Transport,
};

use super::add::AddForm;
use crate::format;

/// Alerts kept per torrent; the oldest is dropped first.
pub(crate) const ALERT_LOG: usize = 32;

/// Session facts for the header lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Header {
    /// Port the session accepts peers on.
    pub(crate) listen_port: Option<u16>,
    /// The session runs a DHT node.
    pub(crate) dht: bool,
    /// The router's public TCP port, when port mapping got one.
    pub(crate) mapped_tcp: Option<u16>,
    /// The router's public UDP port, when port mapping got one.
    pub(crate) mapped_udp: Option<u16>,
    /// How the mappings were made (PCP, NAT-PMP, UPnP).
    pub(crate) map_method: Option<PortMapMethod>,
    /// The last port-mapping error, if mapping failed.
    pub(crate) map_error: Option<String>,
    /// The router's public address, when it told us.
    pub(crate) external_ip: Option<Ipv4Addr>,
    /// The session's peer id.
    pub(crate) peer_id: Option<PeerId>,
    /// Open peer connections in the session.
    pub(crate) connections: usize,
    /// Session-wide connection cap (`None` is unlimited).
    pub(crate) max_connections: Option<usize>,
    /// Piece-hash jobs running now.
    pub(crate) hash_jobs: usize,
    /// Read-cache occupancy and hit counts, when a cache is in use.
    pub(crate) cache: Option<CacheRow>,
    /// Session-wide download limit (bytes/s).
    pub(crate) download_limit: Option<u64>,
    /// Session-wide upload limit (bytes/s).
    pub(crate) upload_limit: Option<u64>,
    /// Queue cap on active downloads (`None` is unlimited).
    pub(crate) max_active_downloads: Option<usize>,
    /// Queue cap on active seeds (`None` is unlimited).
    pub(crate) max_active_seeds: Option<usize>,
    /// Addresses on the session ban list.
    pub(crate) banned: usize,
    /// Disjoint ranges in the IP filter.
    pub(crate) filter_ranges: usize,
    /// The session was started with `--scrape-interval` (periodic scrapes).
    pub(crate) scrape_interval: bool,
    /// HTTP API bind address, when the API is running (for stream URLs).
    pub(crate) http: Option<SocketAddr>,
}

/// Read-cache numbers for the header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CacheRow {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) bytes: u64,
    pub(crate) budget: u64,
}

/// One torrent as shown.
#[derive(Debug, Clone)]
pub(crate) struct TorrentRow {
    pub(crate) info_hash: InfoHash,
    pub(crate) name: String,
    pub(crate) paused: bool,
    pub(crate) running: bool,
    /// Waiting for an active download or seed slot.
    pub(crate) queued: bool,
    /// Live numbers, while running (the last ones that arrived).
    pub(crate) stats: Option<StatsRow>,
    /// Bytes per second.
    pub(crate) down_rate: u64,
    /// Bytes per second.
    pub(crate) up_rate: u64,
}

impl TorrentRow {
    /// A display state. Queued is distinct from stopped: a queued torrent
    /// is waiting for a slot, not paused or failed.
    pub(crate) fn state(&self) -> &'static str {
        if self.paused {
            "paused"
        } else if self.queued {
            "queued"
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
    pub(crate) piece_count: u32,
    pub(crate) pieces_have: u32,
    /// Connection attempts in flight.
    pub(crate) connecting: usize,
    /// Known addresses not yet connected.
    pub(crate) known_peers: usize,
    pub(crate) unchoked: usize,
    pub(crate) interested: usize,
    pub(crate) pending_writes: usize,
    pub(crate) pending_reads: usize,
    /// Peers this torrent has banned for bad data.
    pub(crate) banned: usize,
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
            piece_count: s.piece_count,
            pieces_have: s.pieces_have,
            connecting: s.connecting,
            known_peers: s.known_peers,
            unchoked: s.unchoked,
            interested: s.interested,
            pending_writes: s.pending_writes,
            pending_reads: s.pending_reads,
            banned: s.banned,
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
    /// An HTTP web seed (BEP 19), not a BitTorrent peer.
    pub(crate) web: bool,
    /// RC4-encrypted (MSE).
    pub(crate) encrypted: bool,
    /// Country from the GeoIP database, if one is loaded.
    pub(crate) country: Option<dravity_session::CountryCode>,
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
            web: p.transport == Transport::Web,
            encrypted: p.encrypted,
            country: p.country,
        }
    }
}

impl PeerRow {
    /// Fixed-position flags: `C` we choke them, `c` they choke us, `I` we
    /// are interested, `i` they are interested, `<` incoming, `U` uTP,
    /// `W` web seed, `E` encrypted (MSE); `.` when unset.
    pub(crate) fn flags(&self) -> String {
        [
            (self.am_choking, 'C'),
            (self.peer_choking, 'c'),
            (self.am_interested, 'I'),
            (self.peer_interested, 'i'),
            (self.incoming, '<'),
            (self.utp, 'U'),
            (self.web, 'W'),
            (self.encrypted, 'E'),
        ]
        .iter()
        .map(|&(on, c)| if on { c } else { '.' })
        .collect()
    }

    /// The country code, or `--` when unknown.
    pub(crate) fn country(&self) -> String {
        self.country
            .map_or_else(|| "--".to_owned(), |c| c.to_string())
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
    /// When the last announce was recorded.
    pub(crate) announced: Option<Instant>,
    /// The last scrape, success or failure.
    pub(crate) scrape: Option<ScrapeOutcome>,
    /// When the last scrape was recorded.
    pub(crate) scraped: Option<Instant>,
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
            announced: t.updated,
            scrape: t.scrape.clone(),
            scraped: t.scraped,
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
    /// Add a magnet or a `.torrent`, with extra options.
    Add(AddForm),
    /// `host:port` of a peer for the selected torrent.
    AddPeer { info_hash: InfoHash, text: String },
    /// Editing [`View::filter`].
    Filter,
    /// A session-wide rate limit (`500K`, `2M`; empty or `0` lifts it).
    Limit { direction: Direction, text: String },
    /// Remove a torrent (and maybe its files) once Enter confirms.
    Confirm {
        info_hash: InfoHash,
        name: String,
        delete_files: bool,
    },
    /// Queue caps: max active downloads and seeds (empty lifts a cap).
    QueueCaps {
        downloads: String,
        seeds: String,
        field: CapsField,
    },
    /// Session connection cap (empty lifts it).
    Connections(String),
    /// Live listen-port change (`0` picks a free port).
    ListenPort(String),
    /// Ban an IP address.
    Ban(String),
    /// Lift a ban on an IP address.
    Unban(String),
    /// The session ban list (and IP-filter size).
    BanList,
    /// Path of an IP filter list to load (empty clears the filter).
    IpFilter(String),
}

/// Which field of the queue-caps prompt is being edited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CapsField {
    #[default]
    Downloads,
    Seeds,
}

/// Which session-wide rate limit a prompt sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Download,
    Upload,
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
    /// `L` / `U`: the session-wide download / upload limit prompt.
    OpenLimit(Direction),
    /// `Q`: max active downloads and seeds.
    OpenQueueCaps,
    /// `C`: session connection cap.
    OpenConnections,
    /// `P`: change the listen port while running.
    OpenListenPort,
    /// `b`: ban an IP.
    OpenBan,
    /// Type an IP to unban.
    OpenUnban,
    /// `B`: show the ban list.
    OpenBanList,
    /// `i`: load or replace the IP filter.
    OpenIpFilter,
    /// `s` on the trackers pane: scrape now.
    ForceScrape,
    /// `t` on the files pane: stream the selected file.
    StreamFile,
    /// Tab inside a multi-field prompt.
    NextField,
    /// Shift+Tab inside a multi-field prompt.
    PrevField,
    /// Space on a toggle field (start paused, or cycle a file priority).
    Toggle,
    /// `u` on the ban list: unban the selected address.
    UnbanSelected,
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
    /// The add form; the prompt stays open until the session accepts it.
    Add(AddForm),
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
    /// The limit prompt's line; the prompt stays open until it applies.
    SetLimit {
        direction: Direction,
        text: String,
    },
    /// Inspect a `.torrent` to fill the add form's file list; the prompt
    /// stays open.
    InspectAdd(AddForm),
    SetQueueCaps {
        downloads: String,
        seeds: String,
    },
    SetConnections(String),
    SetListenPort(String),
    Ban(String),
    Unban(String),
    SetIpFilter(String),
    Scrape(InfoHash),
    Stream {
        info_hash: InfoHash,
        file: usize,
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
    /// The session ban list, newest snapshot.
    pub(crate) banned: Vec<IpAddr>,
    /// Selection in the ban-list prompt.
    pub(crate) selected_ban: usize,
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
            Action::Up => self.move_selection(false),
            Action::Down => self.move_selection(true),
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
                if let Some(Prompt::Add(form)) = &mut self.prompt {
                    form.set_file_priority(priority);
                    return None;
                }
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
                self.prompt = Some(Prompt::Add(AddForm::new()));
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
            Action::OpenLimit(direction) => {
                self.prompt = Some(Prompt::Limit {
                    direction,
                    text: String::new(),
                });
                self.message.clear();
            }
            Action::OpenQueueCaps => {
                self.prompt = Some(Prompt::QueueCaps {
                    downloads: cap_text(self.header.max_active_downloads),
                    seeds: cap_text(self.header.max_active_seeds),
                    field: CapsField::Downloads,
                });
                self.message.clear();
            }
            Action::OpenConnections => {
                self.prompt = Some(Prompt::Connections(cap_text(self.header.max_connections)));
                self.message.clear();
            }
            Action::OpenListenPort => {
                self.prompt = Some(Prompt::ListenPort(
                    self.header
                        .listen_port
                        .map(|p| p.to_string())
                        .unwrap_or_default(),
                ));
                self.message.clear();
            }
            Action::OpenBan => {
                self.prompt = Some(Prompt::Ban(String::new()));
                self.message.clear();
            }
            Action::OpenUnban => {
                self.prompt = Some(Prompt::Unban(String::new()));
                self.message.clear();
            }
            Action::OpenBanList => {
                self.selected_ban = self.selected_ban.min(self.banned.len().saturating_sub(1));
                self.prompt = Some(Prompt::BanList);
                self.message.clear();
            }
            Action::OpenIpFilter => {
                self.prompt = Some(Prompt::IpFilter(String::new()));
                self.message.clear();
            }
            Action::ForceScrape => {
                let info_hash = self.selected_row()?.info_hash;
                return Some(Request::Scrape(info_hash));
            }
            Action::StreamFile => {
                if (self.focus, self.pane) != (Focus::Detail, Pane::Files) {
                    return None;
                }
                let info_hash = self.detail_of?;
                let file = self.files.get(self.selected_file)?.index;
                return Some(Request::Stream { info_hash, file });
            }
            Action::NextField => match &mut self.prompt {
                Some(Prompt::Add(form)) => form.next_field(),
                Some(Prompt::QueueCaps { field, .. }) => {
                    *field = match field {
                        CapsField::Downloads => CapsField::Seeds,
                        CapsField::Seeds => CapsField::Downloads,
                    };
                }
                _ => {}
            },
            Action::PrevField => match &mut self.prompt {
                Some(Prompt::Add(form)) => form.prev_field(),
                Some(Prompt::QueueCaps { field, .. }) => {
                    *field = match field {
                        CapsField::Downloads => CapsField::Seeds,
                        CapsField::Seeds => CapsField::Downloads,
                    };
                }
                _ => {}
            },
            Action::Toggle => {
                if let Some(Prompt::Add(form)) = &mut self.prompt {
                    form.toggle();
                }
            }
            Action::UnbanSelected => {
                let ip = *self.banned.get(self.selected_ban)?;
                return Some(Request::Unban(ip.to_string()));
            }
            Action::Input(c) => self.input_char(c),
            Action::Backspace => self.backspace(),
            Action::Cancel => {
                if self.prompt.take() == Some(Prompt::Filter) {
                    self.filter.clear();
                }
                self.message.clear();
            }
            Action::Submit => return self.submit(),
        }
        None
    }

    fn move_selection(&mut self, down: bool) {
        match &mut self.prompt {
            Some(Prompt::BanList) => {
                let last = self.banned.len().saturating_sub(1);
                self.selected_ban = if down {
                    self.selected_ban.saturating_add(1).min(last)
                } else {
                    self.selected_ban.saturating_sub(1)
                };
            }
            Some(Prompt::Add(form)) => form.step_file(down),
            _ => self.step(down),
        }
    }

    fn input_char(&mut self, c: char) {
        match &mut self.prompt {
            Some(Prompt::Add(form)) => {
                if let Some(text) = form.text_mut() {
                    text.push(c);
                }
            }
            Some(Prompt::AddPeer { text, .. } | Prompt::Limit { text, .. }) => text.push(c),
            Some(Prompt::QueueCaps {
                downloads,
                seeds,
                field,
            }) => match field {
                CapsField::Downloads => downloads.push(c),
                CapsField::Seeds => seeds.push(c),
            },
            Some(
                Prompt::Connections(text)
                | Prompt::ListenPort(text)
                | Prompt::Ban(text)
                | Prompt::Unban(text)
                | Prompt::IpFilter(text),
            ) => text.push(c),
            Some(Prompt::Filter) => self.filter.push(c),
            Some(Prompt::Confirm { .. } | Prompt::BanList) | None => {}
        }
    }

    fn backspace(&mut self) {
        match &mut self.prompt {
            Some(Prompt::Add(form)) => {
                if let Some(text) = form.text_mut() {
                    text.pop();
                }
            }
            Some(Prompt::AddPeer { text, .. } | Prompt::Limit { text, .. }) => {
                text.pop();
            }
            Some(Prompt::QueueCaps {
                downloads,
                seeds,
                field,
            }) => match field {
                CapsField::Downloads => {
                    downloads.pop();
                }
                CapsField::Seeds => {
                    seeds.pop();
                }
            },
            Some(
                Prompt::Connections(text)
                | Prompt::ListenPort(text)
                | Prompt::Ban(text)
                | Prompt::Unban(text)
                | Prompt::IpFilter(text),
            ) => {
                text.pop();
            }
            Some(Prompt::Filter) => {
                self.filter.pop();
            }
            Some(Prompt::Confirm { .. } | Prompt::BanList) | None => {}
        }
    }

    fn submit(&mut self) -> Option<Request> {
        match self.prompt.clone()? {
            Prompt::Add(form) => {
                if should_inspect(&form) {
                    return Some(Request::InspectAdd(form));
                }
                Some(Request::Add(form))
            }
            Prompt::AddPeer { info_hash, text } => Some(Request::AddPeer { info_hash, text }),
            Prompt::Filter => {
                self.prompt = None;
                None
            }
            Prompt::Limit { direction, text } => Some(Request::SetLimit { direction, text }),
            Prompt::Confirm {
                info_hash,
                name,
                delete_files,
            } => {
                self.prompt = None;
                Some(Request::Remove {
                    info_hash,
                    name,
                    delete_files,
                })
            }
            Prompt::QueueCaps {
                downloads, seeds, ..
            } => Some(Request::SetQueueCaps { downloads, seeds }),
            Prompt::Connections(text) => Some(Request::SetConnections(text)),
            Prompt::ListenPort(text) => Some(Request::SetListenPort(text)),
            Prompt::Ban(text) => Some(Request::Ban(text)),
            Prompt::Unban(text) => Some(Request::Unban(text)),
            Prompt::IpFilter(text) => Some(Request::SetIpFilter(text)),
            Prompt::BanList => None,
        }
    }
}

/// Prefills a cap prompt: empty means unlimited.
fn cap_text(value: Option<usize>) -> String {
    value.map(|n| n.to_string()).unwrap_or_default()
}

/// Whether submitting the add form should first inspect a `.torrent` for
/// its file list. Magnets and already-inspected forms go straight to add.
fn should_inspect(form: &AddForm) -> bool {
    if form.inspected {
        return false;
    }
    let source = form.source.trim();
    !source.is_empty() && !source.to_ascii_lowercase().starts_with("magnet:")
}

/// The log line for an alert, or `None` for alerts the log leaves out.
/// The log line for an alert, or `None` for alerts the log leaves out.
pub(crate) fn alert_text(alert: &Alert) -> Option<String> {
    Some(match alert {
        Alert::StateChanged(status) => format!("state: {}", format::status(*status)),
        Alert::MetadataReceived => "metadata received".to_owned(),
        Alert::Finished => "finished".to_owned(),
        Alert::HashFailed { piece } => format!("piece {piece} failed the hash check"),
        Alert::PieceCompleted { piece } => format!("piece {piece} complete"),
        Alert::UploadFailed { piece, message } => {
            format!("upload read of piece {piece} failed: {message}")
        }
        Alert::TrackerError { message } => format!("tracker: {message}"),
        Alert::PeerBanned { ip } => format!("banned {ip}"),
        Alert::PieceLayersStalled {
            missing_layers,
            silent,
        } => format!(
            "waiting for piece layers of {missing_layers} file(s): no peer sent any for {}s",
            silent.as_secs()
        ),
        Alert::Error { message } => format!("error: {message}"),
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use dravity_engine::{Alert, Priority, TorrentStatus};

    use super::super::add::AddForm;
    use super::{
        ALERT_LOG, Action, CapsField, FileRow, Focus, Pane, PeerRow, Prompt, Request, Sort,
        StatsRow, TorrentRow, View, alert_text,
    };

    pub(crate) fn row(n: u8, name: &str, permille: u64, down_rate: u64) -> TorrentRow {
        let total = 4 * 1024 * 1024;
        TorrentRow {
            info_hash: format!("{n:02x}").repeat(20).parse().unwrap(),
            name: name.to_owned(),
            paused: false,
            running: true,
            queued: false,
            stats: Some(StatsRow {
                status: TorrentStatus::Downloading,
                total_length: total,
                bytes_have: total * permille / 1000,
                wanted_left: total - total * permille / 1000,
                downloaded: total * permille / 1000,
                uploaded: 0,
                peers: 3,
                piece_count: 4,
                pieces_have: u32::try_from(permille * 4 / 1000).unwrap_or(0),
                connecting: 1,
                known_peers: 5,
                unchoked: 1,
                interested: 2,
                pending_writes: 1,
                pending_reads: 2,
                banned: 0,
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
        assert_eq!(
            v.prompt,
            Some(Prompt::Add(AddForm::with_source("magnet:?xt=")))
        );
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::Add(AddForm::with_source("magnet:?xt=")))
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
        assert!(v.record_alert(ih, &Alert::PieceCompleted { piece: 1 }));
        assert!(v.record_alert(
            ih,
            &Alert::UploadFailed {
                piece: 2,
                message: "disk error".to_owned(),
            }
        ));
        for piece in 0..40 {
            assert!(v.record_alert(ih, &Alert::HashFailed { piece }));
        }
        let log = v.selected_alerts().unwrap();
        assert_eq!(log.len(), ALERT_LOG);
        assert_eq!(log.back().unwrap(), "piece 39 failed the hash check");
        assert_eq!(log.front().unwrap(), "piece 8 failed the hash check");
    }

    #[test]
    fn piece_complete_and_failed_upload_read_have_alert_text() {
        assert_eq!(
            alert_text(&Alert::PieceCompleted { piece: 7 }).as_deref(),
            Some("piece 7 complete")
        );
        assert_eq!(
            alert_text(&Alert::UploadFailed {
                piece: 3,
                message: "EIO".to_owned(),
            })
            .as_deref(),
            Some("upload read of piece 3 failed: EIO")
        );
    }

    #[test]
    fn a_queued_torrent_is_not_drawn_as_stopped() {
        let mut t = row(1, "q", 0, 0);
        t.running = false;
        t.paused = false;
        t.stats = None;
        t.queued = true;
        assert_eq!(t.state(), "queued");
        t.queued = false;
        assert_eq!(t.state(), "stopped");
        t.paused = true;
        t.queued = true;
        assert_eq!(t.state(), "paused", "a user pause wins over the queue");
        t.paused = false;
        t.running = true;
        t.queued = true;
        t.stats = Some(StatsRow {
            status: TorrentStatus::Downloading,
            total_length: 1,
            bytes_have: 0,
            wanted_left: 1,
            downloaded: 0,
            uploaded: 0,
            peers: 0,
            piece_count: 1,
            pieces_have: 0,
            connecting: 0,
            known_peers: 0,
            unchoked: 0,
            interested: 0,
            pending_writes: 0,
            pending_reads: 0,
            banned: 0,
        });
        assert_eq!(t.state(), "queued", "queued is shown even with live stats");
    }

    #[test]
    fn a_torrent_path_is_inspected_before_it_is_added() {
        let mut v = view();
        v.update(Action::OpenAdd);
        for c in "/tmp/film.torrent".chars() {
            v.update(Action::Input(c));
        }
        let form = AddForm::with_source("/tmp/film.torrent");
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::InspectAdd(form.clone()))
        );
        // After inspection the form is submitted as Add.
        let mut inspected = form;
        inspected.inspected = true;
        v.prompt = Some(Prompt::Add(inspected.clone()));
        assert_eq!(v.update(Action::Submit), Some(Request::Add(inspected)));
    }

    #[test]
    fn queue_caps_and_connection_prompts_submit_the_typed_values() {
        let mut v = view();
        v.header.max_active_downloads = Some(2);
        v.header.max_active_seeds = Some(4);
        v.header.max_connections = Some(50);
        v.update(Action::OpenQueueCaps);
        assert_eq!(
            v.prompt,
            Some(Prompt::QueueCaps {
                downloads: "2".to_owned(),
                seeds: "4".to_owned(),
                field: CapsField::Downloads,
            })
        );
        v.update(Action::Backspace);
        v.update(Action::Input('3'));
        v.update(Action::NextField);
        v.update(Action::Input('x')); // will be validated later
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::SetQueueCaps {
                downloads: "3".to_owned(),
                seeds: "4x".to_owned(),
            })
        );
        v.update(Action::Cancel);
        v.update(Action::OpenConnections);
        v.update(Action::Input('0'));
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::SetConnections("500".to_owned()))
        );
        // Prefill is "50", typed "0" appends → "500". Yes.
        v.update(Action::Cancel);
        v.update(Action::OpenListenPort);
        assert!(matches!(v.prompt, Some(Prompt::ListenPort(_))));
        v.prompt = Some(Prompt::ListenPort("6882".to_owned()));
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::SetListenPort("6882".to_owned()))
        );
    }

    #[test]
    fn ban_list_unban_and_stream_and_scrape_requests() {
        let mut v = view();
        let ih = v.torrents[0].info_hash;
        v.banned = vec!["10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap()];
        v.update(Action::OpenBanList);
        assert_eq!(v.prompt, Some(Prompt::BanList));
        v.update(Action::Down);
        assert_eq!(v.selected_ban, 1);
        assert_eq!(
            v.update(Action::UnbanSelected),
            Some(Request::Unban("10.0.0.2".to_owned()))
        );
        v.update(Action::OpenBan);
        for c in "192.0.2.1".chars() {
            v.update(Action::Input(c));
        }
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::Ban("192.0.2.1".to_owned()))
        );
        v.update(Action::Cancel);
        v.update(Action::OpenIpFilter);
        for c in "/tmp/ipfilter.dat".chars() {
            v.update(Action::Input(c));
        }
        assert_eq!(
            v.update(Action::Submit),
            Some(Request::SetIpFilter("/tmp/ipfilter.dat".to_owned()))
        );
        assert_eq!(v.update(Action::ForceScrape), Some(Request::Scrape(ih)));
        v.update(Action::Pane(Pane::Files));
        assert_eq!(
            v.update(Action::StreamFile),
            Some(Request::Stream {
                info_hash: ih,
                file: 0
            })
        );
        v.update(Action::Back);
        assert_eq!(v.update(Action::StreamFile), None, "files pane only");
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
            web: false,
            encrypted: true,
            country: dravity_session::CountryCode::new("se"),
        };
        assert_eq!(peer.country(), "SE");
        assert_eq!(
            PeerRow {
                country: None,
                ..peer
            }
            .country(),
            "--"
        );
        assert_eq!(peer.flags(), "C.I.<U.E");
        assert_eq!(
            PeerRow {
                utp: false,
                web: true,
                encrypted: false,
                ..peer
            }
            .flags(),
            "C.I.<.W."
        );
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
