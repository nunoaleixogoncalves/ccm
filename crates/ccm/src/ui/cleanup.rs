//! The clean-up screen: one line per kind of leftover, with how many there are and what
//! removing them would free. Nothing is removed from here. A line opens its items where they
//! can be marked and removed the usual way.

use ccm_core::format::bytes;
use ccm_core::query::Suggestion;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::{draw_dialog, draw_footer, footer_rows, COLUMN_SPACING};
use crate::app::{App, Cleanup};

pub(super) fn draw(f: &mut Frame, app: &mut App) {
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
    draw_detail(f, app, detail);
    draw_footer(f, app, footer);
    draw_dialog(f, app);
}

fn summary_line(app: &App) -> Line<'static> {
    let tallies = Cleanup::ALL.map(|l| app.tally(l));
    let count: usize = tallies.iter().map(|t| t.count).sum();
    let freed: u64 = tallies.iter().map(|t| t.bytes).sum();
    let text = format!(" ccm  clean up  {count} suggested, {} could be freed", bytes(freed));
    Line::from(Span::styled(text, Style::new().add_modifier(Modifier::BOLD)))
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
    }
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let widths = [Constraint::Min(20), Constraint::Length(8), Constraint::Length(10)];
    let header = Row::new(["Suggestion", "   Items", "     Frees"]).style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let rows: Vec<Row> = Cleanup::ALL
        .iter()
        .map(|&line| {
            let tally = app.tally(line);
            let style = if tally.count == 0 { Style::new().fg(Color::DarkGray) } else { Style::new() };
            let cells = [label(app, line), format!("{:>8}", tally.count), format!("{:>10}", bytes(tally.bytes))];
            Row::new(cells.map(Cell::from)).style(style)
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
    let list = "Enter shows them in the list, where you mark them and delete them the usual way.";
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
    };
    vec![first, "Open ones aren't suggested.".into(), list.into()]
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let line = Cleanup::ALL[app.cleanup_row.min(Cleanup::ALL.len() - 1)];
    let block = Block::new().borders(Borders::TOP).title(" About this suggestion ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = inner.width.saturating_sub(1) as usize;
    let mut text: Vec<Line> = explanation(app, line).iter().flat_map(|l| super::wrap(l, width)).map(|l| Line::from(format!(" {l}"))).collect();
    text.push(Line::from(""));
    text.push(Line::from(Span::styled(" Nothing is deleted from this screen.", Style::new().fg(Color::DarkGray))));
    f.render_widget(Paragraph::new(text), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Action, View};
    use ccm_core::config::Config;
    use ccm_core::model::{Session, Source};
    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
            folders: Vec::new(),
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

    fn app() -> App {
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
        a.set_sessions(vec![junk, open_junk, gone, big_old, big_old_open, big_new]);
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
        let mut a = app();
        a.on_key(KeyCode::Char('C'));
        assert_eq!(a.view, View::Cleanup);
        let screen = render(&mut a, 100, 30);
        let no_reply = line(&screen, "never replied to");
        assert!(no_reply.contains(" 1 ") && no_reply.contains("1M"), "the open one isn't counted:\n{screen}");
        assert!(line(&screen, "folder is gone").contains("3M"));
        assert!(line(&screen, "over 100M not used for 30 days").contains("300M"), "{screen}");
        assert!(line(&screen, " ccm ").contains("3 suggested, 304M could be freed"), "{screen}");
        assert!(screen.contains("Nothing is deleted from this screen."));
        a.on_key(KeyCode::Esc);
        assert_eq!(a.view, View::Conversations);
        a.run(Action::Cleanup);
        a.on_key(KeyCode::Char('C'));
        assert_eq!(a.view, View::Conversations, "Shift+C goes back too");
    }

    #[test]
    fn an_empty_line_stays_with_zero() {
        let mut a = app();
        a.set_sessions(Vec::new());
        a.on_key(KeyCode::Char('C'));
        let screen = render(&mut a, 100, 30);
        assert!(line(&screen, "folder is gone").contains(" 0 "), "{screen}");
    }

    #[test]
    fn a_line_opens_the_list_showing_exactly_those() {
        let mut a = app();
        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Enter);
        assert_eq!(a.view, View::Conversations);
        assert_eq!(names(&a), ["big-old"]);
        let screen = render(&mut a, 120, 30);
        assert!(line(&screen, " ccm ").contains("clean-up: over 100M not used for 30 days"), "{screen}");
        a.on_key(KeyCode::Char('c'));
        assert_eq!(names(&a).len(), 6, "c clears it like any filter");
        assert!(!render(&mut a, 120, 30).contains("clean-up:"));

        a.on_key(KeyCode::Char('C'));
        a.on_key(KeyCode::Up);
        a.on_key(KeyCode::Up);
        a.on_key(KeyCode::Enter);
        assert_eq!(names(&a), ["junk"], "the open conversation Claude hasn't answered yet isn't shown");
    }

    #[test]
    fn a_double_click_on_a_line_opens_it() {
        let mut a = app();
        a.on_key(KeyCode::Char('C'));
        render(&mut a, 100, 30);
        let y = a.hit.body.y + 1;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 5, y);
        assert_eq!((a.cleanup_row, a.view), (1, View::Cleanup));
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 5, y);
        assert_eq!(a.view, View::Conversations);
        assert_eq!(names(&a), ["gone"]);
    }

    #[test]
    fn every_button_fits_and_small_terminals_do_not_panic() {
        let mut a = app();
        a.on_key(KeyCode::Char('C'));
        let screen = render(&mut a, 80, 24);
        for label in ["Enter Open", "q Quit", "Esc Conversations", "w Worktrees"] {
            assert!(screen.contains(label), "{label} missing:\n{screen}");
        }
        for (w, h) in [(20, 5), (40, 10), (1, 1)] {
            render(&mut a, w, h);
        }
    }
}
