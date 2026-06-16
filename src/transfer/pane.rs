//! One side of the transfer screen: a fuzzy-filterable directory listing with navigation.
//!
//! A `Pane` is source-agnostic *state* — the screen loads its entries (the local side via
//! [`read_local_dir`], the remote side via the worker) and hands them back with
//! [`Pane::set_entries`]. The filter / selection / `visible` logic mirrors the key-picker file
//! browser (`ui/browse.rs`); the local and remote listings are deliberately *not* unified
//! behind a synchronous trait, because a remote `list()` would block the UI loop the worker
//! exists to keep responsive.

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

    fn is_parent(&self) -> bool {
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
    pub side: Side,
    pub cwd: PathBuf,
    entries: Vec<PaneEntry>,
    /// Fuzzy filter over entry labels.
    query: String,
    /// Selection index into the *visible* (filtered) entries.
    selected: usize,
    /// A listing is in flight (remote pane between request and reply).
    pub loading: bool,
    /// The last listing error, shown in place of entries.
    pub error: Option<String>,
}

impl Pane {
    pub fn new(side: Side, cwd: PathBuf) -> Self {
        Pane {
            side,
            cwd,
            entries: Vec::new(),
            query: String::new(),
            selected: 0,
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

    /// Entries to display, in filtered order, paired with their label.
    pub fn rows(&self) -> Vec<(&PaneEntry, String)> {
        self.visible()
            .into_iter()
            .map(|i| (&self.entries[i], self.entries[i].label()))
            .collect()
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
        let mut p = Pane::new(Side::Local, PathBuf::from("/home/user/dir"));
        p.set_entries(entries());
        assert_eq!(p.rows()[0].0.name, "..");
        assert_eq!(p.rows()[0].1, "../");
        assert!(!p.loading);
    }

    #[test]
    fn root_has_no_parent_entry() {
        let mut p = Pane::new(Side::Local, PathBuf::from("/"));
        p.set_entries(entries());
        assert!(p.rows().iter().all(|(e, _)| !e.is_parent()));
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
        let mut p = Pane::new(Side::Local, PathBuf::from("/d"));
        p.set_entries(entries());
        p.push_query('a');
        p.push_query('l');
        let names: Vec<&str> = p.rows().iter().map(|(e, _)| e.name.as_str()).collect();
        assert!(names.contains(&"alpha.txt"));
        assert!(!names.contains(&"sub"));
        p.navigate_to(PathBuf::from("/d/sub"));
        assert_eq!(p.query(), "");
        assert!(p.loading);
    }

    #[test]
    fn move_sel_clamps_to_visible() {
        let mut p = Pane::new(Side::Local, PathBuf::from("/d"));
        p.set_entries(entries()); // 3 rows: .., sub, alpha.txt
        p.move_sel(-1);
        assert_eq!(p.selected(), 0);
        p.move_sel(100);
        assert_eq!(p.selected(), 2);
        assert_eq!(p.selected_entry().unwrap().name, "alpha.txt");
    }

    #[test]
    fn set_error_replaces_entries() {
        let mut p = Pane::new(Side::Remote, PathBuf::from("/srv"));
        p.set_error("permission denied".into());
        assert!(!p.loading);
        assert_eq!(p.error.as_deref(), Some("permission denied"));
        assert!(p.rows().is_empty());
    }
}
