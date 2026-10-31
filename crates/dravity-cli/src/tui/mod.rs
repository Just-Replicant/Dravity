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

mod draw;
mod keys;
mod view;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use dravity_core::InfoHash;
use dravity_engine::Stats;
use dravity_session::{
    AddTorrentOptions, PeerStatus, PortMapProtocol, Session, TorrentProgress, TrackerStatus,
};
use ratatui::layout::Rect;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;

use crate::Input;

use view::{Action, Focus, Header, Pane, PeerRow, Request, TrackerRow};
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
    view.header = Header {
        listen_port: session.listen_port(),
        dht: session.dht().is_some(),
        mapped_port: session
            .port_mapping()
            .and_then(|s| s.external_port(PortMapProtocol::Tcp)),
    };
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
}

/// Adds the add prompt's line to the session: a `magnet:` link, or a path
/// to a `.torrent` file parsed the way `dravity download` does. Returns the
/// new torrent and its name.
async fn add(session: &Session, line: &str, download_dir: &Path) -> Result<(InfoHash, String)> {
    let line = line.trim().to_owned();
    anyhow::ensure!(!line.is_empty(), "type a magnet: link or a .torrent path");
    let input = tokio::task::spawn_blocking(move || crate::load(&line))
        .await
        .context("reading the torrent")??;
    let name = input.name();
    let options = AddTorrentOptions::new(download_dir);
    let handle = match input {
        Input::File(metainfo) => session.add_torrent(*metainfo, options),
        Input::Magnet(magnet) => session.add_magnet(&magnet, options),
    }
    .context("adding the torrent")?;
    Ok((handle.info_hash(), name))
}

/// Connects the torrent to one `host:port`.
async fn add_peer(session: &Session, info_hash: InfoHash, text: &str) -> Result<SocketAddr> {
    let text = text.trim();
    let addr = match text.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(_) => tokio::net::lookup_host(text)
            .await
            .with_context(|| format!("resolving {text} (expected host:port)"))?
            .next()
            .with_context(|| format!("{text} has no address"))?,
    };
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
        Request::Add(line) => match add(session, &line, download_dir).await {
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
    };
    true
}

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

    use super::view::tests::view;
    use super::view::{Action, Focus, Prompt, Request, View};
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
        let content: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let pieces: Vec<u8> = content
            .chunks(PIECE_LEN)
            .flat_map(|c| Sha1::digest(c).to_vec())
            .collect();
        let info = dict(vec![
            ("name", bytes(b"tui.bin")),
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
}
