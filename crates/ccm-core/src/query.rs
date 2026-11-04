//! Filtering, sorting and grouping. Plain functions over a slice, shared by any front end.

use std::cmp::Ordering;

use crate::model::Session;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filters {
    pub open: bool,
    pub crashed: bool,
    pub junk: bool,
    pub missing: bool,
    pub large: bool,
}

impl Filters {
    /// A row must pass every active filter.
    pub fn matches(&self, s: &Session, large_bytes: u64) -> bool {
        (!self.open || s.open)
            && (!self.crashed || s.crashed)
            && (!self.junk || s.is_junk())
            && (!self.missing || s.is_folder_missing())
            && (!self.large || s.size_bytes > large_bytes)
    }

    pub fn any(&self) -> bool {
        self.open || self.crashed || self.junk || self.missing || self.large
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
    }
}

/// Indices into `sessions`, filtered and sorted. Ties fall back to newest first and then ID, so
/// rows don't jump between refreshes.
pub fn select(sessions: &[Session], filters: Filters, large_bytes: u64, key: SortKey, descending: bool) -> Vec<usize> {
    let mut rows: Vec<usize> =
        (0..sessions.len()).filter(|&i| filters.matches(&sessions[i], large_bytes)).collect();
    rows.sort_by(|&x, &y| {
        let (a, b) = (&sessions[x], &sessions[y]);
        let primary = compare(a, b, key);
        let primary = if descending { primary.reverse() } else { primary };
        primary.then_with(|| b.last_active_ms.cmp(&a.last_active_ms)).then_with(|| a.id.cmp(&b.id))
    });
    rows
}

/// Rows of a grouped list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRow {
    Header { folder: Option<String>, count: usize, size_bytes: u64 },
    Item(usize),
}

/// Groups already-sorted rows by folder, keeping their order within each group. Groups are
/// alphabetical, and conversations with no recorded folder come last.
pub fn group_by_folder(sessions: &[Session], rows: &[usize]) -> Vec<GroupRow> {
    let mut folders: Vec<Option<&str>> = rows.iter().map(|&i| sessions[i].folder.as_deref()).collect();
    folders.sort_by(|a, b| match (a, b) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (a, b) => a.cmp(b),
    });
    folders.dedup();

    let mut out = Vec::with_capacity(rows.len() + folders.len());
    for folder in folders {
        let members: Vec<usize> =
            rows.iter().copied().filter(|&i| sessions[i].folder.as_deref() == folder).collect();
        out.push(GroupRow::Header {
            folder: folder.map(str::to_owned),
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
            branch: None,
            model: None,
            started_at_ms: None,
            last_active_ms: None,
            context_used: None,
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
            select(&all, f, 1000, SortKey::Name, false).into_iter().map(|i| all[i].id.as_str()).collect()
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
        assert!(!f.matches(&s, 1000));
        s.size_bytes = 1001;
        assert!(f.matches(&s, 1000));
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
        let order = |d| select(&all, Filters::default(), 0, SortKey::Size, d);
        assert_eq!(order(true), [2, 1, 0]);
        assert_eq!(order(false), [1, 0, 2]);
    }

    #[test]
    fn name_sort_ignores_case() {
        let (mut a, mut b) = (session("1"), session("2"));
        a.name = "banana".into();
        b.name = "Apple".into();
        let all = [a, b];
        assert_eq!(select(&all, Filters::default(), 0, SortKey::Name, false), [1, 0]);
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
        let header = |f: Option<&str>, count, size_bytes| GroupRow::Header { folder: f.map(String::from), count, size_bytes };
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
}
