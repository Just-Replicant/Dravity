//! The add-torrent form: source, directory, pause, extra peers and file
//! priorities.

use dravity_engine::Priority;

/// One file the user can include or skip when adding a `.torrent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddFilePick {
    /// Index in the torrent's file list (padding files are left out).
    pub(crate) index: usize,
    pub(crate) path: String,
    pub(crate) length: u64,
    pub(crate) priority: Priority,
}

/// Which field of [`AddForm`] is being edited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AddField {
    #[default]
    Source,
    Dir,
    Paused,
    Peers,
    /// File row `index` in [`AddForm::files`].
    File(usize),
}

/// Everything the add prompt collects before the session is asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AddForm {
    /// A magnet link or a `.torrent` path.
    pub(crate) source: String,
    /// Download directory; empty uses the session's.
    pub(crate) download_dir: String,
    /// Add without starting.
    pub(crate) paused: bool,
    /// Extra peers, as a comma- or space-separated `host:port` list.
    pub(crate) peers: String,
    /// Files of a `.torrent` after it has been inspected; empty for a
    /// magnet (priorities can be set later from the files pane).
    pub(crate) files: Vec<AddFilePick>,
    /// Whether [`files`](Self::files) was filled from a `.torrent` (even
    /// if that torrent has no files, which should not happen). Distinguishes
    /// "not inspected yet" from "inspected, no files to pick".
    pub(crate) inspected: bool,
    pub(crate) field: AddField,
}

impl AddForm {
    /// An empty form, source field focused.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A form whose source is already set (tests, and a pasted magnet).
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_source(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            ..Self::default()
        }
    }

    /// The text field currently being edited, if any (paused and file
    /// rows are not text).
    pub(crate) fn text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            AddField::Source => Some(&mut self.source),
            AddField::Dir => Some(&mut self.download_dir),
            AddField::Peers => Some(&mut self.peers),
            AddField::Paused | AddField::File(_) => None,
        }
    }

    /// The file row under the cursor, if the files list is focused.
    pub(crate) fn selected_file_mut(&mut self) -> Option<&mut AddFilePick> {
        match self.field {
            AddField::File(i) => self.files.get_mut(i),
            _ => None,
        }
    }

    /// Cycles the focused field forward, wrapping. File rows are visited
    /// in order after the peer list.
    pub(crate) fn next_field(&mut self) {
        self.field = match self.field {
            AddField::Source => AddField::Dir,
            AddField::Dir => AddField::Paused,
            AddField::Paused => AddField::Peers,
            AddField::Peers if self.files.is_empty() => AddField::Source,
            AddField::Peers => AddField::File(0),
            AddField::File(i) if i + 1 < self.files.len() => AddField::File(i + 1),
            AddField::File(_) => AddField::Source,
        };
    }

    /// Cycles the focused field backward, wrapping.
    pub(crate) fn prev_field(&mut self) {
        self.field = match self.field {
            AddField::Source if self.files.is_empty() => AddField::Peers,
            AddField::Source => AddField::File(self.files.len().saturating_sub(1)),
            AddField::Dir => AddField::Source,
            AddField::Paused => AddField::Dir,
            AddField::Peers => AddField::Paused,
            AddField::File(0) => AddField::Peers,
            AddField::File(i) => AddField::File(i.saturating_sub(1)),
        };
    }

    /// Moves the file cursor, if a file is focused.
    pub(crate) fn step_file(&mut self, down: bool) {
        let AddField::File(i) = self.field else {
            return;
        };
        if self.files.is_empty() {
            return;
        }
        let last = self.files.len().saturating_sub(1);
        self.field = AddField::File(if down {
            i.saturating_add(1).min(last)
        } else {
            i.saturating_sub(1)
        });
    }

    /// Toggles "start paused", or cycles the selected file's priority.
    pub(crate) fn toggle(&mut self) {
        match self.field {
            AddField::Paused => self.paused = !self.paused,
            AddField::File(_) => {
                if let Some(file) = self.selected_file_mut() {
                    file.priority = cycle_priority(file.priority);
                }
            }
            _ => {}
        }
    }

    /// Sets the selected file's priority.
    pub(crate) fn set_file_priority(&mut self, priority: Priority) {
        if let Some(file) = self.selected_file_mut() {
            file.priority = priority;
        }
    }

    /// Priorities for [`dravity_session::AddTorrentOptions`], one per file
    /// of the metainfo (padding files stay [`Priority::Normal`]). Empty
    /// when the form has no file picks (a magnet, or not inspected).
    #[must_use]
    pub(crate) fn priorities(&self, file_count: usize) -> Vec<Priority> {
        if self.files.is_empty() {
            return Vec::new();
        }
        let mut out = vec![Priority::Normal; file_count];
        for f in &self.files {
            if let Some(slot) = out.get_mut(f.index) {
                *slot = f.priority;
            }
        }
        out
    }
}

fn cycle_priority(p: Priority) -> Priority {
    match p {
        Priority::Skip => Priority::Low,
        Priority::Low => Priority::Normal,
        Priority::Normal => Priority::High,
        Priority::High => Priority::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::{AddField, AddFilePick, AddForm};
    use dravity_engine::Priority;

    fn file(index: usize, name: &str) -> AddFilePick {
        AddFilePick {
            index,
            path: name.to_owned(),
            length: 100,
            priority: Priority::Normal,
        }
    }

    #[test]
    fn tab_visits_every_field_and_wraps() {
        let mut f = AddForm::with_source("x.torrent");
        f.files = vec![file(0, "a"), file(2, "b")];
        let mut seen = vec![f.field];
        for _ in 0..6 {
            f.next_field();
            seen.push(f.field);
        }
        assert_eq!(
            seen,
            [
                AddField::Source,
                AddField::Dir,
                AddField::Paused,
                AddField::Peers,
                AddField::File(0),
                AddField::File(1),
                AddField::Source,
            ]
        );
        f.prev_field();
        assert_eq!(f.field, AddField::File(1));
        f.prev_field();
        assert_eq!(f.field, AddField::File(0));
        f.prev_field();
        assert_eq!(f.field, AddField::Peers);
    }

    #[test]
    fn toggle_pauses_and_cycles_file_priority() {
        let mut f = AddForm::new();
        f.field = AddField::Paused;
        f.toggle();
        assert!(f.paused);
        f.files = vec![file(0, "a")];
        f.field = AddField::File(0);
        f.toggle();
        assert_eq!(f.files[0].priority, Priority::High);
        f.toggle();
        assert_eq!(f.files[0].priority, Priority::Skip);
        f.set_file_priority(Priority::Low);
        assert_eq!(f.files[0].priority, Priority::Low);
    }

    #[test]
    fn priorities_fill_the_metainfo_file_list() {
        let mut f = AddForm::new();
        assert!(f.priorities(3).is_empty());
        f.files = vec![
            AddFilePick {
                index: 0,
                path: "a".to_owned(),
                length: 1,
                priority: Priority::High,
            },
            AddFilePick {
                index: 2,
                path: "c".to_owned(),
                length: 1,
                priority: Priority::Skip,
            },
        ];
        assert_eq!(
            f.priorities(4),
            [
                Priority::High,
                Priority::Normal, // padding or unlisted
                Priority::Skip,
                Priority::Normal,
            ]
        );
    }
}
