//! Maps key presses to [`Action`]s. This and [`View::update`] are the only
//! input path; the mapping depends on what has the focus.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dravity_engine::Priority;

use super::add::AddField;
use super::view::{Action, Direction, Focus, Pane, Prompt, View};

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
        Some(Prompt::BanList) => ban_list(key),
        Some(Prompt::Add(form)) => add_form(key, form.field),
        Some(Prompt::QueueCaps { .. }) => form_keys(key),
        Some(_) => text_entry(key),
        None => match view.focus {
            Focus::Torrents => torrents(key),
            Focus::Detail => detail(key, view.pane),
        },
    }
}

fn form_keys(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Tab => Action::NextField,
        KeyCode::BackTab => Action::PrevField,
        _ => text_entry(key),
    }
}

fn add_form(key: KeyEvent, field: AddField) -> Action {
    match (field, key.code) {
        (_, KeyCode::Tab) => Action::NextField,
        (_, KeyCode::BackTab) => Action::PrevField,
        (AddField::Paused, KeyCode::Char(' ')) => Action::Toggle,
        (AddField::File(_), KeyCode::Char(' ')) => Action::Toggle,
        (AddField::File(_), KeyCode::Up | KeyCode::Char('k')) => Action::Up,
        (AddField::File(_), KeyCode::Down | KeyCode::Char('j')) => Action::Down,
        (AddField::File(_), KeyCode::Char('s')) => Action::SetPriority(Priority::Skip),
        (AddField::File(_), KeyCode::Char('l')) => Action::SetPriority(Priority::Low),
        (AddField::File(_), KeyCode::Char('n')) => Action::SetPriority(Priority::Normal),
        (AddField::File(_), KeyCode::Char('h')) => Action::SetPriority(Priority::High),
        _ => text_entry(key),
    }
}

fn ban_list(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::Cancel,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Char('u') => Action::UnbanSelected,
        KeyCode::Char('U') => Action::OpenUnban,
        KeyCode::Char('b') => Action::OpenBan,
        _ => Action::None,
    }
}

fn text_entry(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter => Action::Submit,
        KeyCode::Esc => Action::Cancel,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Tab => Action::NextField,
        KeyCode::BackTab => Action::PrevField,
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
        KeyCode::Char('Q') | KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Action::OpenQueueCaps
        }
        KeyCode::Char('C') | KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Action::OpenConnections
        }
        KeyCode::Char('P') | KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Action::OpenListenPort
        }
        KeyCode::Char('B') | KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Action::OpenBanList
        }
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
        KeyCode::Char('L') => Action::OpenLimit(Direction::Download),
        KeyCode::Char('U') => Action::OpenLimit(Direction::Upload),
        KeyCode::Char('Q') => Action::OpenQueueCaps,
        KeyCode::Char('C') => Action::OpenConnections,
        KeyCode::Char('P') => Action::OpenListenPort,
        KeyCode::Char('b') => Action::OpenBan,
        KeyCode::Char('B') => Action::OpenBanList,
        KeyCode::Char('i') => Action::OpenIpFilter,
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
        KeyCode::Char('s') if pane == Pane::Trackers => Action::ForceScrape,
        KeyCode::Char('t') if pane == Pane::Files => Action::StreamFile,
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
    use crate::tui::add::{AddField, AddForm};
    use crate::tui::view::{Action, CapsField, Focus, Pane, Prompt, View};

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
    fn capital_l_and_u_open_the_limit_prompts() {
        use crate::tui::view::Direction;
        let v = View::default();
        assert_eq!(action(key('L'), &v), Action::OpenLimit(Direction::Download));
        assert_eq!(action(key('U'), &v), Action::OpenLimit(Direction::Upload));
    }

    #[test]
    fn session_action_keys() {
        let v = View::default();
        assert_eq!(action(key('Q'), &v), Action::OpenQueueCaps);
        assert_eq!(action(key('C'), &v), Action::OpenConnections);
        assert_eq!(action(key('P'), &v), Action::OpenListenPort);
        assert_eq!(action(key('b'), &v), Action::OpenBan);
        assert_eq!(action(key('B'), &v), Action::OpenBanList);
        assert_eq!(action(key('i'), &v), Action::OpenIpFilter);
        assert_eq!(
            action(key('q'), &v),
            Action::Quit,
            "lowercase q still quits"
        );
        assert_eq!(
            action(key('p'), &v),
            Action::TogglePause,
            "lowercase p pauses"
        );
        assert_eq!(
            action(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::SHIFT), &v),
            Action::OpenQueueCaps,
            "shift+q is queue caps when the terminal keeps the letter lowercase"
        );
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
        assert_eq!(action(key('t'), &files), Action::StreamFile);
        assert_eq!(action(key('a'), &files), Action::None);
        assert_eq!(action(key(' '), &files), Action::None);
        assert_eq!(action(key('2'), &files), Action::Pane(Pane::Peers));
        let peers = detail(Pane::Peers);
        assert_eq!(action(key('h'), &peers), Action::None, "priorities off");
        assert_eq!(action(key('a'), &peers), Action::OpenAddPeer);
        assert_eq!(action(key('t'), &peers), Action::None);
        assert_eq!(action(key('3'), &peers), Action::Pane(Pane::Trackers));
        let trackers = detail(Pane::Trackers);
        assert_eq!(action(key('a'), &trackers), Action::None);
        assert_eq!(action(key('s'), &trackers), Action::ForceScrape);
        assert_eq!(action(key('1'), &trackers), Action::Pane(Pane::Files));
    }

    #[test]
    fn prompts_take_text_and_confirm_takes_only_enter() {
        let mut v = View {
            prompt: Some(Prompt::Add(AddForm::new())),
            ..View::default()
        };
        assert_eq!(action(key('q'), &v), Action::Input('q'), "typed, not quit");
        assert_eq!(action(code(KeyCode::Backspace), &v), Action::Backspace);
        assert_eq!(action(code(KeyCode::Enter), &v), Action::Submit);
        assert_eq!(action(code(KeyCode::Esc), &v), Action::Cancel);
        assert_eq!(action(code(KeyCode::Tab), &v), Action::NextField);
        assert_eq!(
            action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), &v),
            Action::Quit
        );
        // On the paused field, space toggles instead of typing.
        if let Some(Prompt::Add(form)) = &mut v.prompt {
            form.field = AddField::Paused;
        }
        assert_eq!(action(key(' '), &v), Action::Toggle);
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
    fn ban_list_and_queue_caps_keys() {
        let v = View {
            prompt: Some(Prompt::BanList),
            ..View::default()
        };
        assert_eq!(action(key('u'), &v), Action::UnbanSelected);
        assert_eq!(action(key('U'), &v), Action::OpenUnban);
        assert_eq!(action(key('b'), &v), Action::OpenBan);
        assert_eq!(action(code(KeyCode::Esc), &v), Action::Cancel);
        assert_eq!(action(key('j'), &v), Action::Down);
        assert_eq!(action(key('q'), &v), Action::None, "q is not quit here");
        let v = View {
            prompt: Some(Prompt::QueueCaps {
                downloads: String::new(),
                seeds: String::new(),
                field: CapsField::Downloads,
            }),
            ..View::default()
        };
        assert_eq!(action(code(KeyCode::Tab), &v), Action::NextField);
        assert_eq!(action(key('3'), &v), Action::Input('3'));
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
