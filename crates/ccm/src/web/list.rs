//! What the page shows, worked out by the same code the terminal app uses. The page only draws it.

use std::collections::{HashMap, HashSet};

use ccm_core::config::Config;
use ccm_core::format::{ago, bytes, sanitize, sanitize_lines, short_path};
use ccm_core::labels;
use ccm_core::model::{Session, Source};
use ccm_core::preview::{Preview, Role};
use ccm_core::query::{group_by_folder, group_by_repo, repo_tag, select, status_text, Filters, Group, GroupRow, SortKey, Suggestion};
use ccm_core::repo::Place;
use ccm_core::resume::copy_command;
use ccm_core::window::{context_label, window_size};
use chrono::{Local, TimeZone};
use serde_json::{json, Value};

use crate::app::Grouping;
use crate::cleanup::{label, Cleanup};

/// Every column, left to right, with the key the page uses for it.
const COLUMNS: [(&str, &str, SortKey); 10] = [
    ("tool", "Tool", SortKey::Tool),
    ("name", "Name", SortKey::Name),
    ("folder", "Folder", SortKey::Folder),
    ("active", "Active", SortKey::LastActive),
    ("context", "Context", SortKey::Context),
    ("messages", "Msgs", SortKey::Messages),
    ("size", "Size", SortKey::Size),
    ("model", "Model", SortKey::Model),
    ("branch", "Branch", SortKey::Branch),
    ("status", "Status", SortKey::Status),
];

fn tool_key(source: Source) -> &'static str {
    match source {
        Source::ClaudeCode => "claude",
        Source::Codex => "codex",
    }
}

/// Splits the part of an address after `?`.
pub fn params(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (key, value) = p.split_once('=').unwrap_or((p, ""));
            (decode(key), decode(value))
        })
        .collect()
}

fn decode(text: &str) -> String {
    let raw = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let hex = |at: usize| raw.get(at).and_then(|b| (*b as char).to_digit(16));
        match (raw[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (b'+', ..) => {
                out.push(b' ');
                i += 1;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the page asked to see.
#[derive(Debug, PartialEq)]
pub struct Query {
    pub filters: Filters,
    pub sort: SortKey,
    pub descending: bool,
    pub grouping: Grouping,
    pub text: String,
}

impl Query {
    /// Anything not understood is left at its default, as this only changes what's shown.
    pub fn parse(params: &HashMap<String, String>, tools: &[Source]) -> Self {
        let on = |name: &str| params.get("filters").is_some_and(|f| f.split(',').any(|x| x == name));
        let filters = Filters {
            open: on("open"),
            crashed: on("crashed"),
            junk: on("junk"),
            missing: on("missing"),
            large: on("large"),
            closed: on("closed"),
            old: on("old"),
            unpinned: on("unpinned"),
            tool: params.get("tool").and_then(|t| tools.iter().copied().find(|s| tool_key(*s) == t)),
        };
        let sort = params.get("sort").and_then(|k| COLUMNS.iter().find(|(key, ..)| key == k)).map_or(SortKey::LastActive, |c| c.2);
        let descending = params.get("desc").map_or(sort.default_descending(), |d| d == "1");
        let grouping = match params.get("group").map(String::as_str) {
            Some("folder") => Grouping::Folder,
            Some("repo") => Grouping::Repo,
            _ => Grouping::Off,
        };
        Self { filters, sort, descending, grouping, text: params.get("q").cloned().unwrap_or_default() }
    }
}

pub struct View<'a> {
    pub sessions: &'a [Session],
    /// `None` when nothing is being searched for.
    pub matches: Option<&'a HashSet<String>>,
    pub places: &'a HashMap<String, Place>,
    pub config: &'a Config,
    pub home: &'a str,
    pub looked_in: &'a str,
    pub tools: &'a [Source],
    pub now_ms: i64,
}

fn item(view: &View, s: &Session, grouping: Grouping) -> Value {
    let context = context_label(s.context_used, window_size(s.window_size.or(s.context_window), s.model.as_deref(), &view.config.windows));
    let tag = if grouping == Grouping::Repo { repo_tag(s, view.places) } else { None };
    let or_dash = |v: Option<&str>| v.map_or("-".to_string(), sanitize);
    json!({
        "id": s.id,
        "bytes": s.size_bytes,
        "tag": tag,
        "pinned": s.pinned,
        "tags": s.tags.iter().map(|t| sanitize(t)).collect::<Vec<_>>(),
        "cells": {
            "tool": s.source.short(),
            "name": sanitize(&s.display_name()),
            "folder": s.folder.as_deref().map_or("-".to_string(), |p| sanitize(&short_path(p, view.home))),
            "active": ago(view.now_ms, s.last_active_ms),
            "context": context,
            "messages": s.messages().to_string(),
            "size": bytes(s.size_bytes),
            "model": or_dash(s.model.as_deref().map(|m| m.trim_start_matches("claude-"))),
            "branch": or_dash(s.branch.as_deref()),
            "status": status_text(s),
        },
    })
}

fn header(view: &View, group: &Group, count: usize, size_bytes: u64) -> Value {
    let shown = |p: &str| sanitize(&short_path(p, view.home));
    let name = match group {
        Group::Folder(p) | Group::Repo(p) => shown(p),
        Group::NotGit(p) => format!("{} (not in Git)", shown(p)),
        Group::Gone(p) => format!("{} (folder gone)", shown(p)),
        Group::NoFolder => "(no folder recorded)".to_string(),
    };
    json!({ "header": name, "count": count, "size": bytes(size_bytes) })
}

pub fn render(view: &View, query: &Query, version: u64, loading: bool, notice: Option<&str>) -> Value {
    let limits = view.config.limits(view.now_ms);
    let mut order = select(view.sessions, query.filters, limits, query.sort, query.descending);
    if let Some(matches) = view.matches {
        order.retain(|&i| matches.contains(&view.sessions[i].id));
    }
    let (tags, _) = labels::split_query(&query.text);
    order.retain(|&i| labels::has_tags(&view.sessions[i], &tags));
    let shown = order.len();
    let rows = match query.grouping {
        Grouping::Off => order.into_iter().map(GroupRow::Item).collect(),
        Grouping::Folder => group_by_folder(view.sessions, &order),
        Grouping::Repo => group_by_repo(view.sessions, &order, view.places),
    };
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| match row {
            GroupRow::Header { group, count, size_bytes } => header(view, group, *count, *size_bytes),
            GroupRow::Item(i) => item(view, &view.sessions[*i], query.grouping),
        })
        .collect();
    let many = view.tools.len() > 1;
    let columns: Vec<Value> = COLUMNS
        .iter()
        .filter(|(key, ..)| many || *key != "tool")
        .map(|(key, title, sort)| json!({ "key": key, "title": title, "descending": sort.default_descending() }))
        .collect();
    let suggestion = Suggestion::ALL.into_iter().find(|s| query.filters == s.filters()).map(|s| label(view.config, Cleanup::Conversations(s)).trim_start_matches("Conversations ").to_string());
    let tools: Vec<Value> = view.tools.iter().map(|t| json!({ "key": tool_key(*t), "label": t.short() })).collect();
    json!({
        "version": version,
        "loading": loading,
        "notice": notice.map(sanitize),
        "looked_in": sanitize(view.looked_in),
        "total": view.sessions.len(),
        "total_size": bytes(view.sessions.iter().map(|s| s.size_bytes).sum()),
        "open": view.sessions.iter().filter(|s| s.open).count(),
        "crashed": view.sessions.iter().filter(|s| s.crashed).count(),
        "shown": shown,
        "filtered": query.filters.any() || view.matches.is_some() || !tags.is_empty(),
        "suggestion": suggestion,
        "columns": columns,
        "tools": tools,
        "rows": rows,
    })
}

fn date(ms: Option<i64>) -> String {
    ms.and_then(|t| Local.timestamp_millis_opt(t).single()).map_or("-".to_string(), |d| d.format("%Y-%m-%d %H:%M").to_string())
}

pub fn details(s: &Session, sessions: &[Session], forks: &HashMap<String, String>, preview: Result<Preview, String>) -> Value {
    let name = |x: &Session| sanitize(&x.display_name());
    let mut notes = Vec::new();
    if let Some(next) = &s.continued_in {
        notes.push(format!("Continued in {}", sanitize(next)));
    }
    if let Some(from) = forks.get(&s.id) {
        notes.push(sessions.iter().find(|x| &x.id == from).map_or("Forked from a deleted conversation".to_string(), |o| format!("Forked from {}", name(o))));
    }
    let mut children: Vec<&Session> = sessions.iter().filter(|x| forks.get(&x.id) == Some(&s.id)).collect();
    children.sort_by_key(|x| x.started_at_ms);
    if !children.is_empty() {
        notes.push(format!("Forks: {}", children.iter().map(|x| name(x)).collect::<Vec<_>>().join(", ")));
    }
    if s.bad_lines > 0 {
        notes.push(format!("{} unreadable lines", s.bad_lines));
    }
    let lines = match (s.lines_added, s.lines_removed) {
        (None, None) => "-".to_string(),
        (a, r) => format!("+{} -{}", a.unwrap_or(0), r.unwrap_or(0)),
    };
    let (turns, truncated, error) = match preview {
        Ok(p) => {
            let turns: Vec<Value> = p
                .turns
                .iter()
                .map(|t| {
                    let who = match t.role {
                        Role::You => "You",
                        Role::Agent => s.source.short(),
                    };
                    json!({ "who": who, "you": t.role == Role::You, "text": sanitize_lines(&t.text), "tool_calls": t.tool_calls })
                })
                .collect();
            (turns, p.truncated, None)
        }
        Err(e) => (Vec::new(), false, Some(format!("Couldn't read this conversation: {}", sanitize(&e)))),
    };
    json!({
        "id": s.id,
        "command": copy_command(s),
        "name": name(s),
        "pinned": s.pinned,
        "tags": s.tags.iter().map(|t| sanitize(t)).collect::<Vec<_>>(),
        "tool": s.source.label(),
        "folder": s.folder.as_deref().map_or("-".to_string(), sanitize),
        "missing": s.is_folder_missing(),
        "started": date(s.started_at_ms),
        "cost": s.cost_usd.map_or("-".to_string(), |c| format!("${c:.2}")),
        "lines": lines,
        "pr": s.pr_url.as_deref().map_or("-".to_string(), sanitize),
        "notes": notes,
        "recaps": s.recaps.iter().map(|r| sanitize_lines(r)).collect::<Vec<_>>(),
        "turns": turns,
        "truncated": truncated,
        "error": error,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ccm_core::preview::Turn;

    pub fn session(id: &str) -> Session {
        Session {
            id: id.into(),
            source: Source::ClaudeCode,
            transcript_path: String::new(),
            transcript_size: 0,
            transcript_mtime_ns: 0,
            size_bytes: 1024,
            name: format!("name {id}"),
            folder: Some("/home/me/work".into()),
            folders: Vec::new(),
            branch: Some("main".into()),
            model: Some("claude-test-1".into()),
            started_at_ms: Some(1_000),
            last_active_ms: Some(1_000),
            context_used: Some(87_000),
            context_window: None,
            prompts: 2,
            replies: 3,
            cost_usd: Some(1.5),
            lines_added: Some(10),
            lines_removed: Some(2),
            pr_url: None,
            continued_in: None,
            forked_from: None,
            recaps: Vec::new(),
            bad_lines: 0,
            open: false,
            crashed: false,
            window_size: None,
            folder_exists: true,
            pinned: false,
            tags: Vec::new(),
        }
    }

    fn query(pairs: &[(&str, &str)]) -> Query {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Query::parse(&map, &[Source::ClaudeCode, Source::Codex])
    }

    fn rendered(sessions: &[Session], matches: Option<&HashSet<String>>, q: &Query, places: &HashMap<String, Place>) -> Value {
        let config = Config::default();
        let tools = [Source::ClaudeCode];
        let view = View { sessions, matches, places, config: &config, home: "/home/me", looked_in: "/home/me/.claude", tools: &tools, now_ms: 3_600_000 };
        render(&view, q, 7, false, None)
    }

    fn ids(value: &Value) -> Vec<String> {
        value["rows"].as_array().unwrap().iter().filter_map(|r| r["id"].as_str().map(str::to_string)).collect()
    }

    #[test]
    fn addresses_are_decoded() {
        let p = params("q=caf%C3%A9+au%20lait&x&filters=open%2Clarge&bad=%zz%2");
        assert_eq!(p["q"], "café au lait");
        assert_eq!(p["x"], "");
        assert_eq!(p["filters"], "open,large");
        assert_eq!(p["bad"], "%zz%2");
    }

    #[test]
    fn a_query_starts_as_the_terminal_app_does() {
        assert_eq!(query(&[]), Query { filters: Filters::default(), sort: SortKey::LastActive, descending: true, grouping: Grouping::Off, text: String::new() });
    }

    #[test]
    fn a_query_reads_everything_it_can_set() {
        let q = query(&[("filters", "open,crashed,junk,missing,large,nonsense"), ("tool", "codex"), ("sort", "size"), ("desc", "0"), ("group", "repo"), ("q", "x")]);
        assert!(q.filters.open && q.filters.crashed && q.filters.junk && q.filters.missing && q.filters.large);
        assert!(!q.filters.old && !q.filters.closed && !q.filters.unpinned);
        assert_eq!(q.filters.tool, Some(Source::Codex));
        assert_eq!((q.sort, q.descending, q.grouping, q.text.as_str()), (SortKey::Size, false, Grouping::Repo, "x"));
    }

    #[test]
    fn a_clean_up_line_is_named_only_when_the_filters_are_exactly_its_own() {
        let suggestion = |filters: &str| rendered(&[session("a")], None, &query(&[("filters", filters)]), &HashMap::new())["suggestion"].clone();
        assert_eq!(suggestion("closed,unpinned,junk"), "Claude never replied to");
        assert!(suggestion("closed,junk").is_null(), "pinned ones would show");
        assert_eq!(suggestion("closed,unpinned,large,old"), "over 100M not used for 30 days");
        assert!(suggestion("junk").is_null());
        assert!(suggestion("closed,unpinned,junk,large").is_null());
    }

    #[test]
    fn a_sort_starts_in_its_own_direction() {
        assert!(!query(&[("sort", "name")]).descending);
        assert!(query(&[("sort", "size")]).descending);
        assert_eq!(query(&[("sort", "nonsense")]).sort, SortKey::LastActive);
    }

    #[test]
    fn a_tool_that_is_not_read_filters_nothing() {
        let map = HashMap::from([("tool".to_string(), "codex".to_string())]);
        assert_eq!(Query::parse(&map, &[Source::ClaudeCode]).filters.tool, None);
    }

    #[test]
    fn rows_are_the_same_figures_as_the_terminal() {
        let out = rendered(&[session("a")], None, &query(&[]), &HashMap::new());
        let cells = &out["rows"][0]["cells"];
        assert_eq!(cells["folder"], "~/work");
        assert_eq!(cells["active"], "59m");
        assert_eq!(cells["messages"], "5");
        assert_eq!(cells["size"], bytes(1024));
        assert_eq!(cells["model"], "test-1");
        assert_eq!(cells["status"], "");
        assert_eq!(out["total"], 1);
        assert_eq!(out["version"], 7);
    }

    #[test]
    fn filters_sort_and_search_apply() {
        let mut open = session("open");
        open.open = true;
        open.last_active_ms = Some(5);
        let mut big = session("big");
        big.size_bytes = 1 << 40;
        big.last_active_ms = Some(3);
        let sessions = [open, big, session("plain")];
        assert_eq!(ids(&rendered(&sessions, None, &query(&[("filters", "open")]), &HashMap::new())), ["open"]);
        assert_eq!(ids(&rendered(&sessions, None, &query(&[("filters", "large")]), &HashMap::new())), ["big"]);
        assert_eq!(ids(&rendered(&sessions, None, &query(&[("sort", "size")]), &HashMap::new()))[0], "big");
        let matches: HashSet<String> = ["plain".to_string()].into();
        let found = rendered(&sessions, Some(&matches), &query(&[("q", "x")]), &HashMap::new());
        assert_eq!(ids(&found), ["plain"]);
        assert_eq!((found["shown"].as_u64(), found["filtered"].as_bool()), (Some(1), Some(true)));
    }

    #[test]
    fn groups_have_headers_and_repos_have_tags() {
        let mut other = session("b");
        other.folder = Some("/home/me/gone".into());
        let sessions = [session("a"), other];
        let out = rendered(&sessions, None, &query(&[("group", "folder")]), &HashMap::new());
        assert_eq!(out["rows"][0]["header"], "~/gone");
        assert_eq!(out["rows"][2]["header"], "~/work");
        let places = HashMap::from([
            ("/home/me/work".to_string(), Place::Repo { repo: "/home/me/repo".into(), worktree: Some("/home/me/work".into()) }),
            ("/home/me/gone".to_string(), Place::Gone),
        ]);
        let out = rendered(&sessions, None, &query(&[("group", "repo")]), &places);
        let rows = out["rows"].as_array().unwrap();
        let tagged = rows.iter().find(|r| r["id"] == "a").unwrap();
        assert_eq!(tagged["tag"], "main · work");
    }

    #[test]
    fn the_tool_column_only_shows_with_two_tools() {
        let out = rendered(&[session("a")], None, &query(&[]), &HashMap::new());
        assert_eq!(out["columns"][0]["key"], "name");
    }

    #[test]
    fn text_that_could_mislead_is_cleaned() {
        let mut s = session("a");
        s.name = "good\u{202e}evil\x1b[31m".into();
        s.folder = Some("/home/me/\u{202e}x".into());
        let out = rendered(&[s.clone()], None, &query(&[]), &HashMap::new());
        assert_eq!(out["rows"][0]["cells"]["name"], "goodevil");
        assert_eq!(out["rows"][0]["cells"]["folder"], "~/x");
        let d = details(&s, &[], &HashMap::new(), Ok(Preview::default()));
        assert_eq!(d["name"], "goodevil");
        assert_eq!(d["folder"], "/home/me/x");
    }

    #[test]
    fn markup_in_a_conversation_stays_plain_data() {
        let mut s = session("a");
        s.name = "<img src=x onerror=alert(1)>".into();
        let out = rendered(&[s], None, &query(&[]), &HashMap::new());
        assert_eq!(out["rows"][0]["cells"]["name"], "<img src=x onerror=alert(1)>");
    }

    #[test]
    fn details_say_what_the_terminal_says() {
        let mut s = session("child");
        s.bad_lines = 2;
        s.recaps = vec!["a recap".into()];
        let original = session("original");
        let forks = HashMap::from([("child".to_string(), "original".to_string())]);
        let preview = Preview { turns: vec![Turn { role: Role::You, text: "hi\nthere".into(), tool_calls: 0 }, Turn { role: Role::Agent, text: "ok".into(), tool_calls: 3 }], truncated: true };
        let d = details(&s, &[s.clone(), original.clone()], &forks, Ok(preview));
        assert_eq!(d["cost"], "$1.50");
        assert_eq!(d["lines"], "+10 -2");
        assert_eq!(d["notes"][0], "Forked from name original");
        assert_eq!(d["notes"][1], "2 unreadable lines");
        assert_eq!(d["recaps"][0], "a recap");
        assert_eq!(d["turns"][0]["text"], "hi\nthere");
        assert_eq!(d["turns"][1]["who"], "Claude");
        assert_eq!(d["turns"][1]["tool_calls"], 3);
        assert_eq!(d["truncated"], true);

        let parent = details(&original, &[s.clone(), original.clone()], &forks, Ok(Preview::default()));
        assert_eq!(parent["notes"][0], "Forks: name child");
        let orphan = details(&s, std::slice::from_ref(&s), &forks, Ok(Preview::default()));
        assert_eq!(orphan["notes"][0], "Forked from a deleted conversation");
    }

    #[test]
    fn a_preview_that_cannot_be_read_says_so() {
        let d = details(&session("a"), &[], &HashMap::new(), Err("gone".into()));
        assert_eq!(d["error"], "Couldn't read this conversation: gone");
    }
}
