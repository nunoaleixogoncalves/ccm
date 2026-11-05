//! What the worktree view shows. Git runs on another thread, so this only holds results.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ccm_core::model::Session;
use ccm_core::removal::Target;
use ccm_core::worktree::{connect, Link, Scan, Worktree};
use ratatui::widgets::TableState;

/// How often Git is asked again while the view is open. Its state changes without any file
/// `ccm` watches, and each look runs a few Git commands per worktree.
const RESCAN: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// A repository, named by its main worktree.
    Header { repo: PathBuf, count: usize },
    /// An index into the scan's worktrees.
    Item(usize),
}

#[derive(Default)]
pub struct Worktrees {
    /// `None` until the first scan finishes.
    pub scan: Option<Scan>,
    /// The conversations of each worktree, in the scan's order.
    pub links: Vec<Vec<Link>>,
    pub rows: Vec<Row>,
    pub table: TableState,
    pub scanning: bool,
    last_scan: Option<Instant>,
    selected_path: Option<PathBuf>,
    /// Worktrees picked for a removal, which otherwise acts on the selected one.
    pub marked: HashSet<PathBuf>,
}

impl Worktrees {
    /// True when a new scan should start.
    pub fn due(&self) -> bool {
        !self.scanning && self.last_scan.is_none_or(|t| t.elapsed() >= RESCAN)
    }

    /// Makes the next check start a scan, unless one is running.
    pub fn refresh_soon(&mut self) {
        self.last_scan = None;
    }

    pub fn start(&mut self) {
        self.scanning = true;
    }

    pub fn finish(&mut self, scan: Scan, sessions: &[Session]) {
        self.scanning = false;
        self.last_scan = Some(Instant::now());
        let present: HashSet<&Path> = scan.worktrees.iter().filter(|w| w.is_linked()).map(|w| w.path.as_path()).collect();
        self.marked.retain(|p| present.contains(p.as_path()));
        self.scan = Some(scan);
        self.relink(sessions);
    }

    /// Drops removed worktrees at once, rather than waiting for the next scan.
    pub fn forget(&mut self, removed: &[PathBuf], sessions: &[Session]) {
        if let Some(scan) = self.scan.as_mut() {
            scan.worktrees.retain(|w| !removed.contains(&w.path));
        }
        self.marked.retain(|p| !removed.contains(p));
        self.relink(sessions);
        self.refresh_soon();
    }

    fn shown(&self) -> impl Iterator<Item = &Worktree> {
        self.rows.iter().filter_map(|r| match r {
            Row::Item(i) => self.worktrees().get(*i),
            Row::Header { .. } => None,
        })
    }

    pub fn marked_shown(&self) -> Vec<&Worktree> {
        self.shown().filter(|w| self.marked.contains(&w.path)).collect()
    }

    pub fn toggle_mark(&mut self) {
        let Some(path) = self.selected().map(|(w, _)| w.path.clone()) else { return };
        if !self.marked.remove(&path) {
            self.marked.insert(path);
        }
        self.move_selection(1);
    }

    /// Marks every row, or clears the marks if they're all set already.
    pub fn mark_all(&mut self) {
        let shown: Vec<PathBuf> = self.shown().map(|w| w.path.clone()).collect();
        if shown.iter().all(|p| self.marked.contains(p)) {
            self.marked.clear();
        } else {
            self.marked.extend(shown);
        }
    }

    /// The marked worktrees, or the selected one when none are marked.
    pub fn targets(&self) -> Vec<Target> {
        let marked = self.marked_shown();
        let picked: Vec<&Worktree> = if marked.is_empty() { self.selected().map(|(w, _)| w).into_iter().collect() } else { marked };
        picked.into_iter().map(|w| Target { path: w.path.clone(), repo: w.repo.clone() }).collect()
    }

    /// Connections follow the conversations, which change far more often than worktrees.
    pub fn relink(&mut self, sessions: &[Session]) {
        let Some(scan) = &self.scan else { return };
        self.links = connect(&scan.worktrees, sessions);
        self.rebuild();
    }

    pub fn worktrees(&self) -> &[Worktree] {
        self.scan.as_ref().map_or(&[], |s| &s.worktrees)
    }

    /// Linked worktrees, grouped by repository. Main worktrees aren't rows, so they can't be
    /// picked for anything.
    fn rebuild(&mut self) {
        let list = self.worktrees();
        let mut items: Vec<usize> = (0..list.len()).filter(|&i| list[i].is_linked()).collect();
        items.sort_by(|&a, &b| (&list[a].repo, &list[a].path).cmp(&(&list[b].repo, &list[b].path)));
        let mut rows = Vec::new();
        for (n, &i) in items.iter().enumerate() {
            let repo = &list[i].repo;
            if n == 0 || list[items[n - 1]].repo != *repo {
                let count = items.iter().filter(|&&j| list[j].repo == *repo).count();
                rows.push(Row::Header { repo: repo.clone(), count });
            }
            rows.push(Row::Item(i));
        }
        self.rows = rows;
        let wanted = self.selected_path.as_deref().and_then(|p| self.row_of(p));
        let row = wanted.or_else(|| self.nearest_item(self.table.selected().unwrap_or(0)));
        self.select_row(row);
    }

    fn row_of(&self, path: &Path) -> Option<usize> {
        self.rows.iter().position(|r| matches!(r, Row::Item(i) if self.worktrees()[*i].path == path))
    }

    fn is_item(&self, row: usize) -> bool {
        matches!(self.rows.get(row), Some(Row::Item(_)))
    }

    fn nearest_item(&self, from: usize) -> Option<usize> {
        let from = from.min(self.rows.len().saturating_sub(1));
        (from..self.rows.len()).find(|&i| self.is_item(i)).or_else(|| (0..from).rev().find(|&i| self.is_item(i)))
    }

    pub fn select_row(&mut self, row: Option<usize>) {
        self.table.select(row);
        self.selected_path = self.selected().map(|(w, _)| w.path.clone());
    }

    pub fn select_first(&mut self) {
        self.select_row(self.nearest_item(0));
    }

    pub fn select_last(&mut self) {
        self.select_row(self.nearest_item(self.rows.len().saturating_sub(1)));
    }

    /// Moves by `delta` rows, skipping repository headers and stopping at the ends.
    pub fn move_selection(&mut self, delta: isize) {
        let Some(current) = self.table.selected() else { return };
        let last = self.rows.len().saturating_sub(1) as isize;
        let mut target = (current as isize + delta).clamp(0, last);
        let step = delta.signum();
        while step != 0 && (0..=last).contains(&target) && !self.is_item(target as usize) {
            target += step;
        }
        if (0..=last).contains(&target) {
            self.select_row(Some(target as usize));
        }
    }

    pub fn selected(&self) -> Option<(&Worktree, &[Link])> {
        match self.rows.get(self.table.selected()?)? {
            Row::Item(i) => Some((self.worktrees().get(*i)?, self.links.get(*i).map_or(&[][..], Vec::as_slice))),
            Row::Header { .. } => None,
        }
    }

    /// Selects a row picked with the mouse, if it's a worktree.
    pub fn click_row(&mut self, row: usize) {
        if self.is_item(row) {
            self.select_row(Some(row));
        }
    }
}

/// Every folder any conversation worked in, for finding their repositories.
pub fn folders_of(sessions: &[Session]) -> Vec<String> {
    let mut out: Vec<String> = sessions.iter().flat_map(|s| s.folders.iter().map(|v| v.path.clone()).chain(s.folder.clone())).collect();
    out.sort_unstable();
    out.dedup();
    out
}
