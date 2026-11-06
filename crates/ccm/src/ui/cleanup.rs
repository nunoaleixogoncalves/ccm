//! The clean-up screen: one line per kind of leftover, with how many there are and what
//! removing them would free. Nothing is removed from here. A line opens its items where they
//! can be marked and removed the usual way.

use ccm_core::format::{ago, bytes, sanitize};
use ccm_core::query::Suggestion;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::{draw_dialog, draw_footer, fit_end, footer_rows, Dialog, COLUMN_SPACING, MIN_NAME_WIDTH};
use crate::app::{App, Cleanup, Tally};

pub(super) fn draw(f: &mut Frame, app: &mut App, now_ms: i64) {
    let [summary, _, table, _, detail, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(Cleanup::ALL.len() as u16 + 1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(footer_rows(app, f.area().width)),
    ])
    .areas(f.area());
    app.hit.preview = Rect::default();
    app.hit.columns.clear();
    f.render_widget(Paragraph::new(summary_line(app)), summary);
    draw_table(f, app, table);
    draw_detail(f, app, detail, now_ms);
    draw_footer(f, app, footer);
    draw_dialog(f, app);
}

fn summary_line(app: &App) -> Line<'static> {
    let tallies: Vec<Option<Tally>> = Cleanup::ALL.iter().map(|l| app.tally(*l)).collect();
    let count: usize = tallies.iter().flatten().map(|t| t.count).sum();
    let freed: u64 = tallies.iter().flatten().filter_map(|t| t.bytes).sum();
    let checking = tallies.iter().any(Option::is_none) || app.worktrees.scanning;
    let mut text = if count == 0 && !checking {
        " ccm  clean up  nothing to clean up right now".to_string()
    } else {
        format!(" ccm  clean up  {count} suggested, {} could be freed", bytes(freed))
    };
    if checking {
        text.push_str("  checking…");
    }
    let marked: Vec<Cleanup> = Cleanup::ALL.into_iter().filter(|l| app.cleanup_marked.contains(l)).collect();
    if !marked.is_empty() {
        let n = marked.len();
        text.push_str(&format!("  {n} line{} marked", if n == 1 { "" } else { "s" }));
    }
    let mut spans = vec![Span::styled(text, Style::new().add_modifier(Modifier::BOLD))];
    if let Some(error) = app.worktrees.scan.as_ref().and_then(|s| s.errors.first()) {
        spans.push(Span::styled(format!("  {}", sanitize(error)), Style::new().fg(Color::Yellow)));
    }
    Line::from(spans)
}

pub(crate) fn label(app: &App, line: Cleanup) -> String {
    match line {
        Cleanup::Conversations(Suggestion::NoReply) => "Conversations Claude never replied to".into(),
        Cleanup::Conversations(Suggestion::FolderGone) => "Conversations whose folder is gone".into(),
        Cleanup::Conversations(Suggestion::LargeOld) => format!(
            "Conversations over {} not used for {} days",
            bytes(app.config.large_threshold_bytes()),
            app.config.old_after_days
        ),
        Cleanup::Worktrees => "Worktrees with no conversation and nothing to lose".into(),
        Cleanup::Jobs => "Background job folders with no conversation".into(),
    }
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let widths = [Constraint::Min(20), Constraint::Length(8), Constraint::Length(10)];
    let header = Row::new(["Suggestion", "   Items", "     Frees"]).style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let rows: Vec<Row> = Cleanup::ALL
        .iter()
        .map(|&line| {
            let tally = app.tally(line);
            let (count, freed) = match tally {
                None => ("…".to_string(), String::new()),
                Some(t) => (t.count.to_string(), t.bytes.map_or("?".into(), bytes)),
            };
            let marked = app.cleanup_marked.contains(&line);
            let style = if marked {
                Style::new().fg(Color::Yellow)
            } else if tally.is_some_and(|t| t.count == 0) {
                Style::new().fg(Color::DarkGray)
            } else {
                Style::new()
            };
            let name = if marked { format!("● {}", label(app, line)) } else { label(app, line) };
            Row::new([Cell::from(name), Cell::from(format!("{count:>8}")), Cell::from(format!("{freed:>10}"))]).style(style)
        })
        .collect();
    app.hit.header = Rect { height: 1, ..area };
    app.hit.body = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
    let table = Table::new(rows, widths)
        .header(header)
        .flex(Flex::Start)
        .column_spacing(COLUMN_SPACING)
        .highlight_spacing(HighlightSpacing::Never)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = TableState::default().with_selected(Some(app.cleanup_row.min(Cleanup::ALL.len() - 1)));
    f.render_stateful_widget(table, area, &mut state);
}

fn explanation(app: &App, line: Cleanup) -> Vec<String> {
    let empty = "Nothing matches this right now, so there's nothing to delete here.";
    let checked = app.tally(line).is_some();
    let is_empty = app.tally(line).is_some_and(|t| t.count == 0);
    let first = match line {
        Cleanup::Conversations(Suggestion::NoReply) => {
            "Conversations where Claude never replied, often started by mistake or cut off at once.".to_string()
        }
        Cleanup::Conversations(Suggestion::FolderGone) => {
            "The folder these started in no longer exists. Resuming one would recreate it empty.".to_string()
        }
        Cleanup::Conversations(Suggestion::LargeOld) => format!(
            "Larger than {} and not active for {} days. Change these with large_threshold_mb and old_after_days in config.toml.",
            bytes(app.config.large_threshold_bytes()),
            app.config.old_after_days
        ),
        Cleanup::Worktrees => {
            let what = if !checked {
                "Still checking the worktrees."
            } else if is_empty {
                empty
            } else {
                "d removes all of them, and their branches are kept. Space marks lines to delete several at once. Enter shows them in the worktree view, to look through them or remove only some."
            };
            return vec![
                "Worktrees no conversation worked in, with no changes, no unpushed commits and a branch, or whose folder is gone.".into(),
                "Locked ones, and ones a running session is in, aren't suggested. Only repositories your conversations used are checked.".into(),
                what.into(),
            ];
        }
        Cleanup::Jobs => {
            let what = if app.leftovers.is_none() {
                "Job folders can't be checked."
            } else if is_empty {
                empty
            } else {
                "d deletes the ones ccm can, and Space marks lines to delete several at once. Enter lists them, where c copies the claude rm commands."
            };
            return vec![
                "Folders in ~/.claude/jobs whose conversation is gone, untouched for a day and not running.".into(),
                "Ones that may have a worktree are only listed, with the claude rm command that removes them safely.".into(),
                what.into(),
            ];
        }
    };
    let what = if is_empty {
        empty
    } else {
        "d deletes all of them, and Space marks lines to delete several at once. Enter shows them in the list, to look through them or delete only some."
    };
    vec![first, "Open ones aren't suggested.".into(), what.into()]
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect, now_ms: i64) {
    let line = Cleanup::ALL[app.cleanup_row.min(Cleanup::ALL.len() - 1)];
    let block = Block::new().borders(Borders::TOP).title(" About this suggestion ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = inner.width.saturating_sub(1) as usize;
    let mut text: Vec<Line> = explanation(app, line).iter().flat_map(|l| super::wrap(l, width)).map(|l| Line::from(format!(" {l}"))).collect();
    if line == Cleanup::Jobs
        && let Some(found) = &app.leftovers
        && !found.is_empty()
    {
        text.push(Line::from(""));
        text.extend(found.iter().map(|l| Line::from(format!(" {}", job_line(l, width.saturating_sub(1), now_ms)))));
    }
    text.push(Line::from(""));
    let safety = "Nothing is deleted without asking. Each one is checked again just before, and any that opened meanwhile are kept.";
    let grey = Style::new().fg(Color::DarkGray);
    text.extend(super::wrap(safety, width).into_iter().map(|l| Line::from(Span::styled(format!(" {l}"), grey))));
    f.render_widget(Paragraph::new(text), inner);
}

/// One job folder: its ID, size and age, then what's known of it.
pub(super) fn job_line(l: &ccm_core::jobs::Leftover, width: usize, now_ms: i64) -> String {
    let what = match (&l.name, l.no_state) {
        (_, true) => "no state file".to_string(),
        (Some(name), _) => sanitize(name),
        (None, _) => "unnamed".to_string(),
    };
    let note = if l.worktree { "  may have a worktree, kept" } else { "" };
    let head = format!("{}  {:>7}  {:>4} ago  ", sanitize(&l.short()), bytes(l.bytes), ago(now_ms, l.last_ms));
    let room = width.saturating_sub(head.width() + note.width()).max(MIN_NAME_WIDTH);
    format!("{head}{}{note}", fit_end(&what, room))
}

pub(super) fn jobs_dialog(app: &App) -> Dialog {
    let confirm = app.confirm_jobs.as_ref().expect("checked by the caller");
    let plain = Style::new();
    let bold = plain.add_modifier(Modifier::BOLD);
    let removable: Vec<_> = confirm.removable().collect();
    let freed: u64 = removable.iter().map(|l| l.bytes).sum();
    let kept = confirm.items.len() - removable.len();
    let mut top = vec![(
        format!("{} job folder{}, {} freed. This can't be undone.", removable.len(), if removable.len() == 1 { "" } else { "s" }, bytes(freed)),
        bold,
    )];
    if kept > 0 {
        let what = if kept == 1 { "1 may have a worktree and is kept. Remove it" } else { "These may have a worktree and are kept. Remove them" };
        let what = if kept == 1 { what.to_string() } else { what.replacen("These", &kept.to_string(), 1) };
        top.push((format!("{what} with claude rm."), plain));
    }
    let width = app.dialog_width.saturating_sub(2) as usize;
    let now_ms = (app.clock)();
    let body = confirm.items.iter().map(|l| (job_line(l, width, now_ms), plain)).collect();
    let copy = if kept > 0 { "c copy claude rm commands   " } else { "" };
    let delete = if removable.is_empty() { "" } else { "y or Del delete   " };
    Dialog { title: " Delete job folders? ", top, body, bottom: format!("{delete}{copy}any other key cancels") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Action, View};
    use ccm_core::config::Config;
    use ccm_core::jobs::{self, Leftover};
    use ccm_core::model::{Session, Source};
    use ccm_core::worktree::{Scan, State, Worktree};
    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::rc::Rc;

    const DAY: i64 = 24 * 60 * 60 * 1000;
    const NOW: i64 = 1000 * DAY;
    const MB: u64 = 1024 * 1024;

    fn session(id: &str, folder: &str, days_ago: i64, size_mb: u64) -> Session {
        Session {
            id: id.into(),
            source: Source::ClaudeCode,
            transcript_path: String::new(),
            transcript_size: 0,
            transcript_mtime_ns: 0,
            size_bytes: size_mb * MB,
            name: format!("name {id}"),
            folder: Some(folder.into()),
            folders: vec![ccm_core::model::FolderVisit { path: folder.into(), records: 5, last_record: 5, last_ms: Some(NOW - days_ago * DAY) }],
            branch: None,
            model: None,
            started_at_ms: None,
            last_active_ms: Some(NOW - days_ago * DAY),
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

    fn leftover(short: &str, worktree: bool) -> Leftover {
        Leftover {
            dir: PathBuf::from(format!("/c/jobs/{short}")),
            name: Some(format!("job {short}")),
            worktree,
            no_state: false,
            bytes: 2 * MB,
            last_ms: Some(NOW - 90 * DAY),
        }
    }

    type Removed = Rc<RefCell<Vec<Vec<PathBuf>>>>;

    fn app() -> (App, Removed) {
        let mut a = App::new(Config::default(), "/c".into(), "/home/me".into());
        a.clock = Box::new(|| NOW);
        a.loading = false;
        let mut junk = session("junk", "/home/me/a", 1, 1);
        junk.replies = 0;
        let mut open_junk = session("open-junk", "/home/me/a", 0, 1);
        open_junk.replies = 0;
        open_junk.open = true;
        let mut gone = session("gone", "/home/me/old", 5, 3);
        gone.folder_exists = false;
        let big_old = session("big-old", "/home/me/a", 40, 300);
        let mut big_old_open = session("big-old-open", "/home/me/a", 40, 300);
        big_old_open.open = true;
        let big_new = session("big-new", "/home/me/a", 2, 300);
        let used = session("used", "/home/me/shop-login", 1, 1);
        a.set_sessions(vec![junk, open_junk, gone, big_old, big_old_open, big_new, used]);
        let found = Rc::new(RefCell::new(vec![leftover("aaaaaaaa", false), leftover("bbbbbbbb", true), leftover("cccccccc", false)]));
        let source = found.clone();
        a.leftovers_finder = Some(Box::new(move || source.borrow().clone()));
        let removed: Removed = Rc::default();
        let (log, left) = (removed.clone(), found);
        a.job_remover = Some(Box::new(move |dirs| {
            log.borrow_mut().push(dirs.to_vec());
            left.borrow_mut().retain(|l| !dirs.contains(&l.dir));
            jobs::Outcome { removed: dirs.to_vec(), bytes: dirs.len() as u64 * 2 * MB, kept: Vec::new() }
        }));
        (a, removed)
    }

    fn worktree(path: &str, changes: u64, unpushed: u64) -> Worktree {
        Worktree {
            path: path.into(),
            repo: "/home/me/shop".into(),
            main: path == "/home/me/shop",
            bare: false,
            branch: Some("b".into()),
            locked: false,
            prunable: false,
            state: Some(State { changes, unpushed, has_remote: true, upstream_gone: false }),
        }
    }

    fn scan(a: &mut App) {
        assert!(a.worktree_request().is_some(), "the clean-up screen looks for worktrees");
        let mut gone = worktree("/home/me/shop-gone", 0, 0);
        gone.prunable = true;
        gone.state = None;
        let mut locked = worktree("/home/me/shop-locked", 0, 0);
        locked.locked = true;
        a.finish_worktree_scan(Scan {
            worktrees: vec![
                worktree("/home/me/shop", 0, 0),
                worktree("/home/me/shop-login", 0, 0),
                worktree("/home/me/shop-tidy", 0, 0),
                worktree("/home/me/shop-wip", 2, 0),
                worktree("/home/me/shop-ahead", 0, 1),
                gone,
                locked,
            ],
            errors: Vec::new(),
        });
    }

    fn render(a: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| super::super::draw(f, a, NOW)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn line<'a>(screen: &'a str, needle: &str) -> &'a str {
        screen.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle} not on screen:\n{screen}"))
    }

    fn names(a: &App) -> Vec<String> {
        a.rows
            .iter()
            .filter_map(|r| match r {
                ccm_core::query::GroupRow::Item(i) => Some(a.sessions[*i].id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn shift_c_opens_a_summary_of_every_suggestion_and_esc_goes_back() {
        let (mut a, _) = app();
        a.on_key(KeyCode::Char('C'));
        assert_eq!(a.view, View::Cleanup);
        let screen = render(&mut a, 100, 30);
        assert!(line(&screen, "never replied to").contains(" 1 ") && line(&screen, "never replied to").contains("1M"), "the open one isn't counted:\n{screen}");
        assert!(line(&screen, "folder is gone").contains("3M"));
        assert!(line(&screen, "over 100M not used for 30 days").contains("300M"), "{screen}");
        assert!(line(&screen, "Worktrees with no conversation").contains('…'), "worktrees are still being checked");
        assert!(line(&screen, "Background job folders").contains(" 3 ") && line(&screen, "Background job folders").contains("4M"), "the one with a worktree frees nothing here");
        assert!(screen.contains("checking…"));
        assert!(screen.contains("Nothing is deleted without asking."), "{screen}");
        a.on_key(KeyCode::Esc);
        assert_eq!(a.view, View::Conversations);
        a.run(Action::Cleanup);
        a.on_key(KeyCode::Char('C'));
        assert_eq!(a.view, View::Conversations, "Shift+C goes back too");
    }

    #[test]
    fn an_empty_line_stays_with_zero() {
        let (mut a, _) = app();
        a.set_sessions(Vec::new());
        a.on_key(KeyCode::Char('C'));
        let screen = render(&mut a, 100, 30);
        assert!(line(&screen, " ccm ").contains("checking…"), "not 'nothing' while worktrees are checked:\n{screen}");
        a.worktree_request();
        a.finish_worktree_scan(Scan { worktrees: Vec::new(), errors: Vec::new() });
        a.leftovers = Some(Vec::new());
        let screen = render(&mut a, 100, 30);
        assert!(line(&screen, "folder is gone").contains(" 0 "), "{screen}");
        assert!(line(&screen, " ccm ").contains("nothing to clean up right now"), "{screen}");
        assert!(screen.contains("nothing to delete here"), "{screen}");
    }

    #[test]
    fn a_conversation_line_opens_the_list_showing_exactly_those() {
        let (mut a, _) = app();
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Enter);
        assert_eq!(a.view, View::Conversations);
        assert_eq!(names(&a), ["big-old"]);
        let screen = render(&mut a, 120, 30);
        assert!(line(&screen, " ccm ").contains("clean-up: over 100M not used for 30 days"), "{screen}");
        a.on_key(KeyCode::Char('c'));
        assert_eq!(names(&a).len(), 7, "c clears it like any filter");
        let screen = render(&mut a, 120, 30);
        assert!(!screen.contains("clean-up:"));

        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Up);
        a.on_key(KeyCode::Up);
        a.on_key(KeyCode::Enter);
        assert_eq!(names(&a), ["junk"], "the open conversation Claude hasn't answered yet isn't shown");
    }

    #[test]
    fn the_worktree_line_counts_idle_worktrees_and_opens_only_them() {
        let (mut a, _) = app();
        a.on_key(KeyCode::Char('C'));
        assert!(a.worktree_sizes_wanted(), "the first scan of a visit measures");
        scan(&mut a);
        assert!(!a.worktree_sizes_wanted());
        let tally = a.tally(Cleanup::Worktrees).unwrap();
        assert_eq!((tally.count, tally.bytes), (2, None), "tidy and the gone one; login has a conversation");
        a.finish_worktree_sizes(HashMap::from([(PathBuf::from("/home/me/shop-tidy"), 5 * MB)]));
        assert_eq!(a.tally(Cleanup::Worktrees).unwrap().bytes, Some(5 * MB));
        let screen = render(&mut a, 100, 30);
        assert!(line(&screen, "Worktrees with no conversation").contains("5M"), "{screen}");

        a.set_busy_worktrees(std::collections::HashSet::from([PathBuf::from("/home/me/shop-gone")]));
        assert_eq!(a.tally(Cleanup::Worktrees).unwrap().count, 1, "a running session with no transcript yet keeps it");
        a.set_busy_worktrees(Default::default());

        a.cleanup_row = 3;
        a.on_key(KeyCode::Enter);
        assert_eq!(a.view, View::Worktrees);
        let screen = render(&mut a, 100, 30);
        assert!(screen.contains("shop-tidy") && screen.contains("shop-gone"), "{screen}");
        for hidden in ["shop-login", "shop-wip", "shop-ahead", "shop-locked"] {
            assert!(!screen.contains(hidden), "{hidden} shown:\n{screen}");
        }
        assert!(line(&screen, " ccm ").contains("showing 2 with nothing to lose"), "{screen}");
        a.worktrees.forget(&[PathBuf::from("/home/me/shop-tidy"), PathBuf::from("/home/me/shop-gone")], &a.sessions.clone());
        assert!(render(&mut a, 100, 30).contains("None left with nothing to lose. Press c to show all worktrees."));
        a.on_key(KeyCode::Char('c'));
        let screen = render(&mut a, 100, 30);
        assert!(screen.contains("shop-wip"), "c shows them all again");
        a.on_key(KeyCode::Char('w'));
        a.on_key(KeyCode::Char('w'));
        assert!(!a.worktrees.only_idle, "w opens the full view");
    }

    #[test]
    fn the_job_line_lists_folders_and_y_deletes_only_those_without_a_worktree() {
        let (mut a, removed) = app();
        a.on_key(KeyCode::Char('C'));
        a.cleanup_row = 4;
        let screen = render(&mut a, 100, 30);
        assert!(line(&screen, "bbbbbbbb").contains("may have a worktree"), "{screen}");
        a.on_key(KeyCode::Enter);
        let screen = render(&mut a, 100, 30);
        assert!(screen.contains("2 job folders, 4M freed"), "{screen}");
        assert!(screen.contains("1 may have a worktree and is kept. Remove it with claude rm."), "{screen}");
        a.on_key(KeyCode::Char('y'));
        assert_eq!(*removed.borrow(), [vec![PathBuf::from("/c/jobs/aaaaaaaa"), PathBuf::from("/c/jobs/cccccccc")]]);
        assert_eq!(a.status.as_deref(), Some("Deleted 2 job folders, freed 4M."));
        assert_eq!(a.tally(Cleanup::Jobs).unwrap().count, 1, "looked again after deleting");
    }

    #[test]
    fn worktree_jobs_can_only_be_copied_as_claude_rm_commands() {
        let (mut a, removed) = app();
        let copied = Rc::new(RefCell::new(Vec::new()));
        let log = copied.clone();
        a.copier = Some(Box::new(move |t| {
            log.borrow_mut().push(t.to_string());
            Ok("Copied".into())
        }));
        a.on_key(KeyCode::Char('C'));
        a.cleanup_row = 4;
        a.on_key(KeyCode::Enter);
        a.on_key(KeyCode::Char('c'));
        assert_eq!(*copied.borrow(), ["claude rm bbbbbbbb"]);
        assert!(removed.borrow().is_empty());

        a.leftovers = Some(vec![leftover("bbbbbbbb", true)]);
        a.on_key(KeyCode::Enter);
        a.on_key(KeyCode::Char('y'));
        assert!(removed.borrow().is_empty(), "nothing ccm would delete");
        assert!(a.status.as_deref().unwrap().contains("claude rm"));
    }

    #[test]
    fn other_keys_and_clicks_cancel_and_a_quick_second_del_waits() {
        let (mut a, removed) = app();
        a.on_key(KeyCode::Char('C'));
        a.cleanup_row = 4;
        a.on_key(KeyCode::Enter);
        a.on_key(KeyCode::Char('x'));
        assert!(a.confirm_jobs.is_none() && removed.borrow().is_empty());
        a.on_key(KeyCode::Enter);
        a.on_key(KeyCode::Delete);
        assert!(a.confirm_jobs.is_some() && removed.borrow().is_empty());
        render(&mut a, 100, 30);
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 1, 1);
        assert!(a.confirm_jobs.is_none() && removed.borrow().is_empty());
        assert_eq!(a.status.as_deref(), Some("Cancelled. Nothing was deleted."));
    }

    #[test]
    fn a_double_click_on_a_line_opens_it() {
        let (mut a, _) = app();
        a.on_key(KeyCode::Char('C'));
        render(&mut a, 100, 30);
        let y = a.hit.body.y + 1;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 5, y);
        assert_eq!((a.cleanup_row, a.view), (1, View::Cleanup));
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 5, y);
        assert_eq!(a.view, View::Conversations);
        assert_eq!(names(&a), ["gone"]);
    }

    type Deletes = std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>;

    /// A fake core: closed conversations can be deleted, and the delete just succeeds.
    fn deletable(a: &mut App) -> Deletes {
        use ccm_core::delete::{Item, Outcome, Plan, Skipped};
        let sessions = a.sessions.clone();
        a.planner = Some(Box::new(move |ids| {
            let mut plan = Plan::default();
            for id in ids {
                let s = sessions.iter().find(|s| s.id == *id).unwrap();
                if s.open {
                    plan.skipped.push(Skipped { id: id.clone(), reason: "it's open".into() });
                } else {
                    plan.items.push(Item { id: id.clone(), side: vec![], transcripts: vec![], bytes: s.size_bytes });
                }
            }
            plan
        }));
        let calls: Deletes = Default::default();
        let log = calls.clone();
        a.deleter = Some(Box::new(move |plan, _| {
            let ids: Vec<String> = plan.items.iter().map(|i| i.id.clone()).collect();
            log.borrow_mut().push(ids.clone());
            Ok(Outcome { deleted: ids, ..Outcome::default() })
        }));
        calls
    }

    #[test]
    fn d_deletes_everything_on_the_selected_line_after_asking() {
        let (mut a, _) = app();
        let calls = deletable(&mut a);
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("Delete permanently?"), "{screen}");
        assert!(line(&screen, "This can't be undone").contains("1 conversation, 300M freed"), "{screen}");
        assert!(screen.contains("From the clean-up suggestions:"), "{screen}");
        assert!(line(&screen, "not used for 30 days:").contains("1, 300M"), "{screen}");
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(*calls.borrow(), [vec!["big-old".to_string()]]);
        assert_eq!(a.view, View::Cleanup, "stays on the screen to carry on");
        let screen = render(&mut a, 120, 30);
        assert!(line(&screen, "not used for 30 days").contains(" 0 "), "{screen}");
        assert!(a.sessions.iter().any(|s| s.id == "big-old-open"), "the open one is left alone");
    }

    #[test]
    fn marked_lines_go_in_one_delete_and_a_conversation_on_two_lines_goes_once() {
        let (mut a, _) = app();
        let mut both = session("both", "/home/me/old", 3, 2);
        both.replies = 0;
        both.folder_exists = false;
        let mut sessions = a.sessions.clone();
        sessions.push(both);
        a.set_sessions(sessions);
        let calls = deletable(&mut a);
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Char(' '));
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Char(' '));
        let screen = render(&mut a, 120, 30);
        assert!(line(&screen, " ccm ").contains("2 lines marked"), "{screen}");
        assert!(line(&screen, "never replied to").contains("● "), "{screen}");
        a.on_key(KeyCode::Delete);
        a.age_confirmation();
        a.on_key(KeyCode::Delete);
        a.perform_delete();
        let mut deleted = calls.borrow()[0].clone();
        deleted.sort();
        assert_eq!(deleted, ["both", "gone", "junk"]);
        assert!(a.cleanup_marked.is_empty());
    }

    #[test]
    fn a_marks_every_line_with_something_in_it_and_again_clears_them() {
        let (mut a, _) = app();
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Char('a'));
        assert_eq!(a.cleanup_marked.len(), 4, "not the worktrees, still being checked");
        a.on_key(KeyCode::Char('a'));
        assert!(a.cleanup_marked.is_empty());
        a.on_key(KeyCode::Char('a'));
        a.on_key(KeyCode::Esc);
        assert!(a.cleanup_marked.is_empty() && a.view == View::Cleanup, "Esc clears the marks first");
        a.on_key(KeyCode::Char('a'));
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Char('C'));
        assert!(a.cleanup_marked.is_empty(), "marks last one visit");

        let (mut a, _) = app();
        a.set_sessions(vec![session("big-new", "/home/me/a", 2, 300)]);
        a.leftovers_finder = Some(Box::new(Vec::new));
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Char('a'));
        assert!(a.cleanup_marked.is_empty(), "empty lines aren't marked");
    }

    #[test]
    fn an_empty_line_has_nothing_to_delete_and_cancelling_deletes_nothing() {
        let (mut a, _) = app();
        a.set_sessions(vec![session("big-new", "/home/me/a", 2, 300)]);
        let calls = deletable(&mut a);
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Char('d'));
        assert!(a.confirm_delete.is_none());
        assert_eq!(a.status.as_deref(), Some("Nothing to delete on this line."));

        let (mut a, _) = app();
        let calls2 = deletable(&mut a);
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Char('d'));
        assert!(a.confirm_delete.is_some());
        a.on_key(KeyCode::Char('n'));
        a.perform_delete();
        assert!(calls.borrow().is_empty() && calls2.borrow().is_empty());
        assert_eq!(a.sessions.len(), 7);
    }

    type Removals = std::rc::Rc<std::cell::RefCell<Vec<Vec<PathBuf>>>>;

    /// A fake core for worktrees: shop-tidy has an ignored file, and the remove just succeeds.
    /// `ahead` gains an unpushed commit between the scan and the delete.
    fn removable(a: &mut App, ahead: &'static str) -> Removals {
        use ccm_core::removal::{Outcome, Plan, Planned};
        a.remove_planner = Some(Box::new(move |targets, _| {
            let mut plan = Plan::default();
            for t in targets {
                let tidy = t.path.ends_with("shop-tidy");
                plan.items.push(Planned {
                    target: t.clone(),
                    branch: Some("b".into()),
                    gone: t.path.ends_with("shop-gone"),
                    ignored: if tidy { vec![".env".into()] } else { Vec::new() },
                    ignored_bytes: if tidy { 1024 } else { 0 },
                    ignored_more: false,
                    links: Vec::new(),
                    unpushed: u64::from(t.path.ends_with(ahead)),
                    has_remote: true,
                    upstream_gone: false,
                });
            }
            plan
        }));
        let calls: Removals = Default::default();
        let log = calls.clone();
        a.remover = Some(Box::new(move |plan, _| {
            let paths: Vec<PathBuf> = plan.items.iter().map(|i| i.target.path.clone()).collect();
            log.borrow_mut().push(paths.clone());
            Outcome { removed: paths, ..Outcome::default() }
        }));
        calls
    }

    #[test]
    fn d_on_the_worktree_line_removes_the_idle_worktrees_after_asking() {
        let (mut a, _) = app();
        let calls = removable(&mut a, "nothing");
        a.on_key(KeyCode::Char('C'));
        a.cleanup_row = 3;
        a.on_key(KeyCode::Char('d'));
        assert!(a.confirm_delete.is_none(), "not while it's still checking");
        assert_eq!(a.status.as_deref(), Some("Still checking. Try again in a moment."));
        scan(&mut a);
        a.finish_worktree_sizes(HashMap::from([(PathBuf::from("/home/me/shop-tidy"), 5 * MB)]));
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 120, 40);
        assert!(line(&screen, "This can't be undone").contains("2 worktrees, 5M freed"), "{screen}");
        assert!(screen.contains("Their branches stay"), "{screen}");
        assert!(screen.contains("Files Git ignores go too, 1K."), "{screen}");
        assert!(screen.contains("~/shop-tidy") && screen.contains("~/shop-gone"), "{screen}");
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(*calls.borrow(), [vec![PathBuf::from("/home/me/shop-tidy"), PathBuf::from("/home/me/shop-gone")]]);
        assert_eq!(a.status.as_deref(), Some("Removed 2 worktrees. Their branches are kept."));
        assert_eq!(a.tally(Cleanup::Worktrees).unwrap().count, 0);
    }

    #[test]
    fn conversations_and_worktrees_go_in_one_delete_and_one_that_gained_commits_is_skipped() {
        let (mut a, _) = app();
        let deletes = deletable(&mut a);
        let removals = removable(&mut a, "shop-gone");
        a.on_key(KeyCode::Char('C'));
        scan(&mut a);
        a.on_key(KeyCode::Char(' '));
        a.cleanup_row = 3;
        a.on_key(KeyCode::Char(' '));
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 120, 40);
        assert!(line(&screen, "This can't be undone").contains("1 conversation and 1 worktree"), "{screen}");
        assert!(screen.contains("Skipping ~/shop-gone: it has unpushed commits now"), "{screen}");
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(*deletes.borrow(), [vec!["junk".to_string()]]);
        assert_eq!(*removals.borrow(), [vec![PathBuf::from("/home/me/shop-tidy")]]);
        let report = a.report.clone().expect("the skip is reported");
        assert_eq!(report[0], "Deleted 1 conversation, freed 1M. Removed 1 worktree. Its branch is kept.");
        assert!(report.iter().any(|l| l.contains("Skipped /home/me/shop-gone")), "{report:?}");
    }

    #[test]
    fn every_button_fits_and_small_terminals_do_not_panic() {
        let (mut a, _) = app();
        a.on_key(KeyCode::Char('C'));
        let screen = render(&mut a, 80, 24);
        for label in ["Enter Open", "d Delete", "Space Mark", "a All", "q Quit", "Esc Conversations", "w Worktrees"] {
            assert!(screen.contains(label), "{label} missing:\n{screen}");
        }
        for (w, h) in [(20, 5), (40, 10), (1, 1)] {
            render(&mut a, w, h);
        }
        a.cleanup_row = 4;
        a.on_key(KeyCode::Enter);
        for (w, h) in [(20, 5), (40, 10), (80, 24)] {
            render(&mut a, w, h);
        }
    }

    #[test]
    fn d_on_the_job_line_deletes_only_folders_without_a_worktree() {
        let (mut a, removed) = app();
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::End);
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 120, 40);
        assert!(line(&screen, "This can't be undone").contains("2 job folders, 4M freed"), "{screen}");
        assert!(screen.contains("Ones that may have a worktree are kept, so remove them with claude rm"), "{screen}");
        assert!(screen.contains("job bbbbbbbb  may have a worktree, kept"), "{screen}");
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(*removed.borrow(), [vec![PathBuf::from("/c/jobs/aaaaaaaa"), PathBuf::from("/c/jobs/cccccccc")]]);
        assert_eq!(a.status.as_deref(), Some("Deleted 2 job folders, freed 4M."));
        assert_eq!(a.tally(Cleanup::Jobs).unwrap().count, 1, "looked again after deleting");

        a.on_key(KeyCode::Char('d'));
        assert!(a.confirm_delete.is_none(), "only the worktree job is left");
        assert!(a.status.as_deref().unwrap().contains("Press Enter on the line, then c"), "{:?}", a.status);
    }

    #[test]
    fn every_kind_goes_in_one_delete() {
        let (mut a, removed) = app();
        let deletes = deletable(&mut a);
        let removals = removable(&mut a, "nothing");
        a.on_key(KeyCode::Char('C'));
        scan(&mut a);
        a.on_key(KeyCode::Char('a'));
        assert_eq!(a.cleanup_marked.len(), 5);
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 120, 50);
        assert!(line(&screen, "This can't be undone").contains("3 conversations, 2 worktrees and 2 job folders"), "{screen}");
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(deletes.borrow().len(), 1);
        assert_eq!(removals.borrow().len(), 1);
        assert_eq!(removed.borrow().len(), 1);
        assert_eq!(
            a.status.as_deref(),
            Some("Deleted 3 conversations, freed 304M. Removed 2 worktrees. Their branches are kept. Deleted 2 job folders, freed 4M.")
        );
    }
}
