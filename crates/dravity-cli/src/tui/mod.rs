//! Full-screen terminal UI (`--tui`): a client for one session.
//!
//! It lists every torrent, shows the selected one's files, peers, trackers
//! and recent alerts, and adds magnets or `.torrent` files, pauses,
//! resumes, removes and sets file priorities. Like any other client it
//! sends session commands and reads alerts; it never polls sockets or
//! drives a torrent.
//!
//! [`view`] holds the model and the state changes keys make, [`keys`] maps
//! keys to actions, [`draw`] renders a view, and this module runs the loop:
//! terminal events, alerts and a refresh tick.

mod add;
mod draw;
mod keys;
mod parse;
mod view;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use dravity_core::InfoHash;
use dravity_engine::{Priority, Stats};
use dravity_session::{
    AddTorrentOptions, IpFilter, PeerStatus, PortMapProtocol, Session, SettingsUpdate,
    TorrentProgress, TrackerStatus,
};
use ratatui::layout::Rect;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;

use add::AddForm;
use view::{Action, Direction, Focus, Header, Pane, PeerRow, Request, TrackerRow};
pub(crate) use view::{TorrentRow, View};

/// How often the screen is refreshed from the session, and how long one
/// refresh waits for the torrents to answer.
const REFRESH: Duration = Duration::from_millis(500);

/// A terminal event the input thread forwards.
#[derive(Debug)]
enum UiEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
}

/// One torrent's answer to a refresh. The instant is when that answer
/// arrived, so a rate is the gap between two replies rather than the end of
/// the whole frame (a slow torrent would otherwise inflate the others).
#[derive(Debug)]
enum Reply {
    Stats(InfoHash, Stats, Instant),
    /// The selected torrent's detail.
    Progress(TorrentProgress),
    Peers(Vec<PeerStatus>, Instant),
    Trackers(Vec<TrackerStatus>),
}

/// Byte counters of the previous sample, for rates.
#[derive(Debug, Default)]
struct Samples {
    torrents: HashMap<InfoHash, (Instant, u64, u64)>,
    peers: HashMap<(InfoHash, SocketAddr), (Instant, u64, u64)>,
}

/// Bytes per second between a previous sample and now.
fn rates(previous: Option<&(Instant, u64, u64)>, now: Instant, down: u64, up: u64) -> (u64, u64) {
    let Some(&(at, prev_down, prev_up)) = previous else {
        return (0, 0);
    };
    let ms = u64::try_from(now.saturating_duration_since(at).as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    (
        down.saturating_sub(prev_down).saturating_mul(1000) / ms,
        up.saturating_sub(prev_up).saturating_mul(1000) / ms,
    )
}

/// Reads the session into `view`. Every running torrent is asked at once;
/// after [`REFRESH`] the frame uses what answered and keeps the previous
/// numbers for the rest, so one stuck torrent cannot freeze the UI.
async fn refresh(session: &Session, view: &mut View, samples: &mut Samples) {
    view.header = session_header(session, view.header.http);
    let mut summaries = session.list();
    summaries.sort_by_key(|t| (t.added_at, t.info_hash));
    let selected = view
        .selected_row()
        .map(|t| t.info_hash)
        .or_else(|| summaries.first().map(|s| s.info_hash));

    let mut set = JoinSet::new();
    for summary in &summaries {
        let Some(handle) = session.torrent(summary.info_hash) else {
            continue;
        };
        let ih = summary.info_hash;
        if Some(ih) == selected {
            let h = handle.clone();
            set.spawn(async move { h.progress().await.ok().map(Reply::Progress) });
            let h = handle.clone();
            set.spawn(async move {
                h.peers()
                    .await
                    .ok()
                    .map(|p| Reply::Peers(p, Instant::now()))
            });
            let h = handle.clone();
            set.spawn(async move { h.trackers().await.ok().map(Reply::Trackers) });
        }
        set.spawn(async move {
            handle
                .stats()
                .await
                .ok()
                .map(|s| Reply::Stats(ih, s, Instant::now()))
        });
    }
    let deadline = tokio::time::Instant::now() + REFRESH;
    let mut stats = HashMap::new();
    let mut progress = None;
    let mut peers = None;
    let mut trackers = None;
    while let Ok(Some(joined)) = tokio::time::timeout_at(deadline, set.join_next()).await {
        match joined.ok().flatten() {
            Some(Reply::Stats(ih, s, at)) => {
                stats.insert(ih, (s, at));
            }
            Some(Reply::Progress(p)) => progress = Some(p),
            Some(Reply::Peers(p, at)) => peers = Some((p, at)),
            Some(Reply::Trackers(t)) => trackers = Some(t),
            None => {}
        }
    }
    // Whatever did not answer in time is dropped (and aborted) here.
    drop(set);

    let previous: HashMap<InfoHash, TorrentRow> = view
        .torrents
        .drain(..)
        .map(|row| (row.info_hash, row))
        .collect();
    view.torrents = summaries
        .iter()
        .map(|summary| {
            let ih = summary.info_hash;
            let mut row = TorrentRow {
                info_hash: ih,
                name: summary.name.clone().unwrap_or_else(|| ih.to_hex()),
                paused: summary.paused,
                running: summary.running,
                queued: summary.queued,
                stats: None,
                down_rate: 0,
                up_rate: 0,
            };
            if let Some((s, at)) = stats.get(&ih) {
                (row.down_rate, row.up_rate) =
                    rates(samples.torrents.get(&ih), *at, s.downloaded, s.uploaded);
                samples.torrents.insert(ih, (*at, s.downloaded, s.uploaded));
                row.stats = Some(s.into());
            } else if summary.running
                && let Some(old) = previous.get(&ih)
            {
                // No answer in time: keep the last numbers.
                row.stats = old.stats;
                row.down_rate = old.down_rate;
                row.up_rate = old.up_rate;
            }
            row
        })
        .collect();
    samples
        .torrents
        .retain(|ih, _| summaries.iter().any(|s| s.info_hash == *ih));

    if view.detail_of != selected {
        view.detail_of = selected;
        view.files.clear();
        view.peers.clear();
        view.trackers.clear();
    }
    if let Some(p) = progress {
        view.files = p
            .files
            .unwrap_or_default()
            .iter()
            .filter(|f| !f.padding)
            .map(Into::into)
            .collect();
    }
    if let (Some((peers, at)), Some(ih)) = (peers, selected) {
        view.peers = peers
            .iter()
            .map(|p| {
                let mut row = PeerRow::from(p);
                let key = (ih, row.addr);
                (row.down_rate, row.up_rate) =
                    rates(samples.peers.get(&key), at, row.downloaded, row.uploaded);
                samples
                    .peers
                    .insert(key, (at, row.downloaded, row.uploaded));
                row
            })
            .collect();
        // Drop peers that disconnected from this torrent. Samples for other
        // torrents stay, so switching back does not zero their rates.
        samples.peers.retain(|(key_ih, addr), _| {
            if *key_ih == ih {
                view.peers.iter().any(|p| p.addr == *addr)
            } else {
                summaries.iter().any(|s| s.info_hash == *key_ih)
            }
        });
    } else {
        samples
            .peers
            .retain(|(key_ih, _), _| summaries.iter().any(|s| s.info_hash == *key_ih));
    }
    if let Some(trackers) = trackers {
        view.trackers = trackers.iter().map(TrackerRow::from).collect();
    }
    if !summaries.iter().any(|s| Some(s.info_hash) == view.selected) {
        view.selected = selected;
    }
    view.selected_file = view.selected_file.min(view.files.len().saturating_sub(1));
    view.selected_peer = view.selected_peer.min(view.peers.len().saturating_sub(1));
    view.selected_tracker = view
        .selected_tracker
        .min(view.trackers.len().saturating_sub(1));
    view.alerts
        .retain(|ih, _| summaries.iter().any(|s| s.info_hash == *ih));
    view.banned = session.banned_ips();
    view.selected_ban = view.selected_ban.min(view.banned.len().saturating_sub(1));
}

/// Snapshot of session-wide numbers for the header.
fn session_header(session: &Session, http: Option<SocketAddr>) -> Header {
    let mapping = session.port_mapping();
    let settings = session.settings();
    Header {
        listen_port: session.listen_port(),
        dht: session.dht().is_some(),
        mapped_tcp: mapping
            .as_ref()
            .and_then(|s| s.external_port(PortMapProtocol::Tcp)),
        mapped_udp: mapping
            .as_ref()
            .and_then(|s| s.external_port(PortMapProtocol::Udp)),
        map_method: mapping.as_ref().and_then(|s| s.method),
        map_error: mapping.as_ref().and_then(|s| s.error.clone()),
        external_ip: mapping.as_ref().and_then(|s| s.external_ip),
        peer_id: Some(session.peer_id()),
        connections: session.connection_count(),
        max_connections: settings.max_connections,
        hash_jobs: session.hash_jobs_running(),
        cache: session.read_cache_stats().map(|c| view::CacheRow {
            hits: c.hits,
            misses: c.misses,
            bytes: c.bytes,
            budget: c.budget,
        }),
        download_limit: settings.download_rate_limit,
        upload_limit: settings.upload_rate_limit,
        max_active_downloads: settings.max_active_downloads,
        max_active_seeds: settings.max_active_seeds,
        banned: session.banned_ips().len(),
        filter_ranges: session.ip_filter().len(),
        scrape_interval: session.config().scrape_interval.is_some(),
        http,
    }
}

/// Adds the add form to the session: a `magnet:` link, or a path to a
/// `.torrent` file parsed the way `dravity download` does. Extra options
/// (pause, peers, file priorities, a different download directory) are
/// applied here. Returns the new torrent and its name.
async fn add(session: &Session, form: &AddForm, download_dir: &Path) -> Result<(InfoHash, String)> {
    let line = form.source.trim().to_owned();
    anyhow::ensure!(!line.is_empty(), "type a magnet: link or a .torrent path");
    let peers = resolve_peers(&form.peers).await?;
    let dir = if form.download_dir.trim().is_empty() {
        download_dir.to_path_buf()
    } else {
        let dir = PathBuf::from(form.download_dir.trim());
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        dir
    };
    let input = tokio::task::spawn_blocking(move || crate::load(&line))
        .await
        .context("reading the torrent")??;
    let name = input.name();
    let file_count = match &input {
        crate::Input::File(m) => m.info().files().len(),
        crate::Input::Magnet(_) => 0,
    };
    let options = AddTorrentOptions::new(dir)
        .paused(form.paused)
        .with_peers(peers)
        .with_file_priorities(form.priorities(file_count));
    let handle = match input {
        crate::Input::File(metainfo) => session.add_torrent(*metainfo, options),
        crate::Input::Magnet(magnet) => session.add_magnet(&magnet, options),
    }
    .context("adding the torrent")?;
    Ok((handle.info_hash(), name))
}

/// Fills the add form's file list from a `.torrent` path so the user can
/// set priorities before it is added.
fn inspect_form(mut form: AddForm) -> Result<AddForm> {
    let line = form.source.trim().to_owned();
    anyhow::ensure!(!line.is_empty(), "type a magnet: link or a .torrent path");
    let input = crate::load(&line).context("reading the torrent")?;
    form.files = match &input {
        crate::Input::File(metainfo) => metainfo
            .info()
            .files()
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.is_padding())
            .map(|(index, f)| add::AddFilePick {
                index,
                path: f.path().to_string(),
                length: f.length(),
                priority: Priority::Normal,
            })
            .collect(),
        crate::Input::Magnet(_) => Vec::new(),
    };
    form.inspected = true;
    if !form.files.is_empty() {
        form.field = add::AddField::File(0);
    }
    Ok(form)
}

/// Resolves a `host:port` list (comma or space separated) to addresses.
async fn resolve_peers(text: &str) -> Result<Vec<SocketAddr>> {
    let list = parse::peer_list(text).map_err(anyhow::Error::msg)?;
    let mut addrs = Vec::with_capacity(list.len());
    for entry in list {
        addrs.push(resolve_addr(&entry).await?);
    }
    Ok(addrs)
}

/// Parses or looks up one `host:port`.
async fn resolve_addr(text: &str) -> Result<SocketAddr> {
    let text = text.trim();
    match text.parse::<SocketAddr>() {
        Ok(addr) => Ok(addr),
        Err(_) => tokio::net::lookup_host(text)
            .await
            .with_context(|| format!("resolving {text} (expected host:port)"))?
            .next()
            .with_context(|| format!("{text} has no address")),
    }
}

/// Connects the torrent to one `host:port`.
async fn add_peer(session: &Session, info_hash: InfoHash, text: &str) -> Result<SocketAddr> {
    let addr = resolve_addr(text).await?;
    let handle = session
        .torrent(info_hash)
        .context("the torrent is not running")?;
    handle.add_peers([addr]).await?;
    Ok(addr)
}

/// Performs a request. Returns `false` to quit.
async fn execute(
    session: &Session,
    view: &mut View,
    request: Request,
    download_dir: &Path,
) -> bool {
    view.message = match request {
        Request::Quit => return false,
        Request::Pause(ih) => match session.pause(ih).await {
            Ok(()) => "paused".to_owned(),
            Err(e) => e.to_string(),
        },
        Request::Resume(ih) => match session.resume(ih) {
            Ok(_) => "resumed".to_owned(),
            Err(e) => e.to_string(),
        },
        Request::SetPriority {
            info_hash,
            file,
            priority,
        } => match session.set_file_priority(info_hash, file, priority).await {
            Ok(()) => format!("file {file}: {priority}"),
            Err(e) => e.to_string(),
        },
        Request::InspectAdd(form) => match tokio::task::spawn_blocking(move || inspect_form(form))
            .await
            .context("inspecting the torrent")
            .and_then(|r| r)
        {
            Ok(form) => {
                let n = form.files.len();
                view.prompt = Some(view::Prompt::Add(form));
                if n == 0 {
                    "torrent has no files to pick; enter adds it".to_owned()
                } else {
                    format!("{n} file(s) — set priorities, then enter to add")
                }
            }
            Err(e) => format!("{e:#}"),
        },
        Request::Add(form) => match add(session, &form, download_dir).await {
            Ok((info_hash, name)) => {
                view.prompt = None;
                view.selected = Some(info_hash);
                view.focus = Focus::Torrents;
                format!("added {name}")
            }
            // The prompt stays open with the error under it.
            Err(e) => format!("{e:#}"),
        },
        Request::AddPeer { info_hash, text } => match add_peer(session, info_hash, &text).await {
            Ok(addr) => {
                view.prompt = None;
                format!("connecting to {addr}")
            }
            Err(e) => format!("{e:#}"),
        },
        Request::Remove {
            info_hash,
            name,
            delete_files,
        } => match session.remove(info_hash, delete_files).await {
            Ok(()) if delete_files => format!("removed {name} and deleted its files"),
            Ok(()) => format!("removed {name}"),
            Err(e) => format!("{e:#}"),
        },
        Request::SetLimit { direction, text } => match set_limit(session, direction, &text) {
            Ok(text) => {
                view.prompt = None;
                text
            }
            Err(e) => format!("{e:#}"),
        },
        Request::SetQueueCaps { downloads, seeds } => {
            match set_queue_caps(session, &downloads, &seeds) {
                Ok(text) => {
                    view.prompt = None;
                    text
                }
                Err(e) => format!("{e:#}"),
            }
        }
        Request::SetConnections(text) => match set_connections(session, &text) {
            Ok(text) => {
                view.prompt = None;
                text
            }
            Err(e) => format!("{e:#}"),
        },
        Request::SetListenPort(text) => match set_listen_port(session, &text).await {
            Ok(text) => {
                view.prompt = None;
                text
            }
            Err(e) => format!("{e:#}"),
        },
        Request::Ban(text) => match ban_ip(session, view, &text, true) {
            Ok(text) => {
                view.prompt = None;
                text
            }
            Err(e) => format!("{e:#}"),
        },
        Request::Unban(text) => match ban_ip(session, view, &text, false) {
            Ok(text) => {
                // Stay on the ban list if that is where we came from, otherwise close.
                if !matches!(view.prompt, Some(view::Prompt::BanList)) {
                    view.prompt = None;
                }
                text
            }
            Err(e) => format!("{e:#}"),
        },
        Request::SetIpFilter(text) => match set_ip_filter(session, &text) {
            Ok(text) => {
                view.prompt = None;
                text
            }
            Err(e) => format!("{e:#}"),
        },
        Request::Scrape(info_hash) => match scrape(session, view, info_hash).await {
            Ok(text) => text,
            Err(e) => format!("{e:#}"),
        },
        Request::Stream { info_hash, file } => {
            match stream_file(session, view, info_hash, file).await {
                Ok(text) => text,
                Err(e) => format!("{e:#}"),
            }
        }
    };
    true
}

/// Applies a session-wide rate limit typed in the limit prompt.
fn set_limit(session: &Session, direction: Direction, text: &str) -> Result<String> {
    let text = text.trim();
    let rate = if text.is_empty() {
        None
    } else {
        Some(crate::format::parse_rate(text).map_err(anyhow::Error::msg)?).filter(|&r| r > 0)
    };
    let mut update = SettingsUpdate::default();
    let what = match direction {
        Direction::Download => {
            update.download_rate_limit = Some(rate);
            "download"
        }
        Direction::Upload => {
            update.upload_rate_limit = Some(rate);
            "upload"
        }
    };
    session.apply_settings(update)?;
    Ok(match rate {
        Some(r) => format!("{what} limit {}/s", crate::format::bytes(r)),
        None => format!("{what} limit lifted"),
    })
}

fn fmt_cap(v: Option<usize>) -> String {
    v.map_or_else(|| "unlimited".to_owned(), |n| n.to_string())
}

fn set_queue_caps(session: &Session, downloads: &str, seeds: &str) -> Result<String> {
    let downloads = parse::cap(downloads).map_err(anyhow::Error::msg)?;
    let seeds = parse::cap(seeds).map_err(anyhow::Error::msg)?;
    let mut update = SettingsUpdate::default();
    update.max_active_downloads = Some(downloads);
    update.max_active_seeds = Some(seeds);
    session.apply_settings(update)?;
    Ok(format!(
        "queue caps: downloads {} · seeds {}",
        fmt_cap(downloads),
        fmt_cap(seeds)
    ))
}

fn set_connections(session: &Session, text: &str) -> Result<String> {
    let max = parse::cap(text).map_err(anyhow::Error::msg)?;
    let mut update = SettingsUpdate::default();
    update.max_connections = Some(max);
    session.apply_settings(update)?;
    Ok(match max {
        Some(n) => format!("connection cap {n}"),
        None => "connection cap lifted".to_owned(),
    })
}

async fn set_listen_port(session: &Session, text: &str) -> Result<String> {
    let port = parse::port(text).map_err(anyhow::Error::msg)?;
    let bound = session.set_listen_port(port).await?;
    Ok(format!("listening on port {bound}"))
}

fn ban_ip(session: &Session, view: &mut View, text: &str, ban: bool) -> Result<String> {
    let ip = parse::ip(text).map_err(anyhow::Error::msg)?;
    let msg = if ban {
        if session.ban_ip(ip) {
            format!("banned {ip}")
        } else {
            format!("{ip} was already banned")
        }
    } else if session.unban_ip(ip) {
        format!("unbanned {ip}")
    } else {
        format!("{ip} was not banned")
    };
    view.banned = session.banned_ips();
    view.selected_ban = view.selected_ban.min(view.banned.len().saturating_sub(1));
    view.header.banned = view.banned.len();
    Ok(msg)
}

fn set_ip_filter(session: &Session, text: &str) -> Result<String> {
    let path = parse::filter_path(text).map_err(anyhow::Error::msg)?;
    match path {
        None => {
            session.set_ip_filter(IpFilter::new());
            Ok("IP filter cleared".to_owned())
        }
        Some(path) => {
            let filter = IpFilter::from_file(&path)
                .with_context(|| format!("loading {}", path.display()))?;
            let ranges = filter.len();
            let skipped = filter.skipped();
            session.set_ip_filter(filter);
            let skip = if skipped == 0 {
                String::new()
            } else {
                format!(" ({skipped} unreadable lines skipped)")
            };
            Ok(format!(
                "IP filter loaded: {ranges} range{}{skip}",
                if ranges == 1 { "" } else { "s" }
            ))
        }
    }
}

async fn scrape(session: &Session, view: &mut View, info_hash: InfoHash) -> Result<String> {
    let handle = session
        .torrent(info_hash)
        .context("the torrent is not running")?;
    let trackers = handle.scrape().await?;
    view.trackers = trackers.iter().map(TrackerRow::from).collect();
    let note = if view.header.scrape_interval {
        "scraped every tracker"
    } else {
        "scraped every tracker (periodic counts need --scrape-interval; these results are from this request)"
    };
    Ok(note.to_owned())
}

async fn stream_file(
    session: &Session,
    view: &View,
    info_hash: InfoHash,
    file: usize,
) -> Result<String> {
    let handle = session
        .torrent(info_hash)
        .context("the torrent is not running")?;
    let stream = handle.open_file(file).await?;
    let len = stream.len();
    drop(stream);
    let hash = info_hash.to_hex();
    Ok(match view.header.http {
        Some(http) => format!(
            "stream http://{http}/api/v1/torrents/{hash}/files/{file}/content ({} · Range supported)",
            crate::format::bytes(len)
        ),
        None => format!(
            "file {file} is {} and open_file can stream it; run `dravity serve --tui` for an HTTP Range URL",
            crate::format::bytes(len)
        ),
    })
}

/// Moves the selection to the torrent or file row
/// Moves the selection to the torrent or file row under a left click.
fn click(view: &mut View, screen: Rect, mouse: MouseEvent) {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) || view.prompt.is_some() {
        return;
    }
    let areas = draw::areas(screen, view);
    let (column, row) = (mouse.column, mouse.row);
    if let Some(index) = draw::table_row_at(areas.torrents, view.selected_index(), column, row) {
        if index < view.visible().len() {
            view.select_index(index);
            view.focus = Focus::Torrents;
        }
    } else if view.pane == Pane::Files
        && let Some(index) = draw::table_row_at(areas.detail, view.selected_file, column, row)
        && index < view.files.len()
    {
        view.selected_file = index;
        view.focus = Focus::Detail;
    }
}

/// Reads terminal events on a thread (they block) until the UI drops the
/// receiver.
fn spawn_input(tx: mpsc::Sender<UiEvent>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    let ui = match event::read() {
                        Ok(Event::Key(key)) => UiEvent::Key(key),
                        Ok(Event::Mouse(mouse)) => UiEvent::Mouse(mouse),
                        Ok(Event::Resize(..)) => UiEvent::Resize,
                        Ok(_) => continue,
                        Err(_) => return,
                    };
                    if tx.blocking_send(ui).is_err() {
                        return;
                    }
                }
                Ok(false) if tx.is_closed() => return,
                Ok(false) => {}
                Err(_) => return,
            }
        }
    })
}

/// The input thread plus its event queue. Dropping it closes the queue,
/// waits for the thread to leave the terminal, then restores the screen,
/// including when the UI task panics.
struct InputLoop {
    events: Option<mpsc::Receiver<UiEvent>>,
    input: Option<std::thread::JoinHandle<()>>,
}

impl InputLoop {
    fn start() -> Self {
        let (tx, events) = mpsc::channel(16);
        let input = spawn_input(tx);
        // Keys still work without mouse reporting.
        let _ = crossterm::execute!(std::io::stdout(), event::EnableMouseCapture);
        Self {
            events: Some(events),
            input: Some(input),
        }
    }

    async fn recv(&mut self) -> Option<UiEvent> {
        match self.events.as_mut() {
            Some(events) => events.recv().await,
            None => None,
        }
    }
}

impl Drop for InputLoop {
    fn drop(&mut self) {
        // Close the queue before joining, or a full channel blocks the thread
        // on send and the join never returns.
        self.events.take();
        if let Some(handle) = self.input.take() {
            let _ = handle.join();
        }
        restore();
    }
}

/// Puts the terminal back, mouse reporting included.
fn restore() {
    let _ = crossterm::execute!(std::io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
}

/// Runs the UI until the user quits or `done` returns `true` on a refresh
/// (e.g. a download completed). Torrents added from the screen download to
/// `download_dir`.
pub(crate) async fn run(
    session: &Session,
    download_dir: PathBuf,
    http: Option<SocketAddr>,
    mut done: impl FnMut(&View) -> bool,
) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("--tui needs a terminal");
    }
    let mut terminal = ratatui::try_init().context("starting the terminal UI")?;
    let mut input = InputLoop::start();
    let mut alerts = session.subscribe();
    let mut view = View::default();
    view.header.http = http;
    let mut samples = Samples::default();
    let mut tick = tokio::time::interval(REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            _ = tick.tick() => {
                refresh(session, &mut view, &mut samples).await;
                if done(&view) {
                    break Ok(());
                }
            }
            event = input.recv() => match event {
                None => break Ok(()),
                Some(UiEvent::Resize) => {}
                Some(UiEvent::Mouse(mouse)) => {
                    let size = terminal.size().unwrap_or_default();
                    click(&mut view, Rect::new(0, 0, size.width, size.height), mouse);
                }
                Some(UiEvent::Key(key)) => {
                    let action = keys::action(key, &view);
                    if action == Action::None {
                        continue;
                    }
                    if let Some(request) = view.update(action) {
                        if !execute(session, &mut view, request, &download_dir).await {
                            break Ok(());
                        }
                        refresh(session, &mut view, &mut samples).await;
                    }
                }
            },
            alert = alerts.recv() => match alert {
                Ok(alert) => {
                    if !view.record_alert(alert.info_hash, &alert.alert) {
                        continue;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    view.message = format!("missed {n} alerts");
                }
                Err(broadcast::error::RecvError::Closed) => break Ok(()),
            },
        }
        if let Err(e) = terminal.draw(|frame| draw::draw(frame, &view)) {
            break Err(e.into());
        }
    };
    // Join the input thread and restore the terminal before the caller
    // shuts the session down.
    drop(input);
    result
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation
    )]

    use std::borrow::Cow;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use dravity_bencode::{Dict, Value};
    use dravity_core::Metainfo;
    use dravity_engine::Alert;
    use dravity_session::{AddTorrentOptions, ListenConfig, Session, SessionConfig};
    use ratatui::layout::Rect;
    use sha1::{Digest, Sha1};
    use tokio::sync::broadcast;

    use super::add::AddForm;
    use super::view::tests::view;
    use super::view::{Action, CapsField, Direction, Focus, Prompt, Request, View};
    use super::{Samples, click, draw, execute, rates, refresh};

    #[test]
    fn rates_need_a_previous_sample() {
        let now = Instant::now();
        assert_eq!(rates(None, now, 100, 100), (0, 0));
        let before = now.checked_sub(Duration::from_secs(2)).unwrap();
        assert_eq!(
            rates(Some(&(before, 0, 1000)), now, 4096, 3000),
            (2048, 1000)
        );
        // A counter that went backwards (a restarted torrent) is not negative.
        assert_eq!(rates(Some(&(before, 5000, 0)), now, 100, 0), (0, 0));
    }

    #[test]
    fn a_click_selects_the_torrent_or_file_row_under_it() {
        let mut v = view();
        let screen = Rect::new(0, 0, 100, 30);
        let areas = draw::areas(screen, &v);
        let mouse = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        click(&mut v, screen, mouse(areas.torrents.y + 3));
        assert_eq!(v.selected, Some(v.torrents[1].info_hash));
        assert_eq!(v.focus, Focus::Torrents);
        click(&mut v, screen, mouse(areas.detail.y + 3));
        assert_eq!(v.selected_file, 1);
        assert_eq!(v.focus, Focus::Detail);
        // Past the last row, and right clicks, change nothing.
        click(&mut v, screen, mouse(areas.torrents.y + 4));
        assert_eq!(v.selected, Some(v.torrents[1].info_hash));
        let mut right = mouse(areas.torrents.y + 2);
        right.kind = MouseEventKind::Down(MouseButton::Right);
        click(&mut v, screen, right);
        assert_eq!(v.selected, Some(v.torrents[1].info_hash));
    }

    fn bytes(b: &[u8]) -> Value<'static> {
        Value::Bytes(Cow::Owned(b.to_vec()))
    }

    fn dict(entries: Vec<(&str, Value<'static>)>) -> Value<'static> {
        let mut d = Dict::new();
        for (k, v) in entries {
            d.insert(Cow::Owned(k.as_bytes().to_vec()), v);
        }
        Value::Dict(d)
    }

    const PIECE_LEN: usize = 16 * 1024;

    /// A single-file torrent; returns it and its content.
    fn torrent() -> (Vec<u8>, Vec<u8>) {
        torrent_named("tui.bin", 0)
    }

    fn torrent_named(name: &str, salt: u8) -> (Vec<u8>, Vec<u8>) {
        let content: Vec<u8> = (0..70_000u32)
            .map(|i| ((i % 251) as u8).wrapping_add(salt))
            .collect();
        let pieces: Vec<u8> = content
            .chunks(PIECE_LEN)
            .flat_map(|c| Sha1::digest(c).to_vec())
            .collect();
        let info = dict(vec![
            ("name", bytes(name.as_bytes())),
            ("length", Value::Int(content.len() as i64)),
            ("piece length", Value::Int(PIECE_LEN as i64)),
            ("pieces", bytes(&pieces)),
        ]);
        (dict(vec![("info", info)]).to_vec(), content)
    }

    fn config() -> SessionConfig {
        let mut c = SessionConfig::default();
        c.tick_interval = Duration::from_millis(50);
        c.stop_announce_timeout = Duration::from_secs(1);
        c.dht = None;
        c.listen = Some(ListenConfig::loopback(0));
        c.port_mapping = None;
        c
    }

    /// Refreshes until `check` holds (or panics after 20 s).
    async fn until(
        session: &Session,
        view: &mut View,
        samples: &mut Samples,
        what: &str,
        check: impl Fn(&View) -> bool,
    ) {
        let start = Instant::now();
        loop {
            refresh(session, view, samples).await;
            if check(view) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "timed out: {what}: {view:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Feeds typed text to the view.
    fn type_text(view: &mut View, text: &str) {
        for c in text.chars() {
            assert_eq!(view.update(Action::Input(c)), None);
        }
    }

    /// `L` sets the session-wide download limit; a bad or too small value
    /// keeps the prompt open, an empty line lifts the limit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_limit_prompt_sets_and_lifts_the_session_limit() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(config()).unwrap();
        let mut v = View::default();
        for (text, ok, expect) in [
            ("fast", false, None),
            ("1K", false, None),
            ("2M", true, Some(2 << 20)),
            ("", true, None),
        ] {
            v.update(Action::OpenLimit(Direction::Download));
            type_text(&mut v, text);
            let request = v.update(Action::Submit).unwrap();
            assert!(execute(&session, &mut v, request, dir.path()).await);
            assert_eq!(v.prompt.is_none(), ok, "{text}: {v:?}");
            assert_eq!(session.settings().download_rate_limit, expect, "{text}");
            v.update(Action::Cancel);
        }
        v.update(Action::OpenLimit(Direction::Upload));
        type_text(&mut v, "64K");
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(session.settings().upload_rate_limit, Some(64 * 1024));
        assert!(v.message.contains("upload limit"), "{}", v.message);
        session.shutdown().await;
    }

    /// The path the UI takes: paste a magnet (metadata from a local seed),
    /// see it listed with its peer, pause it, remove it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paste_a_magnet_then_pause_and_remove_it() {
        let (encoded, content) = torrent();
        let metainfo = Metainfo::from_bytes(&encoded).unwrap();
        let hash = metainfo.info_hash();
        let seed_dir = tempfile::tempdir().unwrap();
        std::fs::write(seed_dir.path().join("tui.bin"), &content).unwrap();
        let seeder = Session::new(config()).unwrap();
        let port = seeder.listen_port().unwrap();
        seeder
            .add_torrent(metainfo, AddTorrentOptions::new(seed_dir.path()))
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(config()).unwrap();
        let mut alerts = session.subscribe();
        let mut v = View::default();
        let mut samples = Samples::default();
        refresh(&session, &mut v, &mut samples).await;
        assert!(v.torrents.is_empty());
        assert!(v.header.listen_port.is_some());
        assert!(!v.header.dht);

        // A bad line keeps the prompt open with the error.
        v.update(Action::OpenAdd);
        type_text(&mut v, "magnet:?xt=urn:btih:nothex");
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(matches!(v.prompt, Some(Prompt::Add(_))), "{v:?}");
        assert!(v.message.contains("magnet"), "{}", v.message);
        v.update(Action::Cancel);

        let missing = dir.path().join("missing.torrent");
        v.update(Action::OpenAdd);
        type_text(&mut v, &missing.display().to_string());
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(v.prompt.is_some());
        assert!(v.message.contains("missing.torrent"), "{}", v.message);
        v.update(Action::Cancel);

        v.update(Action::OpenAdd);
        type_text(
            &mut v,
            &format!("magnet:?xt=urn:btih:{hash}&dn=tui.bin&x.pe=127.0.0.1:{port}"),
        );
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None, "{}", v.message);
        assert_eq!(v.message, "added tui.bin");
        assert_eq!(v.selected, Some(hash));

        until(&session, &mut v, &mut samples, "finished", |v| {
            v.torrents.len() == 1 && v.torrents[0].is_finished()
        })
        .await;
        assert_eq!(std::fs::read(dir.path().join("tui.bin")).unwrap(), content);
        assert_eq!(v.detail_of, Some(hash));
        assert_eq!(v.files.len(), 1);
        assert!(v.trackers.is_empty(), "the magnet has no trackers");

        // The peers pane adds one address; a bad one keeps the prompt open.
        v.update(Action::Pane(super::view::Pane::Peers));
        v.update(Action::OpenAddPeer);
        type_text(&mut v, "not an address");
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(matches!(v.prompt, Some(Prompt::AddPeer { .. })), "{v:?}");
        assert!(v.message.contains("host:port"), "{}", v.message);
        v.update(Action::Cancel);
        v.update(Action::OpenAddPeer);
        type_text(&mut v, &format!("127.0.0.1:{port}"));
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None);
        assert_eq!(v.message, format!("connecting to 127.0.0.1:{port}"));
        v.update(Action::Back);

        // The alert log saw the metadata and the finish.
        loop {
            match alerts.try_recv() {
                Ok(alert) => {
                    v.record_alert(alert.info_hash, &alert.alert);
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        let log = v.selected_alerts().unwrap();
        assert!(log.iter().any(|l| l == "metadata received"), "{log:?}");
        assert!(log.iter().any(|l| l == "finished"), "{log:?}");
        assert!(super::view::alert_text(&Alert::Finished).is_some());
        assert_eq!(
            super::view::alert_text(&Alert::PieceLayersStalled {
                missing_layers: 2,
                silent: Duration::from_secs(130),
            })
            .as_deref(),
            Some("waiting for piece layers of 2 file(s): no peer sent any for 130s")
        );

        let request = v.update(Action::TogglePause).unwrap();
        assert_eq!(request, Request::Pause(hash));
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.message, "paused");
        refresh(&session, &mut v, &mut samples).await;
        assert!(v.torrents[0].paused);
        assert_eq!(v.torrents[0].state(), "paused");

        assert_eq!(v.update(Action::ConfirmRemove { delete_files: true }), None);
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.message, "removed tui.bin and deleted its files");
        refresh(&session, &mut v, &mut samples).await;
        assert!(v.torrents.is_empty());
        assert!(!Path::new(&dir.path().join("tui.bin")).exists());

        session.shutdown().await;
        seeder.shutdown().await;
    }

    /// Live session actions from the TUI: caps, listen port, bans, the IP
    /// filter, a queued torrent, add options, scrape and a stream URL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_session_actions_and_add_options() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let session = Session::new(config()).unwrap();
        let mut v = View::default();
        v.header.http = Some("127.0.0.1:9091".parse().unwrap());
        let mut samples = Samples::default();
        refresh(&session, &mut v, &mut samples).await;
        assert!(v.header.peer_id.is_some());
        assert!(v.header.listen_port.is_some());
        let first_port = v.header.listen_port.unwrap();

        // A bad cap keeps the prompt open; a valid one applies.
        v.update(Action::OpenQueueCaps);
        type_text(&mut v, "nope");
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(v.prompt.is_some(), "{}", v.message);
        assert!(v.message.contains("count"), "{}", v.message);
        v.update(Action::Cancel);
        v.update(Action::OpenQueueCaps);
        v.prompt = Some(Prompt::QueueCaps {
            downloads: "1".to_owned(),
            seeds: String::new(),
            field: CapsField::Downloads,
        });
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None, "{}", v.message);
        assert_eq!(session.settings().max_active_downloads, Some(1));
        assert_eq!(session.settings().max_active_seeds, None);
        assert!(v.message.contains("downloads 1"), "{}", v.message);

        v.update(Action::OpenConnections);
        v.prompt = Some(Prompt::Connections("40".to_owned()));
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(session.settings().max_connections, Some(40));

        // Port 0 picks a free port; a nonsense value is refused.
        v.update(Action::OpenListenPort);
        v.prompt = Some(Prompt::ListenPort("not-a-port".to_owned()));
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(v.prompt.is_some());
        v.prompt = Some(Prompt::ListenPort("0".to_owned()));
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None, "{}", v.message);
        assert!(v.message.starts_with("listening on port"), "{}", v.message);
        let bound = session.listen_port().unwrap();
        assert_ne!(bound, 0);
        let _ = first_port;

        // Ban, reject a host:port, unban, load and clear an IP filter.
        v.update(Action::OpenBan);
        type_text(&mut v, "192.0.2.1:6881");
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(v.prompt.is_some());
        assert!(
            v.message.contains("host:port") || v.message.contains("IP"),
            "{}",
            v.message
        );
        v.update(Action::Cancel);
        v.update(Action::OpenBan);
        type_text(&mut v, "192.0.2.9");
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None, "{}", v.message);
        assert_eq!(
            session.banned_ips(),
            vec!["192.0.2.9".parse::<std::net::IpAddr>().unwrap()]
        );
        refresh(&session, &mut v, &mut samples).await;
        assert_eq!(v.header.banned, 1);
        assert_eq!(v.banned.len(), 1);
        let request = v
            .update(Action::UnbanSelected)
            .or_else(|| {
                v.prompt = Some(Prompt::Unban("192.0.2.9".to_owned()));
                v.update(Action::Submit)
            })
            .unwrap();
        // After refresh the prompt is None; UnbanSelected needs the list selected.
        // We already closed the ban prompt; open the list first if needed.
        let _ = request;
        // Direct unban through the prompt for a reliable assertion.
        v.prompt = Some(Prompt::Unban("192.0.2.9".to_owned()));
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(session.banned_ips().is_empty(), "{}", v.message);

        let filter = dir.path().join("ipfilter.p2p");
        std::fs::write(&filter, "lab:10.0.0.0-10.0.0.255\n").unwrap();
        v.update(Action::OpenIpFilter);
        type_text(&mut v, &filter.display().to_string());
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None, "{}", v.message);
        assert!(session.ip_filter().is_blocked("10.0.0.7".parse().unwrap()));
        assert!(v.message.contains("1 range"), "{}", v.message);
        v.update(Action::OpenIpFilter);
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(session.ip_filter().is_empty(), "{}", v.message);
        assert!(v.message.contains("cleared"), "{}", v.message);

        // First torrent starts (one download slot); the second is queued.
        // A third is added paused into a different directory, with an extra peer.
        let (enc1, _) = torrent_named("one.bin", 1);
        let (enc2, _) = torrent_named("two.bin", 2);
        let (enc3, _) = torrent_named("three.bin", 3);
        let p1 = dir.path().join("one.torrent");
        let p2 = dir.path().join("two.torrent");
        let p3 = dir.path().join("three.torrent");
        std::fs::write(&p1, &enc1).unwrap();
        std::fs::write(&p2, &enc2).unwrap();
        std::fs::write(&p3, &enc3).unwrap();

        // Inspect lists the file; a second enter adds it.
        v.update(Action::OpenAdd);
        type_text(&mut v, &p1.display().to_string());
        let request = v.update(Action::Submit).unwrap();
        assert!(matches!(request, Request::InspectAdd(_)), "{request:?}");
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(v.message.contains("file"), "{}", v.message);
        let Some(Prompt::Add(form)) = &v.prompt else {
            panic!("expected the file-pick form: {}", v.message);
        };
        assert!(form.inspected);
        assert_eq!(form.files.len(), 1);
        assert_eq!(form.files[0].path, "one.bin");
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None, "{}", v.message);
        assert!(v.message.contains("added one.bin"), "{}", v.message);

        // Second torrent, no inspect skip: mark inspected with no file picks.
        let mut form = AddForm::with_source(p2.display().to_string());
        form.inspected = true;
        v.prompt = Some(Prompt::Add(form));
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None, "{}", v.message);

        refresh(&session, &mut v, &mut samples).await;
        let states: Vec<_> = v
            .torrents
            .iter()
            .map(|t| (t.name.as_str(), t.queued, t.paused, t.running, t.state()))
            .collect();
        assert_eq!(v.torrents.len(), 2, "{states:?}");
        let queued = v.torrents.iter().filter(|t| t.queued).count();
        let running = v.torrents.iter().filter(|t| t.running && !t.queued).count();
        assert_eq!(running, 1, "one active download: {states:?}");
        assert_eq!(queued, 1, "the other waits: {states:?}");
        assert!(
            v.torrents.iter().any(|t| t.state() == "queued"),
            "{states:?}"
        );

        // Third: paused, other directory, extra peer (the session's own port).
        let mut form = AddForm::with_source(p3.display().to_string());
        form.inspected = true;
        form.paused = true;
        form.download_dir = other.path().display().to_string();
        form.peers = format!("127.0.0.1:{bound}");
        v.prompt = Some(Prompt::Add(form));
        let request = v.update(Action::Submit).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert_eq!(v.prompt, None, "{}", v.message);
        let three = session
            .list()
            .into_iter()
            .find(|s| s.name.as_deref() == Some("three.bin"))
            .expect("three.bin");
        assert!(three.paused);
        assert!(!three.queued);
        assert!(!three.running);
        assert_eq!(three.download_dir, other.path());

        // Streaming the first torrent's file produces an HTTP Range URL.
        let first = session
            .list()
            .into_iter()
            .find(|s| s.name.as_deref() == Some("one.bin") && s.running)
            .or_else(|| session.list().into_iter().find(|s| s.running))
            .expect("a running torrent");
        v.detail_of = Some(first.info_hash);
        v.focus = Focus::Detail;
        v.pane = super::view::Pane::Files;
        v.files = vec![super::view::FileRow {
            index: 0,
            path: "one.bin".to_owned(),
            length: 70_000,
            have: 0,
            priority: dravity_engine::Priority::Normal,
        }];
        let request = v.update(Action::StreamFile).unwrap();
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(
            v.message.contains("/api/v1/torrents/")
                && v.message.contains("/files/0/content")
                && v.message.contains("Range"),
            "{}",
            v.message
        );

        // A scrape of a torrent with no trackers still succeeds (empty list).
        let request = Request::Scrape(first.info_hash);
        assert!(execute(&session, &mut v, request, dir.path()).await);
        assert!(
            v.message.contains("scraped") && v.message.contains("--scrape-interval"),
            "{}",
            v.message
        );

        session.shutdown().await;
    }
}
