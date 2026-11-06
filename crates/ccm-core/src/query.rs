//! Filtering, sorting and grouping. Plain functions over a slice, shared by any front end.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::model::{Session, Source};
use crate::repo::{infer, Place};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filters {
    pub open: bool,
    pub crashed: bool,
    pub junk: bool,
    pub missing: bool,
    pub large: bool,
    /// Not active for a while. Only the clean-up screen sets this.
    pub old: bool,
    /// Not open. Only the clean-up screen sets this, since open conversations are never suggested.
    pub closed: bool,
    /// Only this tool's conversations.
    pub tool: Option<Source>,
}

/// What counts as large and as old.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// Larger than this is large.
    pub large_bytes: u64,
    /// Last active before this is old. A conversation with no recorded activity never is.
    pub old_before_ms: i64,
}

impl Filters {
    /// A row must pass every active filter.
    pub fn matches(&self, s: &Session, limits: Limits) -> bool {
        (!self.open || s.open)
            && (!self.crashed || s.crashed)
            && (!self.junk || s.is_junk())
            && (!self.missing || s.is_folder_missing())
            && (!self.large || s.size_bytes > limits.large_bytes)
            && (!self.old || s.last_active_ms.is_some_and(|t| t < limits.old_before_ms))
            && (!self.closed || !s.open)
            && self.tool.is_none_or(|t| s.source == t)
    }

    pub fn any(&self) -> bool {
        self.open || self.crashed || self.junk || self.missing || self.large || self.old || self.closed || self.tool.is_some()
    }
}

/// Conversations worth cleaning up. Each is a set of filters, so the normal list can show them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Suggestion {
    NoReply,
    FolderGone,
    LargeOld,
}

impl Suggestion {
    pub const ALL: [Suggestion; 3] = [Suggestion::NoReply, Suggestion::FolderGone, Suggestion::LargeOld];

    pub fn filters(self) -> Filters {
        let closed = Filters { closed: true, ..Filters::default() };
        match self {
            Suggestion::NoReply => Filters { junk: true, ..closed },
            Suggestion::FolderGone => Filters { missing: true, ..closed },
            Suggestion::LargeOld => Filters { large: true, old: true, ..closed },
        }
    }

    /// The conversations it suggests, as indices into `sessions`.
    pub fn select(self, sessions: &[Session], limits: Limits) -> Vec<usize> {
        let filters = self.filters();
        (0..sessions.len()).filter(|&i| filters.matches(&sessions[i], limits)).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Folder,
    LastActive,
    Context,
    Messages,
    Size,
    Model,
    Branch,
    Status,
    /// Only shown, and so only sortable by clicking, when more than one tool is read.
    Tool,
}

impl SortKey {
    pub const ALL: [SortKey; 9] = [
        SortKey::Name,
        SortKey::Folder,
        SortKey::LastActive,
        SortKey::Context,
        SortKey::Messages,
        SortKey::Size,
        SortKey::Model,
        SortKey::Branch,
        SortKey::Status,
    ];

    /// Figures and dates start with the biggest or newest, text starts at A.
    pub fn default_descending(self) -> bool {
        matches!(self, SortKey::LastActive | SortKey::Context | SortKey::Messages | SortKey::Size | SortKey::Status)
    }
}

/// Used by the status column: open, then missing folder, then junk, then the rest.
pub fn status_rank(s: &Session) -> u8 {
    match () {
        _ if s.open => 4,
        _ if s.crashed => 3,
        _ if s.is_folder_missing() => 2,
        _ if s.is_junk() => 1,
        _ => 0,
    }
}

fn compare(a: &Session, b: &Session, key: SortKey) -> Ordering {
    match key {
        SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        SortKey::Folder => a.folder.cmp(&b.folder),
        SortKey::LastActive => a.last_active_ms.cmp(&b.last_active_ms),
        SortKey::Context => a.context_used.cmp(&b.context_used),
        SortKey::Messages => a.messages().cmp(&b.messages()),
        SortKey::Size => a.size_bytes.cmp(&b.size_bytes),
        SortKey::Model => a.model.cmp(&b.model),
        SortKey::Branch => a.branch.cmp(&b.branch),
        SortKey::Status => status_rank(a).cmp(&status_rank(b)),
        SortKey::Tool => a.source.label().cmp(b.source.label()),
    }
}

/// Indices into `sessions`, filtered and sorted. Ties fall back to newest first and then ID, so
/// rows don't jump between refreshes.
pub fn select(sessions: &[Session], filters: Filters, limits: Limits, key: SortKey, descending: bool) -> Vec<usize> {
    let mut rows: Vec<usize> =
        (0..sessions.len()).filter(|&i| filters.matches(&sessions[i], limits)).collect();
    rows.sort_by(|&x, &y| {
        let (a, b) = (&sessions[x], &sessions[y]);
        let primary = compare(a, b, key);
        let primary = if descending { primary.reverse() } else { primary };
        primary.then_with(|| b.last_active_ms.cmp(&a.last_active_ms)).then_with(|| a.id.cmp(&b.id))
    });
    rows
}

/// What a group's conversations share.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// A folder, or, when grouping by repository, a folder Git hasn't answered for yet.
    Folder(String),
    /// A repository, named by its main folder.
    Repo(String),
    /// A folder that isn't in a Git repository.
    NotGit(String),
    /// A folder that no longer exists, in no repository `ccm` knows of.
    Gone(String),
    NoFolder,
}

impl Group {
    fn path(&self) -> Option<&str> {
        match self {
            Group::Folder(p) | Group::Repo(p) | Group::NotGit(p) | Group::Gone(p) => Some(p),
            Group::NoFolder => None,
        }
    }
}

/// Rows of a grouped list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRow {
    Header { group: Group, count: usize, size_bytes: u64 },
    Item(usize),
}

/// Groups already-sorted rows by folder, keeping their order within each group. Groups are
/// alphabetical, and conversations with no recorded folder come last.
pub fn group_by_folder(sessions: &[Session], rows: &[usize]) -> Vec<GroupRow> {
    group_by(sessions, rows, |s| s.folder.clone().map_or(Group::NoFolder, Group::Folder))
}

/// Groups already-sorted rows by the repository of the folder each conversation started in.
/// `places` holds what Git said about each folder. A folder that's gone goes to the repository
/// that contains it, if any.
pub fn group_by_repo(sessions: &[Session], rows: &[usize], places: &HashMap<String, Place>) -> Vec<GroupRow> {
    let repos: Vec<&str> = places
        .values()
        .filter_map(|p| match p {
            Place::Repo { repo, .. } => Some(repo.as_str()),
            _ => None,
        })
        .collect();
    group_by(sessions, rows, |s| {
        let Some(folder) = &s.folder else { return Group::NoFolder };
        match places.get(folder) {
            Some(Place::Repo { repo, .. }) => Group::Repo(repo.clone()),
            Some(Place::NotGit) => Group::NotGit(folder.clone()),
            Some(Place::Gone) => {
                infer(folder, repos.iter().copied()).map_or_else(|| Group::Gone(folder.clone()), |r| Group::Repo(r.to_string()))
            }
            None => Group::Folder(folder.clone()),
        }
    })
}

/// The worktree a conversation started in, when it's not the repository's main folder, for
/// telling apart the work in one repository group.
pub fn worktree_of<'a>(s: &'a Session, places: &'a HashMap<String, Place>) -> Option<&'a str> {
    let folder = s.folder.as_deref()?;
    match places.get(folder)? {
        Place::Repo { worktree, .. } => worktree.as_deref(),
        Place::Gone => {
            let repos = places.values().filter_map(|p| match p {
                Place::Repo { repo, .. } => Some(repo.as_str()),
                _ => None,
            });
            infer(folder, repos).filter(|r| *r != folder).map(|_| folder)
        }
        Place::NotGit => None,
    }
}

fn group_by(sessions: &[Session], rows: &[usize], key: impl Fn(&Session) -> Group) -> Vec<GroupRow> {
    let keys: Vec<Group> = rows.iter().map(|&i| key(&sessions[i])).collect();
    let mut groups: Vec<&Group> = keys.iter().collect();
    groups.sort_by(|a, b| match (a.path(), b.path()) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (x, y) => x.cmp(&y).then_with(|| a.cmp(b)),
    });
    groups.dedup();

    let mut out = Vec::with_capacity(rows.len() + groups.len());
    for group in groups {
        let members: Vec<usize> = rows.iter().zip(&keys).filter(|(_, k)| *k == group).map(|(&i, _)| i).collect();
        out.push(GroupRow::Header {
            group: group.clone(),
            count: members.len(),
            size_bytes: members.iter().map(|&i| sessions[i].size_bytes).sum(),
        });
        out.extend(members.into_iter().map(GroupRow::Item));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;

    fn session(id: &str) -> Session {
        Session {
            id: id.into(),
            source: Source::ClaudeCode,
            transcript_path: String::new(),
            transcript_size: 0,
            transcript_mtime_ns: 0,
            size_bytes: 0,
            name: id.into(),
            folder: Some("/f".into()),
            folders: Vec::new(),
            branch: None,
            model: None,
            started_at_ms: None,
            last_active_ms: None,
            context_used: None,
            context_window: None,
            prompts: 1,
            replies: 1,
            cost_usd: None,
            lines_added: None,
            lines_removed: None,
            pr_url: None,
            continued_in: None,
            recaps: Vec::new(),
            bad_lines: 0,
            open: false,
            crashed: false,
            window_size: None,
            folder_exists: true,
        }
    }

    const LIMITS: Limits = Limits { large_bytes: 1000, old_before_ms: 1000 };

    #[test]
    fn suggestions_never_include_open_conversations() {
        let mut junk = session("junk");
        junk.replies = 0;
        let mut open_junk = session("open-junk");
        open_junk.replies = 0;
        open_junk.open = true;
        let mut gone = session("gone");
        gone.folder_exists = false;
        let mut big_old = session("big-old");
        big_old.size_bytes = 2000;
        big_old.last_active_ms = Some(999);
        let mut big_new = session("big-new");
        big_new.size_bytes = 2000;
        big_new.last_active_ms = Some(1000);
        let mut big_unknown = session("big-unknown");
        big_unknown.size_bytes = 2000;
        big_unknown.last_active_ms = None;
        let all = [junk, open_junk, gone, big_old, big_new, big_unknown];
        let ids = |s: Suggestion| -> Vec<&str> { s.select(&all, LIMITS).into_iter().map(|i| all[i].id.as_str()).collect() };
        assert_eq!(ids(Suggestion::NoReply), ["junk"]);
        assert_eq!(ids(Suggestion::FolderGone), ["gone"]);
        assert_eq!(ids(Suggestion::LargeOld), ["big-old"], "old means last active before the cutoff, and unknown isn't old");
        let shown = select(&all, Suggestion::LargeOld.filters(), LIMITS, SortKey::Name, false);
        assert_eq!(shown, Suggestion::LargeOld.select(&all, LIMITS), "the list shows exactly what was counted");
    }

    #[test]
    fn each_filter_selects_its_rows_and_they_combine() {
        let mut open = session("open");
        open.open = true;
        let mut junk = session("junk");
        junk.replies = 0;
        let mut gone = session("gone");
        gone.folder_exists = false;
        let mut big = session("big");
        big.size_bytes = 2000;
        let mut both = session("both");
        both.replies = 0;
        both.size_bytes = 2000;
        let all = [open, junk, gone, big, both, session("plain")];
        let ids = |f: Filters| -> Vec<&str> {
            select(&all, f, LIMITS, SortKey::Name, false).into_iter().map(|i| all[i].id.as_str()).collect()
        };
        assert_eq!(ids(Filters::default()).len(), 6);
        assert_eq!(ids(Filters { open: true, ..Default::default() }), ["open"]);
        assert_eq!(ids(Filters { junk: true, ..Default::default() }), ["both", "junk"]);
        assert_eq!(ids(Filters { missing: true, ..Default::default() }), ["gone"]);
        assert_eq!(ids(Filters { large: true, ..Default::default() }), ["big", "both"]);
        assert_eq!(ids(Filters { junk: true, large: true, ..Default::default() }), ["both"]);
    }

    #[test]
    fn a_conversation_with_no_folder_is_not_a_missing_folder() {
        let mut s = session("a");
        s.folder = None;
        s.folder_exists = true;
        assert!(!s.is_folder_missing());
    }

    #[test]
    fn large_means_strictly_over_the_threshold() {
        let mut s = session("a");
        s.size_bytes = 1000;
        let f = Filters { large: true, ..Default::default() };
        assert!(!f.matches(&s, LIMITS));
        s.size_bytes = 1001;
        assert!(f.matches(&s, LIMITS));
    }

    #[test]
    fn sorts_with_stable_ties() {
        let mut a = session("a");
        a.size_bytes = 5;
        a.last_active_ms = Some(1);
        let mut b = session("b");
        b.size_bytes = 5;
        b.last_active_ms = Some(2);
        let mut c = session("c");
        c.size_bytes = 9;
        let all = [a, b, c];
        let order = |d| select(&all, Filters::default(), Limits::default(), SortKey::Size, d);
        assert_eq!(order(true), [2, 1, 0]);
        assert_eq!(order(false), [1, 0, 2]);
    }

    #[test]
    fn name_sort_ignores_case() {
        let (mut a, mut b) = (session("1"), session("2"));
        a.name = "banana".into();
        b.name = "Apple".into();
        let all = [a, b];
        assert_eq!(select(&all, Filters::default(), Limits::default(), SortKey::Name, false), [1, 0]);
    }

    #[test]
    fn groups_keep_row_order_and_put_unknown_folders_last() {
        let mut x1 = session("x1");
        x1.folder = Some("/x".into());
        x1.size_bytes = 10;
        let mut none = session("none");
        none.folder = None;
        let mut a = session("a");
        a.folder = Some("/a".into());
        let mut x2 = session("x2");
        x2.folder = Some("/x".into());
        x2.size_bytes = 5;
        let all = [x1, none, a, x2];
        let rows = group_by_folder(&all, &[3, 1, 2, 0]);
        let header = |f: Option<&str>, count, size_bytes| GroupRow::Header {
            group: f.map_or(Group::NoFolder, |f| Group::Folder(f.into())),
            count,
            size_bytes,
        };
        assert_eq!(
            rows,
            [
                header(Some("/a"), 1, 0),
                GroupRow::Item(2),
                header(Some("/x"), 2, 15),
                GroupRow::Item(3),
                GroupRow::Item(0),
                header(None, 1, 0),
                GroupRow::Item(1),
            ]
        );
    }

    #[test]
    fn repository_groups_join_worktrees_and_keep_other_folders_apart() {
        let at = |id: &str, folder: Option<&str>| {
            let mut s = session(id);
            s.folder = folder.map(String::from);
            s
        };
        let all = [
            at("main", Some("/w/shop")),
            at("login", Some("/w/shop-login/src")),
            at("tidy-gone", Some("/w/shop/.claude/worktrees/tidy")),
            at("notes", Some("/w/notes")),
            at("lost", Some("/w/lost")),
            at("waiting", Some("/w/new")),
            at("none", None),
        ];
        let repo = |worktree: Option<&str>| Place::Repo { repo: "/w/shop".into(), worktree: worktree.map(String::from) };
        let places = HashMap::from([
            ("/w/shop".to_string(), repo(None)),
            ("/w/shop-login/src".to_string(), repo(Some("/w/shop-login"))),
            ("/w/shop/.claude/worktrees/tidy".to_string(), Place::Gone),
            ("/w/notes".to_string(), Place::NotGit),
            ("/w/lost".to_string(), Place::Gone),
        ]);
        let header = |group, count| GroupRow::Header { group, count, size_bytes: 0 };
        assert_eq!(
            group_by_repo(&all, &[0, 1, 2, 3, 4, 5, 6], &places),
            [
                header(Group::Gone("/w/lost".into()), 1),
                GroupRow::Item(4),
                header(Group::Folder("/w/new".into()), 1),
                GroupRow::Item(5),
                header(Group::NotGit("/w/notes".into()), 1),
                GroupRow::Item(3),
                header(Group::Repo("/w/shop".into()), 3),
                GroupRow::Item(0),
                GroupRow::Item(1),
                GroupRow::Item(2),
                header(Group::NoFolder, 1),
                GroupRow::Item(6),
            ]
        );
        let worktree = |i: usize| worktree_of(&all[i], &places);
        assert_eq!(worktree(0), None);
        assert_eq!(worktree(1), Some("/w/shop-login"));
        assert_eq!(worktree(2), Some("/w/shop/.claude/worktrees/tidy"), "a gone worktree is named by its folder");
        assert_eq!(worktree(4), None);
    }
}
