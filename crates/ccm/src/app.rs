use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ccm_core::config::Config;
use ccm_core::delete::{Outcome, Plan, Skipped};
use ccm_core::format::bytes;
use ccm_core::model::Session;
use ccm_core::preview::{self, Preview};
use ccm_core::rename::MAX_NAME;
use ccm_core::resume::{decide, is_safe_folder, manual_command, Decision};
use ccm_core::query::{group_by_folder, group_by_repo, select, status_rank, Filters, GroupRow, SortKey};
use ccm_core::repo::Place;
use ccm_core::window::{context_label, window_size};
use ccm_core::format::sanitize;
use ccm_core::removal::{self, Target};
use crossterm::event::{KeyCode, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use crate::worktrees::{folders_of, Worktrees};

/// An action reachable from both a key and a footer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ToggleOpen,
    ToggleCrashed,
    ToggleJunk,
    ToggleMissing,
    ToggleLarge,
    ClearFilters,
    ToggleGroup,
    NextSort,
    ReverseSort,
    Resume,
    Search,
    Delete,
    Rename,
    CopyId,
    CopyCommand,
    Mark,
    MarkAll,
    Reopen,
    /// Switches between the conversations and the worktrees.
    Worktrees,
    Quit,
}

/// How the conversation list is grouped. `g` steps through them in this order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Grouping {
    #[default]
    Off,
    Folder,
    Repo,
}

impl Grouping {
    fn next(self) -> Self {
        match self {
            Grouping::Off => Grouping::Folder,
            Grouping::Folder => Grouping::Repo,
            Grouping::Repo => Grouping::Off,
        }
    }
}

/// Folders to ask Git about. A full request also asks again about folders that weren't in Git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRequest {
    pub folders: Vec<String>,
    /// Every folder a conversation started in, so answers for others can be dropped.
    pub used: Vec<String>,
    pub full: bool,
}

/// Which list is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Conversations,
    Worktrees,
}

/// Runs a query. `None` means the query has nothing to search for.
/// Starts a resumed conversation and returns what to tell the user.
pub type ResumeFn = Box<dyn FnMut(&str, &Path) -> Result<String, String>>;
/// Works out what a delete would remove, without changing anything.
pub type PlanFn = Box<dyn FnMut(&[String]) -> Plan>;
/// Carries out a plan, including the memory notes when asked.
pub type DeleteFn = Box<dyn FnMut(&Plan, bool) -> Result<Outcome, String>>;
/// Takes the ID, transcript path and new name, and returns the name as saved.
pub type RenameFn = Box<dyn FnMut(&str, &Path, &str) -> Result<String, String>>;
/// Stops these conversations showing as crashed.
pub type DismissFn = Box<dyn FnMut(&[String]) -> Result<(), String>>;
/// Works out what removing these worktrees would do, without changing anything.
pub type RemovePlanFn = Box<dyn FnMut(&[Target], &[Session]) -> removal::Plan>;
/// Removes the worktrees of a plan, checking each again first.
pub type RemoveFn = Box<dyn FnMut(&removal::Plan, &[Session]) -> removal::Outcome>;
/// Copies text and returns what to tell the user.
pub type CopyFn = Box<dyn FnMut(&str) -> Result<String, String>>;

/// Two clicks on a row this close together are a double-click. Terminals report each click
/// separately, so this has to be timed here.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Del both opens and confirms a delete, and a held key looks like repeated presses. Longer than
/// the usual pause before a key starts repeating, so holding Del can't delete anything.
const DEL_PAUSE: Duration = Duration::from_secs(1);
/// A resumed session takes a moment to register as open. Until then, block a second launch.
const RESUME_COOLDOWN: Duration = Duration::from_secs(20);
/// How long removing worktrees with unpushed commits stays locked, so the warning gets read.
pub const UNPUSHED_COUNTDOWN: Duration = Duration::from_secs(5);
const DELETE_REPORT: &str = " Delete finished ";
const REOPEN_REPORT: &str = " Reopen finished ";
const REMOVE_REPORT: &str = " Remove finished ";

pub type SearchFn = Box<dyn FnMut(&str) -> Result<Option<HashSet<String>>, String>>;

/// What the preview pane shows, and which version of which conversation it came from.
pub struct LoadedPreview {
    id: String,
    mtime_ns: i64,
    pub result: Result<Preview, String>,
}

/// Screen positions from the last draw, used to interpret mouse input.
#[derive(Debug, Default)]
pub struct Hit {
    pub header: Rect,
    pub body: Rect,
    pub preview: Rect,
    pub columns: Vec<(Rect, SortKey)>,
    pub buttons: Vec<(Rect, Action)>,
}

/// A delete waiting for an answer. Nothing has changed yet.
pub struct ConfirmDelete {
    pub plan: Plan,
    pub include_notes: bool,
    opened: Instant,
}

/// A worktree removal waiting for an answer. Nothing has changed yet.
pub struct ConfirmRemove {
    pub plan: removal::Plan,
    opened: Instant,
    /// Set when a worktree has unpushed commits. Confirming stays locked until then, and trying
    /// early starts it over, so a held key or a quick double press can't get through.
    unlocks: Option<Instant>,
    /// Confirming was tried early at least once. Said in the popup, because a status message
    /// would replace the footer's Remove button.
    pub restarted: bool,
}

impl ConfirmRemove {
    /// Whole seconds left before confirming works, or `None` once it does.
    pub fn seconds_left(&self) -> Option<u64> {
        let left = self.unlocks?.checked_duration_since(Instant::now())?;
        (!left.is_zero()).then(|| left.as_millis().div_ceil(1000) as u64)
    }

    fn restart(&mut self) {
        if self.unlocks.is_some() {
            self.unlocks = Some(Instant::now() + UNPUSHED_COUNTDOWN);
            self.restarted = true;
        }
    }
}

/// One row of the "reopen all" list.
pub struct ReopenRow {
    pub id: String,
    pub ticked: bool,
}

/// The "reopen all" list. Nothing starts until it's confirmed.
pub struct Reopen {
    pub rows: Vec<ReopenRow>,
    pub cursor: usize,
}

/// A rename in progress. It holds its own target, because a refresh can move the selection.
pub struct Renaming {
    id: String,
    transcript: String,
    pub name: String,
}

pub struct App {
    pub config: Config,
    pub claude_dir: String,
    pub home: String,
    pub sessions: Vec<Session>,
    pub rows: Vec<GroupRow>,
    pub filters: Filters,
    pub sort: SortKey,
    pub descending: bool,
    pub grouping: Grouping,
    /// What Git said about each folder conversations started in.
    pub places: HashMap<String, Place>,
    /// Set when repository grouping is picked, so every folder is looked at again.
    repo_full_due: bool,
    repo_looking: bool,
    /// Folders asked about since the last full look, so one Git can't answer isn't asked forever.
    repo_asked: HashSet<String>,
    pub table: TableState,
    pub hit: Hit,
    pub status: Option<String>,
    pub search: Option<SearchFn>,
    pub query: String,
    /// Keys go to the search box.
    pub typing: bool,
    matches: Option<HashSet<String>>,
    pub preview: Option<LoadedPreview>,
    /// Lines scrolled back from the newest message.
    pub preview_scroll: usize,
    pub resume: Option<ResumeFn>,
    /// Waiting for `y` to recreate this folder for this conversation.
    pub confirm_folder: Option<(String, PathBuf)>,
    /// Conversations picked for a delete, which otherwise acts on the selected row.
    pub marked: HashSet<String>,
    pub planner: Option<PlanFn>,
    pub deleter: Option<DeleteFn>,
    pub confirm_delete: Option<ConfirmDelete>,
    /// Confirmed and waiting to run, so the screen can say so first.
    pending_delete: Option<ConfirmDelete>,
    /// What a delete couldn't do. Shown until a key is pressed.
    pub report: Option<Vec<String>>,
    pub report_title: &'static str,
    pub reopen: Option<Reopen>,
    pub dismisser: Option<DismissFn>,
    /// Lines scrolled in the delete confirmation or report. Drawing keeps it in range.
    pub dialog_scroll: usize,
    /// Set while drawing, so a list can be cut to fit.
    pub dialog_width: u16,
    pub renamer: Option<RenameFn>,
    /// Keys go to the name while this is set.
    pub renaming: Option<Renaming>,
    pub copier: Option<CopyFn>,
    just_resumed: HashMap<String, Instant>,
    last_click: Option<(usize, Instant)>,
    /// True until the first scan finishes, while the list may come from the cache.
    pub loading: bool,
    pub quit: bool,
    selected_id: Option<String>,
    pub view: View,
    pub worktrees: Worktrees,
    pub remove_planner: Option<RemovePlanFn>,
    pub remover: Option<RemoveFn>,
    pub confirm_remove: Option<ConfirmRemove>,
    pending_remove: Option<ConfirmRemove>,
}

impl App {
    pub fn new(config: Config, claude_dir: String, home: String) -> Self {
        Self {
            config,
            claude_dir,
            home,
            sessions: Vec::new(),
            rows: Vec::new(),
            filters: Filters::default(),
            sort: SortKey::LastActive,
            descending: true,
            grouping: Grouping::Off,
            places: HashMap::new(),
            repo_full_due: false,
            repo_looking: false,
            repo_asked: HashSet::new(),
            table: TableState::default(),
            hit: Hit::default(),
            status: None,
            search: None,
            query: String::new(),
            typing: false,
            matches: None,
            preview: None,
            preview_scroll: 0,
            resume: None,
            confirm_folder: None,
            marked: HashSet::new(),
            planner: None,
            deleter: None,
            confirm_delete: None,
            pending_delete: None,
            report: None,
            report_title: DELETE_REPORT,
            reopen: None,
            dismisser: None,
            dialog_scroll: 0,
            dialog_width: 0,
            renamer: None,
            renaming: None,
            copier: None,
            just_resumed: HashMap::new(),
            last_click: None,
            loading: true,
            quit: false,
            selected_id: None,
            view: View::Conversations,
            worktrees: Worktrees::default(),
            remove_planner: None,
            remover: None,
            confirm_remove: None,
            pending_remove: None,
        }
    }

    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
        let present: HashSet<&str> = self.sessions.iter().map(|s| s.id.as_str()).collect();
        self.marked.retain(|id| present.contains(id.as_str()));
        self.run_search();
        self.worktrees.relink(&self.sessions);
    }

    /// The folders to scan for worktrees, when a scan is due. The caller runs Git elsewhere and
    /// hands the result to `finish_worktree_scan`.
    pub fn worktree_request(&mut self) -> Option<Vec<String>> {
        if self.view != View::Worktrees || !self.worktrees.due() {
            return None;
        }
        self.worktrees.start();
        Some(folders_of(&self.sessions))
    }

    /// Folders to ask Git about, while grouping by repository. The caller runs Git elsewhere and
    /// hands the answers to `finish_repo_lookup`.
    pub fn repo_request(&mut self) -> Option<RepoRequest> {
        if self.grouping != Grouping::Repo || self.repo_looking {
            return None;
        }
        let mut used: Vec<String> = self.sessions.iter().filter_map(|s| s.folder.clone()).collect();
        used.sort_unstable();
        used.dedup();
        let full = std::mem::take(&mut self.repo_full_due);
        if full {
            self.repo_asked.clear();
        }
        let folders: Vec<String> =
            used.iter().filter(|f| full || !(self.places.contains_key(*f) || self.repo_asked.contains(*f))).cloned().collect();
        if folders.is_empty() {
            return None;
        }
        self.repo_asked.extend(folders.iter().cloned());
        self.repo_looking = true;
        Some(RepoRequest { folders, used, full })
    }

    pub fn finish_repo_lookup(&mut self, answers: HashMap<String, Place>) {
        self.repo_looking = false;
        self.places.extend(answers);
        self.rebuild();
    }

    pub fn finish_worktree_scan(&mut self, scan: ccm_core::worktree::Scan) {
        self.worktrees.finish(scan, &self.sessions);
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            View::Conversations => {
                // Fresh on every visit, as Git's state changes without anything `ccm` watches.
                self.worktrees.refresh_soon();
                View::Worktrees
            }
            View::Worktrees => View::Conversations,
        };
    }

    fn on_worktree_key(&mut self, key: KeyCode) {
        let page = self.hit.body.height.max(1) as isize;
        let w = &mut self.worktrees;
        match key {
            KeyCode::Up | KeyCode::Char('k') => w.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => w.move_selection(1),
            KeyCode::PageUp => w.move_selection(-page),
            KeyCode::PageDown => w.move_selection(page),
            KeyCode::Home => w.select_first(),
            KeyCode::End => w.select_last(),
            KeyCode::Char('d') | KeyCode::Delete => self.run(Action::Delete),
            KeyCode::Char(' ') => self.run(Action::Mark),
            KeyCode::Char('a') => self.run(Action::MarkAll),
            KeyCode::Esc if !self.worktrees.marked.is_empty() => self.worktrees.marked.clear(),
            KeyCode::Char('w') | KeyCode::Esc => self.run(Action::Worktrees),
            KeyCode::Char('q') => self.run(Action::Quit),
            _ => {}
        }
    }

    /// Whether the delete or remove button can do anything in the current view.
    pub fn can_press_delete(&self) -> bool {
        match self.view {
            View::Conversations => self.can_change() || !self.marked_shown().is_empty(),
            View::Worktrees if self.confirm_remove.as_ref().is_some_and(|c| c.seconds_left().is_some()) => false,
            View::Worktrees => self.worktrees.selected().is_some() || !self.worktrees.marked_shown().is_empty(),
        }
    }

    fn start_remove(&mut self) {
        let targets = self.worktrees.targets();
        if targets.is_empty() {
            return;
        }
        let Some(planner) = self.remove_planner.as_mut() else { return };
        let plan = planner(&targets, &self.sessions);
        if plan.items.is_empty() {
            let reason = plan.refused.first().map_or("nothing to remove".to_string(), |r| sanitize(&r.reason));
            self.status = Some(format!("Can't remove: {reason}."));
            return;
        }
        self.dialog_scroll = 0;
        let unlocks = plan.items.iter().any(|i| i.unpushed > 0).then(|| Instant::now() + UNPUSHED_COUNTDOWN);
        self.confirm_remove = Some(ConfirmRemove { plan, opened: Instant::now(), unlocks, restarted: false });
    }

    fn answer_remove(&mut self, key: KeyCode) {
        if self.scroll_dialog(key) {
            return;
        }
        let Some(confirm) = self.confirm_remove.as_mut() else { return };
        if matches!(key, KeyCode::Char('y') | KeyCode::Delete) && confirm.seconds_left().is_some() {
            confirm.restart();
            return;
        }
        match key {
            KeyCode::Char('y') => self.confirm_removal(),
            KeyCode::Delete if confirm.opened.elapsed() < DEL_PAUSE => {
                confirm.opened = Instant::now();
                self.status = Some("Too quick. Wait a moment, then press Del again.".into());
            }
            KeyCode::Delete => self.confirm_removal(),
            _ => self.cancel_removal(),
        }
    }

    fn cancel_removal(&mut self) {
        self.confirm_remove = None;
        self.status = Some("Cancelled. Nothing was removed.".into());
    }

    fn confirm_removal(&mut self) {
        self.pending_remove = self.confirm_remove.take();
        self.status = Some("Removing…".into());
    }

    fn perform_remove(&mut self) {
        let Some(ConfirmRemove { plan, .. }) = self.pending_remove.take() else { return };
        let Some(remover) = self.remover.as_mut() else { return };
        self.status = None;
        let outcome = remover(&plan, &self.sessions);
        let count = outcome.removed.len();
        let summary = match count {
            1 => "Removed 1 worktree. Its branch is kept.".to_string(),
            n => format!("Removed {n} worktrees. Their branches are kept."),
        };
        let path = |p: &std::path::Path| sanitize(&p.display().to_string());
        let mut lines = Vec::new();
        for f in &outcome.failures {
            lines.push(format!("Not removed {}: {}", path(&f.path), sanitize(&f.reason)));
        }
        for s in plan.refused.iter().chain(&outcome.skipped) {
            lines.push(format!("Skipped {}: {}", path(&s.path), sanitize(&s.reason)));
        }
        self.worktrees.forget(&outcome.removed, &self.sessions);
        if lines.is_empty() {
            self.status = Some(summary);
        } else {
            lines.insert(0, summary);
            self.dialog_scroll = 0;
            self.report_title = REMOVE_REPORT;
            self.report = Some(lines);
        }
    }

    /// Lets a test act as if the pause before a second Del has passed.
    #[cfg(test)]
    pub fn age_removal(&mut self) {
        if let Some(confirm) = self.confirm_remove.as_mut() {
            confirm.opened -= DEL_PAUSE;
        }
    }

    /// Lets a test act as if the countdown has run out.
    #[cfg(test)]
    pub fn end_countdown(&mut self) {
        if let Some(confirm) = self.confirm_remove.as_mut() {
            confirm.unlocks = confirm.unlocks.map(|_| Instant::now());
        }
    }

    /// True while something on screen counts down, so it needs redrawing without input.
    pub fn counting_down(&self) -> bool {
        self.confirm_remove.as_ref().is_some_and(|c| c.unlocks.is_some_and(|t| t.elapsed() < Duration::from_secs(1)))
    }

    /// Recomputes the rows and keeps the selection on the same conversation where possible.
    fn rebuild(&mut self) {
        let large = self.config.large_threshold_bytes();
        let mut order = select(&self.sessions, self.filters, large, self.sort, self.descending);
        if let Some(matches) = &self.matches {
            order.retain(|&i| matches.contains(&self.sessions[i].id));
        }
        self.rows = match self.grouping {
            Grouping::Off => order.into_iter().map(GroupRow::Item).collect(),
            Grouping::Folder => group_by_folder(&self.sessions, &order),
            // Folders Git hasn't answered for yet stay grouped by folder meanwhile.
            Grouping::Repo => group_by_repo(&self.sessions, &order, &self.places),
        };
        let wanted = self.selected_id.as_deref().and_then(|id| {
            self.rows.iter().position(|r| matches!(r, GroupRow::Item(i) if self.sessions[*i].id == id))
        });
        let row = wanted.or_else(|| {
            let near = self.table.selected().unwrap_or(0).min(self.rows.len().saturating_sub(1));
            self.nearest_item(near)
        });
        self.select_row(row);
    }

    /// Re-runs the current query. Rows follow its results, not their relevance, so the chosen
    /// sort order always holds.
    fn run_search(&mut self) {
        self.matches = match self.search.as_mut() {
            Some(search) => match search(&self.query) {
                Ok(matches) => matches,
                Err(e) => {
                    self.status = Some(format!("Search failed: {e}"));
                    None
                }
            },
            None => None,
        };
        self.rebuild();
    }

    /// False for an open conversation or one just started, which a resume must not touch.
    pub fn can_resume(&self) -> bool {
        self.selected().is_some_and(|s| !s.open && !self.recently_resumed(&s.id))
    }

    /// Delete and rename need the conversation closed, and a just-resumed one soon won't be.
    pub fn can_change(&self) -> bool {
        self.selected().is_some_and(|s| !s.open && !self.recently_resumed(&s.id))
    }

    /// Conversations on screen, in order.
    fn shown(&self) -> impl Iterator<Item = &Session> {
        self.rows.iter().filter_map(|r| match r {
            GroupRow::Item(i) => self.sessions.get(*i),
            GroupRow::Header { .. } => None,
        })
    }

    /// Marked conversations that are on screen, so a delete can't reach one a filter has hidden.
    pub fn marked_shown(&self) -> Vec<&Session> {
        self.shown().filter(|s| self.marked.contains(&s.id)).collect()
    }

    fn toggle_mark(&mut self) {
        let Some(id) = self.selected().map(|s| s.id.clone()) else { return };
        if !self.marked.remove(&id) {
            self.marked.insert(id);
        }
        self.move_selection(1);
    }

    /// Marks everything on screen, or clears those marks if they're all set already.
    fn mark_all(&mut self) {
        let shown: Vec<String> = self.shown().map(|s| s.id.clone()).collect();
        if shown.iter().all(|id| self.marked.contains(id)) {
            for id in &shown {
                self.marked.remove(id);
            }
        } else {
            self.marked.extend(shown);
        }
    }

    fn name_of(&self, id: &str) -> String {
        self.sessions.iter().find(|s| s.id == id).map_or_else(
            || id.to_string(),
            |s| if s.name.is_empty() { "(untitled)".to_string() } else { sanitize(&s.name) },
        )
    }

    fn start_delete(&mut self) {
        let marked = self.marked_shown();
        let targets: Vec<&Session> = if marked.is_empty() { self.selected().into_iter().collect() } else { marked };
        if targets.is_empty() {
            return;
        }
        let (starting, ids): (Vec<&Session>, Vec<&Session>) = targets.into_iter().partition(|s| self.recently_resumed(&s.id));
        let starting: Vec<Skipped> = starting
            .into_iter()
            .map(|s| Skipped { id: s.id.clone(), reason: "it was just resumed".into() })
            .collect();
        let ids: Vec<String> = ids.into_iter().map(|s| s.id.clone()).collect();
        let Some(planner) = self.planner.as_mut() else { return };
        let mut plan = planner(&ids);
        plan.skipped.extend(starting);
        if plan.items.is_empty() {
            let reasons: Vec<String> = plan.skipped.iter().map(|s| s.reason.clone()).collect();
            self.status = Some(format!("Can't delete: {}.", reasons.first().map_or("nothing to delete", String::as_str)));
            return;
        }
        self.dialog_scroll = 0;
        self.confirm_delete = Some(ConfirmDelete { plan, include_notes: false, opened: Instant::now() });
    }

    /// True if the key scrolled the dialog that's showing.
    fn scroll_dialog(&mut self, key: KeyCode) -> bool {
        let page = self.hit.body.height.max(1) as usize;
        match key {
            KeyCode::Up | KeyCode::Char('k') => self.dialog_scroll = self.dialog_scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.dialog_scroll += 1,
            KeyCode::PageUp => self.dialog_scroll = self.dialog_scroll.saturating_sub(page),
            KeyCode::PageDown => self.dialog_scroll += page,
            KeyCode::Home => self.dialog_scroll = 0,
            KeyCode::End => self.dialog_scroll = usize::MAX,
            _ => return false,
        }
        true
    }

    fn answer_delete(&mut self, key: KeyCode) {
        if self.scroll_dialog(key) {
            return;
        }
        let Some(confirm) = self.confirm_delete.as_mut() else { return };
        match key {
            KeyCode::Char('y') => self.confirm(),
            KeyCode::Delete if confirm.opened.elapsed() < DEL_PAUSE => {
                confirm.opened = Instant::now();
                self.status = Some("Too quick. Wait a moment, then press Del again.".into());
            }
            KeyCode::Delete => self.confirm(),
            KeyCode::Char('m') if !confirm.plan.notes.is_empty() => confirm.include_notes = !confirm.include_notes,
            _ => {
                self.confirm_delete = None;
                self.status = Some("Cancelled. Nothing was deleted.".into());
            }
        }
    }

    fn confirm(&mut self) {
        self.pending_delete = self.confirm_delete.take();
        self.status = Some("Deleting…".into());
    }

    /// Lets a test act as if the double-click window has passed.
    #[cfg(test)]
    pub fn age_confirmation(&mut self) {
        if let Some(confirm) = self.confirm_delete.as_mut() {
            confirm.opened -= DEL_PAUSE;
        }
    }

    pub fn has_pending_delete(&self) -> bool {
        self.pending_delete.is_some() || self.pending_remove.is_some()
    }

    /// Runs a confirmed delete. Called after a draw, so "Deleting…" is on screen meanwhile.
    pub fn perform_delete(&mut self) {
        if self.pending_remove.is_some() {
            return self.perform_remove();
        }
        let Some(ConfirmDelete { plan, include_notes, .. }) = self.pending_delete.take() else { return };
        let Some(deleter) = self.deleter.as_mut() else { return };
        self.status = None;
        match deleter(&plan, include_notes) {
            Ok(outcome) => self.finish_delete(&plan, include_notes, &outcome),
            Err(e) => {
                self.dialog_scroll = 0;
                self.report_title = DELETE_REPORT;
                self.report = Some(vec![sanitize(&e)]);
            }
        }
    }

    fn finish_delete(&mut self, plan: &Plan, include_notes: bool, outcome: &Outcome) {
        let freed: u64 = plan.items.iter().filter(|i| outcome.deleted.contains(&i.id)).map(|i| i.bytes).sum();
        let freed = freed + if include_notes { plan.note_bytes() } else { 0 };
        let count = outcome.deleted.len();
        let summary = format!("Deleted {count} conversation{}, freed {}.", if count == 1 { "" } else { "s" }, bytes(freed));

        let mut lines = Vec::new();
        for f in &outcome.failures {
            lines.push(format!("Not removed: {} ({})", sanitize(&f.path.display().to_string()), sanitize(&f.error)));
        }
        for s in plan.skipped.iter().chain(&outcome.skipped) {
            lines.push(format!("Skipped {}: {}", self.name_of(&s.id), s.reason));
        }
        lines.extend(plan.warnings.iter().map(|w| sanitize(w)));

        self.sessions.retain(|s| !outcome.deleted.contains(&s.id));
        self.marked.retain(|id| !outcome.deleted.contains(id));
        self.run_search();
        if lines.is_empty() {
            self.status = Some(summary);
        } else {
            lines.insert(0, summary);
            self.dialog_scroll = 0;
            self.report_title = DELETE_REPORT;
            self.report = Some(lines);
        }
    }

    fn start_rename(&mut self) {
        let Some(s) = self.selected() else { return };
        if !self.can_change() {
            self.status = Some("This conversation is open, so rename it from there with /rename.".into());
            return;
        }
        self.renaming = Some(Renaming { id: s.id.clone(), transcript: s.transcript_path.clone(), name: sanitize(&s.name) });
    }

    fn on_rename_key(&mut self, key: KeyCode) {
        let Some(Renaming { name, .. }) = self.renaming.as_mut() else { return };
        match key {
            KeyCode::Char(c) if name.chars().count() < MAX_NAME => name.push(c),
            KeyCode::Backspace => {
                name.pop();
            }
            KeyCode::Enter => self.save_rename(),
            KeyCode::Esc => {
                self.renaming = None;
                self.status = Some("Cancelled.".into());
            }
            _ => {}
        }
    }

    fn save_rename(&mut self) {
        let Some(Renaming { id, transcript, name }) = self.renaming.take() else { return };
        let Some(renamer) = self.renamer.as_mut() else { return };
        match renamer(&id, Path::new(&transcript), &name) {
            Ok(saved) => {
                self.status = Some(format!("Renamed to {saved}."));
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
                    s.name = saved;
                }
                self.rebuild();
            }
            Err(e) => self.status = Some(sanitize(&e)),
        }
    }

    fn copy(&mut self, text: String) {
        let Some(copier) = self.copier.as_mut() else { return };
        self.status = Some(match copier(&text) {
            Ok(message) => format!("{message}: {}", sanitize(&text)),
            Err(e) => sanitize(&e),
        });
    }

    /// This gets pasted into a shell, so a folder that isn't safe is left out rather than quoted.
    /// A control character in it could end the paste early, whatever the quoting.
    fn resume_command(s: &Session) -> String {
        match s.folder.as_deref().filter(|f| is_safe_folder(f)) {
            Some(folder) => manual_command(&s.id, Path::new(folder)),
            None => format!("claude --resume {}", s.id),
        }
    }

    fn recently_resumed(&self, id: &str) -> bool {
        self.just_resumed.get(id).is_some_and(|t| t.elapsed() < RESUME_COOLDOWN)
    }

    fn resume_selected(&mut self) {
        let Some(s) = self.selected() else { return };
        let id = s.id.clone();
        if self.recently_resumed(&id) {
            self.status = Some("Already starting. It will show as open in a moment.".into());
            return;
        }
        match decide(s) {
            Decision::Go(folder) => self.launch(id, &folder),
            Decision::AlreadyOpen => self.status = Some("This conversation is already open.".into()),
            Decision::NoFolder => self.status = Some("No folder is recorded for this conversation.".into()),
            Decision::UnsafeFolder => self.status = Some("The recorded folder isn't a safe path, so it won't be used.".into()),
            Decision::InvalidId => self.status = Some("This conversation's ID isn't valid.".into()),
            Decision::FolderMissing(folder) => self.confirm_folder = Some((id, folder)),
        }
    }

    fn launch(&mut self, id: String, folder: &Path) {
        match self.start_resume(&id, folder) {
            Ok(message) => self.status = Some(message),
            Err(e) => self.status = Some(e),
        }
    }

    fn start_resume(&mut self, id: &str, folder: &Path) -> Result<String, String> {
        let Some(resume) = self.resume.as_mut() else { return Err("Resume isn't available.".into()) };
        match resume(id, folder) {
            Ok(message) => {
                self.just_resumed.insert(id.to_string(), Instant::now());
                Ok(message)
            }
            Err(e) => Err(sanitize(&e)),
        }
    }

    /// Opens the list of crashed conversations, newest first. One whose folder is gone starts
    /// unticked, so a folder isn't recreated without someone asking for it.
    fn start_reopen(&mut self) {
        let mut crashed: Vec<&Session> = self.sessions.iter().filter(|s| s.crashed).collect();
        if crashed.is_empty() {
            self.status = Some("Nothing to reopen. Crash information needs ccm setup, and only covers sessions started after it.".into());
            return;
        }
        crashed.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms).then_with(|| a.id.cmp(&b.id)));
        let rows = crashed.iter().map(|s| ReopenRow { id: s.id.clone(), ticked: !s.is_folder_missing() }).collect();
        self.dialog_scroll = 0;
        self.reopen = Some(Reopen { rows, cursor: 0 });
    }

    fn on_reopen_key(&mut self, key: KeyCode) {
        let Some(reopen) = self.reopen.as_mut() else { return };
        let last = reopen.rows.len().saturating_sub(1);
        match key {
            KeyCode::Up | KeyCode::Char('k') => reopen.cursor = reopen.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => reopen.cursor = (reopen.cursor + 1).min(last),
            KeyCode::Char(' ') => reopen.rows[reopen.cursor].ticked = !reopen.rows[reopen.cursor].ticked,
            KeyCode::Enter => self.finish_reopen(),
            KeyCode::Esc => {
                self.reopen = None;
                self.status = Some("Cancelled.".into());
            }
            _ => {}
        }
    }

    /// Resumes what's ticked the normal way, and stops showing the rest as crashed.
    fn finish_reopen(&mut self) {
        let Some(reopen) = self.reopen.take() else { return };
        let (ticked, unticked): (Vec<ReopenRow>, Vec<ReopenRow>) = reopen.rows.into_iter().partition(|r| r.ticked);
        let mut lines = Vec::new();
        for row in &ticked {
            let result = self.reopen_one(&row.id);
            let name = self.name_of(&row.id);
            lines.push(match result {
                Ok(message) => format!("{name}: {message}"),
                Err(e) => format!("{name}: not reopened. {e}"),
            });
        }
        let dismissed: Vec<String> = unticked.into_iter().map(|r| r.id).collect();
        if !dismissed.is_empty()
            && let Some(dismisser) = self.dismisser.as_mut()
        {
            match dismisser(&dismissed) {
                Ok(()) => {
                    self.sessions.iter_mut().filter(|s| dismissed.contains(&s.id)).for_each(|s| s.crashed = false);
                    let count = dismissed.len();
                    lines.push(format!("{count} unticked no longer show as crashed."));
                }
                Err(e) => lines.push(format!("Unticked ones still show as crashed: {}", sanitize(&e))),
            }
        }
        self.rebuild();
        self.dialog_scroll = 0;
        self.report_title = REOPEN_REPORT;
        self.report = Some(lines);
    }

    fn reopen_one(&mut self, id: &str) -> Result<String, String> {
        let session = self.sessions.iter().find(|s| s.id == id).ok_or("It's gone.")?;
        match decide(session) {
            Decision::Go(folder) => self.start_resume(id, &folder),
            Decision::FolderMissing(folder) => {
                std::fs::create_dir_all(&folder).map_err(|e| format!("Couldn't create its folder: {e}"))?;
                self.start_resume(id, &folder)
            }
            Decision::AlreadyOpen => Err("It's already open.".into()),
            Decision::NoFolder => Err("No folder is recorded.".into()),
            Decision::UnsafeFolder => Err("The recorded folder isn't a safe path.".into()),
            Decision::InvalidId => Err("Its ID isn't valid.".into()),
        }
    }

    /// Answers the "recreate this folder?" question. Any key but `y` cancels.
    fn answer_folder(&mut self, key: KeyCode) {
        let Some((id, folder)) = self.confirm_folder.take() else { return };
        if key != KeyCode::Char('y') {
            self.status = Some("Cancelled.".into());
            return;
        }
        match std::fs::create_dir_all(&folder) {
            Ok(()) => self.launch(id, &folder),
            Err(e) => self.status = Some(format!("Couldn't create {}: {e}", sanitize(&folder.display().to_string()))),
        }
    }

    pub fn is_searching(&self) -> bool {
        self.matches.is_some()
    }

    fn clear_search(&mut self) {
        self.query.clear();
        self.typing = false;
        self.run_search();
    }

    /// Call before drawing. Reloads only when the selection or its transcript changed.
    pub fn ensure_preview(&mut self) {
        let Some(s) = self.selected() else {
            self.preview = None;
            return;
        };
        let (id, mtime_ns) = (s.id.clone(), s.transcript_mtime_ns);
        if self.preview.as_ref().is_some_and(|p| p.id == id && p.mtime_ns == mtime_ns) {
            return;
        }
        let result = preview::read(Path::new(&s.transcript_path)).map_err(|e| e.to_string());
        if self.preview.as_ref().is_none_or(|p| p.id != id) {
            self.preview_scroll = 0;
        }
        self.preview = Some(LoadedPreview { id, mtime_ns, result });
    }

    fn nearest_item(&self, from: usize) -> Option<usize> {
        let is_item = |i: &usize| matches!(self.rows.get(*i), Some(GroupRow::Item(_)));
        (from..self.rows.len()).find(|i| is_item(i)).or_else(|| (0..from).rev().find(|i| is_item(i)))
    }

    fn select_row(&mut self, row: Option<usize>) {
        self.table.select(row);
        self.selected_id = self.selected().map(|s| s.id.clone());
    }

    pub fn selected(&self) -> Option<&Session> {
        match self.rows.get(self.table.selected()?)? {
            GroupRow::Item(i) => self.sessions.get(*i),
            GroupRow::Header { .. } => None,
        }
    }

    /// Moves by `delta` rows, skipping group headers and stopping at the ends.
    pub fn move_selection(&mut self, delta: isize) {
        let Some(current) = self.table.selected() else { return };
        let last = self.rows.len().saturating_sub(1) as isize;
        let mut target = (current as isize + delta).clamp(0, last);
        let step = delta.signum();
        while step != 0
            && (0..=last).contains(&target)
            && matches!(self.rows[target as usize], GroupRow::Header { .. })
        {
            target += step;
        }
        if (0..=last).contains(&target) {
            self.select_row(Some(target as usize));
        }
    }

    pub fn context_text(&self, s: &Session) -> String {
        context_label(s.context_used, window_size(s.window_size, s.model.as_deref(), &self.config.windows))
    }

    pub fn status_text(s: &Session) -> &'static str {
        match status_rank(s) {
            4 => "open",
            3 => "crashed",
            2 => "missing",
            1 => "junk",
            _ => "",
        }
    }

    pub fn sort_by(&mut self, key: SortKey) {
        if self.sort == key {
            self.descending = !self.descending;
        } else {
            self.sort = key;
            self.descending = key.default_descending();
        }
        self.rebuild();
    }

    pub fn run(&mut self, action: Action) {
        match action {
            Action::ToggleOpen => self.filters.open = !self.filters.open,
            Action::ToggleCrashed => self.filters.crashed = !self.filters.crashed,
            Action::Reopen => self.start_reopen(),
            Action::ToggleJunk => self.filters.junk = !self.filters.junk,
            Action::ToggleMissing => self.filters.missing = !self.filters.missing,
            Action::ToggleLarge => self.filters.large = !self.filters.large,
            Action::ClearFilters => self.filters = Filters::default(),
            Action::ToggleGroup => {
                self.grouping = self.grouping.next();
                self.repo_full_due = self.grouping == Grouping::Repo;
            }
            Action::NextSort => {
                let at = SortKey::ALL.iter().position(|k| *k == self.sort).unwrap_or(0);
                self.sort = SortKey::ALL[(at + 1) % SortKey::ALL.len()];
                self.descending = self.sort.default_descending();
            }
            Action::ReverseSort => self.descending = !self.descending,
            Action::Search => self.typing = true,
            Action::Resume => self.resume_selected(),
            Action::Delete if self.view == View::Worktrees => self.start_remove(),
            Action::Delete => self.start_delete(),
            Action::Rename => self.start_rename(),
            Action::CopyId => {
                if let Some(id) = self.selected().map(|s| s.id.clone()) {
                    self.copy(id);
                }
            }
            Action::CopyCommand => {
                if let Some(command) = self.selected().map(Self::resume_command) {
                    self.copy(command);
                }
            }
            Action::Mark if self.view == View::Worktrees => self.worktrees.toggle_mark(),
            Action::MarkAll if self.view == View::Worktrees => self.worktrees.mark_all(),
            Action::Mark => self.toggle_mark(),
            Action::MarkAll => self.mark_all(),
            Action::Worktrees => self.toggle_view(),
            Action::Quit => self.quit = true,
        }
        self.rebuild();
        if action == Action::ToggleJunk {
            self.status = Some(self.junk_status());
        }
    }

    /// The filter's name doesn't say what junk means, and the list alone can look unchanged.
    fn junk_status(&self) -> String {
        let narrowed = Filters { junk: false, ..self.filters }.any() || self.is_searching();
        if !self.filters.junk {
            return if narrowed { "Junk filter off." } else { "Showing all conversations." }.into();
        }
        let undo = if narrowed { "turn this off" } else { "show all" };
        let shown = self.rows.iter().filter(|r| matches!(r, GroupRow::Item(_))).count();
        match shown {
            0 => format!("No junk conversations to show. Press Shift+J again to {undo}."),
            1 => format!("Showing 1 junk conversation: Claude never replied. Press Shift+J again to {undo}."),
            n => format!("Showing {n} junk conversations: Claude never replied. Press Shift+J again to {undo}."),
        }
    }

    /// Keys pressed with Ctrl held. Raw mode means Ctrl-C arrives here rather than as a signal.
    pub fn on_ctrl(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('c') => self.quit = true,
            KeyCode::Char('u') if self.renaming.is_some() => {
                if let Some(rename) = self.renaming.as_mut() {
                    rename.name.clear();
                }
            }
            KeyCode::Char('u') if self.typing => {
                self.query.clear();
                self.run_search();
            }
            _ => {}
        }
    }

    fn on_search_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char(c) => {
                self.query.push(c);
                self.run_search();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.run_search();
            }
            KeyCode::Enter => self.typing = false,
            KeyCode::Esc => self.clear_search(),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-(self.hit.body.height.max(1) as isize)),
            KeyCode::PageDown => self.move_selection(self.hit.body.height.max(1) as isize),
            _ => {}
        }
    }

    pub fn on_key(&mut self, key: KeyCode) {
        self.status = None;
        if self.report.is_some() {
            if !self.scroll_dialog(key) {
                self.report = None;
            }
            return;
        }
        if self.confirm_folder.is_some() {
            return self.answer_folder(key);
        }
        if self.confirm_delete.is_some() {
            return self.answer_delete(key);
        }
        if self.confirm_remove.is_some() {
            return self.answer_remove(key);
        }
        if self.reopen.is_some() {
            return self.on_reopen_key(key);
        }
        if self.renaming.is_some() {
            return self.on_rename_key(key);
        }
        if self.view == View::Worktrees {
            return self.on_worktree_key(key);
        }
        if self.typing {
            return self.on_search_key(key);
        }
        let page = self.hit.body.height.max(1) as isize;
        match key {
            KeyCode::Enter => self.run(Action::Resume),
            KeyCode::Char('/') => self.run(Action::Search),
            KeyCode::Char('[') => self.preview_scroll += 3,
            KeyCode::Char(']') => self.preview_scroll = self.preview_scroll.saturating_sub(3),
            KeyCode::Char('d') | KeyCode::Delete => self.run(Action::Delete),
            KeyCode::Char('R') => self.run(Action::Rename),
            KeyCode::Char('y') => self.run(Action::CopyId),
            KeyCode::Char('Y') => self.run(Action::CopyCommand),
            KeyCode::Char(' ') => self.run(Action::Mark),
            KeyCode::Char('a') => self.run(Action::MarkAll),
            KeyCode::Esc if !self.marked.is_empty() => self.marked.clear(),
            KeyCode::Esc if !self.query.is_empty() => self.clear_search(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::Home => self.select_row(self.nearest_item(0)),
            KeyCode::End => {
                let last = self.rows.len().saturating_sub(1);
                self.select_row(self.nearest_item(last));
            }
            KeyCode::Char('o') => self.run(Action::ToggleOpen),
            KeyCode::Char('x') => self.run(Action::ToggleCrashed),
            KeyCode::Char('O') => self.run(Action::Reopen),
            KeyCode::Char('J') => self.run(Action::ToggleJunk),
            KeyCode::Char('m') => self.run(Action::ToggleMissing),
            KeyCode::Char('l') => self.run(Action::ToggleLarge),
            KeyCode::Char('c') => self.run(Action::ClearFilters),
            KeyCode::Char('g') => self.run(Action::ToggleGroup),
            KeyCode::Char('s') => self.run(Action::NextSort),
            KeyCode::Char('r') => self.run(Action::ReverseSort),
            KeyCode::Char('w') => self.run(Action::Worktrees),
            KeyCode::Esc => self.status = Some("Press q to quit.".into()),
            KeyCode::Char('q') => self.run(Action::Quit),
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) {
        let waiting = self.confirm_delete.is_some() || self.confirm_remove.is_some() || self.renaming.is_some() || self.report.is_some() || self.reopen.is_some();
        if waiting && !matches!(kind, MouseEventKind::Down(crossterm::event::MouseButton::Left)) {
            match kind {
                MouseEventKind::ScrollUp if self.reopen.is_some() => self.on_reopen_key(KeyCode::Up),
                MouseEventKind::ScrollDown if self.reopen.is_some() => self.on_reopen_key(KeyCode::Down),
                MouseEventKind::ScrollUp => self.dialog_scroll = self.dialog_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.dialog_scroll = self.dialog_scroll.saturating_add(3),
                _ => {}
            }
            return;
        }
        if self.view == View::Worktrees {
            match kind {
                MouseEventKind::ScrollUp => self.worktrees.move_selection(-3),
                MouseEventKind::ScrollDown => self.worktrees.move_selection(3),
                MouseEventKind::Down(crossterm::event::MouseButton::Left) => self.click(x, y),
                _ => {}
            }
            return;
        }
        match kind {
            MouseEventKind::ScrollUp if self.over_preview(x, y) => self.preview_scroll += 3,
            MouseEventKind::ScrollDown if self.over_preview(x, y) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::ScrollDown => self.move_selection(3),
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => self.click(x, y),
            _ => {}
        }
    }

    fn over_preview(&self, x: u16, y: u16) -> bool {
        let r = self.hit.preview;
        x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
    }

    fn click(&mut self, x: u16, y: u16) {
        self.status = None;
        if self.report.take().is_some() {
            return;
        }
        if self.confirm_folder.is_some() {
            return self.answer_folder(KeyCode::Esc);
        }
        let inside = |r: &Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        if let Some(confirm) = &self.confirm_delete {
            let on_delete = self.hit.buttons.iter().any(|(r, a)| *a == Action::Delete && inside(r));
            // The second click must be its own decision, not the tail of a double-click.
            if on_delete && confirm.opened.elapsed() < DOUBLE_CLICK {
                return;
            }
            if on_delete {
                self.confirm();
            } else {
                self.confirm_delete = None;
                self.status = Some("Cancelled. Nothing was deleted.".into());
            }
            return;
        }
        if let Some(confirm) = self.confirm_remove.as_mut() {
            let on_remove = self.hit.buttons.iter().any(|(r, a)| *a == Action::Delete && inside(r));
            if on_remove && confirm.opened.elapsed() < DOUBLE_CLICK {
                return;
            }
            if on_remove && confirm.seconds_left().is_some() {
                confirm.restart();
                return;
            }
            if on_remove { self.confirm_removal() } else { self.cancel_removal() }
            return;
        }
        if self.renaming.take().is_some() || self.reopen.take().is_some() {
            self.status = Some("Cancelled.".into());
            return;
        }
        if let Some((_, action)) = self.hit.buttons.iter().find(|(r, _)| inside(r)) {
            let action = *action;
            self.run(action);
        } else if self.view == View::Worktrees {
            if inside(&self.hit.body) {
                let row = self.worktrees.table.offset() + (y - self.hit.body.y) as usize;
                self.worktrees.click_row(row);
            }
        } else if inside(&self.hit.header) {
            if let Some((_, key)) = self.hit.columns.iter().find(|(r, _)| x >= r.x && x < r.x + r.width) {
                let key = *key;
                self.sort_by(key);
            }
        } else if inside(&self.hit.body) {
            let row = self.table.offset() + (y - self.hit.body.y) as usize;
            if matches!(self.rows.get(row), Some(GroupRow::Item(_))) {
                self.select_row(Some(row));
                let double = self.last_click.is_some_and(|(r, t)| r == row && t.elapsed() < DOUBLE_CLICK);
                self.last_click = if double { None } else { Some((row, Instant::now())) };
                if double {
                    self.run(Action::Resume);
                }
            }
        }
    }
}
