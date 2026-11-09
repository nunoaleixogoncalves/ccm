//! The worktree view on the page. Git runs on another thread, and the page only ever names a
//! worktree from the latest scan, so nothing it sends can point Git anywhere else.

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread;

use ccm_core::format::{sanitize, short_path};
use ccm_core::model::Session;
use ccm_core::removal::Target;
use ccm_core::worktree::{Link, Scan, Worktree};
use serde_json::{json, Value};

use super::deletion::{self, Held};
use super::{lock, now_ms, read, State};
use crate::app::Countdown;
use crate::wording::{changes_text, conversations_text, link_notes, plural, removal_head, state_lines, unpushed_text, worktree_status};
use crate::worktrees::{folders_of, Row};

/// Starts Git if a look is due. The page asks again until it's done, so nothing waits for it.
pub(super) fn look(state: &Arc<State>, sessions: &[Session], fresh: bool) {
    let (folders, measure) = {
        let mut w = lock(&state.worktrees);
        if fresh {
            w.refresh_soon();
        }
        if !w.due() {
            return;
        }
        w.start();
        (folders_of(sessions), std::mem::take(&mut w.measure_due))
    };
    let state = Arc::clone(state);
    thread::spawn(move || {
        let found = catch_unwind(AssertUnwindSafe(|| (state.effects.scan)(&folders, measure)));
        let (scan, sizes, busy) = found.unwrap_or_else(|_| (Scan { errors: vec!["Looking for worktrees stopped unexpectedly.".into()], ..Scan::default() }, None, HashSet::new()));
        let sessions = Arc::clone(&read(&state.live).sessions);
        let mut w = lock(&state.worktrees);
        w.busy = busy;
        if let Some(sizes) = sizes {
            w.sizes = sizes;
        }
        w.finish(scan, &sessions);
    });
}

fn open_count(sessions: &[Session], links: &[Link]) -> usize {
    links.iter().filter(|l| sessions.iter().any(|s| s.id == l.id && s.open)).count()
}

fn conversation(sessions: &[Session], l: &Link, now: i64) -> Value {
    let s = sessions.iter().find(|s| s.id == l.id);
    let state = match s {
        Some(s) if s.open => "open",
        Some(s) if s.crashed => "crashed",
        _ => "",
    };
    json!({
        "name": s.map_or_else(|| sanitize(&l.id), |s| sanitize(&s.display_name())),
        "state": state,
        "notes": link_notes(l, now).join(", "),
    })
}

fn row(w: &Worktree, links: &[Link], sessions: &[Session], home: &str, now: i64) -> Value {
    let open = open_count(sessions, links);
    let status = worktree_status(w, open);
    let class = match status {
        "missing" => "is-dim",
        "open" => "is-open",
        _ => "",
    };
    let mut newest: Vec<&Link> = links.iter().collect();
    newest.sort_by_key(|l| std::cmp::Reverse(l.last_ms));
    let shown = |p: &std::path::Path| sanitize(&short_path(&p.to_string_lossy(), home));
    let mut state = state_lines(w);
    if w.locked {
        state.push("Locked in Git, so Git won't remove it until it's unlocked.".into());
    }
    json!({
        "id": w.path.to_string_lossy(),
        "class": class,
        "cells": {
            "folder": shown(&w.path),
            "branch": w.branch.as_deref().map_or("(detached)".to_string(), sanitize),
            "changes": changes_text(w),
            "unpushed": unpushed_text(w),
            "conversations": conversations_text(links.len(), open),
            "status": status,
        },
        "path": sanitize(&w.path.to_string_lossy()),
        "branch": w.branch.as_deref().map_or("none, a commit is checked out directly".to_string(), sanitize),
        "repo": shown(&w.repo),
        "state": state,
        "worked_here": newest.into_iter().map(|l| conversation(sessions, l, now)).collect::<Vec<_>>(),
    })
}

pub fn list(state: &Arc<State>, params: &HashMap<String, String>) -> Value {
    let sessions = Arc::clone(&read(&state.live).sessions);
    look(state, &sessions, params.contains_key("fresh"));
    let only_idle = params.get("idle").is_some_and(|v| v == "1");
    let now = now_ms();
    let mut w = lock(&state.worktrees);
    // Set on every request, so one tab's choice can't show in another.
    w.show_idle(only_idle, &sessions);
    let rows: Vec<Value> = w
        .rows
        .iter()
        .map(|r| match r {
            Row::Header { repo, count } => json!({ "header": sanitize(&short_path(&repo.to_string_lossy(), &state.home)), "count": plural(*count as u64, "worktree", "worktrees") }),
            Row::Item(i) => row(&w.worktrees()[*i], w.links.get(*i).map_or(&[][..], Vec::as_slice), &sessions, &state.home, now),
        })
        .collect();
    let linked: Vec<&Worktree> = w.worktrees().iter().filter(|x| x.is_linked()).collect();
    let repos = w.rows.iter().filter(|r| matches!(r, Row::Header { .. })).count();
    let shown = w.rows.len() - repos;
    let mut parts = vec![if only_idle { format!("showing {shown} with nothing to lose, of {}", linked.len()) } else { format!("{} in {repos} {}", linked.len(), if repos == 1 { "repository" } else { "repositories" }) }];
    let changed = linked.iter().filter(|x| x.state.is_some_and(|s| s.changes > 0)).count();
    let unpushed = linked.iter().filter(|x| x.state.is_some_and(|s| s.unpushed > 0)).count();
    if changed > 0 {
        parts.push(format!("{changed} with changes"));
    }
    if unpushed > 0 {
        parts.push(format!("{unpushed} with unpushed commits"));
    }
    if w.scanning {
        parts.push("checking…".into());
    }
    let errors: Vec<String> = w.scan.as_ref().map_or(Vec::new(), |s| s.errors.iter().map(|e| sanitize(e)).collect());
    let empty = match &w.scan {
        None => "Looking for worktrees…".to_string(),
        Some(_) if !errors.is_empty() => errors.join(" "),
        Some(_) if only_idle => "None left with nothing to lose. Press c to show all worktrees.".to_string(),
        Some(_) => "No worktrees found. Only repositories that a conversation worked in are checked.".to_string(),
    };
    if let Some(error) = errors.first() {
        parts.push(error.clone());
    }
    json!({ "summary": parts.join("  ·  "), "rows": rows, "empty": empty, "only_idle": only_idle })
}

/// What removing these would do, waiting for an answer. Each is found in the latest scan.
pub fn plan(state: &Arc<State>, body: &Value) -> Value {
    let wanted: HashSet<&str> = body["paths"].as_array().map_or(HashSet::new(), |list| list.iter().filter_map(Value::as_str).collect());
    let sessions = Arc::clone(&read(&state.live).sessions);
    let targets: Vec<Target> = {
        let w = lock(&state.worktrees);
        w.worktrees().iter().filter(|x| x.is_linked() && wanted.contains(x.path.to_string_lossy().as_ref())).map(|x| Target { path: x.path.clone(), repo: x.repo.clone() }).collect()
    };
    if targets.is_empty() {
        return deletion::refused("Can't remove: nothing to remove.");
    }
    let plan = (state.effects.plan_remove)(&targets, &sessions);
    if plan.items.is_empty() {
        let reason = plan.refused.first().map_or("nothing to remove", |r| r.reason.as_str());
        return deletion::refused(&format!("Can't remove: {reason}."));
    }
    let head = deletion::toned(removal_head(&plan));
    let lines = deletion::removal_lines(state, &sessions, &plan);
    let countdown = Countdown::new(plan.items.iter().any(|i| i.unpushed > 0));
    let held = Held { remove: Some(plan), countdown, ..Held::default() };
    deletion::hold(state, &sessions, held, |reply| {
        reply["kind"] = json!("remove");
        reply["head"] = json!(head);
        reply["removing"] = json!(lines);
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use ccm_core::model::FolderVisit;
    use ccm_core::removal::{self, Planned, Refused};
    use ccm_core::worktree::State as GitState;

    use super::super::list::tests::session;
    use super::*;
    use crate::web::{deletion::{cancel, confirm}, write};

    const A: &str = "11111111-2222-4333-8444-555555555555";

    pub fn worktree(path: &str, changes: u64, unpushed: u64) -> Worktree {
        Worktree {
            path: path.into(),
            repo: "/home/me/shop".into(),
            main: path == "/home/me/shop",
            bare: false,
            branch: Some("b".into()),
            locked: false,
            prunable: false,
            state: Some(GitState { changes, unpushed, has_remote: true, upstream_gone: false }),
        }
    }

    pub fn worked_in(id: &str, folder: &str) -> Session {
        let mut s = session(id);
        s.folder = Some(folder.into());
        s.folders = vec![FolderVisit { path: folder.into(), records: 5, last_record: 5, last_ms: Some(1000) }];
        s
    }

    fn planned(t: &Target, unpushed: u64) -> Planned {
        Planned { target: t.clone(), branch: Some("b".into()), gone: false, ignored: Vec::new(), ignored_bytes: 0, ignored_more: false, links: Vec::new(), unpushed, has_remote: false, upstream_gone: false, bytes: 0 }
    }

    type Calls = Arc<Mutex<Vec<Vec<PathBuf>>>>;
    type Asked = Arc<Mutex<Vec<(Vec<String>, bool)>>>;

    /// A scan already finished, so nothing runs Git. `shop-ahead` has an unpushed commit, and
    /// `shop-locked` is refused when planned.
    fn state(dir: &Path, sessions: Vec<Session>) -> (Arc<State>, Calls) {
        let calls: Calls = Arc::default();
        let mut made = State::for_tests(dir, sessions.clone());
        let s = Arc::get_mut(&mut made).unwrap();
        s.effects.plan_remove = Box::new(|targets, _| {
            let mut plan = removal::Plan::default();
            for t in targets {
                if t.path.ends_with("shop-locked") {
                    plan.refused.push(Refused { path: t.path.clone(), reason: "it's locked".into() });
                } else {
                    plan.items.push(planned(t, u64::from(t.path.ends_with("shop-ahead"))));
                }
            }
            plan
        });
        let log = Arc::clone(&calls);
        s.effects.remove = Box::new(move |plan, _| {
            let paths: Vec<PathBuf> = plan.items.iter().map(|i| i.target.path.clone()).collect();
            log.lock().unwrap().push(paths.clone());
            removal::Outcome { removed: paths, ..removal::Outcome::default() }
        });
        let scan = Scan {
            worktrees: vec![
                worktree("/home/me/shop", 0, 0),
                worktree("/home/me/shop-tidy", 0, 0),
                worktree("/home/me/shop-ahead", 0, 1),
                worktree("/home/me/shop-wip", 3, 0),
                worktree("/home/me/shop-locked", 0, 0),
            ],
            errors: Vec::new(),
        };
        lock(&made.worktrees).finish(scan, &sessions);
        (made, calls)
    }

    fn items(out: &Value) -> Vec<Value> {
        out["rows"].as_array().unwrap().iter().filter(|r| r["id"].is_string()).cloned().collect()
    }

    fn no_params() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn linked_worktrees_are_listed_by_repository_with_their_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut open = worked_in(A, "/home/me/shop-wip/src");
        open.open = true;
        let (state, _) = state(dir.path(), vec![open]);
        let out = list(&state, &no_params());
        assert_eq!(out["rows"][0]["header"], "~/shop");
        assert_eq!(out["rows"][0]["count"], "4 worktrees");
        let rows = items(&out);
        assert_eq!(rows.len(), 4, "the main worktree isn't a row");
        let wip = rows.iter().find(|r| r["cells"]["folder"] == "~/shop-wip").unwrap();
        assert_eq!(wip["cells"]["changes"], "3 files");
        assert_eq!(wip["cells"]["conversations"], "1 (1 open)");
        assert_eq!(wip["cells"]["status"], "open");
        assert_eq!(wip["state"][0], "3 files not committed. Every commit is on a remote branch, as of the last fetch.");
        assert_eq!(wip["worked_here"][0]["state"], "open");
        assert!(out["summary"].as_str().unwrap().starts_with("4 in 1 repository  ·  1 with changes  ·  1 with unpushed commits"), "{}", out["summary"]);
    }

    #[test]
    fn only_those_with_nothing_to_lose_can_be_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![worked_in(A, "/home/me/shop-locked")]);
        let out = list(&state, &HashMap::from([("idle".to_string(), "1".to_string())]));
        let rows = items(&out);
        assert_eq!(rows.iter().map(|r| r["cells"]["folder"].as_str().unwrap()).collect::<Vec<_>>(), ["~/shop-tidy"]);
        assert!(out["summary"].as_str().unwrap().starts_with("showing 1 with nothing to lose, of 4"));
        assert_eq!(items(&list(&state, &no_params())).len(), 4, "one tab's choice isn't another's");
    }

    #[test]
    fn a_page_can_only_name_linked_worktrees_from_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), Vec::new());
        for paths in [json!(["/etc"]), json!(["/home/me/shop"]), json!([]), json!("x")] {
            assert_eq!(plan(&state, &json!({ "paths": paths }))["message"], "Can't remove: nothing to remove.");
        }
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn removing_reports_keeps_the_branch_and_drops_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), Vec::new());
        let out = plan(&state, &json!({ "paths": ["/home/me/shop-tidy"] }));
        assert_eq!(out["head"][0]["text"], "1 worktree will be removed. Branches stay, so commits are kept.");
        assert_eq!(out["removing"][0]["text"], "~/shop-tidy  b");
        assert!(out["seconds_left"].is_null());
        assert!(calls.lock().unwrap().is_empty(), "nothing changes until it's confirmed");
        let done = confirm(&state, &json!({ "plan_id": out["plan_id"] }));
        assert_eq!(done["summary"], "Removed 1 worktree. Its branch is kept.");
        assert_eq!(*calls.lock().unwrap(), [vec![PathBuf::from("/home/me/shop-tidy")]]);
        assert_eq!(items(&list(&state, &no_params())).len(), 3);
    }

    #[test]
    fn a_refusal_for_all_says_why_and_for_some_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), Vec::new());
        assert_eq!(plan(&state, &json!({ "paths": ["/home/me/shop-locked"] }))["message"], "Can't remove: it's locked.");
        let out = plan(&state, &json!({ "paths": ["/home/me/shop-locked", "/home/me/shop-tidy"] }));
        assert_eq!(out["removing"].as_array().unwrap().last().unwrap()["text"], "Skipping ~/shop-locked: it's locked");
    }

    #[test]
    fn unpushed_commits_lock_the_removal_here_and_trying_early_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), Vec::new());
        let out = plan(&state, &json!({ "paths": ["/home/me/shop-ahead"] }));
        assert_eq!(out["seconds_left"], 5);
        assert_eq!(out["head"][1]["text"], "Warning: It has 1 unpushed commit. Branches keep them, but only on this machine.");
        let early = confirm(&state, &json!({ "plan_id": out["plan_id"] }));
        assert_eq!((early["ok"].as_bool(), early["restarted"].as_bool()), (Some(false), Some(true)));
        assert!(calls.lock().unwrap().is_empty());
        deletion::run_out(&state);
        assert_eq!(confirm(&state, &json!({ "plan_id": out["plan_id"] }))["summary"], "Removed 1 worktree. Its branch is kept.");
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_cancelled_removal_is_gone_and_a_plan_works_once() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), Vec::new());
        let id = plan(&state, &json!({ "paths": ["/home/me/shop-tidy"] }))["plan_id"].clone();
        cancel(&state, &json!({ "plan_id": id }));
        assert_eq!(confirm(&state, &json!({ "plan_id": id }))["ok"], false);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_look_runs_once_at_a_time_and_measures_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let mut made = State::for_tests(dir.path(), vec![worked_in(A, "/home/me/shop-tidy")]);
        let asked: Asked = Arc::default();
        let log = Arc::clone(&asked);
        Arc::get_mut(&mut made).unwrap().effects.scan = Box::new(move |folders, measure| {
            log.lock().unwrap().push((folders.to_vec(), measure));
            (Scan { worktrees: vec![worktree("/home/me/shop-tidy", 0, 0)], errors: Vec::new() }, Some(HashMap::from([(PathBuf::from("/home/me/shop-tidy"), 7)])), HashSet::new())
        });
        lock(&made.worktrees).measure_due = true;
        assert_eq!(list(&made, &no_params())["empty"], "Looking for worktrees…");
        for _ in 0..200 {
            if lock(&made.worktrees).scan.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        list(&made, &no_params());
        assert_eq!(*asked.lock().unwrap(), [(vec!["/home/me/shop-tidy".to_string()], true)], "not due again yet");
        assert_eq!(lock(&made.worktrees).sizes[Path::new("/home/me/shop-tidy")], 7);
    }

    #[test]
    fn a_conversation_changing_shows_without_asking_git_again() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), Vec::new());
        assert_eq!(items(&list(&state, &no_params()))[3]["cells"]["conversations"], "-");
        write(&state.live).sessions = Arc::new(vec![worked_in(A, "/home/me/shop-wip")]);
        assert_eq!(items(&list(&state, &no_params()))[3]["cells"]["conversations"], "1");
    }
}
