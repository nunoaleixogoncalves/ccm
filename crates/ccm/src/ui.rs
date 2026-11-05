use ccm_core::delete::Item;
use ccm_core::format::{ago, bytes, sanitize};
use ccm_core::model::Session;
use ccm_core::preview::{Preview, Role};
use ccm_core::query::{GroupRow, SortKey};
use chrono::{Local, TimeZone};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Table};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{Action, App, View};

mod worktrees;

const COLUMN_SPACING: u16 = 1;
const MIN_NAME_WIDTH: usize = 10;
/// The footer sets its own background, so its text sets fixed colours too. Colours from the
/// terminal's theme could be unreadable on it.
const BAR: Color = Color::Indexed(236);
const BAR_KEY: Color = Color::Indexed(231);
const BAR_LABEL: Color = Color::Indexed(250);
const BAR_OFF: Color = Color::Indexed(244);
const BAR_NOTICE: Color = Color::Indexed(221);
const BAR_ON: Color = Color::Indexed(252);
const BAR_DANGER: Color = Color::Indexed(203);
const BAR_GROUP: Color = Color::Indexed(109);
/// Where a group's buttons start, so wrapped ones stay under their group.
const FOOTER_INDENT: u16 = 9;
const MAX_FOOTER_ROWS: u16 = 4;
const MIN_HEIGHT_FOR_PREVIEW: u16 = 18;

struct Column {
    title: &'static str,
    key: SortKey,
    width: Constraint,
    /// Dropped first when the terminal is narrow.
    min_terminal_width: u16,
}

const COLUMNS: [Column; 9] = [
    Column { title: "Name", key: SortKey::Name, width: Constraint::Min(20), min_terminal_width: 0 },
    Column { title: "Folder", key: SortKey::Folder, width: Constraint::Length(26), min_terminal_width: 70 },
    Column { title: "Active", key: SortKey::LastActive, width: Constraint::Length(7), min_terminal_width: 0 },
    Column { title: "Context", key: SortKey::Context, width: Constraint::Length(10), min_terminal_width: 50 },
    Column { title: "Msgs", key: SortKey::Messages, width: Constraint::Length(6), min_terminal_width: 90 },
    Column { title: "Size", key: SortKey::Size, width: Constraint::Length(7), min_terminal_width: 60 },
    Column { title: "Model", key: SortKey::Model, width: Constraint::Length(12), min_terminal_width: 110 },
    Column { title: "Branch", key: SortKey::Branch, width: Constraint::Length(16), min_terminal_width: 130 },
    Column { title: "Status", key: SortKey::Status, width: Constraint::Length(8), min_terminal_width: 40 },
];

pub fn draw(f: &mut Frame, app: &mut App, now_ms: i64) {
    if app.view == View::Worktrees {
        return worktrees::draw(f, app, now_ms);
    }
    let bar = if app.typing || !app.query.is_empty() { 1 } else { 0 };
    let preview_height = if f.area().height >= MIN_HEIGHT_FOR_PREVIEW { Constraint::Fill(1) } else { Constraint::Length(0) };
    let [summary, search, table, detail, preview, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(bar),
        Constraint::Fill(1),
        Constraint::Length(6),
        preview_height,
        Constraint::Length(footer_rows(app, f.area().width)),
    ])
    .areas(f.area());
    // The gaps come out of the preview, so the list keeps its height.
    let (detail, preview) = if preview.height > 2 {
        let [_, detail, preview, _] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(detail.height), Constraint::Fill(1), Constraint::Length(1)]).areas(detail.union(preview));
        (detail, preview)
    } else {
        (detail, preview)
    };

    f.render_widget(Paragraph::new(summary_line(app)), summary);
    draw_search(f, app, search);
    draw_table(f, app, table, now_ms);
    draw_detail(f, app, detail);
    draw_preview(f, app, preview);
    draw_footer(f, app, footer);
    draw_dialog(f, app);
}

/// Lines in the middle of a dialog scroll, and the top and bottom lines stay put, so the totals
/// and the keys are always on screen however long the list is.
struct Dialog {
    title: &'static str,
    top: Vec<(String, Style)>,
    body: Vec<(String, Style)>,
    bottom: String,
}

fn draw_dialog(f: &mut Frame, app: &mut App) {
    app.dialog_width = f.area().width.saturating_sub(4).min(90);
    let dialog = if let Some(report) = &app.report {
        let body = report.iter().map(|l| (l.clone(), Style::new())).collect();
        Dialog { title: app.report_title, top: Vec::new(), body, bottom: "Press any other key to close.".into() }
    } else if app.confirm_delete.is_some() {
        delete_dialog(app)
    } else if app.confirm_remove.is_some() {
        worktrees::remove_dialog(app)
    } else if app.reopen.is_some() {
        reopen_dialog(app)
    } else {
        return;
    };
    let area = f.area();
    let width = app.dialog_width;
    let inner_width = width.saturating_sub(2) as usize;
    let cursor = app.reopen.as_ref().map(|r| r.cursor);
    let wrapped = |lines: &[(String, Style)]| -> Vec<Line<'static>> {
        lines
            .iter()
            .flat_map(|(text, style)| wrap_indented(text, inner_width).into_iter().map(|l| Line::from(Span::styled(l, *style))).collect::<Vec<_>>())
            .collect()
    };
    let top = wrapped(&dialog.top);
    let body = wrapped(&dialog.body);
    let mut bottom = wrapped(&[(dialog.bottom, Style::new())]);

    let fixed = top.len() + bottom.len() + 1;
    let room = (area.height as usize).saturating_sub(2 + fixed);
    // The scroll hint takes a line of its own, but only when it's needed.
    let scrolls = body.len() > room;
    let visible = body.len().min(if scrolls { room.saturating_sub(1) } else { room });
    if let Some(cursor) = cursor {
        // Rows in this list never wrap, so a row's number is its line number.
        if cursor < app.dialog_scroll {
            app.dialog_scroll = cursor;
        } else if visible > 0 && cursor >= app.dialog_scroll + visible {
            app.dialog_scroll = cursor + 1 - visible;
        }
    }
    app.dialog_scroll = app.dialog_scroll.min(body.len() - visible);
    if scrolls {
        let (from, to) = (app.dialog_scroll + 1, app.dialog_scroll + visible);
        bottom.insert(0, Line::from(Span::styled(format!("↑↓ scroll   {from}-{to} of {}", body.len()), Style::new().fg(Color::Cyan))));
    }

    let height = ((top.len() + visible + 1 + bottom.len()) as u16 + 2).min(area.height);
    let box_area = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::new().borders(Borders::ALL).title(dialog.title).border_style(Style::new().fg(Color::Red));
    let inner = block.inner(box_area);
    f.render_widget(Clear, box_area);
    f.render_widget(block, box_area);

    let [top_area, list_area, _, bottom_area] = Layout::vertical([
        Constraint::Length(top.len() as u16),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(bottom.len() as u16),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(top), top_area);
    let shown: Vec<Line> = body.into_iter().skip(app.dialog_scroll).take(visible).collect();
    f.render_widget(Paragraph::new(shown), list_area);
    f.render_widget(Paragraph::new(bottom), bottom_area);
}

fn reopen_dialog(app: &App) -> Dialog {
    let reopen = app.reopen.as_ref().expect("checked by the caller");
    let ticked = reopen.rows.iter().filter(|r| r.ticked).count();
    let top = vec![(format!("{ticked} of {} will be resumed. Unticked ones stop showing as crashed.", reopen.rows.len()), Style::new().add_modifier(Modifier::BOLD))];
    let width = app.dialog_width.saturating_sub(2) as usize;
    let body = reopen
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let session = app.sessions.iter().find(|s| s.id == row.id);
            let name = session.map_or(row.id.clone(), |s| sanitize(&display_name(s)));
            let folder = session.and_then(|s| s.folder.as_deref()).map_or("-".to_string(), |p| sanitize(&short_path(p, &app.home)));
            let missing = if session.is_some_and(Session::is_folder_missing) { "  (folder missing, recreated if ticked)" } else { "" };
            let mark = if row.ticked { "[x]" } else { "[ ]" };
            // A long name or folder mustn't push out the warning about that folder. The end of a
            // path is the telling part.
            let name_room = name.width().min(MIN_NAME_WIDTH);
            let folder_room = width.saturating_sub(mark.len() + 3 + name_room + missing.width()).max(MIN_NAME_WIDTH);
            let folder = fit_start(&folder, folder_room);
            let room = width.saturating_sub(mark.len() + 3 + folder.width() + missing.width()).max(MIN_NAME_WIDTH);
            let text = fit_end(&format!("{mark} {}  {folder}{missing}", fit_end(&name, room)), width);
            let style = if i == reopen.cursor { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            (text, style)
        })
        .collect();
    Dialog { title: " Reopen crashed conversations ", top, body, bottom: "Space tick or untick   Enter reopen   Esc cancel".into() }
}

fn delete_dialog(app: &App) -> Dialog {
    let confirm = app.confirm_delete.as_ref().expect("checked by the caller");
    let plan = &confirm.plan;
    let name_of = |id: &str| {
        app.sessions.iter().find(|s| s.id == id).map_or_else(|| id.to_string(), |s| sanitize(&display_name(s)))
    };
    let plain = Style::new();
    let bold = plain.add_modifier(Modifier::BOLD);

    let freed = plan.bytes() + if confirm.include_notes { plan.note_bytes() } else { 0 };
    let count = plan.items.len();
    let plural = if count == 1 { "" } else { "s" };
    let top = vec![(format!("{count} conversation{plural}, {} freed. This can't be undone.", bytes(freed)), bold)];

    // One line each with a short ID, so a long or repeated name can't pass for other rows.
    let width = app.dialog_width.saturating_sub(2) as usize;
    let item = |i: &Item| {
        let tail = format!("  {}  {}", &i.id[..i.id.len().min(8)], bytes(i.bytes));
        (format!("{}{tail}", fit_end(&name_of(&i.id), width.saturating_sub(tail.width()).max(MIN_NAME_WIDTH))), plain)
    };
    let mut body: Vec<(String, Style)> = plan.items.iter().map(item).collect();
    body.extend(plan.skipped.iter().map(|s| (format!("Skipping {}: {}", name_of(&s.id), s.reason), plain)));
    body.extend(plan.warnings.iter().map(|w| (sanitize(w), plain)));
    if !plan.notes.is_empty() {
        let what = if confirm.include_notes { "will be deleted" } else { "will be kept" };
        body.push((String::new(), plain));
        body.push((format!("Memory notes nothing else wrote {what}:"), bold));
        body.extend(plan.notes.iter().map(|n| (format!("  {}", sanitize(&n.name())), plain)));
    }
    let notes = if plan.notes.is_empty() { "" } else { "m toggle notes   " };
    let bottom = format!("y or Del delete   {notes}any other key cancels   or click Delete again");
    Dialog { title: " Delete permanently? ", top, body, bottom }
}

fn summary_line(app: &App) -> Line<'static> {
    let total: u64 = app.sessions.iter().map(|s| s.size_bytes).sum();
    let open = app.sessions.iter().filter(|s| s.open).count();
    let shown = app.rows.iter().filter(|r| matches!(r, GroupRow::Item(_))).count();
    let mut text = format!(" ccm  {} conversations  {} on disk  {} open", app.sessions.len(), bytes(total), open);
    if app.filters.any() || app.is_searching() {
        text.push_str(&format!("  showing {shown}"));
    }
    let marked = app.marked_shown();
    let crashed = app.sessions.iter().filter(|s| s.crashed).count();
    if crashed > 0 {
        text.push_str(&format!("  {crashed} crashed"));
    }
    if !marked.is_empty() {
        let size: u64 = marked.iter().map(|s| s.size_bytes).sum();
        text.push_str(&format!("  {} marked {}", marked.len(), bytes(size)));
    }
    if app.loading {
        text.push_str("  scanning…");
    }
    Line::from(Span::styled(text, Style::new().add_modifier(Modifier::BOLD)))
}

fn draw_search(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let style = if app.typing { Style::new().add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::Gray) };
    let text = format!(" / {}", app.query);
    if app.typing {
        let x = area.x + (text.width() as u16).min(area.width.saturating_sub(1));
        f.set_cursor_position((x, area.y));
    }
    f.render_widget(Paragraph::new(text).style(style), area);
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect, now_ms: i64) {
    let columns: Vec<&Column> = COLUMNS.iter().filter(|c| f.area().width >= c.min_terminal_width).collect();
    let widths: Vec<Constraint> = columns.iter().map(|c| c.width).collect();

    let header = Row::new(columns.iter().map(|c| {
        let mark = match (c.key == app.sort, app.descending) {
            (true, true) => " ▼",
            (true, false) => " ▲",
            _ => "",
        };
        Cell::from(format!("{}{mark}", c.title))
    }))
    .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));

    let cells = Layout::horizontal(widths.iter().copied())
        .flex(Flex::Start)
        .spacing(COLUMN_SPACING)
        .split(area);
    let rows: Vec<Row> = app
        .rows
        .iter()
        .map(|row| match row {
            GroupRow::Header { folder, count, size_bytes } => {
                let name = folder.as_deref().map_or("(no folder recorded)".to_string(), |p| sanitize(&short_path(p, &app.home)));
                Row::new([Cell::from(format!("{name}  {count}  {}", bytes(*size_bytes)))])
                    .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            }
            GroupRow::Item(i) => {
                let s = &app.sessions[*i];
                Row::new(columns.iter().zip(cells.iter()).map(|(c, cell)| {
                    Cell::from(column_text(app, s, c.key, cell.width as usize, now_ms))
                }))
                .style(if app.marked.contains(&s.id) { Style::new().fg(Color::Yellow) } else { row_style(s) })
            }
        })
        .collect();

    app.hit.header = Rect { height: 1, ..area };
    app.hit.body = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
    app.hit.columns = cells.iter().copied().zip(columns.iter().map(|c| c.key)).collect();

    if rows.is_empty() {
        let message = if !app.sessions.is_empty() {
            "No conversations match. Press Esc to clear the search or c to clear filters.".to_string()
        } else if app.loading {
            "Scanning…".to_string()
        } else {
            format!("No conversations found in {}", app.claude_dir)
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
    f.render_stateful_widget(table, area, &mut app.table);
}

fn header_only_lines(message: String) -> Vec<Line<'static>> {
    vec![Line::from(""), Line::from(format!("  {message}"))]
}

fn row_style(s: &Session) -> Style {
    if s.open {
        Style::new().fg(Color::Green)
    } else if s.crashed {
        Style::new().fg(Color::Red)
    } else if s.is_junk() || s.is_folder_missing() {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new()
    }
}

fn column_text(app: &App, s: &Session, key: SortKey, width: usize, now_ms: i64) -> String {
    match key {
        SortKey::Name if app.marked.contains(&s.id) => format!("● {}", fit_end(&display_name(s), width.saturating_sub(2))),
        SortKey::Name => fit_end(&display_name(s), width),
        SortKey::Folder => s.folder.as_deref().map_or("-".into(), |p| fit_start(&sanitize(&short_path(p, &app.home)), width)),
        SortKey::LastActive => ago(now_ms, s.last_active_ms),
        SortKey::Context => app.context_text(s),
        SortKey::Messages => s.messages().to_string(),
        SortKey::Size => bytes(s.size_bytes),
        SortKey::Model => fit_end(s.model.as_deref().unwrap_or("-").trim_start_matches("claude-"), width),
        SortKey::Branch => fit_end(s.branch.as_deref().unwrap_or("-"), width),
        SortKey::Status => App::status_text(s).to_string(),
    }
}

fn display_name(s: &Session) -> String {
    if s.name.is_empty() { "(untitled)".to_string() } else { s.name.clone() }
}

fn short_path(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Shortens text keeping its start.
fn fit_end(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut used = 0;
    let kept: String = s
        .chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used < width
        })
        .collect();
    format!("{kept}…")
}

/// Shortens text keeping its end, which is the most telling part of a path.
fn fit_start(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    let kept: String = s.chars().skip(count + 1 - width.max(1)).collect();
    format!("…{kept}")
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::new().borders(Borders::TOP).title(" Details ");
    let Some(s) = app.selected() else {
        f.render_widget(block, area);
        return;
    };
    let date = |ms: Option<i64>| {
        ms.and_then(|t| Local.timestamp_millis_opt(t).single())
            .map_or("-".to_string(), |d| d.format("%Y-%m-%d %H:%M").to_string())
    };
    let cost = s.cost_usd.map_or("-".to_string(), |c| format!("${c:.2}"));
    let lines = match (s.lines_added, s.lines_removed) {
        (None, None) => "-".to_string(),
        (a, r) => format!("+{} -{}", a.unwrap_or(0), r.unwrap_or(0)),
    };
    let folder = sanitize(s.folder.as_deref().unwrap_or("-"));
    let mut text = vec![
        Line::from(format!("{}  {}", s.id, display_name(s))),
        Line::from(format!("Folder  {folder}{}", if s.is_folder_missing() { "  (missing)" } else { "" })),
        Line::from(format!("Started {}   Cost {cost}   Lines {lines}", date(s.started_at_ms))),
        Line::from(format!("PR      {}", s.pr_url.as_deref().unwrap_or("-"))),
    ];
    let mut notes = Vec::new();
    if let Some(next) = &s.continued_in {
        notes.push(format!("Continued in {}", sanitize(next)));
    }
    if s.bad_lines > 0 {
        notes.push(format!("{} unreadable lines", s.bad_lines));
    }
    if !notes.is_empty() {
        text.push(Line::from(notes.join("   ")));
    }
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_preview(f: &mut Frame, app: &mut App, area: Rect) {
    app.hit.preview = area;
    if area.height == 0 {
        return;
    }
    let block = Block::new().borders(Borders::TOP).title(" Preview ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner };
    let width = inner.width as usize;
    let height = inner.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let recaps: Vec<Line> = app.selected().map_or(Vec::new(), |s| recap_lines(&s.recaps, width));
    let pinned = recaps.len().min(height / 2);
    let [top, rest] = Layout::vertical([Constraint::Length(pinned as u16), Constraint::Fill(1)]).areas(inner);
    f.render_widget(Paragraph::new(recaps.into_iter().take(pinned).collect::<Vec<_>>()), top);

    let lines = match app.preview.as_ref().map(|p| &p.result) {
        None => Vec::new(),
        Some(Err(e)) => vec![Line::from(format!("Couldn't read this conversation: {}", sanitize(e)))],
        Some(Ok(p)) if p.turns.is_empty() => vec![Line::from("No messages.")],
        Some(Ok(p)) => message_lines(p, width),
    };
    // Starts at the newest message and scrolls back from there.
    let visible = rest.height as usize;
    app.preview_scroll = app.preview_scroll.min(lines.len().saturating_sub(visible));
    let end = lines.len() - app.preview_scroll;
    let start = end.saturating_sub(visible);
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), rest);
}

fn recap_lines(recaps: &[String], width: usize) -> Vec<Line<'static>> {
    let style = Style::new().fg(Color::Cyan);
    recaps
        .iter()
        .flat_map(|r| wrap(r, width.saturating_sub(2)))
        .map(|l| Line::from(Span::styled(format!("» {l}"), style)))
        .collect()
}

fn message_lines(p: &Preview, width: usize) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let mut out = Vec::new();
    if p.truncated {
        out.push(Line::from(Span::styled("… older messages not shown", dim)));
    }
    for (i, turn) in p.turns.iter().enumerate() {
        if i > 0 || p.truncated {
            out.push(Line::from(""));
        }
        let (label, color) = match turn.role {
            Role::You => ("You", Color::Yellow),
            Role::Claude => ("Claude", Color::Magenta),
        };
        let mut head = vec![Span::styled(label, Style::new().fg(color).add_modifier(Modifier::BOLD))];
        if turn.tool_calls > 0 {
            let plural = if turn.tool_calls == 1 { "" } else { "s" };
            head.push(Span::styled(format!("  {} tool call{plural}", turn.tool_calls), dim));
        }
        out.push(Line::from(head));
        out.extend(wrap(&turn.text, width).into_iter().map(Line::from));
    }
    out
}

/// Word wraps by display width, so wide characters such as Japanese line up. Words longer than a
/// line are split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        let mut used = 0;
        for word in line.split(' ') {
            let w = word.width();
            if used > 0 && used + 1 + w > width {
                out.push(std::mem::take(&mut current));
                used = 0;
            }
            if used > 0 {
                current.push(' ');
                used += 1;
            }
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > width && used > 0 {
                    out.push(std::mem::take(&mut current));
                    used = 0;
                }
                current.push(c);
                used += cw;
            }
        }
        out.push(current);
    }
    out
}

/// Wraps a dialog line, keeping its leading spaces on every line it becomes, so nested lines
/// stay under their heading. At most half the width is kept as indent.
fn wrap_indented(text: &str, width: usize) -> Vec<String> {
    let indent = (text.len() - text.trim_start_matches(' ').len()).min(width / 2);
    let pad = " ".repeat(indent);
    wrap(text.trim_start_matches(' '), width.saturating_sub(indent)).into_iter().map(|l| format!("{pad}{l}")).collect()
}

/// Every button, in priority order, since the last ones drop off a narrow terminal.
type Button = (&'static str, &'static str, Action, bool);

fn footer_groups(app: &App) -> [(&'static str, Vec<Button>); 2] {
    if app.view == View::Worktrees {
        let actions = vec![
            ("d", "Remove", Action::Delete, app.confirm_remove.is_some()),
            ("Space", "Mark", Action::Mark, false),
            ("a", "All", Action::MarkAll, false),
            ("q", "Quit", Action::Quit, false),
        ];
        return [("Actions", actions), ("View", vec![("w", "Conversations", Action::Worktrees, false)])];
    }
    [
        (
            "Actions",
            vec![
                ("Enter", "Resume", Action::Resume, false),
                ("d", "Delete", Action::Delete, app.confirm_delete.is_some()),
                ("Shift+R", "Rename", Action::Rename, false),
                ("Space", "Mark", Action::Mark, false),
                ("a", "All", Action::MarkAll, false),
                ("y", "Copy ID", Action::CopyId, false),
                ("Shift+Y", "Copy command", Action::CopyCommand, false),
                ("Shift+O", "Reopen", Action::Reopen, false),
                ("q", "Quit", Action::Quit, false),
            ],
        ),
        (
            "View",
            vec![
                ("/", "Search", Action::Search, !app.query.is_empty()),
                ("o", "Open", Action::ToggleOpen, app.filters.open),
                ("x", "Crashed", Action::ToggleCrashed, app.filters.crashed),
                ("Shift+J", "Junk", Action::ToggleJunk, app.filters.junk),
                ("m", "Missing", Action::ToggleMissing, app.filters.missing),
                ("l", "Large", Action::ToggleLarge, app.filters.large),
                ("c", "Clear", Action::ClearFilters, false),
                ("g", "Group", Action::ToggleGroup, app.grouped),
                ("s", "Sort", Action::NextSort, false),
                ("r", "Reverse", Action::ReverseSort, false),
                ("w", "Worktrees", Action::Worktrees, false),
            ],
        ),
    ]
}

/// Lays out the group names, with no action, and their buttons in rows, as `(row, x, text, action, on)`.
/// Each group starts its own row.
fn footer_layout(app: &App, width: u16) -> Vec<(u16, u16, String, Option<Action>, bool)> {
    let mut row = 0;
    let mut out = Vec::new();
    for (name, buttons) in footer_groups(app) {
        out.push((row, 0, format!(" {name:<0$}", FOOTER_INDENT as usize - 1), None, false));
        let mut x = FOOTER_INDENT;
        for (key, label, action, on) in buttons {
            let text = format!(" {key} {label} ");
            let w = text.chars().count() as u16;
            if x > FOOTER_INDENT && x + w > width {
                row += 1;
                x = FOOTER_INDENT;
            }
            out.push((row, x, text, Some(action), on));
            x += w + 1;
        }
        row += 1;
    }
    out
}

/// Stays the same whatever the footer is showing, so the table doesn't jump. Narrower terminals
/// lose the last buttons rather than more of the table.
fn footer_rows(app: &App, width: u16) -> u16 {
    footer_layout(app, width).last().map_or(1, |b| b.0 + 1).min(MAX_FOOTER_ROWS)
}

fn draw_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let bar = Style::new().bg(BAR).fg(BAR_LABEL);
    if let Some((_, folder)) = &app.confirm_folder {
        app.hit.buttons.clear();
        // The end of a path is the telling part, and the keys must stay on screen.
        let rest = " no longer exists. Press y to recreate it and resume, any other key to cancel.";
        let room = (area.width as usize).saturating_sub(rest.width() + 1).max(MIN_NAME_WIDTH);
        let text = format!(" {}{rest}", fit_start(&sanitize(&folder.display().to_string()), room));
        f.render_widget(Paragraph::new(text).style(bar.fg(BAR_NOTICE).add_modifier(Modifier::BOLD)), area);
        return;
    }
    if let Some(rename) = &app.renaming {
        app.hit.buttons.clear();
        let text = format!(" Rename: {}", rename.name);
        let x = area.x + (text.width() as u16).min(area.width.saturating_sub(1));
        f.set_cursor_position((x, area.y));
        let hint = Line::from(" Enter save   Esc cancel   Ctrl-U clear");
        let name = Line::from(Span::styled(text, Style::new().fg(BAR_KEY).add_modifier(Modifier::BOLD)));
        f.render_widget(Paragraph::new(vec![name, hint]).style(bar), area);
        return;
    }
    if let Some(status) = &app.status {
        f.render_widget(Paragraph::new(status.clone()).style(bar.fg(BAR_NOTICE)), area);
        app.hit.buttons.clear();
        return;
    }
    if app.typing {
        app.hit.buttons.clear();
        let hint = " Type to search   ↑↓ move   Enter keep results, then Enter again to resume   Esc clear ";
        f.render_widget(Paragraph::new(hint).style(bar), area);
        return;
    }
    let layout = footer_layout(app, area.width);
    let mut rows: Vec<Vec<Span>> = vec![Vec::new(); area.height as usize];
    app.hit.buttons.clear();
    for (row, x, text, action, on) in layout {
        let Some(spans) = rows.get_mut(row as usize) else { break };
        let Some(action) = action else {
            spans.push(Span::styled(text, bar.fg(BAR_GROUP).add_modifier(Modifier::BOLD)));
            continue;
        };
        if spans.is_empty() {
            spans.push(Span::raw(" ".repeat(x as usize)));
        }
        let width = text.chars().count() as u16;
        let disabled = match action {
            Action::Resume => !app.can_resume(),
            Action::Rename => !app.can_change(),
            Action::Delete => !app.can_press_delete(),
            _ => false,
        };
        let solid = |background| Style::new().fg(BAR).bg(background).add_modifier(Modifier::BOLD);
        let (key_style, label_style) = if action == Action::Delete && on {
            (solid(BAR_DANGER), solid(BAR_DANGER))
        } else if on {
            (solid(BAR_ON), solid(BAR_ON))
        } else if disabled {
            (bar.fg(BAR_OFF), bar.fg(BAR_OFF))
        } else {
            (bar.fg(BAR_KEY).add_modifier(Modifier::BOLD), bar)
        };
        if x + width <= area.width {
            app.hit.buttons.push((Rect { x: area.x + x, y: area.y + row, width, height: 1 }, action));
        }
        // Keys have no spaces, so the first one after the leading space ends the key.
        let key_end = text[1..].find(' ').map_or(text.len(), |at| at + 1);
        let (key, label) = text.split_at(key_end);
        spans.push(Span::styled(key.to_owned(), key_style));
        spans.push(Span::styled(label.to_owned(), label_style));
        spans.push(Span::raw(" "));
    }
    let lines: Vec<Line> = rows.into_iter().map(Line::from).collect();
    f.render_widget(Paragraph::new(lines).style(bar), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccm_core::config::Config;
    use ccm_core::delete::{Failure, Note, Outcome, Plan, Skipped};
    use ccm_core::model::Source;
    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    const NOW: i64 = 1_000 * 60 * 60 * 24 * 365;
    const HOUR: i64 = 60 * 60 * 1000;

    fn session(id: &str, name: &str, folder: &str, hours_ago: i64) -> Session {
        Session {
            id: id.into(),
            source: Source::ClaudeCode,
            transcript_path: String::new(),
            transcript_size: 0,
            transcript_mtime_ns: 0,
            size_bytes: 2 * 1024 * 1024,
            name: name.into(),
            folder: Some(folder.into()),
            folders: Vec::new(),
            branch: Some("main".into()),
            model: Some("claude-opus-5-5".into()),
            started_at_ms: Some(0),
            last_active_ms: Some(NOW - hours_ago * HOUR),
            context_used: Some(87_000),
            prompts: 3,
            replies: 3,
            cost_usd: Some(1.5),
            lines_added: Some(10),
            lines_removed: Some(2),
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
        a.loading = false;
        let mut open = session("id-open", "Open one", "/home/me/a", 1);
        open.open = true;
        let mut junk = session("id-junk", "Junk one", "/home/me/b", 2);
        junk.replies = 0;
        let mut old = session("id-old", "Old model", "/home/me/a", 3);
        old.model = Some("claude-opus-4-8".into());
        old.size_bytes = 300 * 1024 * 1024;
        a.set_sessions(vec![junk, old, open]);
        a
    }

    fn render(a: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, a, NOW)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn row_of(screen: &str, needle: &str) -> usize {
        screen.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle} not on screen:\n{screen}"))
    }

    #[test]
    fn the_table_shows_every_column() {
        let screen = render(&mut app(), 160, 20);
        for title in ["Name", "Folder", "Active", "Context", "Msgs", "Size", "Model", "Branch", "Status"] {
            assert!(screen.contains(title), "{title} missing:\n{screen}");
        }
        let line = screen.lines().find(|l| l.contains("Open one")).unwrap();
        for value in ["~/a", "1h", "87k/1M", "6", "2M", "opus-5-5", "main", "open"] {
            assert!(line.contains(value), "{value} missing from: {line}");
        }
    }

    #[test]
    fn unknown_window_sizes_show_a_question_mark() {
        let screen = render(&mut app(), 160, 20);
        assert!(screen.lines().find(|l| l.contains("Old model")).unwrap().contains("87k/?"));
    }

    #[test]
    fn newest_comes_first_by_default_and_summary_counts() {
        let screen = render(&mut app(), 160, 20);
        assert!(row_of(&screen, "Open one") < row_of(&screen, "Junk one"));
        assert!(row_of(&screen, "Junk one") < row_of(&screen, "Old model"));
        assert!(screen.contains("3 conversations  304M on disk  1 open"), "{screen}");
    }

    #[test]
    fn details_follow_the_selection() {
        let mut a = app();
        assert!(render(&mut a, 160, 20).contains("id-open"));
        a.on_key(KeyCode::Down);
        let screen = render(&mut a, 160, 20);
        assert!(screen.contains("id-junk") && !screen.contains("id-open"), "{screen}");
        assert!(screen.contains("Cost $1.50") && screen.contains("Lines +10 -2"));
    }

    fn buffer(a: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, a, NOW)).unwrap();
        t.backend().buffer().clone()
    }

    #[test]
    fn the_footer_is_a_full_width_bar_set_apart_from_the_preview() {
        let mut a = app();
        let (w, h) = (160, 30);
        let screen = render(&mut a, w, h);
        let lines: Vec<&str> = screen.lines().collect();
        let footer = lines.iter().position(|l| l.contains("Enter Resume")).unwrap();
        assert!(lines[footer - 1].is_empty(), "{screen}");
        let details = lines.iter().position(|l| l.contains(" Details ")).unwrap();
        assert!(lines[details - 1].is_empty(), "{screen}");

        let cells = buffer(&mut a, w, h);
        assert_eq!(cells[(w - 1, footer as u16)].bg, BAR, "reaches the right edge");
        assert_eq!(cells[(0, footer as u16 - 1)].bg, Color::Reset);
        assert_eq!(cells[(1, footer as u16)].fg, BAR_GROUP);
        let search = row_of(&screen, "/ Search");
        let key = lines[search].find("/ Search").unwrap() as u16;
        assert_eq!((cells[(key, search as u16)].fg, cells[(key + 2, search as u16)].fg), (BAR_KEY, BAR_LABEL));
    }

    #[test]
    fn the_gaps_come_out_of_the_preview_and_not_the_list() {
        let mut a = app();
        let screen = render(&mut a, 160, 30);
        let lines: Vec<&str> = screen.lines().collect();
        let details = lines.iter().position(|l| l.contains(" Details ")).unwrap();
        // What an even split of the space beside the details and footer gives the list.
        let footer = footer_rows(&a, 160) as usize;
        let shared = 30 - 1 - 6 - footer;
        assert_eq!(details, 1 + shared.div_ceil(2) + 1, "{screen}");
    }

    #[test]
    fn small_terminals_without_a_preview_add_no_gaps() {
        let mut a = app();
        let screen = render(&mut a, 160, 14);
        let lines: Vec<&str> = screen.lines().collect();
        let details = lines.iter().position(|l| l.contains(" Details ")).unwrap();
        assert_eq!(details, 14 - 6 - footer_rows(&a, 160) as usize, "right below the list: {screen}");
        assert!(!screen.contains(" Preview "), "{screen}");
    }

    #[test]
    fn preview_text_is_indented_under_its_heading() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = app();
        with_transcript(&mut a, "id-open", dir.path(), 1, CHAT);
        a.ensure_preview();
        let screen = render(&mut a, 160, 30);
        let line = screen.lines().find(|l| l.contains("first answer")).unwrap();
        assert!(line.starts_with(" first answer"), "{screen}");
    }

    #[test]
    fn status_messages_keep_the_bar() {
        let mut a = app();
        a.on_key(KeyCode::Char('J'));
        let cells = buffer(&mut a, 160, 30);
        assert_eq!(cells[(159, 29)].bg, BAR);
    }

    #[test]
    fn keys_that_need_shift_say_so_and_quit_fits_at_80_columns() {
        let mut a = app();
        let screen = render(&mut a, 80, 30);
        for button in ["Shift+R Rename", "Shift+Y Copy command", "Shift+O Reopen", "Shift+J Junk", "q Quit"] {
            assert!(screen.contains(button), "{button} missing from {screen}");
        }
    }

    #[test]
    fn actions_and_view_controls_sit_in_their_own_labelled_groups() {
        let mut a = app();
        let screen = render(&mut a, 160, 30);
        let lines: Vec<&str> = screen.lines().collect();
        let actions = lines[row_of(&screen, "Enter Resume")];
        let view = lines[row_of(&screen, "o Open")];
        assert!(actions.starts_with(" Actions") && actions.contains("q Quit") && !actions.contains("o Open"), "{screen}");
        assert!(view.starts_with(" View") && view.contains("r Reverse") && !view.contains("q Quit"), "{screen}");
    }

    #[test]
    fn every_button_fits_at_80_columns_and_wrapped_ones_stay_under_their_group() {
        let mut a = app();
        let screen = render(&mut a, 80, 30);
        let count: usize = footer_groups(&a).iter().map(|(_, buttons)| buttons.len()).sum();
        assert_eq!(a.hit.buttons.len(), count);
        assert_eq!(footer_rows(&a, 80), MAX_FOOTER_ROWS);
        let lines: Vec<&str> = screen.lines().collect();
        let start = |text| lines[row_of(&screen, text)].find(text).unwrap();
        assert_eq!(start("y Copy ID"), start("Enter Resume"), "{screen}");
        assert_eq!(start("c Clear"), start("/ Search"), "{screen}");
    }

    #[test]
    fn clicking_a_group_name_does_nothing() {
        let mut a = app();
        let screen = render(&mut a, 160, 20);
        let y = row_of(&screen, "o Open") as u16;
        assert!(!a.hit.buttons.iter().any(|(r, _)| r.x <= 2 && 2 < r.x + r.width && r.y == y));
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 2, y);
        assert!(!a.quit && a.status.is_none());
    }

    #[test]
    fn fit_end_counts_wide_characters_by_width() {
        assert_eq!(fit_end("日本語日本語", 5), "日本…");
        assert_eq!(fit_end("short", 5), "short");
    }

    #[test]
    fn the_junk_filter_says_what_it_shows_and_how_to_undo_it() {
        let mut a = app();
        a.on_key(KeyCode::Char('J'));
        assert_eq!(a.status.as_deref(), Some("Showing 1 junk conversation: Claude never replied. Press Shift+J again to show all."));
        assert!(render(&mut a, 160, 20).contains("Press Shift+J again"));
        a.on_key(KeyCode::Char('J'));
        assert_eq!(a.status.as_deref(), Some("Showing all conversations."));
        a.on_key(KeyCode::Down);
        assert_eq!(a.status, None, "the next key clears it");
    }

    #[test]
    fn the_junk_message_does_not_promise_all_when_another_filter_is_on() {
        let mut a = app();
        a.on_key(KeyCode::Char('l'));
        a.on_key(KeyCode::Char('J'));
        assert_eq!(a.status.as_deref(), Some("No junk conversations to show. Press Shift+J again to turn this off."));
        a.on_key(KeyCode::Char('J'));
        assert_eq!(a.status.as_deref(), Some("Junk filter off."));
    }

    #[test]
    fn filters_hide_rows_and_clear_restores_them() {
        let mut a = app();
        a.on_key(KeyCode::Char('l'));
        let screen = render(&mut a, 160, 20);
        assert!(screen.contains("Old model") && !screen.contains("Open one"), "{screen}");
        assert!(screen.contains("showing 1"));
        a.on_key(KeyCode::Char('c'));
        assert!(render(&mut a, 160, 20).contains("Open one"));
    }

    #[test]
    fn a_filter_with_no_matches_says_so() {
        let mut a = app();
        a.on_key(KeyCode::Char('m'));
        assert!(render(&mut a, 160, 20).contains("No conversations match"));
        assert!(a.selected().is_none());
    }

    #[test]
    fn the_empty_state_names_the_folder() {
        let mut a = App::new(Config::default(), "/c".into(), "/home/me".into());
        a.loading = false;
        assert!(render(&mut a, 100, 20).contains("No conversations found in /c"));
    }

    #[test]
    fn grouping_adds_headers_that_selection_skips() {
        let mut a = app();
        a.on_key(KeyCode::Char('g'));
        let screen = render(&mut a, 160, 20);
        let a_header = row_of(&screen, "~/a  2  302M");
        let b_header = row_of(&screen, "~/b  1  2M");
        assert!(a_header < row_of(&screen, "Open one") && row_of(&screen, "Old model") < b_header, "{screen}");
        assert_eq!(a.selected().unwrap().id, "id-open");
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Down);
        assert_eq!(a.selected().unwrap().id, "id-junk", "the header between groups is skipped");
    }

    #[test]
    fn the_selection_stays_on_the_same_conversation_across_a_refresh() {
        let mut a = app();
        a.on_key(KeyCode::Down);
        assert_eq!(a.selected().unwrap().id, "id-junk");
        let mut updated = a.sessions.clone();
        updated.push(session("id-new", "Brand new", "/x", 0));
        a.set_sessions(updated);
        assert_eq!(a.selected().unwrap().id, "id-junk");
    }

    #[test]
    fn selection_stops_at_the_ends() {
        let mut a = app();
        a.on_key(KeyCode::Up);
        assert_eq!(a.selected().unwrap().id, "id-open");
        a.on_key(KeyCode::PageDown);
        a.on_key(KeyCode::Down);
        assert_eq!(a.selected().unwrap().id, "id-old");
    }

    #[test]
    fn clicking_a_row_selects_it() {
        let mut a = app();
        let screen = render(&mut a, 160, 20);
        let y = row_of(&screen, "Old model") as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 3, y);
        assert_eq!(a.selected().unwrap().id, "id-old");
    }

    #[test]
    fn clicking_a_header_sorts_and_clicking_again_reverses() {
        let mut a = app();
        let screen = render(&mut a, 160, 20);
        let y = row_of(&screen, "Status") as u16;
        let x = screen.lines().nth(y as usize).unwrap().find("Size").unwrap() as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        assert_eq!((a.sort, a.descending), (SortKey::Size, true));
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        assert_eq!((a.sort, a.descending), (SortKey::Size, false));
    }

    #[test]
    fn footer_buttons_work_like_their_keys() {
        let mut a = app();
        let screen = render(&mut a, 160, 20);
        let y = row_of(&screen, "Large");
        let x = screen.lines().nth(y).unwrap().find("Large").unwrap() as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y as u16);
        assert!(a.filters.large);
    }

    #[test]
    fn the_wheel_moves_the_selection() {
        let mut a = app();
        render(&mut a, 160, 20);
        a.on_mouse(MouseEventKind::ScrollDown, 3, 3);
        assert_eq!(a.selected().unwrap().id, "id-old");
    }

    #[test]
    fn narrow_terminals_drop_less_important_columns_and_do_not_panic() {
        let mut a = app();
        let screen = render(&mut a, 80, 20);
        assert!(!screen.contains("Branch") && screen.contains("Context"), "{screen}");
        for (w, h) in [(30, 8), (10, 3), (1, 1)] {
            render(&mut a, w, h);
        }
    }

    fn searchable(a: &mut App) {
        a.search = Some(Box::new(|q| {
            Ok(match q.trim() {
                "" => None,
                "old" => Some(HashSet::from(["id-old".to_string()])),
                "boom" => return Err("disk on fire".into()),
                _ => Some(HashSet::new()),
            })
        }));
    }

    fn type_text(a: &mut App, text: &str) {
        a.on_key(KeyCode::Char('/'));
        for c in text.chars() {
            a.on_key(KeyCode::Char(c));
        }
    }

    #[test]
    fn typing_filters_the_list_as_you_go_and_esc_restores_it() {
        let mut a = app();
        searchable(&mut a);
        type_text(&mut a, "ol");
        assert!(!render(&mut a, 160, 20).contains("Old model"), "no rows yet for a partial query");
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 160, 20);
        assert!(screen.contains(" / old") && screen.contains("showing 1"), "{screen}");
        assert!(screen.contains("Old model") && !screen.contains("Open one"), "{screen}");
        assert_eq!(a.selected().unwrap().id, "id-old");
        a.on_key(KeyCode::Esc);
        let screen = render(&mut a, 160, 20);
        assert!(screen.contains("Open one") && !screen.contains(" / old"), "{screen}");
        assert!(!a.quit);
    }

    #[test]
    fn letters_go_to_the_search_box_not_the_shortcuts() {
        let mut a = app();
        searchable(&mut a);
        type_text(&mut a, "qglo");
        assert_eq!(a.query, "qglo");
        assert!(!a.quit && !a.grouped && !a.filters.large && !a.filters.open);
    }

    #[test]
    fn enter_keeps_the_results_and_esc_then_clears_them() {
        let mut a = app();
        searchable(&mut a);
        type_text(&mut a, "old");
        a.on_key(KeyCode::Enter);
        assert!(!a.typing);
        a.on_key(KeyCode::Char('g'));
        assert!(a.grouped, "shortcuts work again");
        assert!(render(&mut a, 160, 20).contains(" / old"));
        a.on_key(KeyCode::Esc);
        assert!(a.query.is_empty() && !a.quit);
    }

    #[test]
    fn esc_never_quits_and_says_how_to() {
        let mut a = app();
        a.on_key(KeyCode::Esc);
        assert!(!a.quit);
        assert!(render(&mut a, 160, 20).contains("Press q to quit."));
        a.on_key(KeyCode::Char('q'));
        assert!(a.quit);
    }

    #[test]
    fn del_opens_the_delete_dialog_and_a_second_del_confirms() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Delete);
        assert!(a.confirm_delete.is_some());
        a.age_confirmation();
        a.on_key(KeyCode::Delete);
        assert!(a.has_pending_delete());
        a.perform_delete();
        assert_eq!(*calls.borrow(), vec![(vec!["id-old".to_string()], false)]);
    }

    #[test]
    fn y_confirms_a_delete_opened_with_del() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Delete);
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn holding_del_never_deletes() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        for _ in 0..50 {
            a.on_key(KeyCode::Delete);
        }
        assert!(a.confirm_delete.is_some() && !a.has_pending_delete());
        assert!(render(&mut a, 160, 30).contains("Too quick"));
        a.perform_delete();
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn del_does_nothing_while_typing_a_search() {
        let mut a = app();
        searchable(&mut a);
        type_text(&mut a, "old");
        a.on_key(KeyCode::Delete);
        assert!(a.confirm_delete.is_none());
        assert_eq!(a.query, "old");
    }

    #[test]
    fn repeated_esc_on_the_delete_dialog_keeps_ccm_open() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('d'));
        for _ in 0..3 {
            a.on_key(KeyCode::Esc);
        }
        assert!(a.confirm_delete.is_none() && !a.quit);
        a.perform_delete();
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn search_combines_with_filters_and_survives_a_refresh() {
        let mut a = app();
        searchable(&mut a);
        type_text(&mut a, "old");
        a.on_key(KeyCode::Enter);
        let sessions = a.sessions.clone();
        a.set_sessions(sessions);
        assert_eq!(a.selected().unwrap().id, "id-old");
        a.on_key(KeyCode::Char('o'));
        assert!(a.selected().is_none(), "the only match isn't open");
    }

    #[test]
    fn backspace_and_ctrl_u_edit_the_query() {
        let mut a = app();
        searchable(&mut a);
        type_text(&mut a, "olde");
        a.on_key(KeyCode::Backspace);
        assert_eq!(a.selected().unwrap().id, "id-old");
        a.on_ctrl(KeyCode::Char('u'));
        assert!(a.query.is_empty() && !a.is_searching());
    }

    #[test]
    fn a_search_error_is_shown_and_the_list_stays() {
        let mut a = app();
        searchable(&mut a);
        type_text(&mut a, "boom");
        let screen = render(&mut a, 160, 20);
        assert!(screen.contains("Search failed: disk on fire") && screen.contains("Open one"), "{screen}");
    }

    #[test]
    fn ctrl_c_quits_even_while_typing() {
        let mut a = app();
        a.on_key(KeyCode::Char('/'));
        a.on_ctrl(KeyCode::Char('c'));
        assert!(a.quit);
    }

    /// Points the given conversation at a real transcript file.
    fn with_transcript(a: &mut App, id: &str, dir: &std::path::Path, mtime_ns: i64, body: &str) {
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, body).unwrap();
        let mut sessions = a.sessions.clone();
        let s = sessions.iter_mut().find(|s| s.id == id).unwrap();
        s.transcript_path = path.to_str().unwrap().into();
        s.transcript_mtime_ns = mtime_ns;
        a.set_sessions(sessions);
    }

    const CHAT: &str = concat!(
        "{\"type\":\"user\",\"message\":{\"content\":\"first question\"}}\n",
        "{\"type\":\"assistant\",\"message\":{\"model\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"first answer\"},{\"type\":\"tool_use\",\"input\":{\"secret\":\"hidden-input\"}},{\"type\":\"tool_use\",\"input\":{}}]}}\n",
    );

    #[test]
    fn the_preview_follows_the_selection_and_collapses_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = app();
        with_transcript(&mut a, "id-open", dir.path(), 1, CHAT);
        with_transcript(&mut a, "id-junk", dir.path(), 1, "{\"type\":\"user\",\"message\":{\"content\":\"second chat\"}}\n");
        a.ensure_preview();
        let screen = render(&mut a, 160, 30);
        assert!(screen.contains("first question") && screen.contains("first answer"), "{screen}");
        assert!(screen.contains("Claude  2 tool calls") && !screen.contains("hidden-input"), "{screen}");
        a.on_key(KeyCode::Down);
        a.ensure_preview();
        let screen = render(&mut a, 160, 30);
        assert!(screen.contains("second chat") && !screen.contains("first question"), "{screen}");
    }

    #[test]
    fn recaps_come_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = app();
        with_transcript(&mut a, "id-open", dir.path(), 1, CHAT);
        let mut sessions = a.sessions.clone();
        sessions.iter_mut().find(|s| s.id == "id-open").unwrap().recaps = vec!["saved recap".into()];
        a.set_sessions(sessions);
        a.ensure_preview();
        let screen = render(&mut a, 160, 30);
        assert!(row_of(&screen, "saved recap") < row_of(&screen, "first question"), "{screen}");
    }

    #[test]
    fn the_preview_opens_at_the_newest_message_and_scrolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = app();
        let body: String = (0..12)
            .map(|i| format!("{{\"type\":\"user\",\"message\":{{\"content\":\"message-{i}\"}}}}\n"))
            .collect();
        with_transcript(&mut a, "id-open", dir.path(), 1, &body);
        a.ensure_preview();
        let screen = render(&mut a, 160, 30);
        assert!(screen.contains("message-11") && !screen.contains("message-0\n"), "{screen}");
        for _ in 0..50 {
            a.on_key(KeyCode::Char('['));
        }
        let screen = render(&mut a, 160, 30);
        assert!(screen.contains("message-0") && !screen.contains("message-11"), "{screen}");
        let lines = a.preview_scroll;
        a.on_key(KeyCode::Char(']'));
        assert_eq!(a.preview_scroll, lines - 3, "over-scrolling doesn't build up");
        a.on_key(KeyCode::Down);
        a.ensure_preview();
        assert_eq!(a.preview_scroll, 0, "a new selection starts at its newest message");
    }

    #[test]
    fn the_wheel_scrolls_whichever_pane_it_is_over() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = app();
        with_transcript(&mut a, "id-open", dir.path(), 1, CHAT);
        a.ensure_preview();
        render(&mut a, 160, 30);
        let (x, y) = (a.hit.preview.x + 1, a.hit.preview.y + 1);
        a.on_mouse(MouseEventKind::ScrollUp, x, y);
        assert_eq!(a.selected().unwrap().id, "id-open", "the table didn't move");
        assert_eq!(a.preview_scroll, 3);
    }

    #[test]
    fn a_transcript_change_reloads_the_preview_but_keeps_your_place() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = app();
        with_transcript(&mut a, "id-open", dir.path(), 1, CHAT);
        a.ensure_preview();
        a.preview_scroll = 2;
        with_transcript(&mut a, "id-open", dir.path(), 2, "{\"type\":\"user\",\"message\":{\"content\":\"newer\"}}\n");
        a.ensure_preview();
        assert_eq!(a.preview_scroll, 2);
        assert!(render(&mut a, 160, 30).contains("newer"));
    }

    #[test]
    fn an_unreadable_transcript_says_so() {
        let mut a = app();
        a.ensure_preview();
        assert!(render(&mut a, 160, 30).contains("Couldn't read this conversation"));
    }

    #[test]
    fn short_terminals_hide_the_preview_and_do_not_panic() {
        let mut a = app();
        a.ensure_preview();
        assert!(!render(&mut a, 100, 12).contains("Preview"));
        a.on_key(KeyCode::Char('/'));
        for (w, h) in [(30, 8), (10, 3), (1, 1), (5, 40)] {
            render(&mut a, w, h);
        }
    }

    #[test]
    fn wrapping_respects_words_and_wide_characters() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("a\n\nb", 5), ["a", "", "b"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap("日本語", 4), ["日本", "語"]);
    }

    #[test]
    fn dialog_lines_keep_their_indent_when_wrapped() {
        assert_eq!(wrap_indented("  one two three", 9), ["  one two", "  three"]);
        assert_eq!(wrap_indented("plain", 9), ["plain"]);
        assert_eq!(wrap_indented("        deep", 6), ["   dee", "   p"], "indent is capped at half the width");
    }

    /// Records resumes instead of starting anything.
    fn resumable(a: &mut App) -> std::rc::Rc<std::cell::RefCell<Vec<(String, std::path::PathBuf)>>> {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = calls.clone();
        a.resume = Some(Box::new(move |id, folder| {
            seen.borrow_mut().push((id.to_string(), folder.to_owned()));
            Ok("Opened in a new terminal.".to_string())
        }));
        calls
    }

    const VALID_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    fn resume_app(folder: &str, exists: bool, open: bool) -> App {
        let mut a = App::new(Config::default(), "/c".into(), "/home/me".into());
        a.loading = false;
        let mut s = session(VALID_A, "Chat", folder, 1);
        s.folder_exists = exists;
        s.open = open;
        a.set_sessions(vec![s]);
        a
    }

    #[test]
    fn enter_resumes_in_the_recorded_folder_and_reports_it() {
        let mut a = resume_app("/work/a", true, false);
        let calls = resumable(&mut a);
        a.on_key(KeyCode::Enter);
        assert_eq!(*calls.borrow(), [(VALID_A.to_string(), "/work/a".into())]);
        assert!(render(&mut a, 160, 30).contains("Opened in a new terminal."));
    }

    #[test]
    fn a_second_enter_does_not_start_it_twice() {
        let mut a = resume_app("/work/a", true, false);
        let calls = resumable(&mut a);
        a.on_key(KeyCode::Enter);
        a.on_key(KeyCode::Enter);
        assert_eq!(calls.borrow().len(), 1);
        assert!(!a.can_resume());
        assert!(a.status.as_deref().unwrap().contains("Already starting"));
    }

    #[test]
    fn a_failed_start_can_be_retried() {
        let mut a = resume_app("/work/a", true, false);
        a.resume = Some(Box::new(|_, _| Err("no terminal".to_string())));
        a.on_key(KeyCode::Enter);
        assert_eq!(a.status.as_deref(), Some("no terminal"));
        assert!(a.can_resume());
    }

    #[test]
    fn an_open_conversation_is_never_resumed() {
        let mut a = resume_app("/work/a", true, true);
        let calls = resumable(&mut a);
        a.on_key(KeyCode::Enter);
        assert!(calls.borrow().is_empty());
        assert!(a.status.as_deref().unwrap().contains("already open"));
        assert!(!a.can_resume());
    }

    #[test]
    fn a_missing_folder_asks_first_and_y_recreates_it() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("gone/deep");
        let mut a = resume_app(folder.to_str().unwrap(), false, false);
        let calls = resumable(&mut a);
        a.on_key(KeyCode::Enter);
        assert!(calls.borrow().is_empty() && !folder.exists(), "nothing happens before the answer");
        assert!(render(&mut a, 200, 30).contains("no longer exists. Press y"));
        a.on_key(KeyCode::Char('y'));
        assert!(folder.is_dir());
        assert_eq!(calls.borrow()[0].1, folder);
    }

    #[test]
    fn any_other_key_or_a_click_cancels_recreating() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("gone");
        let mut a = resume_app(folder.to_str().unwrap(), false, false);
        let calls = resumable(&mut a);
        a.on_key(KeyCode::Enter);
        a.on_key(KeyCode::Char('q'));
        assert!(!a.quit, "the answer key doesn't also act as a shortcut");
        a.on_key(KeyCode::Enter);
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), 1, 1);
        assert!(a.confirm_folder.is_none());
        assert!(calls.borrow().is_empty() && !folder.exists());
    }

    #[test]
    fn a_double_click_resumes_and_two_slow_clicks_do_not() {
        let mut a = resume_app("/work/a", true, false);
        let calls = resumable(&mut a);
        let screen = render(&mut a, 160, 30);
        let y = row_of(&screen, "Chat") as u16;
        let click = MouseEventKind::Down(MouseButton::Left);
        a.on_mouse(click, 3, y);
        assert!(calls.borrow().is_empty(), "one click only selects");
        std::thread::sleep(std::time::Duration::from_millis(450));
        a.on_mouse(click, 3, y);
        assert!(calls.borrow().is_empty(), "too slow to be a double-click");
        a.on_mouse(click, 3, y);
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn the_resume_button_works_and_dims_when_unavailable() {
        let mut a = resume_app("/work/a", true, false);
        let calls = resumable(&mut a);
        let screen = render(&mut a, 160, 30);
        let y = row_of(&screen, "Enter Resume");
        let x = screen.lines().nth(y).unwrap().find("Resume").unwrap() as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y as u16);
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn enter_while_typing_keeps_results_without_resuming() {
        let mut a = resume_app("/work/a", true, false);
        let calls = resumable(&mut a);
        a.on_key(KeyCode::Char('/'));
        a.on_key(KeyCode::Enter);
        assert!(calls.borrow().is_empty() && !a.typing);
        a.on_key(KeyCode::Enter);
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn escape_sequences_in_a_folder_never_reach_the_screen() {
        let mut a = resume_app("/work/a\x1b[31mb", false, false);
        let _calls = resumable(&mut a);
        a.on_key(KeyCode::Enter);
        let screen = render(&mut a, 160, 30);
        assert!(!screen.contains('\x1b') && screen.contains("/work/ab"), "{screen}");
    }

    #[test]
    fn long_text_is_shortened_keeping_the_useful_end() {
        assert_eq!(fit_start("/one/two/three", 7), "…/three");
        assert_eq!(fit_end("abcdefgh", 4), "abc…");
        assert_eq!(fit_end("日本語のタイトル", 3), "日…");
    }

    #[test]
    fn home_is_only_shortened_on_a_path_boundary() {
        assert_eq!(short_path("/home/me/x", "/home/me"), "~/x");
        assert_eq!(short_path("/home/me", "/home/me"), "~");
        assert_eq!(short_path("/home/meow", "/home/me"), "/home/meow");
    }

    type Deletes = Rc<RefCell<Vec<(Vec<String>, bool)>>>;

    /// A fake core: every closed conversation can be deleted, and the delete just succeeds.
    fn deletable(a: &mut App, notes: &[&str]) -> Deletes {
        let sessions = a.sessions.clone();
        let notes: Vec<Note> = notes.iter().map(|n| Note { path: format!("/m/{n}").into(), bytes: 1024 }).collect();
        a.planner = Some(Box::new(move |ids| {
            let mut plan = Plan { notes: notes.clone(), ..Plan::default() };
            for id in ids {
                let s = sessions.iter().find(|s| s.id == *id).unwrap();
                if s.open {
                    plan.skipped.push(Skipped { id: id.clone(), reason: "it's open".into() });
                } else {
                    plan.items.push(Item { id: id.clone(), side: vec![], transcripts: vec![], bytes: s.size_bytes });
                }
            }
            if plan.items.is_empty() {
                plan.notes.clear();
            }
            plan
        }));
        let calls: Deletes = Rc::default();
        let log = calls.clone();
        a.deleter = Some(Box::new(move |plan, notes| {
            let ids: Vec<String> = plan.items.iter().map(|i| i.id.clone()).collect();
            log.borrow_mut().push((ids.clone(), notes));
            Ok(Outcome { deleted: ids, ..Outcome::default() })
        }));
        calls
    }

    fn select_named(a: &mut App, name: &str) {
        a.on_key(KeyCode::Home);
        while a.selected().unwrap().name != name {
            a.on_key(KeyCode::Down);
        }
    }

    fn click_label(a: &mut App, label: &str) {
        let screen = render(a, 160, 30);
        let lines: Vec<&str> = screen.lines().collect();
        let y = lines.iter().rposition(|l| l.contains(label)).unwrap();
        let x = lines[y][..lines[y].find(label).unwrap()].chars().count() as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y as u16);
    }

    #[test]
    fn the_confirmation_names_the_title_and_the_space() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('d'));
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("Delete permanently?"), "{screen}");
        let body = screen.lines().filter(|l| l.contains("Old model")).any(|l| l.contains("300M"));
        assert!(body, "{screen}");
        assert!(screen.contains("1 conversation, 300M freed"), "{screen}");
        assert!(calls.borrow().is_empty(), "nothing goes before the answer");
    }

    #[test]
    fn d_then_y_deletes_and_says_what_was_freed() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('d'));
        a.on_key(KeyCode::Char('y'));
        assert!(render(&mut a, 120, 30).contains("Deleting…"));
        assert!(a.has_pending_delete());
        a.perform_delete();
        assert_eq!(*calls.borrow(), vec![(vec!["id-old".to_string()], false)]);
        let screen = render(&mut a, 120, 30);
        assert!(!screen.contains("Old model"), "{screen}");
        assert!(screen.contains("Deleted 1 conversation, freed 300M."), "{screen}");
    }

    #[test]
    fn any_other_key_cancels() {
        for key in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char('d'), KeyCode::Char('n'), KeyCode::Char('m'), KeyCode::Char('Y')] {
            let mut a = app();
            let calls = deletable(&mut a, &[]);
            select_named(&mut a, "Old model");
            a.on_key(KeyCode::Char('d'));
            a.on_key(key);
            assert!(a.confirm_delete.is_none() && !a.has_pending_delete(), "{key:?}");
            a.perform_delete();
            assert!(calls.borrow().is_empty(), "{key:?}");
            assert_eq!(a.status.as_deref(), Some("Cancelled. Nothing was deleted."));
        }
    }

    #[test]
    fn an_open_conversation_cannot_be_deleted() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Open one");
        a.on_key(KeyCode::Char('d'));
        assert!(a.confirm_delete.is_none());
        assert_eq!(a.status.as_deref(), Some("Can't delete: it's open."));
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn a_just_resumed_conversation_cannot_be_deleted() {
        let mut a = resume_app("/work/a", true, false);
        let _resumes = resumable(&mut a);
        let calls = deletable(&mut a, &[]);
        a.on_key(KeyCode::Enter);
        a.on_key(KeyCode::Char('d'));
        assert!(a.confirm_delete.is_none());
        assert_eq!(a.status.as_deref(), Some("Can't delete: it was just resumed."));
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn memory_notes_are_listed_kept_by_default_and_m_includes_them() {
        let mut a = app();
        let calls = deletable(&mut a, &["only-this.md"]);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('d'));
        let kept = render(&mut a, 120, 30);
        assert!(kept.contains("will be kept") && kept.contains("only-this.md"), "{kept}");
        a.on_key(KeyCode::Char('m'));
        assert!(render(&mut a, 120, 30).contains("will be deleted"));
        a.on_key(KeyCode::Char('m'));
        a.on_key(KeyCode::Char('m'));
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert!(calls.borrow()[0].1);
    }

    #[test]
    fn without_m_the_notes_stay_out_of_the_delete() {
        let mut a = app();
        let calls = deletable(&mut a, &["only-this.md"]);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('d'));
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert!(!calls.borrow()[0].1);
    }

    #[test]
    fn marks_show_totals_and_delete_them_together_skipping_open_ones() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        a.on_key(KeyCode::Char('a'));
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("3 marked") && screen.contains("● Open one"), "{screen}");
        a.on_key(KeyCode::Char('d'));
        let dialog = render(&mut a, 120, 30);
        assert!(dialog.contains("2 conversations, 302M freed"), "{dialog}");
        assert!(dialog.contains("Skipping Open one: it's open"), "{dialog}");
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        let mut ids = calls.borrow()[0].0.clone();
        ids.sort();
        assert_eq!(ids, vec!["id-junk", "id-old"]);
        assert_eq!(a.sessions.len(), 1);
        let report = render(&mut a, 120, 30);
        assert!(report.contains("Skipped Open one: it's open"), "{report}");
    }

    #[test]
    fn space_marks_and_moves_on_and_a_second_press_unmarks() {
        let mut a = app();
        let first = a.selected().unwrap().id.clone();
        a.on_key(KeyCode::Char(' '));
        assert!(a.marked.contains(&first));
        assert_ne!(a.selected().unwrap().id, first);
        a.on_key(KeyCode::Up);
        a.on_key(KeyCode::Char(' '));
        assert!(a.marked.is_empty());
    }

    #[test]
    fn a_second_a_clears_the_marks_and_esc_clears_them_without_quitting() {
        let mut a = app();
        a.on_key(KeyCode::Char('a'));
        a.on_key(KeyCode::Char('a'));
        assert!(a.marked.is_empty());
        a.on_key(KeyCode::Char('a'));
        a.on_key(KeyCode::Esc);
        assert!(a.marked.is_empty() && !a.quit);
    }

    #[test]
    fn a_marked_row_hidden_by_a_filter_is_not_deleted() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Junk one");
        a.on_key(KeyCode::Char(' '));
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char(' '));
        a.on_key(KeyCode::Char('J'));
        a.on_key(KeyCode::Char('d'));
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(calls.borrow()[0].0, vec!["id-junk"]);
        assert!(a.marked.contains("id-old"), "the hidden mark stays for later");
    }

    #[test]
    fn with_the_mouse_the_first_click_arms_and_the_second_confirms() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        click_label(&mut a, "d Delete");
        assert!(a.confirm_delete.is_some());
        a.age_confirmation();
        click_label(&mut a, "d Delete");
        assert!(a.has_pending_delete());
        a.perform_delete();
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn a_double_click_does_not_confirm() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        click_label(&mut a, "d Delete");
        click_label(&mut a, "d Delete");
        assert!(a.confirm_delete.is_some(), "still waiting for a real second click");
        assert!(!a.has_pending_delete());
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn clicking_anywhere_else_cancels() {
        let mut a = app();
        let calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        click_label(&mut a, "d Delete");
        a.age_confirmation();
        click_label(&mut a, "q Quit");
        assert!(a.confirm_delete.is_none() && !a.quit);
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn the_wheel_does_nothing_during_a_confirmation() {
        let mut a = app();
        let _calls = deletable(&mut a, &[]);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('d'));
        let before = a.table.selected();
        a.on_mouse(MouseEventKind::ScrollDown, 1, 1);
        assert_eq!(a.table.selected(), before);
        assert!(a.confirm_delete.is_some());
    }

    #[test]
    fn a_failed_delete_lists_what_stayed_until_a_key_is_pressed() {
        let mut a = app();
        let _ = deletable(&mut a, &[]);
        a.deleter = Some(Box::new(|_, _| {
            Ok(Outcome {
                deleted: vec![],
                skipped: vec![],
                failures: vec![Failure { path: "/c/file-history/x\x1b[31m".into(), error: "Permission denied".into() }],
            })
        }));
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('d'));
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("Delete finished") && screen.contains("Deleted 0 conversations"), "{screen}");
        assert!(screen.contains("Not removed: /c/file-history/x") && screen.contains("Permission denied"), "{screen}");
        assert!(!screen.contains('\x1b'));
        assert!(screen.contains("Old model"), "the row stays so it can be retried");
        a.on_key(KeyCode::Char('q'));
        assert!(a.report.is_none() && !a.quit, "the key only closes the report");
    }

    #[test]
    fn a_refused_delete_says_nothing_was_deleted() {
        let mut a = app();
        let _ = deletable(&mut a, &[]);
        a.deleter = Some(Box::new(|_, _| Err("history.jsonl could not be edited, so nothing was deleted: busy".into())));
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('d'));
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("nothing was deleted"), "{screen}");
        assert!(screen.contains("Old model"));
    }

    #[test]
    fn a_hostile_name_cannot_reach_the_dialog() {
        let mut a = app();
        a.sessions[0].name = "bad\x1b]0;pwn\x07name".into();
        let _ = deletable(&mut a, &[]);
        a.run(Action::ClearFilters);
        a.on_key(KeyCode::Home);
        while !a.selected().unwrap().name.starts_with("bad") {
            a.on_key(KeyCode::Down);
        }
        a.on_key(KeyCode::Char('d'));
        assert!(!render(&mut a, 120, 30).contains('\x1b'));
    }

    type Renames = Rc<RefCell<Vec<(String, String)>>>;

    fn renameable(a: &mut App) -> Renames {
        let calls: Renames = Rc::default();
        let log = calls.clone();
        a.renamer = Some(Box::new(move |id, _, name| {
            log.borrow_mut().push((id.to_string(), name.to_string()));
            Ok(name.trim().to_string())
        }));
        calls
    }

    #[test]
    fn r_renames_starting_from_the_current_name() {
        let mut a = app();
        let calls = renameable(&mut a);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('R'));
        assert!(render(&mut a, 120, 30).contains("Rename: Old model"));
        for c in " two".chars() {
            a.on_key(KeyCode::Char(c));
        }
        a.on_key(KeyCode::Backspace);
        a.on_key(KeyCode::Enter);
        assert_eq!(*calls.borrow(), vec![("id-old".to_string(), "Old model tw".to_string())]);
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("Renamed to Old model tw.") && a.renaming.is_none(), "{screen}");
        assert!(a.sessions.iter().any(|s| s.name == "Old model tw"), "shows at once, before the next scan");
    }

    #[test]
    fn keys_go_to_the_name_box_and_esc_cancels_the_rename() {
        let mut a = app();
        let calls = renameable(&mut a);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('R'));
        a.on_ctrl(KeyCode::Char('u'));
        for c in "dyq".chars() {
            a.on_key(KeyCode::Char(c));
        }
        assert_eq!(a.renaming.as_ref().unwrap().name, "dyq");
        assert!(!a.quit && a.confirm_delete.is_none());
        a.on_key(KeyCode::Esc);
        assert!(a.renaming.is_none() && !a.quit && calls.borrow().is_empty());
    }

    #[test]
    fn a_refresh_during_a_rename_cannot_redirect_it_to_another_conversation() {
        let mut a = app();
        let calls = renameable(&mut a);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('R'));
        let without: Vec<Session> = a.sessions.iter().filter(|s| s.id != "id-old").cloned().collect();
        a.set_sessions(without);
        assert_ne!(a.selected().unwrap().id, "id-old");
        a.on_key(KeyCode::Enter);
        assert_eq!(calls.borrow()[0].0, "id-old");
    }

    #[test]
    fn rename_is_disabled_while_open() {
        let mut a = app();
        let calls = renameable(&mut a);
        select_named(&mut a, "Open one");
        a.on_key(KeyCode::Char('R'));
        assert!(a.renaming.is_none());
        assert!(a.status.as_deref().unwrap().contains("open"));
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn a_failed_rename_is_reported() {
        let mut a = app();
        a.renamer = Some(Box::new(|_, _, _| Err("The name is empty.".into())));
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('R'));
        a.on_key(KeyCode::Enter);
        assert_eq!(a.status.as_deref(), Some("The name is empty."));
        assert!(a.sessions.iter().any(|s| s.name == "Old model"));
    }

    fn copyable(a: &mut App) -> Rc<RefCell<Vec<String>>> {
        let calls: Rc<RefCell<Vec<String>>> = Rc::default();
        let log = calls.clone();
        a.copier = Some(Box::new(move |text| {
            log.borrow_mut().push(text.to_string());
            Ok("Copied".to_string())
        }));
        calls
    }

    #[test]
    fn y_copies_the_id_and_capital_y_the_resume_command() {
        let mut a = app();
        let calls = copyable(&mut a);
        select_named(&mut a, "Old model");
        a.on_key(KeyCode::Char('y'));
        a.on_key(KeyCode::Char('Y'));
        assert_eq!(*calls.borrow(), vec!["id-old".to_string(), "cd '/home/me/a' && claude --resume id-old".to_string()]);
        assert_eq!(a.status.as_deref(), Some("Copied: cd '/home/me/a' && claude --resume id-old"));
    }

    #[test]
    fn copy_leaves_out_a_folder_with_hidden_characters() {
        let mut a = resume_app("/work/a\x1b[31mb", false, false);
        let calls = copyable(&mut a);
        a.on_key(KeyCode::Char('Y'));
        assert!(calls.borrow()[0].starts_with("claude --resume "));
        assert!(!a.status.as_deref().unwrap().contains('\x1b'));
    }

    #[test]
    fn with_no_folder_the_command_is_just_the_resume() {
        let mut a = app();
        let calls = copyable(&mut a);
        for s in &mut a.sessions {
            s.folder = None;
        }
        a.run(Action::ClearFilters);
        a.on_key(KeyCode::Char('Y'));
        assert!(calls.borrow()[0].starts_with("claude --resume id-"));
    }

    #[test]
    fn every_button_is_reachable_on_a_normal_width_and_narrow_does_not_panic() {
        let mut a = app();
        render(&mut a, 120, 30);
        assert_eq!(a.hit.buttons.len(), footer_groups(&a).iter().map(|(_, buttons)| buttons.len()).sum::<usize>());
        for width in [160, 120, 100, 60, 20, 5, 1] {
            render(&mut a, width, 30);
            render(&mut a, width, 3);
        }
    }

    #[test]
    fn a_button_on_the_second_row_works() {
        let mut a = app();
        let screen = render(&mut a, 100, 30);
        let y = row_of(&screen, "q Quit");
        assert_eq!(y, 27, "{screen}");
        let x = screen.lines().nth(y).unwrap().find("q Quit").unwrap() as u16;
        a.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y as u16);
        assert!(a.quit);
    }

    fn many(count: usize) -> App {
        let mut a = App::new(Config::default(), "/c".into(), "/home/me".into());
        a.loading = false;
        a.set_sessions((0..count).map(|i| session(&format!("id-{i}"), &format!("Name{i:03}"), "/home/me/a", i as i64 + 1)).collect());
        a
    }

    #[test]
    fn every_marked_conversation_can_be_read_before_confirming() {
        let mut a = many(40);
        let calls = deletable(&mut a, &[]);
        a.on_key(KeyCode::Char('a'));
        a.on_key(KeyCode::Char('d'));
        let mut seen = HashSet::new();
        for _ in 0..50 {
            let screen = render(&mut a, 120, 30);
            assert!(screen.contains("40 conversations, 80M freed"), "the total stays:\n{screen}");
            assert!(screen.contains("y or Del delete"), "the keys stay:\n{screen}");
            assert!(screen.contains("of 40"), "{screen}");
            for i in 0..40 {
                if screen.contains(&format!("Name{i:03}  ")) {
                    seen.insert(i);
                }
            }
            a.on_key(KeyCode::Down);
        }
        assert_eq!(seen.len(), 40);
        assert!(a.confirm_delete.is_some(), "scrolling doesn't cancel");
        assert!(calls.borrow().is_empty());
        a.on_key(KeyCode::Char('y'));
        a.perform_delete();
        assert_eq!(calls.borrow()[0].0.len(), 40);
    }

    #[test]
    fn the_list_scrolls_with_every_scroll_input() {
        let mut a = many(40);
        let _calls = deletable(&mut a, &[]);
        a.on_key(KeyCode::Char('a'));
        a.on_key(KeyCode::Char('d'));
        render(&mut a, 120, 30);
        for key in [KeyCode::Down, KeyCode::Char('j'), KeyCode::PageDown, KeyCode::End] {
            let before = a.dialog_scroll;
            a.on_key(key);
            render(&mut a, 120, 30);
            assert!(a.dialog_scroll > before, "{key:?}");
        }
        for key in [KeyCode::Up, KeyCode::Char('k'), KeyCode::PageUp, KeyCode::Home] {
            a.dialog_scroll = 12;
            a.on_key(key);
            assert!(a.dialog_scroll < 12, "{key:?}");
        }
        a.dialog_scroll = 0;
        a.on_mouse(MouseEventKind::ScrollDown, 1, 1);
        assert_eq!(a.dialog_scroll, 3);
        assert!(a.confirm_delete.is_some());
    }

    #[test]
    fn a_short_list_says_nothing_about_scrolling() {
        let mut a = app();
        let _calls = deletable(&mut a, &[]);
        a.on_key(KeyCode::Char('d'));
        assert!(!render(&mut a, 120, 30).contains("↑↓ scroll"));
    }

    #[test]
    fn a_long_failure_report_scrolls_too() {
        let mut a = many(1);
        a.report = Some((0..40).map(|i| format!("Not removed: /c/file{i:03}")).collect());
        render(&mut a, 120, 20);
        a.on_key(KeyCode::End);
        assert!(a.report.is_some(), "scrolling doesn't close it");
        assert!(render(&mut a, 120, 20).contains("file039"));
        a.on_key(KeyCode::Char('x'));
        assert!(a.report.is_none());
    }

    #[test]
    fn a_dialog_fits_a_tiny_terminal() {
        let mut a = app();
        let _ = deletable(&mut a, &["a.md", "b.md"]);
        a.on_key(KeyCode::Char('a'));
        a.on_key(KeyCode::Char('d'));
        for (w, h) in [(120, 30), (40, 10), (10, 4), (4, 2)] {
            render(&mut a, w, h);
        }
    }

    const JUNK_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const OLD_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    fn crashed_app() -> App {
        let mut a = app();
        for s in &mut a.sessions {
            if s.id != "id-open" {
                s.id = if s.id == "id-junk" { JUNK_ID } else { OLD_ID }.into();
                s.crashed = true;
                s.folder = Some(std::env::temp_dir().display().to_string());
            }
        }
        a.run(Action::ClearFilters);
        a
    }

    type Dismissals = Rc<RefCell<Vec<String>>>;

    type Resumes = Rc<RefCell<Vec<(String, std::path::PathBuf)>>>;

    fn reopenable(a: &mut App) -> (Resumes, Dismissals) {
        let resumes = resumable(a);
        let dismissals: Dismissals = Rc::default();
        let log = dismissals.clone();
        a.dismisser = Some(Box::new(move |ids| {
            log.borrow_mut().extend(ids.iter().cloned());
            Ok(())
        }));
        (resumes, dismissals)
    }

    #[test]
    fn a_crashed_conversation_says_so_and_can_be_filtered() {
        let mut a = crashed_app();
        let screen = render(&mut a, 160, 30);
        assert!(screen.contains("2 crashed"), "{screen}");
        assert!(screen.lines().find(|l| l.contains("Old model")).unwrap().contains("crashed"));
        a.on_key(KeyCode::Char('x'));
        let filtered = render(&mut a, 160, 30);
        assert!(filtered.contains("Old model") && !filtered.contains("Open one"), "{filtered}");
    }

    #[test]
    fn without_crash_information_the_app_works_and_says_why() {
        let mut a = app();
        assert!(!render(&mut a, 160, 30).contains("crashed"));
        a.on_key(KeyCode::Char('O'));
        assert!(a.reopen.is_none());
        assert!(a.status.as_deref().unwrap().contains("ccm setup"));
    }

    #[test]
    fn the_saved_window_size_beats_the_model_table() {
        let mut a = app();
        for s in &mut a.sessions {
            s.window_size = Some(200_000);
        }
        a.run(Action::ClearFilters);
        let screen = render(&mut a, 160, 30);
        assert!(screen.lines().find(|l| l.contains("Open one")).unwrap().contains("87k/200k"), "{screen}");
        assert!(screen.lines().find(|l| l.contains("Old model")).unwrap().contains("87k/200k"), "{screen}");
    }

    #[test]
    fn reopen_lists_everything_ticked_and_starts_nothing_until_enter() {
        let mut a = crashed_app();
        let (resumes, _) = reopenable(&mut a);
        a.on_key(KeyCode::Char('O'));
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("2 of 2 will be resumed"), "{screen}");
        assert!(screen.contains("[x] Junk one") && screen.contains("[x] Old model"), "{screen}");
        assert!(!screen.contains("[x] Open one"));
        assert!(resumes.borrow().is_empty());
    }

    #[test]
    fn unticked_ones_stay_closed_and_stop_showing_as_crashed() {
        let mut a = crashed_app();
        let (resumes, dismissals) = reopenable(&mut a);
        a.on_key(KeyCode::Char('O'));
        // Newest first, so Junk one (2h) is first and Old model (3h) second.
        a.on_key(KeyCode::Down);
        a.on_key(KeyCode::Char(' '));
        a.on_key(KeyCode::Enter);
        assert_eq!(resumes.borrow().iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), [JUNK_ID]);
        assert_eq!(*dismissals.borrow(), [OLD_ID]);
        assert!(!a.sessions.iter().find(|s| s.id == OLD_ID).unwrap().crashed);
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("Reopen finished") && screen.contains("Junk one:"), "{screen}");
        assert!(screen.contains("1 unticked no longer show as crashed"), "{screen}");
    }

    #[test]
    fn esc_cancels_the_list_changing_nothing() {
        let mut a = crashed_app();
        let (resumes, dismissals) = reopenable(&mut a);
        a.on_key(KeyCode::Char('O'));
        a.on_key(KeyCode::Char(' '));
        a.on_key(KeyCode::Esc);
        assert!(a.reopen.is_none());
        assert!(resumes.borrow().is_empty() && dismissals.borrow().is_empty());
        assert!(a.sessions.iter().filter(|s| s.crashed).count() == 2);
    }

    #[test]
    fn stray_keys_in_the_list_do_nothing() {
        let mut a = crashed_app();
        let (resumes, _) = reopenable(&mut a);
        a.on_key(KeyCode::Char('O'));
        for c in "qdyRx".chars() {
            a.on_key(KeyCode::Char(c));
        }
        assert!(a.reopen.is_some() && !a.quit && a.confirm_delete.is_none());
        assert!(resumes.borrow().is_empty());
    }

    #[test]
    fn a_missing_folder_starts_unticked_and_is_recreated_only_if_ticked() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let mut a = resume_app(gone.to_str().unwrap(), false, false);
        a.sessions[0].crashed = true;
        a.sessions[0].folder_exists = false;
        let (resumes, _) = reopenable(&mut a);
        a.run(Action::ClearFilters);

        a.on_key(KeyCode::Char('O'));
        let screen = render(&mut a, 150, 30);
        assert!(screen.contains("[ ]") && screen.contains("folder missing"), "{screen}");
        a.on_key(KeyCode::Char(' '));
        a.on_key(KeyCode::Enter);
        assert!(gone.is_dir());
        assert_eq!(resumes.borrow().len(), 1);
    }

    #[test]
    fn a_long_missing_folder_keeps_its_warning_and_its_end() {
        let gone = format!("/somewhere{}/gone-folder", "/very-long-part".repeat(12));
        let mut a = resume_app(&gone, false, false);
        a.sessions[0].crashed = true;
        a.sessions[0].folder_exists = false;
        a.run(Action::ClearFilters);
        a.on_key(KeyCode::Char('O'));
        let screen = render(&mut a, 150, 30);
        let row = screen.lines().find(|l| l.contains("[ ]")).unwrap();
        assert!(row.contains("gone-folder  (folder missing, recreated if ticked)"), "{screen}");
    }

    #[test]
    fn one_failure_does_not_stop_the_others_and_is_reported() {
        let mut a = crashed_app();
        a.resume = Some(Box::new(|id, _| if id == JUNK_ID { Err("no terminal".into()) } else { Ok("Opened.".into()) }));
        a.on_key(KeyCode::Char('O'));
        a.on_key(KeyCode::Enter);
        let screen = render(&mut a, 120, 30);
        assert!(screen.contains("Junk one: not reopened. no terminal"), "{screen}");
        assert!(screen.contains("Old model: Opened."), "{screen}");
    }

    #[test]
    fn a_long_reopen_list_keeps_the_cursor_on_screen() {
        let mut a = many(40);
        for s in &mut a.sessions {
            s.crashed = true;
        }
        a.run(Action::ClearFilters);
        let (_resumes, _) = reopenable(&mut a);
        a.on_key(KeyCode::Char('O'));
        for _ in 0..39 {
            a.on_key(KeyCode::Down);
            render(&mut a, 120, 20);
        }
        assert!(render(&mut a, 120, 20).contains("Name039"));
        a.on_key(KeyCode::Up);
        render(&mut a, 120, 20);
        for _ in 0..38 {
            a.on_key(KeyCode::Up);
        }
        assert!(render(&mut a, 120, 20).contains("[x] Name000"));
    }

    #[test]
    fn the_reopen_button_works() {
        let mut a = crashed_app();
        let _ = reopenable(&mut a);
        click_label(&mut a, "O Reopen");
        assert!(a.reopen.is_some());
    }
}
