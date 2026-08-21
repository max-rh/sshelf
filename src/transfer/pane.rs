//! One side of the transfer screen: a fuzzy-filterable directory listing with navigation.
//!
//! A `Pane` is source-agnostic *state* — the screen loads its entries (the local side via
//! [`read_local_dir`], the remote side via the worker) and hands them back with
//! [`Pane::set_entries`]. The filter / selection / `visible` logic mirrors the key-picker file
//! browser (`ui/browse.rs`); the local and remote listings are deliberately *not* unified
//! behind a synchronous trait, because a remote `list()` would block the UI loop the worker
//! exists to keep responsive.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::search;

use super::RemoteEntry;

/// Which machine a pane browses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

/// A directory entry shown in a pane. `name` is the bare basename; `size` is the file size in
/// bytes (used as the progress total when transferring a single file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
}

impl PaneEntry {
    /// The synthetic parent entry shown at the top of a directory.
    fn parent() -> Self {
        PaneEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            size: 0,
        }
    }

    pub fn is_parent(&self) -> bool {
        self.name == ".."
    }

    /// Display label: `name/` for directories, `name@` for symlinks (ls -F style), else `name`.
    /// Control characters are stripped so a hostile filename can't scramble the layout.
    pub fn label(&self) -> String {
        let name: String = self.name.chars().filter(|c| !c.is_control()).collect();
        if self.is_dir {
            format!("{name}/")
        } else if self.is_symlink {
            format!("{name}@")
        } else {
            name
        }
    }
}

impl From<RemoteEntry> for PaneEntry {
    fn from(e: RemoteEntry) -> Self {
        PaneEntry {
            name: e.name,
            is_dir: e.is_dir,
            is_symlink: e.is_symlink,
            size: e.size,
        }
    }
}

pub struct Pane {
    pub cwd: PathBuf,
    entries: Vec<PaneEntry>,
    /// Fuzzy filter over entry labels.
    query: String,
    /// Selection index into the *visible* (filtered) entries.
    selected: usize,
    /// Entries marked for a batch send, as indices into `entries`. **Positional**: any listing
    /// replacement (a directory change or a refresh) drops them, because the indices would no
    /// longer mean the same files. See D-026.
    marks: HashSet<usize>,
    /// A listing is in flight (remote pane between request and reply).
    pub loading: bool,
    /// The last listing error, shown in place of entries.
    pub error: Option<String>,
}

impl Pane {
    pub fn new(cwd: PathBuf) -> Self {
        Pane {
            cwd,
            entries: Vec::new(),
            query: String::new(),
            selected: 0,
            marks: HashSet::new(),
            loading: true,
            error: None,
        }
    }

    /// Move into `dir`: reset the view and mark it loading. The caller then loads the entries
    /// (synchronously for local, or via the worker for remote) and calls [`set_entries`].
    pub fn navigate_to(&mut self, dir: PathBuf) {
        self.cwd = dir;
        self.entries.clear();
        self.query.clear();
        self.selected = 0;
        self.marks.clear();
        self.loading = true;
        self.error = None;
    }

    /// The parent of the current directory, if any (for "go up").
    pub fn parent(&self) -> Option<PathBuf> {
        self.cwd.parent().map(Path::to_path_buf)
    }

    /// Replace the listing for the current directory. Prepends a `..` entry when the directory
    /// has a parent, clears the loading/error state, and clamps the selection.
    pub fn set_entries(&mut self, entries: Vec<PaneEntry>) {
        let mut all = Vec::with_capacity(entries.len() + 1);
        if self.cwd.parent().is_some() {
            all.push(PaneEntry::parent());
        }
        all.extend(entries);
        self.entries = all;
        // Marks index into the list we just replaced, so they can no longer be trusted.
        self.marks.clear();
        self.loading = false;
        self.error = None;
        let visible = self.visible().len();
        if self.selected >= visible {
            self.selected = visible.saturating_sub(1);
        }
    }

    /// Record a listing failure to show in place of entries.
    pub fn set_error(&mut self, message: String) {
        self.entries.clear();
        self.marks.clear();
        self.loading = false;
        self.error = Some(message);
    }

    /// Indices into the full entry list that match the current filter, best-first.
    pub fn visible(&self) -> Vec<usize> {
        let labels: Vec<String> = self.entries.iter().map(PaneEntry::label).collect();
        search::fuzzy_filter(&labels, &self.query)
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// The visible selection index, clamped to the current listing.
    pub fn selected(&self) -> usize {
        self.selected.min(self.visible().len().saturating_sub(1))
    }

    /// The entry under the selection, if any.
    pub fn selected_entry(&self) -> Option<&PaneEntry> {
        let visible = self.visible();
        visible.get(self.selected).map(|&i| &self.entries[i])
    }

    /// Whether the (unfiltered) listing already holds an entry named `name`, ignoring the
    /// synthetic `..`. Used to avoid silently clobbering a destination on transfer.
    pub fn contains(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|e| !e.is_parent() && e.name == name)
    }

    /// Entries to display, in filtered order, paired with their label and whether they're marked.
    pub fn rows(&self) -> Vec<(&PaneEntry, String, bool)> {
        self.visible()
            .into_iter()
            .map(|i| {
                (
                    &self.entries[i],
                    self.entries[i].label(),
                    self.marks.contains(&i),
                )
            })
            .collect()
    }

    /// Toggle the mark on the selected entry. The synthetic `..` can't be marked — it isn't a
    /// file. Returns `false` when there was nothing to toggle.
    pub fn toggle_mark(&mut self) -> bool {
        let visible = self.visible();
        let Some(&i) = visible.get(self.selected) else {
            return false;
        };
        if self.entries[i].is_parent() {
            return false;
        }
        if !self.marks.insert(i) {
            self.marks.remove(&i);
        }
        true
    }

    /// Mark every entry the filter currently shows — or, if they are all marked already, clear
    /// every mark (so the same key both selects and deselects all). Returns the resulting count.
    pub fn toggle_mark_all(&mut self) -> usize {
        let markable: Vec<usize> = self
            .visible()
            .into_iter()
            .filter(|&i| !self.entries[i].is_parent())
            .collect();
        if !markable.is_empty() && markable.iter().all(|i| self.marks.contains(i)) {
            self.marks.clear();
        } else {
            self.marks.extend(markable);
        }
        self.marks.len()
    }

    /// Drop every mark. Returns `false` if there were none (so Esc can fall through).
    pub fn clear_marks(&mut self) -> bool {
        if self.marks.is_empty() {
            return false;
        }
        self.marks.clear();
        true
    }

    pub fn marked_count(&self) -> usize {
        self.marks.len()
    }

    /// The marked entries in listing order (not filter order), so a batch send is deterministic
    /// no matter what the filter looked like when each mark was set.
    pub fn marked_entries(&self) -> Vec<&PaneEntry> {
        let mut idx: Vec<usize> = self.marks.iter().copied().collect();
        idx.sort_unstable();
        idx.into_iter().map(|i| &self.entries[i]).collect()
    }

    /// Put the selection on the entry named `name`, if the filter currently shows it. Used
    /// after creating a directory, so the new one is under the cursor.
    pub fn select_name(&mut self, name: &str) {
        if let Some(pos) = self
            .visible()
            .iter()
            .position(|&i| self.entries[i].name == name)
        {
            self.selected = pos;
        }
    }

    pub fn move_sel(&mut self, delta: isize) {
        let n = self.visible().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, n as isize - 1) as usize;
    }

    pub fn push_query(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    /// Remove the last filter char. Returns `false` if the filter was already empty (so the
    /// caller can treat a further Backspace as "go up").
    pub fn pop_query(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        self.query.pop();
        self.selected = 0;
        true
    }

    /// Clear the filter. Returns `false` if it was already empty (so Esc can mean "close").
    pub fn clear_query(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        self.query.clear();
        self.selected = 0;
        true
    }
}

/// Read a local directory into pane entries (dirs first, then case-insensitive by name). The
/// `..` entry is added by [`Pane::set_entries`], not here.
pub fn read_local_dir(cwd: &Path) -> Result<Vec<PaneEntry>, String> {
    let rd = std::fs::read_dir(cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
    let mut items: Vec<PaneEntry> = rd
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // file_type() does not follow symlinks, so a symlink reads as a symlink.
            let ft = e.file_type().ok();
            let is_symlink = ft.is_some_and(|t| t.is_symlink());
            let meta = e.metadata().ok(); // follows symlinks (so a link-to-dir shows as a dir)
            let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
            let size = meta.as_ref().map(std::fs::Metadata::len).unwrap_or(0);
            PaneEntry {
                name,
                is_dir,
                is_symlink,
                size,
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let root = std::env::temp_dir().join(format!("sshelf-pane-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("alpha.txt"), b"hello").unwrap();
        std::fs::write(root.join("beta.log"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("alpha.txt"), root.join("zlink")).unwrap();
        root
    }

    fn entries() -> Vec<PaneEntry> {
        vec![
            PaneEntry {
                name: "sub".into(),
                is_dir: true,
                is_symlink: false,
                size: 0,
            },
            PaneEntry {
                name: "alpha.txt".into(),
                is_dir: false,
                is_symlink: false,
                size: 5,
            },
        ]
    }

    #[test]
    fn read_local_dir_sorts_dirs_first_and_flags_symlinks() {
        let items = read_local_dir(&scratch()).unwrap();
        assert_eq!(items[0].name, "sub");
        assert!(items[0].is_dir);
        let link = items.iter().find(|e| e.name == "zlink").unwrap();
        assert!(link.is_symlink);
        let alpha = items.iter().find(|e| e.name == "alpha.txt").unwrap();
        assert_eq!(alpha.size, 5);
    }

    #[test]
    fn set_entries_prepends_parent_when_not_at_root() {
        let mut p = Pane::new(PathBuf::from("/home/user/dir"));
        p.set_entries(entries());
        assert_eq!(p.rows()[0].0.name, "..");
        assert_eq!(p.rows()[0].1, "../");
        assert!(!p.loading);
    }

    #[test]
    fn root_has_no_parent_entry() {
        let mut p = Pane::new(PathBuf::from("/"));
        p.set_entries(entries());
        assert!(p.rows().iter().all(|(e, ..)| !e.is_parent()));
    }

    #[test]
    fn labels_mark_dirs_and_symlinks() {
        let dir = PaneEntry {
            name: "sub".into(),
            is_dir: true,
            is_symlink: false,
            size: 0,
        };
        let link = PaneEntry {
            name: "lnk".into(),
            is_dir: false,
            is_symlink: true,
            size: 0,
        };
        assert_eq!(dir.label(), "sub/");
        assert_eq!(link.label(), "lnk@");
    }

    #[test]
    fn control_chars_are_stripped_from_labels() {
        let nasty = PaneEntry {
            name: "ev\u{1b}[2Jil".into(),
            is_dir: false,
            is_symlink: false,
            size: 0,
        };
        let label = nasty.label();
        // The ESC that would arm the escape sequence is gone, so the leftover "[2J" is inert.
        assert!(!label.chars().any(char::is_control));
        assert_eq!(label, "ev[2Jil");
    }

    #[test]
    fn typing_filters_and_navigate_clears_query() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries());
        p.push_query('a');
        p.push_query('l');
        let names: Vec<&str> = p.rows().iter().map(|(e, ..)| e.name.as_str()).collect();
        assert!(names.contains(&"alpha.txt"));
        assert!(!names.contains(&"sub"));
        p.navigate_to(PathBuf::from("/d/sub"));
        assert_eq!(p.query(), "");
        assert!(p.loading);
    }

    #[test]
    fn move_sel_clamps_to_visible() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries()); // 3 rows: .., sub, alpha.txt
        p.move_sel(-1);
        assert_eq!(p.selected(), 0);
        p.move_sel(100);
        assert_eq!(p.selected(), 2);
        assert_eq!(p.selected_entry().unwrap().name, "alpha.txt");
    }

    #[test]
    fn contains_ignores_the_parent_entry() {
        let mut p = Pane::new(PathBuf::from("/home/user/dir"));
        p.set_entries(entries());
        assert!(p.contains("alpha.txt"));
        assert!(p.contains("sub"));
        assert!(!p.contains(".."));
        assert!(!p.contains("missing"));
    }

    #[test]
    fn marking_toggles_and_skips_the_parent_entry() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries()); // rows: .., sub, alpha.txt
        // The synthetic `..` is not a file, so it can't be marked.
        assert!(!p.toggle_mark());
        assert_eq!(p.marked_count(), 0);
        p.move_sel(1);
        assert!(p.toggle_mark());
        assert_eq!(p.marked_count(), 1);
        assert_eq!(p.marked_entries()[0].name, "sub");
        // …and the same key unmarks it.
        assert!(p.toggle_mark());
        assert_eq!(p.marked_count(), 0);
    }

    #[test]
    fn mark_all_covers_the_filter_then_clears() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries());
        assert_eq!(p.toggle_mark_all(), 2); // both files; `..` excluded
        // Pressing it again when everything is marked clears every mark.
        assert_eq!(p.toggle_mark_all(), 0);
        // With a filter active it only marks what is visible.
        p.push_query('a');
        assert_eq!(p.toggle_mark_all(), 1);
        assert_eq!(p.marked_entries()[0].name, "alpha.txt");
    }

    #[test]
    fn marked_entries_follow_listing_order_not_mark_order() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries());
        p.move_sel(2); // alpha.txt, listed after sub
        p.toggle_mark();
        p.move_sel(-1); // sub
        p.toggle_mark();
        let names: Vec<&str> = p.marked_entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "alpha.txt"]);
    }

    #[test]
    fn marks_are_positional_and_die_with_the_listing() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries());
        p.move_sel(1);
        p.toggle_mark();
        assert_eq!(p.marked_count(), 1);
        // A refresh of the same directory replaces the entries the indices pointed at.
        p.set_entries(entries());
        assert_eq!(p.marked_count(), 0);
        // As does navigating away, or a listing failure.
        p.set_entries(entries());
        p.move_sel(1);
        p.toggle_mark();
        p.navigate_to(PathBuf::from("/d/sub"));
        assert_eq!(p.marked_count(), 0);
        p.set_entries(entries());
        p.move_sel(1);
        p.toggle_mark();
        p.set_error("boom".into());
        assert_eq!(p.marked_count(), 0);
    }

    #[test]
    fn clear_marks_reports_whether_there_were_any() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries());
        assert!(!p.clear_marks());
        p.move_sel(1);
        p.toggle_mark();
        assert!(p.clear_marks());
        assert_eq!(p.marked_count(), 0);
    }

    #[test]
    fn rows_report_which_entries_are_marked() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries());
        p.move_sel(1);
        p.toggle_mark();
        let marked: Vec<(&str, bool)> = p
            .rows()
            .iter()
            .map(|(e, _, m)| (e.name.as_str(), *m))
            .collect();
        assert_eq!(
            marked,
            vec![("..", false), ("sub", true), ("alpha.txt", false)]
        );
    }

    #[test]
    fn select_name_moves_the_cursor_to_a_visible_entry() {
        let mut p = Pane::new(PathBuf::from("/d"));
        p.set_entries(entries());
        p.select_name("alpha.txt");
        assert_eq!(p.selected_entry().unwrap().name, "alpha.txt");
        // A name that isn't listed leaves the selection alone.
        p.select_name("nope");
        assert_eq!(p.selected_entry().unwrap().name, "alpha.txt");
    }

    #[test]
    fn set_error_replaces_entries() {
        let mut p = Pane::new(PathBuf::from("/srv"));
        p.set_error("permission denied".into());
        assert!(!p.loading);
        assert_eq!(p.error.as_deref(), Some("permission denied"));
        assert!(p.rows().is_empty());
    }
}
