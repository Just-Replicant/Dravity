//! Drawing: a pure function of the [`View`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap};

use super::view::{Focus, Pane, Prompt, StatsRow, TrackerState, View};
use crate::format;

/// Alert lines shown under the detail pane.
pub(crate) const ALERT_LINES: u16 = 4;

/// The full key list `?` shows in place of the footer.
const HELP: [&str; 6] = [
    "torrents: a add · enter detail · space/p pause or resume · d remove · D remove and delete files",
    "          o sort (added, name, progress, down rate) · / filter by name · tab files pane",
    "detail:   1 files · 2 peers · 3 trackers · tab next pane · esc back · ↑↓/j k move",
    "files:    s skip · l low · n normal · h high   peers: a add a peer (host:port)",
    "peer flags: C we choke them · c they choke us · I we want · i they want · < incoming · U uTP",
    "q quit · ctrl-c quit from anywhere · mouse: click a torrent or file row · any key closes this",
];

/// Where each part of the screen goes. Shared with mouse handling, so a
/// click maps to the row it hit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Areas {
    pub(crate) header: Rect,
    pub(crate) torrents: Rect,
    pub(crate) selected: Rect,
    pub(crate) detail: Rect,
    pub(crate) alerts: Rect,
    pub(crate) footer: Rect,
}

/// Splits the screen.
pub(crate) fn areas(area: Rect, view: &View) -> Areas {
    let footer = if view.help {
        u16::try_from(HELP.len()).unwrap_or(1)
    } else {
        1
    };
    let [header, torrents, selected, detail, alerts, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(2),
        Constraint::Length(3),
        Constraint::Fill(3),
        Constraint::Length(ALERT_LINES + 2),
        Constraint::Length(footer),
    ])
    .areas(area);
    Areas {
        header,
        torrents,
        selected,
        detail,
        alerts,
        footer,
    }
}

/// The list row under `(column, row)` in a bordered table with a header
/// line at `area`, given the selected index (tables scroll just enough to
/// show the selection).
pub(crate) fn table_row_at(area: Rect, selected: usize, column: u16, row: u16) -> Option<usize> {
    let inner_x = area.x.saturating_add(1)..area.right().saturating_sub(1);
    let first = area.y.saturating_add(2);
    let shown = usize::from(area.height.saturating_sub(3));
    if !inner_x.contains(&column) || row < first || shown == 0 {
        return None;
    }
    let line = usize::from(row - first);
    if line >= shown {
        return None;
    }
    let offset = selected.saturating_add(1).saturating_sub(shown);
    Some(offset + line)
}

/// Draws the whole screen.
pub(crate) fn draw(frame: &mut Frame<'_>, view: &View) {
    let a = areas(frame.area(), view);
    frame.render_widget(Paragraph::new(header(view)).bold(), a.header);
    draw_torrents(frame, view, a.torrents);
    draw_selected(frame, view, a.selected);
    match view.pane {
        Pane::Files => draw_files(frame, view, a.detail),
        Pane::Peers => draw_peers(frame, view, a.detail),
        Pane::Trackers => draw_trackers(frame, view, a.detail),
    }
    draw_alerts(frame, view, a.alerts);
    draw_footer(frame, view, a.footer);
    match &view.prompt {
        Some(Prompt::Add(text)) => popup(
            frame,
            " Add a magnet link or a .torrent file ",
            vec![Line::from(format!("{text}▏")), message_line(view)],
        ),
        Some(Prompt::AddPeer { text, .. }) => popup(
            frame,
            " Add a peer (host:port) ",
            vec![Line::from(format!("{text}▏")), message_line(view)],
        ),
        Some(Prompt::Confirm {
            name, delete_files, ..
        }) => {
            let question = if *delete_files {
                format!("Remove {name} and delete its files?")
            } else {
                format!("Remove {name}? Its files are kept.")
            };
            popup(
                frame,
                " Confirm ",
                vec![
                    Line::from(question),
                    Line::from("enter confirms · esc cancels"),
                ],
            );
        }
        Some(Prompt::Filter) | None => {}
    }
}

fn message_line(view: &View) -> Line<'static> {
    Line::from(view.message.clone()).dim()
}

fn header(view: &View) -> String {
    let n = view.torrents.len();
    let mut text = format!(" dravity — {n} torrent{}", if n == 1 { "" } else { "s" });
    let h = view.header;
    if let Some(port) = h.listen_port {
        text.push_str(&format!(" · port {port}"));
    }
    text.push_str(if h.dht { " · DHT on" } else { " · DHT off" });
    if let Some(port) = h.mapped_port {
        text.push_str(&format!(" · mapped {port}"));
    }
    let (down, up) = view.totals();
    text.push_str(&format!(
        " · ↓ {}/s ↑ {}/s",
        format::bytes(down),
        format::bytes(up)
    ));
    text
}

fn percent(have: u64, total: u64) -> String {
    let p = format::permille(have, total);
    format!("{}.{}%", p / 10, p % 10)
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::UNDERLINED)
    }
}

fn bold_row<'a>(cells: impl IntoIterator<Item = &'a str>) -> Row<'a> {
    Row::new(cells.into_iter().collect::<Vec<_>>()).style(Style::new().add_modifier(Modifier::BOLD))
}

fn draw_torrents(frame: &mut Frame<'_>, view: &View, area: Rect) {
    let visible = view.visible();
    let rows = visible.iter().map(|t| {
        let (done, size, ratio, peers) = t.stats.as_ref().map_or_else(
            || {
                (
                    "-".to_owned(),
                    "-".to_owned(),
                    "—".to_owned(),
                    "-".to_owned(),
                )
            },
            |s| {
                let (done, size) = if s.size_known() {
                    (
                        percent(s.bytes_have, s.total_length),
                        format::bytes(s.total_length),
                    )
                } else {
                    ("-".to_owned(), "-".to_owned())
                };
                (
                    done,
                    size,
                    format::ratio(s.uploaded, s.downloaded),
                    s.peers.to_string(),
                )
            },
        );
        Row::new(vec![
            t.name.clone(),
            t.state().to_owned(),
            done,
            size,
            format!("{}/s", format::bytes(t.down_rate)),
            format!("{}/s", format::bytes(t.up_rate)),
            ratio,
            t.eta(),
            peers,
        ])
    });
    let mut title = format!(" Torrents · sort: {} ", view.sort.as_str());
    if !view.filter.is_empty() {
        title.push_str(&format!("· filter: {} ", view.filter));
    }
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(17),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(5),
        ],
    )
    .header(bold_row([
        "Name", "State", "Done", "Size", "Down", "Up", "Ratio", "ETA", "Peers",
    ]))
    .row_highlight_style(highlight(view.focus == Focus::Torrents))
    .block(Block::bordered().title(title));
    let mut state =
        TableState::new().with_selected((!visible.is_empty()).then_some(view.selected_index()));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_selected(frame: &mut Frame<'_>, view: &View, area: Rect) {
    let block = Block::bordered().title(" Selected ");
    let Some(row) = view.selected_row() else {
        let text = if view.torrents.is_empty() {
            "No torrents. `a` adds a magnet or a .torrent file."
        } else {
            "No torrent matches the filter."
        };
        frame.render_widget(Paragraph::new(text).block(block), area);
        return;
    };
    let Some(stats) = row.stats.filter(StatsRow::size_known) else {
        let peers = row
            .stats
            .map(|s| format!(" · {} peers", s.peers))
            .unwrap_or_default();
        let text = format!("{} · {}{peers}", row.info_hash, row.state());
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
        "{} of {} · {} left wanted · up {} · ratio {} · {} peers{}",
        format::bytes(stats.bytes_have),
        format::bytes(stats.total_length),
        format::bytes(stats.wanted_left),
        format::bytes(stats.uploaded),
        format::ratio(stats.uploaded, stats.downloaded),
        stats.peers,
        if row.is_finished() { " · seeding" } else { "" },
    );
    frame.render_widget(
        Gauge::default().block(block).ratio(ratio).label(label),
        area,
    );
}

/// The detail block, its title naming the panes with the current one
/// highlighted.
fn detail_block(view: &View) -> Block<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, pane) in Pane::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" │ "));
        }
        let label = format!("{} {}", i + 1, pane.title());
        spans.push(if *pane == view.pane {
            Span::styled(
                label,
                Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )
        } else {
            Span::raw(label)
        });
    }
    spans.push(Span::raw(" "));
    Block::bordered().title(Line::from(spans))
}

fn detail_focused(view: &View) -> bool {
    view.focus == Focus::Detail
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
    .header(bold_row(["Priority", "Done", "Size", "Path"]))
    .row_highlight_style(highlight(detail_focused(view)))
    .block(detail_block(view));
    let mut state =
        TableState::new().with_selected((!view.files.is_empty()).then_some(view.selected_file));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_peers(frame: &mut Frame<'_>, view: &View, area: Rect) {
    if view.peers.is_empty() {
        frame.render_widget(
            Paragraph::new("No peers connected.").block(detail_block(view)),
            area,
        );
        return;
    }
    let rows = view.peers.iter().map(|p| {
        Row::new(vec![
            p.addr.to_string(),
            p.have(),
            format!("{}/s", format::bytes(p.down_rate)),
            format!("{}/s", format::bytes(p.up_rate)),
            p.flags(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(7),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(6),
        ],
    )
    .header(bold_row(["Address", "Has", "Down", "Up", "Flags"]))
    .row_highlight_style(highlight(detail_focused(view)))
    .block(detail_block(view));
    let mut state = TableState::new().with_selected(Some(view.selected_peer));
    frame.render_stateful_widget(table, area, &mut state);
}

fn count(n: Option<u32>) -> String {
    n.map_or_else(|| "-".to_owned(), |n| n.to_string())
}

fn draw_trackers(frame: &mut Frame<'_>, view: &View, area: Rect) {
    if view.trackers.is_empty() {
        frame.render_widget(
            Paragraph::new("No trackers: peers come from the DHT, PEX or added addresses.")
                .block(detail_block(view)),
            area,
        );
        return;
    }
    let rows = view.trackers.iter().map(|t| {
        let (status, seeds, leech, peers, every, text) = match &t.state {
            TrackerState::Waiting => (
                "waiting",
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                t.url.clone(),
            ),
            TrackerState::Ok {
                interval,
                seeders,
                leechers,
                peers,
            } => (
                "ok",
                count(*seeders),
                count(*leechers),
                peers.to_string(),
                interval.map_or_else(|| "-".to_owned(), |i| format::eta(i.as_secs(), 1)),
                t.url.clone(),
            ),
            TrackerState::Failed(error) => (
                "error",
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                format!("{} — {error}", t.url),
            ),
        };
        Row::new(vec![
            t.tier.to_string(),
            status.to_owned(),
            seeds,
            leech,
            peers,
            every,
            text,
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Fill(1),
        ],
    )
    .header(bold_row([
        "Tier", "Status", "Seeds", "Leech", "Peers", "Every", "URL",
    ]))
    .row_highlight_style(highlight(detail_focused(view)))
    .block(detail_block(view));
    let mut state = TableState::new().with_selected(Some(view.selected_tracker));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_alerts(frame: &mut Frame<'_>, view: &View, area: Rect) {
    let block = Block::bordered().title(" Alerts ");
    let lines: Vec<Line<'_>> = match view.selected_alerts() {
        Some(log) if !log.is_empty() => {
            let skip = log.len().saturating_sub(usize::from(ALERT_LINES));
            log.iter()
                .skip(skip)
                .map(|l| Line::from(l.as_str()))
                .collect()
        }
        _ => vec![Line::from("No alerts yet.").dim()],
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The keys for what has the focus.
fn keys(view: &View) -> String {
    match (&view.prompt, view.focus, view.pane) {
        (Some(Prompt::Add(_)), ..) => "enter add · esc cancel".to_owned(),
        (Some(Prompt::AddPeer { .. }), ..) => "enter connect · esc cancel".to_owned(),
        (Some(Prompt::Confirm { .. }), ..) => "enter confirm · esc cancel".to_owned(),
        (Some(Prompt::Filter), ..) => {
            format!("filter: {}▏ · enter keep · esc clear", view.filter)
        }
        (None, Focus::Torrents, _) => {
            "q quit · a add · enter detail · space pause/resume · d/D remove · o sort · / filter · ? keys"
                .to_owned()
        }
        (None, Focus::Detail, Pane::Files) => {
            "esc back · ↑↓ file · s/l/n/h skip/low/normal/high · 1/2/3 pane · tab torrents · ? keys"
                .to_owned()
        }
        (None, Focus::Detail, Pane::Peers) => {
            "esc back · ↑↓ peer · a add peer · 1/2/3 pane · tab trackers · ? keys".to_owned()
        }
        (None, Focus::Detail, Pane::Trackers) => {
            "esc back · ↑↓ tracker · 1/2/3 pane · tab files · ? keys".to_owned()
        }
    }
}

fn draw_footer(frame: &mut Frame<'_>, view: &View, area: Rect) {
    if view.help {
        let lines: Vec<Line<'_>> = HELP.iter().map(|l| Line::from(*l)).collect();
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let keys = keys(view);
    // A popup shows its own message.
    let in_popup = matches!(
        view.prompt,
        Some(Prompt::Add(_) | Prompt::AddPeer { .. } | Prompt::Confirm { .. })
    );
    let text = if view.message.is_empty() || in_popup {
        keys
    } else {
        format!("{} · {keys}", view.message)
    };
    frame.render_widget(Paragraph::new(text).dim(), area);
}

/// A centered box over the screen.
fn popup(frame: &mut Frame<'_>, title: &str, lines: Vec<Line<'_>>) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(80);
    let height = 4.min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(title.to_owned())),
        rect,
    );
}

/// The screen as text, for tests.
#[cfg(test)]
pub(crate) fn render(view: &View, width: u16, height: u16) -> String {
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
    use std::time::Duration;

    use ratatui::layout::Rect;

    use super::{areas, render, table_row_at};
    use crate::tui::view::tests::view;
    use crate::tui::view::{
        Action, Focus, Header, Pane, PeerRow, Prompt, TrackerRow, TrackerState, View,
    };

    #[test]
    fn torrents_and_files_are_listed_with_progress_ratio_and_eta() {
        let mut v = view();
        v.header = Header {
            listen_port: Some(6881),
            dht: true,
            mapped_port: Some(40_000),
        };
        let screen = render(&v, 130, 30);
        assert!(
            screen.contains(
                "dravity — 2 torrents · port 6881 · DHT on · mapped 40000 · ↓ 512.0 KiB/s ↑ 0 B/s"
            ),
            "{screen}"
        );
        assert!(screen.contains("debian.iso"), "{screen}");
        assert!(screen.contains("downloading"), "{screen}");
        assert!(screen.contains("25.0%"), "{screen}");
        assert!(screen.contains("512.0 KiB/s"), "{screen}");
        assert!(screen.contains("0.00"), "ratio: {screen}");
        assert!(screen.contains("6s"), "eta 3 MiB at 512 KiB/s: {screen}");
        assert!(screen.contains("1.0 MiB of 4.0 MiB"), "{screen}");
        assert!(screen.contains("high"), "{screen}");
        assert!(screen.contains("skip"), "{screen}");
        assert!(screen.contains("debian/README"), "{screen}");
        assert!(
            screen.contains("1 Files │ 2 Peers │ 3 Trackers"),
            "{screen}"
        );
        assert!(screen.contains("space pause/resume"), "{screen}");
        assert!(screen.contains("No alerts yet."), "{screen}");
    }

    #[test]
    fn a_magnet_without_metadata_shows_no_sizes() {
        let mut v = view();
        v.torrents.truncate(1);
        let stats = v.torrents[0].stats.as_mut().unwrap();
        stats.status = dravity_engine::TorrentStatus::FetchingMetadata;
        stats.total_length = 0;
        stats.bytes_have = 0;
        let screen = render(&v, 130, 30);
        assert!(!screen.contains("100.0%"), "{screen}");
        assert!(screen.contains("fetching metadata · 3 peers"), "{screen}");
    }

    #[test]
    fn an_empty_session_names_the_add_key() {
        let screen = render(&View::default(), 110, 24);
        assert!(screen.contains("dravity — 0 torrents"), "{screen}");
        assert!(screen.contains("DHT off"), "{screen}");
        assert!(
            screen.contains("No torrents. `a` adds a magnet or a .torrent file."),
            "{screen}"
        );
        assert!(screen.contains("q quit"), "{screen}");
    }

    #[test]
    fn a_magnet_in_the_add_buffer_is_visible_with_the_last_error() {
        let mut v = View {
            prompt: Some(Prompt::Add("magnet:?xt=urn:btih:abc".to_owned())),
            message: "parsing the magnet link: bad hash".to_owned(),
            ..View::default()
        };
        let screen = render(&v, 110, 24);
        assert!(
            screen.contains("Add a magnet link or a .torrent file"),
            "{screen}"
        );
        assert!(screen.contains("magnet:?xt=urn:btih:abc▏"), "{screen}");
        assert!(screen.contains("bad hash"), "{screen}");
        assert!(screen.contains("enter add · esc cancel"), "{screen}");
        v.update(Action::Cancel);
        assert!(!render(&v, 110, 24).contains("Add a magnet"));
    }

    #[test]
    fn confirm_names_the_torrent() {
        let mut v = view();
        v.update(Action::ConfirmRemove { delete_files: true });
        let screen = render(&v, 110, 30);
        assert!(
            screen.contains("Remove debian.iso and delete its files?"),
            "{screen}"
        );
        v.update(Action::Cancel);
        v.update(Action::ConfirmRemove {
            delete_files: false,
        });
        let screen = render(&v, 110, 30);
        assert!(
            screen.contains("Remove debian.iso? Its files are kept."),
            "{screen}"
        );
    }

    #[test]
    fn peers_render_from_the_view_model() {
        let mut v = view();
        v.update(Action::Pane(Pane::Peers));
        let screen = render(&v, 110, 30);
        assert!(screen.contains("No peers connected."), "{screen}");
        v.peers = vec![PeerRow {
            addr: "10.0.0.2:6881".parse().unwrap(),
            pieces_have: 3,
            piece_count: 4,
            downloaded: 0,
            uploaded: 0,
            down_rate: 2048,
            up_rate: 0,
            am_choking: true,
            peer_choking: false,
            am_interested: true,
            peer_interested: false,
            incoming: false,
            utp: true,
        }];
        let screen = render(&v, 110, 30);
        assert!(screen.contains("10.0.0.2:6881"), "{screen}");
        assert!(screen.contains("75.0%"), "{screen}");
        assert!(screen.contains("2.0 KiB/s"), "{screen}");
        assert!(screen.contains("C.I..U"), "{screen}");
        assert!(
            screen.contains("a add peer"),
            "footer follows the pane: {screen}"
        );
    }

    #[test]
    fn trackers_render_from_the_view_model() {
        let mut v = view();
        v.update(Action::Pane(Pane::Trackers));
        assert!(render(&v, 110, 30).contains("No trackers"));
        v.trackers = vec![
            TrackerRow {
                tier: 0,
                url: "http://good/announce".to_owned(),
                state: TrackerState::Ok {
                    interval: Some(Duration::from_secs(1800)),
                    seeders: Some(12),
                    leechers: Some(3),
                    peers: 15,
                },
            },
            TrackerRow {
                tier: 0,
                url: "http://dead/announce".to_owned(),
                state: TrackerState::Failed("connection refused".to_owned()),
            },
            TrackerRow {
                tier: 1,
                url: "udp://later:80".to_owned(),
                state: TrackerState::Waiting,
            },
        ];
        let screen = render(&v, 120, 30);
        assert!(screen.contains("http://good/announce"), "{screen}");
        assert!(screen.contains("30m00s"), "{screen}");
        assert!(screen.contains("12"), "{screen}");
        assert!(
            screen.contains("http://dead/announce — connection refused"),
            "{screen}"
        );
        assert!(screen.contains("waiting"), "{screen}");
        assert!(screen.contains("tab files"), "{screen}");
    }

    #[test]
    fn the_footer_follows_the_focus_and_shows_the_last_message() {
        let mut v = view();
        v.focus = Focus::Detail;
        v.message = "file 1: low".to_owned();
        let screen = render(&v, 120, 30);
        assert!(screen.contains("file 1: low · esc back"), "{screen}");
        assert!(screen.contains("s/l/n/h"), "{screen}");
        v.update(Action::Help);
        let screen = render(&v, 120, 30);
        assert!(screen.contains("peer flags:"), "{screen}");
        v.update(Action::DismissHelp);
        assert!(!render(&v, 120, 30).contains("peer flags:"));
    }

    #[test]
    fn the_filter_shows_in_the_title_and_the_footer() {
        let mut v = view();
        v.update(Action::OpenFilter);
        v.update(Action::Input('a'));
        v.update(Action::Input('r'));
        let screen = render(&v, 120, 30);
        assert!(screen.contains("filter: ar"), "{screen}");
        assert!(screen.contains("Arch.iso"), "{screen}");
        assert!(!screen.contains("debian.iso"), "{screen}");
    }

    #[test]
    fn alerts_show_the_newest_lines() {
        let mut v = view();
        let ih = v.torrents[0].info_hash;
        for piece in 0..6 {
            v.record_alert(ih, &dravity_engine::Alert::HashFailed { piece });
        }
        let screen = render(&v, 110, 30);
        assert!(screen.contains("piece 5 failed"), "{screen}");
        assert!(screen.contains("piece 2 failed"), "{screen}");
        assert!(!screen.contains("piece 1 failed"), "{screen}");
    }

    #[test]
    fn clicks_map_to_table_rows() {
        let area = Rect::new(0, 1, 50, 6);
        // Border at y=1, header at y=2, rows at y=3..=5.
        assert_eq!(table_row_at(area, 0, 5, 3), Some(0));
        assert_eq!(table_row_at(area, 0, 5, 5), Some(2));
        assert_eq!(table_row_at(area, 0, 5, 2), None, "header");
        assert_eq!(table_row_at(area, 0, 5, 6), None, "bottom border");
        assert_eq!(table_row_at(area, 0, 0, 3), None, "left border");
        // Selection 5 of 3 visible rows: rows 3..=5 are shown.
        assert_eq!(table_row_at(area, 5, 5, 3), Some(3));

        let v = view();
        let a = areas(Rect::new(0, 0, 100, 30), &v);
        assert_eq!(a.header.height, 1);
        assert!(a.torrents.height >= 4 && a.detail.height >= a.torrents.height);
    }
}
