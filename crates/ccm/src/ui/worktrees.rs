//! The worktree view. It's laid out like the conversation list, with the conversations that
//! worked in the selected worktree where the preview would be.

use ccm_core::format::{ago, bytes, sanitize, short_path};
use ccm_core::removal::Planned;
use ccm_core::model::Session;
use ccm_core::worktree::{Link, State, Worktree};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table};
use ratatui::Frame;

use crate::wording::{planned_lines, plural, share, Tone};
use super::{draw_dialog, Dialog, draw_footer, fit_end, fit_start, footer_rows, header_only_lines, COLUMN_SPACING, MIN_HEIGHT_FOR_PREVIEW};
use crate::app::App;
use crate::worktrees::Row as ListRow;

struct Column {
    title: &'static str,
    width: Constraint,
    /// Dropped first when the terminal is narrow.
    min_terminal_width: u16,
}

#[derive(Clone, Copy)]
enum Field {
    Folder,
    Branch,
    Changes,
    Unpushed,
    Conversations,
    Status,
}

const COLUMNS: [(Field, Column); 6] = [
    (Field::Folder, Column { title: "Folder", width: Constraint::Min(20), min_terminal_width: 0 }),
    (Field::Branch, Column { title: "Branch", width: Constraint::Length(24), min_terminal_width: 70 }),
    (Field::Changes, Column { title: "Changes", width: Constraint::Length(9), min_terminal_width: 0 }),
    (Field::Unpushed, Column { title: "Unpushed", width: Constraint::Length(8), min_terminal_width: 0 }),
    (Field::Conversations, Column { title: "Conversations", width: Constraint::Length(13), min_terminal_width: 60 }),
    (Field::Status, Column { title: "Status", width: Constraint::Length(7), min_terminal_width: 50 }),
];

pub(super) fn draw(f: &mut Frame, app: &mut App, now_ms: i64) {
    let lower = if f.area().height >= MIN_HEIGHT_FOR_PREVIEW { Constraint::Fill(1) } else { Constraint::Length(0) };
    let [summary, table, detail, lower, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(6),
        lower,
        Constraint::Length(footer_rows(app, f.area().width)),
    ])
    .areas(f.area());
    let (detail, lower) = if lower.height > 2 {
        let [_, detail, lower, _] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(detail.height), Constraint::Fill(1), Constraint::Length(1)]).areas(detail.union(lower));
        (detail, lower)
    } else {
        (detail, lower)
    };
    app.hit.preview = Rect::default();
    f.render_widget(Paragraph::new(summary_line(app)), summary);
    draw_table(f, app, table);
    draw_detail(f, app, detail);
    draw_conversations(f, app, lower, now_ms);
    draw_footer(f, app, footer);
    draw_dialog(f, app);
}

fn summary_line(app: &App) -> Line<'static> {
    let w = &app.worktrees;
    let linked: Vec<&Worktree> = w.worktrees().iter().filter(|w| w.is_linked()).collect();
    let repos = w.rows.iter().filter(|r| matches!(r, ListRow::Header { .. })).count();
    let mut text = format!(" ccm  worktrees  {} in {} {}", linked.len(), repos, if repos == 1 { "repository" } else { "repositories" });
    if w.only_idle {
        let shown = w.rows.iter().filter(|r| matches!(r, ListRow::Item(_))).count();
        text = format!(" ccm  worktrees  showing {shown} with nothing to lose, of {}", linked.len());
    }
    let changed = linked.iter().filter(|w| w.state.is_some_and(|s| s.changes > 0)).count();
    let unpushed = linked.iter().filter(|w| w.state.is_some_and(|s| s.unpushed > 0)).count();
    if changed > 0 {
        text.push_str(&format!("  {changed} with changes"));
    }
    if unpushed > 0 {
        text.push_str(&format!("  {unpushed} with unpushed commits"));
    }
    let marked = w.marked_shown().len();
    if marked > 0 {
        text.push_str(&format!("  {marked} marked"));
    }
    if w.scanning {
        text.push_str("  checking…");
    }
    let mut spans = vec![Span::styled(text, Style::new().add_modifier(Modifier::BOLD))];
    if let Some(error) = w.scan.as_ref().and_then(|s| s.errors.first()) {
        spans.push(Span::styled(format!("  {}", sanitize(error)), Style::new().fg(Color::Yellow)));
    }
    Line::from(spans)
}

fn open_count(app: &App, links: &[Link]) -> usize {
    links.iter().filter(|l| session(app, &l.id).is_some_and(|s| s.open)).count()
}

fn session<'a>(app: &'a App, id: &str) -> Option<&'a Session> {
    app.sessions.iter().find(|s| s.id == id)
}

fn status(app: &App, w: &Worktree, links: &[Link]) -> &'static str {
    if w.prunable {
        "missing"
    } else if w.from_windows() {
        "Windows"
    } else if open_count(app, links) > 0 {
        "open"
    } else if w.locked {
        "locked"
    } else {
        ""
    }
}

fn cell_text(app: &App, field: Field, w: &Worktree, links: &[Link], width: usize) -> String {
    let unknown = if w.prunable { "-" } else { "?" };
    match field {
        Field::Folder if app.worktrees.marked.contains(&w.path) => {
            format!("● {}", fit_start(&sanitize(&short_path(&w.path.to_string_lossy(), &app.home)), width.saturating_sub(2)))
        }
        Field::Folder => fit_start(&sanitize(&short_path(&w.path.to_string_lossy(), &app.home)), width),
        Field::Branch => fit_end(&w.branch.as_deref().map_or("(detached)".to_string(), sanitize), width),
        Field::Changes => match w.state {
            None => unknown.into(),
            Some(State { changes: 0, .. }) => "-".into(),
            Some(State { changes: 1, .. }) => "1 file".into(),
            Some(s) => format!("{} files", s.changes),
        },
        Field::Unpushed => match w.state {
            None => unknown.into(),
            Some(State { unpushed: 0, .. }) => "-".into(),
            Some(s) => s.unpushed.to_string(),
        },
        Field::Conversations => match (links.len(), open_count(app, links)) {
            (0, _) => "-".into(),
            (n, 0) => n.to_string(),
            (n, open) => format!("{n} ({open} open)"),
        },
        Field::Status => status(app, w, links).into(),
    }
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let columns: Vec<&(Field, Column)> = COLUMNS.iter().filter(|(_, c)| f.area().width >= c.min_terminal_width).collect();
    let widths: Vec<Constraint> = columns.iter().map(|(_, c)| c.width).collect();
    let header = Row::new(columns.iter().map(|(_, c)| Cell::from(c.title))).style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let cells = Layout::horizontal(widths.iter().copied()).flex(Flex::Start).spacing(COLUMN_SPACING).split(area);

    app.hit.header = Rect { height: 1, ..area };
    app.hit.body = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
    app.hit.columns.clear();

    let wt = &app.worktrees;
    let rows: Vec<Row> = wt
        .rows
        .iter()
        .map(|row| match row {
            ListRow::Header { repo, count } => {
                let plural = if *count == 1 { "worktree" } else { "worktrees" };
                Row::new([Cell::from(format!("{}  {count} {plural}", sanitize(&short_path(&repo.to_string_lossy(), &app.home))))])
                    .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            }
            ListRow::Item(i) => {
                let w = &wt.worktrees()[*i];
                let links = wt.links.get(*i).map_or(&[][..], Vec::as_slice);
                let style = match status(app, w, links) {
                    _ if wt.marked.contains(&w.path) => Style::new().fg(Color::Yellow),
                    "missing" => Style::new().fg(Color::DarkGray),
                    "open" => Style::new().fg(Color::Green),
                    _ => Style::new(),
                };
                Row::new(columns.iter().zip(cells.iter()).map(|((field, _), cell)| Cell::from(cell_text(app, *field, w, links, cell.width as usize)))).style(style)
            }
        })
        .collect();

    if rows.is_empty() {
        let message = match &wt.scan {
            None => "Looking for worktrees…".to_string(),
            Some(scan) if !scan.errors.is_empty() => sanitize(&scan.errors.join(" ")),
            Some(_) if wt.only_idle => "None left with nothing to lose. Press c to show all worktrees.".to_string(),
            Some(_) => "No worktrees found. Only repositories that a conversation worked in are checked.".to_string(),
        };
        f.render_widget(Paragraph::new(header_only_lines(message)), area);
        return;
    }
    let table = Table::new(rows, widths)
        .header(header)
        .flex(Flex::Start)
        .column_spacing(COLUMN_SPACING)
        .highlight_spacing(HighlightSpacing::Never)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(table, area, &mut app.worktrees.table);
}

/// What the state means, in words. Remote branches are as Git last saw them, as `ccm` never
/// fetches.
fn state_lines(w: &Worktree) -> Vec<String> {
    if w.prunable {
        return vec!["Its folder is gone. Git still lists it until it's pruned.".into()];
    }
    if w.from_windows() {
        return vec!["Made by Git for Windows. Git here can't reach its folder, so ccm leaves it alone. Manage it from Windows.".into()];
    }
    let Some(s) = w.state else {
        return vec!["Git couldn't say whether it has changes or unpushed commits.".into()];
    };
    let changes = match s.changes {
        0 => "No uncommitted changes.".to_string(),
        n => format!("{} not committed.", plural(n, "file", "files")),
    };
    let unpushed = match (s.unpushed, s.has_remote) {
        (0, true) => "Every commit is on a remote branch, as of the last fetch.".to_string(),
        (0, false) => "No remote, and every commit is on another branch too.".to_string(),
        (n, true) => format!("{} on no remote branch, as of the last fetch.", plural(n, "commit", "commits")),
        (n, false) => format!("No remote, and {} on no other branch.", plural(n, "commit is", "commits are")),
    };
    let mut lines = vec![format!("{changes} {unpushed}")];
    if s.upstream_gone {
        lines.push("Its remote branch was deleted, which usually means it was merged. ccm can't check.".into());
    }
    lines
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::new().borders(Borders::TOP).title(" Details ");
    let Some((w, _)) = app.worktrees.selected() else {
        f.render_widget(block, area);
        return;
    };
    let branch = w.branch.as_deref().map_or("none, a commit is checked out directly".to_string(), sanitize);
    let mut text = vec![
        Line::from(sanitize(&w.path.to_string_lossy())),
        Line::from(format!("Branch  {branch}   Repository {}", sanitize(&short_path(&w.repo.to_string_lossy(), &app.home)))),
    ];
    text.extend(state_lines(w).into_iter().map(Line::from));
    if w.locked {
        text.push(Line::from("Locked in Git, so Git won't remove it until it's unlocked."));
    }
    f.render_widget(Paragraph::new(text).block(block), area);
}

/// The removal popup. Each worktree says what goes with it and which conversations worked there,
/// so nothing about it is a surprise.
pub(super) fn remove_dialog(app: &App) -> Dialog {
    let confirm = app.confirm_remove.as_ref().expect("checked by the caller");
    let plan = &confirm.plan;
    let plain = Style::new();
    let bold = plain.add_modifier(Modifier::BOLD);
    let warn = Style::new().fg(Color::Yellow);
    let count = plan.items.len();
    let mut top = vec![(format!("{count} worktree{} will be removed. Branches stay, so commits are kept.", if count == 1 { "" } else { "s" }), bold)];
    let danger = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    let ahead: Vec<&Planned> = plan.items.iter().filter(|i| i.unpushed > 0).collect();
    if !ahead.is_empty() {
        let commits: u64 = ahead.iter().map(|i| i.unpushed).sum();
        let (of, have) = if count == 1 { ("It".to_string(), "has") } else { (format!("{} of these", ahead.len()), "have") };
        let commits = plural(commits, "unpushed commit", "unpushed commits");
        top.push((format!("Warning: {of} {have} {commits}. Branches keep them, but only on this machine."), danger));
    }
    let ignored: u64 = plan.items.iter().map(|i| i.ignored_bytes).sum();
    if plan.items.iter().any(|i| !i.ignored.is_empty()) {
        let at_least = if plan.items.iter().any(|i| i.ignored_more) { "at least " } else { "" };
        top.push((format!("Files Git ignores go too, {at_least}{}. This can't be undone.", bytes(ignored)), warn.add_modifier(Modifier::BOLD)));
    }

    let mut body = Vec::new();
    for item in &plan.items {
        body.extend(item_lines(app, item, bold, warn, plain));
    }
    body.extend(plan.refused.iter().map(|r| {
        let path = sanitize(&short_path(&r.path.to_string_lossy(), &app.home));
        (format!("Skipping {path}: {}", sanitize(&r.reason)), plain)
    }));
    if body.last().is_some_and(|(l, _): &(String, Style)| l.is_empty()) {
        body.pop();
    }
    let bottom = match confirm.countdown.seconds_left() {
        Some(left) => {
            let again = if confirm.countdown.restarted { "Too early, so it started over. " } else { "" };
            format!("{again}Remove unlocks in {left}s. Pressing y, Del or Remove before then starts it over.   Any other key cancels.")
        }
        None => "y or Del remove   any other key cancels   or click Remove again".into(),
    };
    Dialog { title: " Remove worktrees? ", top, body, bottom }
}

pub(super) fn item_lines(app: &App, item: &Planned, bold: Style, warn: Style, plain: Style) -> Vec<(String, Style)> {
    let danger = Style::new().fg(Color::Red);
    planned_lines(item, &app.sessions, &app.home)
        .into_iter()
        .map(|(text, tone)| {
            let style = match tone {
                Tone::Plain => plain,
                Tone::Bold => bold,
                Tone::Warn => warn,
                Tone::Danger => danger,
            };
            (text, style)
        })
        .collect()
}

fn draw_conversations(f: &mut Frame, app: &App, area: Rect, now_ms: i64) {
    if area.height == 0 {
        return;
    }
    let block = Block::new().borders(Borders::TOP).title(" Conversations that worked here ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner };
    let width = inner.width as usize;
    let Some((_, links)) = app.worktrees.selected() else { return };
    if links.is_empty() {
        f.render_widget(Paragraph::new("None that ccm knows of."), inner);
        return;
    }
    let mut links: Vec<&Link> = links.iter().collect();
    links.sort_by_key(|l| std::cmp::Reverse(l.last_ms));
    let lines: Vec<Line> = links
        .iter()
        .map(|l| {
            let s = session(app, &l.id);
            let mut notes = vec![format!("last here {}", ago(now_ms, l.last_ms)), format!("{} of its activity", share(l))];
            if l.last_here {
                notes.push("still here when it stopped".into());
            }
            if l.started_here {
                notes.push("started here".into());
            }
            let state = match s {
                Some(s) if s.open => "open  ",
                Some(s) if s.crashed => "crashed  ",
                _ => "",
            };
            let tail = format!("  {state}{}", notes.join(", "));
            let name = s.map_or(l.id.clone(), |s| sanitize(&s.display_name()));
            let room = width.saturating_sub(unicode_width::UnicodeWidthStr::width(tail.as_str())).max(10);
            let style = match s {
                Some(s) if s.open => Style::new().fg(Color::Green),
                _ => Style::new(),
            };
            Line::from(Span::styled(fit_end(&format!("{}{tail}", fit_end(&name, room)), width), style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::View;
    use ccm_core::config::Config;
    use ccm_core::model::{FolderVisit, Source};
    use ccm_core::worktree::Scan;
    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const NOW: i64 = 1_000 * 60 * 60 * 24 * 365;
    const HOUR: i64 = 60 * 60 * 1000;

    fn worktree(path: &str, repo: &str, branch: Option<&str>, state: Option<State>) -> Worktree {
        Worktree {
            path: path.into(),
            repo: repo.into(),
            main: path == repo,
            bare: false,
            branch: branch.map(String::from),
            locked: false,
            prunable: false,
            state,
        }
    }

    fn state(changes: u64, unpushed: u64) -> Option<State> {
        Some(State { changes, unpushed, has_remote: true, upstream_gone: false })
    }

    fn session(id: &str, name: &str, visits: &[(&str, u64)], open: bool) -> Session {
        Session {
            id: id.into(),
            source: Source::ClaudeCode,
            transcript_path: String::new(),
            transcript_size: 0,
            transcript_mtime_ns: 0,
            size_bytes: 0,
            name: name.into(),
            folder: visits.first().map(|v| v.0.into()),
            folders: visits
                .iter()
                .enumerate()
                .map(|(n, &(path, records))| FolderVisit { path: path.into(), records, last_record: n as u64 + 1, last_ms: Some(NOW - (3 - n as i64) * HOUR) })
                .collect(),
            branch: None,
            model: None,
            started_at_ms: None,
            last_active_ms: Some(NOW),
            context_used: None,
            context_window: None,
            prompts: 1,
            replies: 1,
            cost_usd: None,
            lines_added: None,
            lines_removed: None,
            pr_url: None,
            continued_in: None,
            forked_from: None,
            recaps: Vec::new(),
            bad_lines: 0,
            open,
            crashed: false,
            window_size: None,
            folder_exists: true,
        }
    }

    fn app() -> App {
        let mut a = App::new(Config::default(), "/c".into(), "/home/me".into());
        a.loading = false;
        a.set_sessions(vec![
            // Works in the main repository and passes through the login worktree.
            session("id-a", "Coordinator", &[("/home/me/shop", 95), ("/home/me/shop-login/src", 5)], true),
            session("id-b", "Login fix", &[("/home/me/shop-login", 40)], false),
        ]);
        a.on_key(KeyCode::Char('w'));
        assert_eq!(a.worktree_request(), Some(vec!["/home/me/shop".into(), "/home/me/shop-login".into(), "/home/me/shop-login/src".into()]));
        assert_eq!(a.worktree_request(), None, "one scan at a time");
        let mut gone = worktree("/home/me/shop-old", "/home/me/shop", Some("old"), None);
        gone.prunable = true;
        a.finish_worktree_scan(Scan {
            worktrees: vec![
                worktree("/home/me/shop", "/home/me/shop", Some("main"), None),
                worktree("/home/me/shop-login", "/home/me/shop", Some("login-fix"), state(3, 0)),
                worktree("/home/me/shop-api", "/home/me/shop", Some("api"), state(0, 4)),
                gone,
                worktree("/home/me/blog-x", "/home/me/blog", None, None),
            ],
            errors: Vec::new(),
        });
        a
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

    #[test]
    fn linked_worktrees_are_listed_by_repository_with_their_state() {
        let mut a = app();
        let screen = render(&mut a, 140, 30);
        assert!(line(&screen, "ccm  worktrees").contains("4 in 2 repositories  1 with changes  1 with unpushed commits"), "{screen}");
        assert!(line(&screen, "~/blog ").contains("1 worktree"));
        assert!(line(&screen, "~/shop ").contains("3 worktrees"));
        assert!(!screen.lines().any(|l| l.contains("~/shop ") && l.contains("main")), "the main worktree isn't a row:\n{screen}");
        let login = line(&screen, "~/shop-login");
        for value in ["login-fix", "3 files", "2 (1 open)", "open"] {
            assert!(login.contains(value), "{value} missing from: {login}");
        }
        assert!(line(&screen, "~/shop-api").contains(" 4 "));
        assert!(line(&screen, "~/shop-old").contains("missing"));
        let detached = line(&screen, "~/blog-x");
        assert!(detached.contains("(detached)") && detached.contains('?'), "unknown state shows as ?: {detached}");
    }

    #[test]
    fn the_selected_worktree_lists_its_conversations_and_how_much_they_did_there() {
        let mut a = app();
        // The first row is the blog's detached worktree.
        assert!(render(&mut a, 140, 30).contains("Git couldn't say"));
        // Down skips the shop header, then passes the api worktree.
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Down);
        let screen = render(&mut a, 140, 30);
        assert!(screen.contains("Branch  login-fix") && screen.contains("3 files not committed."), "{screen}");
        let coordinator = line(&screen, "Coordinator");
        assert!(coordinator.contains("open") && coordinator.contains("5% of its activity") && coordinator.contains("still here when it stopped"), "{coordinator}");
        let fix = line(&screen, "Login fix");
        assert!(fix.contains("100% of its activity") && fix.contains("started here"), "{fix}");
        assert!(screen.find("Coordinator").unwrap() < screen.find("Login fix").unwrap(), "most recent first");
    }

    #[test]
    fn selection_skips_repository_headers_and_survives_a_rescan() {
        let mut a = app();
        render(&mut a, 140, 30);
        a.on_key(KeyCode::End);
        assert!(a.worktrees.selected().unwrap().0.path.ends_with("shop-old"));
        a.on_key(KeyCode::Home);
        a.on_key(KeyCode::Down);
        assert!(a.worktrees.selected().unwrap().0.path.ends_with("shop-api"), "skipped the header");
        let scan = a.worktrees.scan.clone().unwrap();
        a.finish_worktree_scan(Scan { worktrees: scan.worktrees.into_iter().rev().collect(), ..scan });
        assert!(a.worktrees.selected().unwrap().0.path.ends_with("shop-api"));
    }

    #[test]
    fn w_and_esc_go_back_and_nothing_else_reaches_the_conversations() {
        let mut a = app();
        for key in ['R', '/', 'J', 'o', 'g', 'y'] {
            a.on_key(KeyCode::Char(key));
        }
        assert!(a.renaming.is_none() && !a.typing && !a.filters.junk && !a.filters.open && a.grouping == crate::app::Grouping::Off && a.status.is_none());
        a.on_key(KeyCode::Char('a'));
        assert!(a.marked.is_empty() && !a.worktrees.marked.is_empty(), "marks go on worktrees here, not conversations");
        a.on_key(KeyCode::Esc);
        a.on_key(KeyCode::Esc);
        assert_eq!(a.view, View::Conversations);
        assert!(!a.quit);
        a.on_key(KeyCode::Char('w'));
        assert_eq!(a.view, View::Worktrees);
        assert!(a.worktree_request().is_some(), "each visit looks again");
        a.on_key(KeyCode::Char('w'));
        assert_eq!(a.view, View::Conversations);
        assert_eq!(a.worktree_request(), None, "no scans while the view is closed");
    }

    #[test]
    fn conversations_changing_update_the_connections() {
        let mut a = app();
        let sessions = a.sessions.iter().filter(|s| s.id != "id-b").cloned().collect();
        a.set_sessions(sessions);
        assert!(line(&render(&mut a, 140, 30), "~/shop-login").contains(" 1 (1 open)"));
    }

    #[test]
    fn clicks_select_rows_and_the_footer_switches_back() {
        let mut a = app();
        let screen = render(&mut a, 140, 30);
        let y = screen.lines().position(|l| l.contains("~/shop-api")).unwrap() as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 5, y);
        assert!(a.worktrees.selected().unwrap().0.path.ends_with("shop-api"));
        let header = screen.lines().position(|l| l.contains("~/shop ")).unwrap() as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 5, header);
        assert!(a.worktrees.selected().unwrap().0.path.ends_with("shop-api"), "a header can't be selected");
        let screen = render(&mut a, 140, 30);
        let footer = screen.lines().position(|l| l.contains("w Conversations")).unwrap() as u16;
        let x = line(&screen, "w Conversations").find("w Conversations").unwrap() as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x + 1, footer);
        assert_eq!(a.view, View::Conversations);
    }

    #[test]
    fn states_are_explained_in_words() {
        let mut w = worktree("/x", "/r", Some("b"), Some(State { changes: 0, unpushed: 2, has_remote: true, upstream_gone: true }));
        let text = state_lines(&w).join(" ");
        assert!(text.contains("2 commits on no remote branch, as of the last fetch") && text.contains("usually means it was merged"), "{text}");
        w.state = Some(State { changes: 1, unpushed: 1, has_remote: false, upstream_gone: false });
        assert_eq!(state_lines(&w), ["1 file not committed. No remote, and 1 commit is on no other branch."]);
    }

    #[test]
    fn before_the_first_scan_and_with_nothing_found_it_says_so() {
        let mut a = App::new(Config::default(), "/c".into(), "/home/me".into());
        a.on_key(KeyCode::Char('w'));
        a.worktree_request();
        let screen = render(&mut a, 100, 20);
        assert!(screen.contains("Looking for worktrees…") && screen.contains("checking…"), "{screen}");
        a.finish_worktree_scan(Scan::default());
        assert!(render(&mut a, 100, 20).contains("No worktrees found"));
        a.finish_worktree_scan(Scan { errors: vec!["Git wasn't found, so worktrees can't be listed.".into()], ..Scan::default() });
        assert!(render(&mut a, 100, 20).contains("Git wasn't found"));
    }

    type Calls = std::rc::Rc<std::cell::RefCell<Vec<Vec<std::path::PathBuf>>>>;

    /// Planning refuses the blog's worktree, whose state is unknown, and accepts the rest. Removal records what it was given.
    fn removable(a: &mut App, fail: bool) -> (Calls, Calls) {
        use ccm_core::removal::{Outcome, Plan, Refused};
        let (planned, removed): (Calls, Calls) = Default::default();
        let (p, r) = (planned.clone(), removed.clone());
        let links = a.worktrees.links.clone();
        let worktrees = a.worktrees.worktrees().to_vec();
        a.remove_planner = Some(Box::new(move |targets, _| {
            p.borrow_mut().push(targets.iter().map(|t| t.path.clone()).collect());
            let mut plan = Plan::default();
            for t in targets {
                if t.path.ends_with("blog-x") {
                    plan.refused.push(Refused { path: t.path.clone(), reason: "Git couldn't read its state".into() });
                    continue;
                }
                let at = worktrees.iter().position(|w| w.path == t.path).unwrap();
                plan.items.push(Planned {
                    target: t.clone(),
                    branch: worktrees[at].branch.clone(),
                    gone: worktrees[at].prunable,
                    ignored: if t.path.ends_with("shop-login") { vec![".env".into(), "node_modules/".into()] } else { Vec::new() },
                    ignored_bytes: if t.path.ends_with("shop-login") { 3 * 1024 * 1024 } else { 0 },
                    ignored_more: false,
                    links: links[at].clone(),
                    unpushed: worktrees[at].state.map_or(0, |s| s.unpushed),
                    has_remote: true,
                    upstream_gone: t.path.ends_with("shop-merged"),
                    bytes: 0,
                });
            }
            plan
        }));
        a.remover = Some(Box::new(move |plan, _| {
            let paths: Vec<std::path::PathBuf> = plan.items.iter().map(|i| i.target.path.clone()).collect();
            r.borrow_mut().push(paths.clone());
            if fail {
                Outcome { failures: paths.into_iter().map(|path| Refused { path, reason: "Git refused: no".into() }).collect(), ..Outcome::default() }
            } else {
                Outcome { removed: paths, ..Outcome::default() }
            }
        }));
        (planned, removed)
    }

    fn select(a: &mut App, name: &str) {
        a.on_key(KeyCode::Home);
        for _ in 0..a.worktrees.rows.len() {
            if a.worktrees.selected().unwrap().0.path.ends_with(name) {
                return;
            }
            a.on_key(KeyCode::Down);
        }
        panic!("{name} isn't a row");
    }

    #[test]
    fn the_popup_says_what_goes_and_which_conversations_worked_there() {
        let mut a = app();
        removable(&mut a, false);
        select(&mut a, "shop-login");
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 140, 40);
        for text in [
            "Remove worktrees?",
            "1 worktree will be removed. Branches stay, so commits are kept.",
            "Files Git ignores go too, 3M. This can't be undone.",
            "~/shop-login  login-fix",
            "Deletes ignored files, 3M: .env, node_modules/",
            "2 conversations worked here:",
            "Coordinator  (5% of its activity)",
            "Login fix  (100% of its activity)",
            "resuming it later recreates this folder empty",
            "y or Del remove",
        ] {
            assert!(screen.contains(text), "{text} missing:\n{screen}");
        }
    }

    #[test]
    fn y_removes_and_the_row_goes() {
        let mut a = app();
        let (_, removed) = removable(&mut a, false);
        select(&mut a, "shop-login");
        a.on_key(KeyCode::Char('d'));
        a.on_key(KeyCode::Char('y'));
        assert_eq!(a.status.as_deref(), Some("Removing…"));
        a.perform_delete();
        assert_eq!(removed.borrow().len(), 1);
        assert_eq!(a.status.as_deref(), Some("Removed 1 worktree. Its branch is kept."));
        assert!(!render(&mut a, 140, 30).contains("~/shop-login"));
        assert!(a.worktree_request().is_some(), "it looks again straight away");
    }

    #[test]
    fn any_other_key_cancels_and_a_quick_second_del_waits() {
        let mut a = app();
        let (_, removed) = removable(&mut a, false);
        select(&mut a, "shop-login");
        a.on_key(KeyCode::Delete);
        a.on_key(KeyCode::Char('x'));
        assert!(a.confirm_remove.is_none());
        assert_eq!(a.status.as_deref(), Some("Cancelled. Nothing was removed."));
        a.on_key(KeyCode::Delete);
        a.on_key(KeyCode::Delete);
        assert!(a.confirm_remove.is_some() && !a.has_pending_delete(), "held Del");
        a.age_removal();
        a.on_key(KeyCode::Delete);
        a.perform_delete();
        assert_eq!(removed.borrow().len(), 1);
    }

    #[test]
    fn marks_pick_what_goes_and_refusals_are_listed() {
        let mut a = app();
        let (planned, _) = removable(&mut a, false);
        select(&mut a, "blog-x");
        a.on_key(KeyCode::Char(' '));
        assert!(a.worktrees.selected().unwrap().0.path.ends_with("shop-api"), "space moves on");
        a.on_key(KeyCode::Char(' '));
        let screen = render(&mut a, 140, 40);
        assert!(line(&screen, "ccm  worktrees").contains("2 marked") && line(&screen, "~/blog-x").contains("● "), "{screen}");
        a.on_key(KeyCode::Char('d'));
        assert_eq!(planned.borrow()[0].len(), 2);
        let screen = render(&mut a, 140, 40);
        assert!(screen.contains("Skipping ~/blog-x: Git couldn't read its state"), "{screen}");
        a.on_key(KeyCode::Char('n'));

        a.on_key(KeyCode::Esc);
        assert!(a.worktrees.marked.is_empty() && a.view == View::Worktrees, "Esc clears the marks first");
        a.on_key(KeyCode::Char('a'));
        assert_eq!(a.worktrees.marked.len(), 4);
        a.on_key(KeyCode::Char('a'));
        assert!(a.worktrees.marked.is_empty());
    }

    #[test]
    fn when_everything_is_refused_it_says_why_without_a_popup() {
        let mut a = app();
        removable(&mut a, false);
        select(&mut a, "blog-x");
        a.on_key(KeyCode::Char('d'));
        assert!(a.confirm_remove.is_none());
        assert_eq!(a.status.as_deref(), Some("Can't remove: Git couldn't read its state."));
    }

    #[test]
    fn a_failure_is_reported_until_a_key_is_pressed() {
        let mut a = app();
        removable(&mut a, true);
        select(&mut a, "shop-login");
        a.on_key(KeyCode::Char('d'));
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        let screen = render(&mut a, 140, 30);
        assert!(screen.contains("Remove finished") && screen.contains("Removed 0 worktrees") && screen.contains("Not removed /home/me/shop-login: Git refused: no"), "{screen}");
        a.on_key(KeyCode::Char('j'));
        a.on_key(KeyCode::Char('x'));
        assert!(a.report.is_none());
    }

    #[test]
    fn with_the_mouse_the_second_click_on_remove_confirms_and_elsewhere_cancels() {
        let mut a = app();
        let (_, removed) = removable(&mut a, false);
        select(&mut a, "shop-login");
        let screen = render(&mut a, 140, 30);
        let y = screen.lines().position(|l| l.contains("d Remove")).unwrap() as u16;
        let x = line(&screen, "d Remove").find("d Remove").unwrap() as u16 + 1;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        assert!(a.confirm_remove.is_some());
        render(&mut a, 140, 30);
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        assert!(a.confirm_remove.is_some(), "the tail of a double-click doesn't confirm");
        a.age_removal();
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        a.perform_delete();
        assert_eq!(removed.borrow().len(), 1);

        select(&mut a, "shop-old");
        render(&mut a, 140, 30);
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 2, 2);
        assert!(a.confirm_remove.is_none() && removed.borrow().len() == 1);
    }

    #[test]
    fn unpushed_commits_warn_and_lock_confirming_while_a_countdown_runs() {
        let mut a = app();
        let (_, removed) = removable(&mut a, false);
        select(&mut a, "shop-api");
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 140, 40);
        for text in [
            "Warning: It has 4 unpushed commits. Branches keep them, but only on this machine.",
            "  4 commits on no remote branch, as of the last fetch.",
            "Remove unlocks in 5s. Pressing y, Del or Remove before then starts it over.",
        ] {
            assert!(screen.contains(text), "{text} missing:\n{screen}");
        }
        assert!(!a.can_press_delete(), "the Remove button is dimmed");
        for key in [KeyCode::Char('y'), KeyCode::Delete, KeyCode::Char('y')] {
            a.age_removal();
            a.on_key(key);
            assert!(a.confirm_remove.is_some() && !a.has_pending_delete(), "{key:?} got through the countdown");
        }
        assert!(render(&mut a, 140, 40).contains("Too early, so it started over. Remove unlocks in 5s."));

        a.end_countdown();
        assert!(render(&mut a, 140, 40).contains("y or Del remove"));
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(removed.borrow().len(), 1);
    }

    #[test]
    fn during_the_countdown_any_other_key_still_cancels() {
        let mut a = app();
        let (_, removed) = removable(&mut a, false);
        select(&mut a, "shop-api");
        a.on_key(KeyCode::Char('d'));
        a.on_key(KeyCode::Down);
        assert!(a.confirm_remove.is_some(), "scrolling isn't an answer");
        a.on_key(KeyCode::Char('n'));
        assert!(a.confirm_remove.is_none() && removed.borrow().is_empty());
    }

    #[test]
    fn clicking_remove_during_the_countdown_starts_it_over_and_clicking_elsewhere_cancels() {
        let mut a = app();
        let (_, removed) = removable(&mut a, false);
        select(&mut a, "shop-api");
        let screen = render(&mut a, 140, 30);
        let y = screen.lines().position(|l| l.contains("d Remove")).unwrap() as u16;
        let x = line(&screen, "d Remove").find("d Remove").unwrap() as u16 + 1;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        render(&mut a, 140, 30);
        a.age_removal();
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        assert!(a.confirm_remove.as_ref().is_some_and(|c| c.countdown.restarted) && !a.has_pending_delete());
        a.end_countdown();
        render(&mut a, 140, 30);
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        a.perform_delete();
        assert_eq!(removed.borrow().len(), 1);

        select(&mut a, "shop-old");
        render(&mut a, 140, 30);
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 2, 2);
        assert!(a.confirm_remove.is_none() && removed.borrow().len() == 1);
    }

    #[test]
    fn a_deleted_remote_branch_is_explained_and_no_countdown_without_unpushed_commits() {
        let mut a = app();
        removable(&mut a, false);
        let scan = a.worktrees.scan.clone().unwrap();
        let mut worktrees = scan.worktrees.clone();
        worktrees.push(worktree("/home/me/shop-merged", "/home/me/shop", Some("merged"), state(0, 1)));
        a.finish_worktree_scan(Scan { worktrees, ..scan });
        removable(&mut a, false);
        select(&mut a, "shop-merged");
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 140, 40);
        assert!(screen.contains("usually means it was merged") && screen.contains("Remove unlocks in"), "{screen}");
        a.on_key(KeyCode::Esc);
        select(&mut a, "shop-login");
        a.on_key(KeyCode::Char('d'));
        assert!(a.confirm_remove.as_ref().unwrap().countdown.seconds_left().is_none());
        assert!(!render(&mut a, 140, 40).contains("Warning:"));
    }

    #[test]
    fn narrow_and_short_terminals_do_not_panic() {
        let mut a = app();
        for (w, h) in [(30, 8), (50, 12), (80, 24), (20, 3)] {
            render(&mut a, w, h);
        }
    }
}
