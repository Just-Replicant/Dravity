//! Full-screen terminal UI (`--tui`): every torrent of the session, the
//! selected torrent's files, and keys to pause, resume and change
//! priorities. It reads the session like any other client: summaries,
//! stats and progress through the public API.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dravity_core::InfoHash;
use dravity_engine::{Priority, Stats, TorrentStatus};
use dravity_session::{FileProgress, Session, TorrentSummary};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::widgets::{Block, Gauge, Paragraph, Row, Table, TableState};
use tokio::sync::mpsc;

use crate::format;

/// How often the screen is refreshed from the session.
const REFRESH: Duration = Duration::from_millis(500);

/// One torrent as shown.
#[derive(Debug, Clone)]
pub(crate) struct TorrentRow {
    pub(crate) info_hash: InfoHash,
    pub(crate) name: String,
    pub(crate) paused: bool,
    pub(crate) running: bool,
    /// Live numbers, while running.
    pub(crate) stats: Option<StatsRow>,
    /// Bytes per second.
    pub(crate) down_rate: u64,
    /// Bytes per second.
    pub(crate) up_rate: u64,
}

/// The numbers shown for a running torrent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StatsRow {
    pub(crate) status: TorrentStatus,
    pub(crate) total_length: u64,
    pub(crate) bytes_have: u64,
    pub(crate) wanted_left: u64,
    pub(crate) uploaded: u64,
    pub(crate) peers: usize,
}

impl From<&Stats> for StatsRow {
    fn from(s: &Stats) -> Self {
        Self {
            status: s.status,
            total_length: s.total_length,
            bytes_have: s.bytes_have,
            wanted_left: s.wanted_left,
            uploaded: s.uploaded,
            peers: s.peers,
        }
    }
}

impl TorrentRow {
    fn new(summary: &TorrentSummary, stats: Option<&Stats>) -> Self {
        Self {
            info_hash: summary.info_hash,
            name: summary
                .name
                .clone()
                .unwrap_or_else(|| summary.info_hash.to_hex()),
            paused: summary.paused,
            running: summary.running,
            stats: stats.map(Into::into),
            down_rate: 0,
            up_rate: 0,
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

/// What the screen shows.
#[derive(Debug, Default)]
pub(crate) struct View {
    pub(crate) torrents: Vec<TorrentRow>,
    pub(crate) selected: usize,
    /// Files of the selected torrent (empty when unknown).
    pub(crate) files: Vec<FileRow>,
    pub(crate) selected_file: usize,
    /// `true` while the file list has the focus.
    pub(crate) in_files: bool,
    /// Last action's outcome.
    pub(crate) message: String,
}

impl View {
    fn selected(&self) -> Option<&TorrentRow> {
        self.torrents.get(self.selected)
    }
}

/// Draws the whole screen.
pub(crate) fn draw(frame: &mut Frame<'_>, view: &View) {
    let [header, list, detail, files, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let title = format!(
        " dravity — {} torrent{}",
        view.torrents.len(),
        if view.torrents.len() == 1 { "" } else { "s" }
    );
    frame.render_widget(Paragraph::new(title).bold(), header);
    draw_torrents(frame, view, list);
    draw_detail(frame, view, detail);
    draw_files(frame, view, files);
    let keys = if view.in_files {
        "q quit · ↑↓ file · s/l/n/h skip/low/normal/high · tab torrents"
    } else {
        "q quit · ↑↓ torrent · space pause/resume · tab files"
    };
    let footer_text = if view.message.is_empty() {
        keys.to_owned()
    } else {
        format!("{} · {keys}", view.message)
    };
    frame.render_widget(Paragraph::new(footer_text).dim(), footer);
}

fn state(row: &TorrentRow) -> &'static str {
    if row.paused {
        "paused"
    } else if let Some(stats) = &row.stats {
        format::status(stats.status)
    } else if row.running {
        "starting"
    } else {
        "stopped"
    }
}

fn percent(have: u64, total: u64) -> String {
    let p = format::permille(have, total);
    format!("{}.{}%", p / 10, p % 10)
}

fn draw_torrents(frame: &mut Frame<'_>, view: &View, area: Rect) {
    let rows = view.torrents.iter().map(|t| {
        let (progress, size, peers) = t.stats.as_ref().map_or_else(
            || ("-".to_owned(), "-".to_owned(), "-".to_owned()),
            |s| {
                (
                    percent(s.bytes_have, s.total_length),
                    format::bytes(s.total_length),
                    s.peers.to_string(),
                )
            },
        );
        Row::new(vec![
            t.name.clone(),
            state(t).to_owned(),
            progress,
            size,
            format!("{}/s", format::bytes(t.down_rate)),
            format!("{}/s", format::bytes(t.up_rate)),
            peers,
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(17),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(5),
        ],
    )
    .header(
        Row::new(["Name", "State", "Done", "Size", "Down", "Up", "Peers"])
            .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(highlight(!view.in_files))
    .block(Block::bordered().title(" Torrents "));
    let mut state =
        TableState::new().with_selected((!view.torrents.is_empty()).then_some(view.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::UNDERLINED)
    }
}

fn draw_detail(frame: &mut Frame<'_>, view: &View, area: Rect) {
    let block = Block::bordered().title(" Selected ");
    let Some(row) = view.selected() else {
        frame.render_widget(
            Paragraph::new("No torrents. Add some through the API or the watch folder.")
                .block(block),
            area,
        );
        return;
    };
    let Some(stats) = &row.stats else {
        let text = format!("{} · {}", row.info_hash, state(row));
        frame.render_widget(Paragraph::new(text).block(block), area);
        return;
    };
    let ratio = if stats.total_length == 0 {
        1.0
    } else {
        // Precision loss is irrelevant for a progress bar.
        #[allow(clippy::cast_precision_loss)]
        let r = stats.bytes_have as f64 / stats.total_length as f64;
        r.clamp(0.0, 1.0)
    };
    let label = format!(
        "{} of {} · {} left wanted · up {} · {} peers{}",
        format::bytes(stats.bytes_have),
        format::bytes(stats.total_length),
        format::bytes(stats.wanted_left),
        format::bytes(stats.uploaded),
        stats.peers,
        if stats.status == TorrentStatus::Finished {
            " · seeding"
        } else {
            ""
        },
    );
    frame.render_widget(
        Gauge::default().block(block).ratio(ratio).label(label),
        area,
    );
}

fn draw_files(frame: &mut Frame<'_>, view: &View, area: Rect) {
    let rows = view.files.iter().map(|f| {
        Row::new(vec![
            f.priority.as_str().to_owned(),
            percent(f.have, f.length),
            format::bytes(f.length),
            f.path.clone(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new(["Priority", "Done", "Size", "Path"])
            .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(highlight(view.in_files))
    .block(Block::bordered().title(" Files "));
    let mut state =
        TableState::new().with_selected((!view.files.is_empty()).then_some(view.selected_file));
    frame.render_stateful_widget(table, area, &mut state);
}

/// What a key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Quit,
    Up,
    Down,
    SwitchFocus,
    TogglePause,
    SetPriority(Priority),
    None,
}

/// Maps a key press to an action.
pub(crate) fn action(key: KeyEvent, in_files: bool) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Tab | KeyCode::BackTab => Action::SwitchFocus,
        KeyCode::Char(' ' | 'p') if !in_files => Action::TogglePause,
        KeyCode::Char(c) if in_files => match c {
            's' => Action::SetPriority(Priority::Skip),
            'l' => Action::SetPriority(Priority::Low),
            'n' => Action::SetPriority(Priority::Normal),
            'h' => Action::SetPriority(Priority::High),
            _ => Action::None,
        },
        _ => Action::None,
    }
}

/// Reads the session into `view`, keeping the selection on the same
/// torrent. `previous` holds last refresh's byte counters for the rates.
async fn refresh(
    session: &Session,
    view: &mut View,
    previous: &mut HashMap<InfoHash, (Instant, u64, u64)>,
) {
    let selected = view.selected().map(|t| t.info_hash);
    let mut summaries = session.list();
    summaries.sort_by_key(|t| (t.added_at, t.info_hash));
    let mut rows = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let stats = match session.torrent(summary.info_hash) {
            Some(handle) => tokio::time::timeout(REFRESH, handle.stats())
                .await
                .ok()
                .and_then(Result::ok),
            None => None,
        };
        let now = Instant::now();
        let (down_rate, up_rate) = match (&stats, previous.get(&summary.info_hash)) {
            (Some(s), Some(&(at, down, up))) => {
                let ms = u64::try_from(now.duration_since(at).as_millis())
                    .unwrap_or(u64::MAX)
                    .max(1);
                (
                    s.downloaded.saturating_sub(down).saturating_mul(1000) / ms,
                    s.uploaded.saturating_sub(up).saturating_mul(1000) / ms,
                )
            }
            _ => (0, 0),
        };
        if let Some(s) = &stats {
            previous.insert(summary.info_hash, (now, s.downloaded, s.uploaded));
        }
        let mut row = TorrentRow::new(&summary, stats.as_ref());
        row.down_rate = down_rate;
        row.up_rate = up_rate;
        rows.push(row);
    }
    view.torrents = rows;
    view.selected = selected
        .and_then(|ih| view.torrents.iter().position(|t| t.info_hash == ih))
        .unwrap_or(0)
        .min(view.torrents.len().saturating_sub(1));
    view.files = match view.selected().and_then(|t| session.torrent(t.info_hash)) {
        Some(handle) => handle
            .progress()
            .await
            .ok()
            .and_then(|p| p.files)
            .unwrap_or_default()
            .iter()
            .filter(|f| !f.padding)
            .map(Into::into)
            .collect(),
        None => Vec::new(),
    };
    view.selected_file = view.selected_file.min(view.files.len().saturating_sub(1));
}

/// Applies an action; returns `false` to quit.
async fn apply(session: &Session, view: &mut View, action: Action) -> bool {
    match action {
        Action::Quit => return false,
        Action::Up if view.in_files => view.selected_file = view.selected_file.saturating_sub(1),
        Action::Up => view.selected = view.selected.saturating_sub(1),
        Action::Down if view.in_files => view.selected_file = view.selected_file.saturating_add(1),
        Action::Down => view.selected = view.selected.saturating_add(1),
        Action::SwitchFocus => view.in_files = !view.in_files,
        Action::TogglePause => {
            if let Some(row) = view.selected() {
                let ih = row.info_hash;
                let result = if row.paused || !row.running {
                    session.resume(ih).map(|_| "resumed")
                } else {
                    session.pause(ih).await.map(|()| "paused")
                };
                view.message = match result {
                    Ok(done) => done.to_owned(),
                    Err(e) => e.to_string(),
                };
            }
        }
        Action::SetPriority(priority) => {
            let file = view.files.get(view.selected_file).map(|f| f.index);
            if let (Some(row), Some(index)) = (view.selected(), file) {
                view.message = match session
                    .set_file_priority(row.info_hash, index, priority)
                    .await
                {
                    Ok(()) => format!("file {index}: {priority}"),
                    Err(e) => e.to_string(),
                };
            }
        }
        Action::None => {}
    }
    true
}

/// Runs the UI until the user quits or `done` resolves to `true` on a
/// refresh (e.g. a download completed).
pub(crate) async fn run(session: &Session, mut done: impl FnMut(&View) -> bool) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("--tui needs a terminal");
    }
    let mut terminal = ratatui::try_init().context("starting the terminal UI")?;
    let (tx, mut keys) = mpsc::channel(16);
    // Terminal input is blocking; read it on its own thread, which ends
    // when the UI drops the receiver.
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) => {
                        if tx.blocking_send(key).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                },
                Ok(false) if tx.is_closed() => return,
                Ok(false) => {}
                Err(_) => return,
            }
        }
    });
    let mut view = View::default();
    let mut previous = HashMap::new();
    let mut tick = tokio::time::interval(REFRESH);
    let result = loop {
        tokio::select! {
            _ = tick.tick() => {
                refresh(session, &mut view, &mut previous).await;
                if done(&view) {
                    break Ok(());
                }
            }
            key = keys.recv() => {
                let Some(key) = key else { break Ok(()) };
                let action = action(key, view.in_files);
                if !apply(session, &mut view, action).await {
                    break Ok(());
                }
                refresh(session, &mut view, &mut previous).await;
            }
        }
        if let Err(e) = terminal.draw(|frame| draw(frame, &view)) {
            break Err(e.into());
        }
    };
    ratatui::restore();
    result
}

/// The screen as text, for tests.
#[cfg(test)]
fn render(view: &View, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, view)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let line: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use dravity_engine::Priority;

    use dravity_engine::TorrentStatus;

    use super::{Action, FileRow, StatsRow, TorrentRow, View, action, render};

    fn view() -> View {
        let info_hash = "ab".repeat(20).parse().unwrap();
        View {
            torrents: vec![TorrentRow {
                info_hash,
                name: "debian.iso".to_owned(),
                paused: false,
                running: true,
                stats: Some(StatsRow {
                    status: TorrentStatus::Downloading,
                    total_length: 4 * 1024 * 1024,
                    bytes_have: 1024 * 1024,
                    wanted_left: 3 * 1024 * 1024,
                    uploaded: 0,
                    peers: 3,
                }),
                down_rate: 512 * 1024,
                up_rate: 0,
            }],
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
    fn torrents_and_files_are_listed_with_progress() {
        let screen = render(&view(), 110, 20);
        assert!(screen.contains("dravity — 1 torrent\n"), "{screen}");
        assert!(screen.contains("debian.iso"), "{screen}");
        assert!(screen.contains("downloading"), "{screen}");
        assert!(screen.contains("25.0%"), "{screen}");
        assert!(screen.contains("512.0 KiB/s"), "{screen}");
        assert!(screen.contains("1.0 MiB of 4.0 MiB"), "{screen}");
        assert!(screen.contains("high"), "{screen}");
        assert!(screen.contains("skip"), "{screen}");
        assert!(screen.contains("debian/README"), "{screen}");
        assert!(screen.contains("space pause/resume"), "{screen}");
    }

    #[test]
    fn the_footer_follows_the_focus_and_shows_the_last_message() {
        let mut v = view();
        v.in_files = true;
        v.message = "file 1: low".to_owned();
        let screen = render(&v, 110, 20);
        assert!(screen.contains("file 1: low · q quit"), "{screen}");
        assert!(screen.contains("s/l/n/h"), "{screen}");
    }

    #[test]
    fn an_empty_session_says_how_to_add_torrents() {
        let screen = render(&View::default(), 100, 16);
        assert!(screen.contains("dravity — 0 torrents"), "{screen}");
        assert!(screen.contains("No torrents."), "{screen}");
        assert!(screen.contains("q quit"), "{screen}");
    }

    #[test]
    fn keys_map_to_actions_by_focus() {
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(action(key('q'), false), Action::Quit);
        assert_eq!(action(key(' '), false), Action::TogglePause);
        assert_eq!(action(key(' '), true), Action::None);
        assert_eq!(action(key('h'), true), Action::SetPriority(Priority::High));
        assert_eq!(action(key('s'), true), Action::SetPriority(Priority::Skip));
        assert_eq!(action(key('h'), false), Action::None);
        assert_eq!(
            action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                true
            ),
            Action::Quit
        );
        assert_eq!(
            action(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), false),
            Action::SwitchFocus
        );
    }
}
