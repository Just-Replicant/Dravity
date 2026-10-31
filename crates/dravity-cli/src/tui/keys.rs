//! Maps key presses to [`Action`]s. This and [`View::update`] are the only
//! input path; the mapping depends on what has the focus.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dravity_engine::Priority;

use super::view::{Action, Focus, Pane, Prompt, View};

/// Maps a key press to an action.
pub(crate) fn action(key: KeyEvent, view: &View) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    // Ctrl-C quits from anywhere, even a prompt.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }
    if view.help {
        return Action::DismissHelp;
    }
    match &view.prompt {
        Some(Prompt::Confirm { .. }) => match key.code {
            KeyCode::Enter => Action::Submit,
            KeyCode::Esc => Action::Cancel,
            _ => Action::None,
        },
        Some(_) => text_entry(key),
        None => match view.focus {
            Focus::Torrents => torrents(key),
            Focus::Detail => detail(key, view.pane),
        },
    }
}

fn text_entry(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter => Action::Submit,
        KeyCode::Esc => Action::Cancel,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Action::Input(c)
        }
        _ => Action::None,
    }
}

fn torrents(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Tab | KeyCode::BackTab => Action::Tab,
        KeyCode::Enter => Action::OpenDetail,
        KeyCode::Char(' ' | 'p') => Action::TogglePause,
        KeyCode::Char('a') => Action::OpenAdd,
        KeyCode::Char('1') => Action::Pane(Pane::Files),
        KeyCode::Char('2') => Action::Pane(Pane::Peers),
        KeyCode::Char('3') => Action::Pane(Pane::Trackers),
        // Shift+D deletes. Some terminals send `D`, others `d` with Shift.
        KeyCode::Char('D') | KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Action::ConfirmRemove { delete_files: true }
        }
        KeyCode::Char('d') => Action::ConfirmRemove {
            delete_files: false,
        },
        KeyCode::Char('D') => Action::ConfirmRemove { delete_files: true },
        KeyCode::Char('o') => Action::CycleSort,
        KeyCode::Char('/') => Action::OpenFilter,
        KeyCode::Char('?') => Action::Help,
        _ => Action::None,
    }
}

fn detail(key: KeyEvent, pane: Pane) -> Action {
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Esc => Action::Back,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Tab | KeyCode::BackTab => Action::Tab,
        KeyCode::Char('1') => Action::Pane(Pane::Files),
        KeyCode::Char('2') => Action::Pane(Pane::Peers),
        KeyCode::Char('3') => Action::Pane(Pane::Trackers),
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('a') if pane == Pane::Peers => Action::OpenAddPeer,
        KeyCode::Char(c) if pane == Pane::Files => match c {
            's' => Action::SetPriority(Priority::Skip),
            'l' => Action::SetPriority(Priority::Low),
            'n' => Action::SetPriority(Priority::Normal),
            'h' => Action::SetPriority(Priority::High),
            _ => Action::None,
        },
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use dravity_engine::Priority;

    use super::action;
    use crate::tui::view::{Action, Focus, Pane, Prompt, View};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn detail(pane: Pane) -> View {
        View {
            focus: Focus::Detail,
            pane,
            ..View::default()
        }
    }

    #[test]
    fn torrent_keys() {
        let v = View::default();
        assert_eq!(action(key('q'), &v), Action::Quit);
        assert_eq!(action(code(KeyCode::Esc), &v), Action::Quit);
        assert_eq!(action(key(' '), &v), Action::TogglePause);
        assert_eq!(action(key('p'), &v), Action::TogglePause);
        assert_eq!(action(key('a'), &v), Action::OpenAdd);
        assert_eq!(action(key('1'), &v), Action::Pane(Pane::Files));
        assert_eq!(action(key('2'), &v), Action::Pane(Pane::Peers));
        assert_eq!(action(key('3'), &v), Action::Pane(Pane::Trackers));
        assert_eq!(
            action(key('d'), &v),
            Action::ConfirmRemove {
                delete_files: false
            }
        );
        assert_eq!(
            action(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT), &v),
            Action::ConfirmRemove { delete_files: true }
        );
        assert_eq!(
            action(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::SHIFT), &v),
            Action::ConfirmRemove { delete_files: true },
            "shift+d deletes when the terminal keeps the letter lowercase"
        );
        assert_eq!(action(key('o'), &v), Action::CycleSort);
        assert_eq!(action(key('/'), &v), Action::OpenFilter);
        assert_eq!(action(key('?'), &v), Action::Help);
        assert_eq!(action(code(KeyCode::Enter), &v), Action::OpenDetail);
        assert_eq!(action(code(KeyCode::Tab), &v), Action::Tab);
        assert_eq!(action(key('h'), &v), Action::None, "no priorities here");
    }

    #[test]
    fn detail_keys_follow_the_pane() {
        let files = detail(Pane::Files);
        assert_eq!(action(code(KeyCode::Esc), &files), Action::Back);
        assert_eq!(action(key('q'), &files), Action::Quit);
        assert_eq!(
            action(key('h'), &files),
            Action::SetPriority(Priority::High)
        );
        assert_eq!(
            action(key('s'), &files),
            Action::SetPriority(Priority::Skip)
        );
        assert_eq!(action(key('a'), &files), Action::None);
        assert_eq!(action(key(' '), &files), Action::None);
        assert_eq!(action(key('2'), &files), Action::Pane(Pane::Peers));
        let peers = detail(Pane::Peers);
        assert_eq!(action(key('h'), &peers), Action::None, "priorities off");
        assert_eq!(action(key('a'), &peers), Action::OpenAddPeer);
        assert_eq!(action(key('3'), &peers), Action::Pane(Pane::Trackers));
        let trackers = detail(Pane::Trackers);
        assert_eq!(action(key('a'), &trackers), Action::None);
        assert_eq!(action(key('1'), &trackers), Action::Pane(Pane::Files));
    }

    #[test]
    fn prompts_take_text_and_confirm_takes_only_enter() {
        let mut v = View {
            prompt: Some(Prompt::Add(String::new())),
            ..View::default()
        };
        assert_eq!(action(key('q'), &v), Action::Input('q'), "typed, not quit");
        assert_eq!(action(code(KeyCode::Backspace), &v), Action::Backspace);
        assert_eq!(action(code(KeyCode::Enter), &v), Action::Submit);
        assert_eq!(action(code(KeyCode::Esc), &v), Action::Cancel);
        assert_eq!(
            action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), &v),
            Action::Quit
        );
        v.prompt = Some(Prompt::Confirm {
            info_hash: "ab".repeat(20).parse().unwrap(),
            name: "x".to_owned(),
            delete_files: true,
        });
        assert_eq!(action(key('y'), &v), Action::None);
        assert_eq!(action(key('d'), &v), Action::None);
        assert_eq!(action(code(KeyCode::Enter), &v), Action::Submit);
        assert_eq!(action(code(KeyCode::Esc), &v), Action::Cancel);
    }

    #[test]
    fn help_closes_on_the_next_key_and_releases_are_ignored() {
        let v = View {
            help: true,
            ..View::default()
        };
        assert_eq!(action(key('q'), &v), Action::DismissHelp);
        let mut release = key('q');
        release.kind = KeyEventKind::Release;
        assert_eq!(action(release, &View::default()), Action::None);
    }
}
