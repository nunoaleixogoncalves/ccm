//! The clean-up screen on the page. The page names lines, and what's on them is worked out here.

use std::collections::HashMap;
use std::sync::Arc;

use ccm_core::delete::Plan;
use ccm_core::format::{ago, bytes, sanitize};
use ccm_core::jobs::Leftover;
use ccm_core::query::Suggestion;
use ccm_core::removal::Target;
use serde_json::{json, Value};

use super::deletion::{self, plan_for, refused, Held};
use super::worktrees::look;
use super::{lock, now_ms, read, State};
use crate::cleanup::{conversations, explanation, job_what, label, rm_commands, tally, without_unpushed, Cleanup, JOB_KEPT_NOTE, KEPT_JOBS, SAFETY};
use crate::wording::{ignored_line, Tone};

fn key(line: Cleanup) -> &'static str {
    match line {
        Cleanup::Conversations(Suggestion::NoReply) => "no_reply",
        Cleanup::Conversations(Suggestion::FolderGone) => "folder_gone",
        Cleanup::Conversations(Suggestion::LargeOld) => "large_old",
        Cleanup::Worktrees => "worktrees",
        Cleanup::Jobs => "jobs",
    }
}

/// The list filters that show a line's conversations, by the names the list understands.
fn filters(line: Cleanup) -> Vec<&'static str> {
    match line {
        Cleanup::Conversations(Suggestion::NoReply) => vec!["closed", "junk"],
        Cleanup::Conversations(Suggestion::FolderGone) => vec!["closed", "missing"],
        Cleanup::Conversations(Suggestion::LargeOld) => vec!["closed", "large", "old"],
        Cleanup::Worktrees | Cleanup::Jobs => Vec::new(),
    }
}

fn job(l: &Leftover, now: i64) -> Value {
    json!({
        "short": sanitize(&l.short()),
        "size": bytes(l.bytes),
        "age": format!("{} ago", ago(now, l.last_ms)),
        "what": job_what(l),
        "note": if l.worktree { JOB_KEPT_NOTE } else { "" },
    })
}

pub fn overview(state: &Arc<State>, params: &HashMap<String, String>) -> Value {
    let sessions = Arc::clone(&read(&state.live).sessions);
    let fresh = params.contains_key("fresh");
    if fresh {
        lock(&state.worktrees).measure_due = true;
        // Each visit looks again, as the terminal app's does.
        let found = (state.effects.leftovers)();
        *lock(&state.leftovers) = Some(found);
    }
    look(state, &sessions, fresh);
    let now = now_ms();
    let mut w = lock(&state.worktrees);
    w.relink(&sessions);
    let leftovers = lock(&state.leftovers);
    let found = leftovers.as_deref();
    let tallies: Vec<_> = Cleanup::ALL.iter().map(|&l| tally(l, &sessions, &state.config, now, &w, found)).collect();
    let lines: Vec<Value> = Cleanup::ALL
        .iter()
        .zip(&tallies)
        .map(|(&line, t)| {
            let mut value = json!({
                "key": key(line),
                "label": label(&state.config, line),
                "count": t.map(|t| t.count),
                "items": t.map_or("…".to_string(), |t| t.count.to_string()),
                "frees": t.map_or(String::new(), |t| t.bytes.map_or("?".into(), bytes)),
                "about": explanation(&state.config, line, *t),
                "filters": filters(line),
            });
            if line == Cleanup::Jobs {
                value["jobs"] = found.unwrap_or_default().iter().map(|l| job(l, now)).collect();
            }
            value
        })
        .collect();
    let count: usize = tallies.iter().flatten().map(|t| t.count).sum();
    let freed: u64 = tallies.iter().flatten().filter_map(|t| t.bytes).sum();
    let checking = tallies.iter().any(Option::is_none) || w.scanning;
    let mut parts = vec![if count == 0 && !checking { "nothing to clean up right now".to_string() } else { format!("{count} suggested, {} could be freed", bytes(freed)) }];
    if checking {
        parts.push("checking…".into());
    }
    if let Some(error) = w.scan.as_ref().and_then(|s| s.errors.first()) {
        parts.push(sanitize(error));
    }
    json!({ "summary": parts.join("  ·  "), "lines": lines, "safety": SAFETY })
}

/// Asks to delete everything on these lines, in one confirmation. A conversation on two lines
/// is deleted once. With `list`, the job folders are shown even when ccm can delete none of them.
pub fn plan(state: &Arc<State>, body: &Value) -> Value {
    let wanted: Vec<&str> = body["lines"].as_array().map_or(Vec::new(), |list| list.iter().filter_map(Value::as_str).collect());
    let lines: Vec<Cleanup> = Cleanup::ALL.into_iter().filter(|l| wanted.contains(&key(*l))).collect();
    let listing = body["list"].as_bool().unwrap_or(false);
    let sessions = Arc::clone(&read(&state.live).sessions);
    let now = now_ms();
    let (from, sizes, targets, jobs, checking) = {
        let mut w = lock(&state.worktrees);
        w.relink(&sessions);
        let leftovers = lock(&state.leftovers);
        let tallies: Vec<_> = lines.iter().map(|&l| (l, tally(l, &sessions, &state.config, now, &w, leftovers.as_deref()))).collect();
        let from: Vec<String> = tallies
            .iter()
            .filter_map(|(l, t)| t.filter(|t| t.count > 0).map(|t| format!("{}: {}, {}", label(&state.config, *l), t.count, t.bytes.map_or("size unknown".into(), bytes))))
            .collect();
        let targets: Vec<Target> = if lines.contains(&Cleanup::Worktrees) { w.idle().into_iter().map(|x| Target { path: x.path.clone(), repo: x.repo.clone() }).collect() } else { Vec::new() };
        let jobs: Vec<Leftover> = if lines.contains(&Cleanup::Jobs) { leftovers.clone().unwrap_or_default() } else { Vec::new() };
        if listing && jobs.is_empty() {
            return refused(if leftovers.is_none() { "Job folders can't be checked." } else { "No job folders to clean up." });
        }
        (from, w.sizes.clone(), targets, jobs, tallies.iter().any(|(_, t)| t.is_none()))
    };
    let mut picked: Vec<usize> = lines.iter().flat_map(|&l| conversations(l, &sessions, &state.config, now)).collect();
    picked.sort_unstable();
    picked.dedup();
    let can_go = |j: &&Leftover| !j.worktree;
    if picked.is_empty() && targets.is_empty() && !jobs.iter().any(|j| can_go(&j)) && !listing {
        return refused(if !jobs.is_empty() {
            KEPT_JOBS
        } else if checking {
            "Still checking. Try again in a moment."
        } else {
            "Nothing to delete on this line."
        });
    }
    let ids: Vec<String> = picked.into_iter().map(|i| sessions[i].id.clone()).collect();
    let plan = if ids.is_empty() { Plan::default() } else { plan_for(state, &sessions, &ids) };
    let remove = (!targets.is_empty()).then(|| without_unpushed((state.effects.plan_remove)(&targets, &sessions)));
    let removable = remove.as_ref().map_or(0, |r| r.items.len());
    let job_count = jobs.iter().filter(can_go).count();
    if plan.items.is_empty() && removable == 0 && job_count == 0 && !listing {
        let reason = plan.skipped.first().map(|s| s.reason.clone()).or_else(|| remove.as_ref().and_then(|r| r.refused.first()).map(|r| sanitize(&r.reason)));
        return refused(&format!("Can't delete: {}.", reason.as_deref().unwrap_or("nothing to delete")));
    }

    let mut removing = Vec::new();
    let mut removing_bytes = 0;
    if let Some(r) = &remove {
        if !r.items.is_empty() {
            removing.push(("Worktrees to remove. Their branches stay, so commits are kept:".to_string(), Tone::Bold));
        }
        removing.extend(ignored_line(r, ""));
        // A worktree's whole folder goes, as far as it was measured.
        removing_bytes = r.items.iter().map(|i| sizes.get(&i.target.path).copied().unwrap_or(i.bytes)).sum::<u64>();
    }
    let mut lines_out = deletion::toned(removing);
    if let Some(r) = &remove {
        lines_out.extend(deletion::removal_lines(state, &sessions, r));
    }
    let job_bytes: u64 = jobs.iter().filter(can_go).map(|j| j.bytes).sum();
    let job_values: Vec<Value> = jobs.iter().map(|l| job(l, now)).collect();
    let commands = rm_commands(&jobs);
    let nothing = plan.items.is_empty() && removable == 0 && job_count == 0;
    let kept_jobs = jobs.len() - job_count;
    let held = Held { plan, remove, jobs, ..Held::default() };
    deletion::hold(state, &sessions, held, |reply| {
        reply["kind"] = json!("cleanup");
        reply["from"] = json!(from);
        reply["removing"] = json!(lines_out);
        reply["removing_count"] = json!(removable);
        reply["bytes"]["removing"] = json!(removing_bytes);
        reply["jobs"] = json!(job_values);
        reply["job_count"] = json!(job_count);
        reply["kept_jobs"] = json!(kept_jobs);
        reply["bytes"]["jobs"] = json!(job_bytes);
        reply["rm_commands"] = json!(commands);
        reply["nothing"] = json!(nothing);
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use ccm_core::delete::{Item, Outcome};
    use ccm_core::jobs;
    use ccm_core::model::Source;
    use ccm_core::removal::{self, Planned};
    use ccm_core::worktree::{Scan, State as GitState, Worktree};

    use super::super::deletion::confirm;
    use super::super::list::tests::session;
    use super::*;

    const JUNK: &str = "11111111-2222-4333-8444-555555555555";
    const GONE: &str = "21111111-2222-4333-8444-555555555555";
    const BOTH: &str = "31111111-2222-4333-8444-555555555555";
    const OPEN_JUNK: &str = "41111111-2222-4333-8444-555555555555";
    const MB: u64 = 1024 * 1024;

    fn worktree(path: &str, unpushed: u64) -> Worktree {
        Worktree {
            path: path.into(),
            repo: "/home/me/shop".into(),
            main: path == "/home/me/shop",
            bare: false,
            branch: Some("b".into()),
            locked: false,
            prunable: false,
            state: Some(GitState { changes: 0, unpushed, has_remote: true, upstream_gone: false }),
        }
    }

    fn leftover(short: &str, kept: bool) -> Leftover {
        Leftover { dir: PathBuf::from(format!("/c/jobs/{short}")), name: Some(format!("job {short}")), worktree: kept, no_state: false, bytes: 2 * MB, last_ms: Some(0) }
    }

    fn sessions() -> Vec<ccm_core::model::Session> {
        let mut junk = session(JUNK);
        junk.replies = 0;
        junk.size_bytes = MB;
        let mut gone = session(GONE);
        gone.folder_exists = false;
        gone.size_bytes = 3 * MB;
        let mut both = session(BOTH);
        both.replies = 0;
        both.folder_exists = false;
        both.size_bytes = MB;
        let mut open = session(OPEN_JUNK);
        open.replies = 0;
        open.open = true;
        vec![junk, gone, both, open]
    }

    type Log = Arc<Mutex<Vec<String>>>;

    /// A scan and a look at the job folders already done. `shop-ahead` gains a commit between
    /// the scan and the plan.
    fn state(dir: &Path, found: Vec<Leftover>) -> (Arc<State>, Log) {
        let log: Log = Arc::default();
        let list = sessions();
        let mut made = State::for_tests(dir, list.clone());
        let s = Arc::get_mut(&mut made).unwrap();
        s.effects.plan_delete = Box::new(|sessions| ccm_core::delete::Plan {
            items: sessions.iter().map(|x| Item { id: x.id.clone(), source: Source::ClaudeCode, side: Vec::new(), transcripts: Vec::new(), bytes: x.size_bytes }).collect(),
            ..ccm_core::delete::Plan::default()
        });
        let l = Arc::clone(&log);
        s.effects.delete = Box::new(move |plan, _| {
            let mut ids: Vec<String> = plan.items.iter().map(|i| i.id[..1].to_string()).collect();
            ids.sort();
            l.lock().unwrap().push(format!("delete {}", ids.join(",")));
            Ok(Outcome { deleted: plan.items.iter().map(|i| i.id.clone()).collect(), ..Outcome::default() })
        });
        s.effects.plan_remove = Box::new(|targets, _| removal::Plan {
            items: targets
                .iter()
                .map(|t| Planned { target: t.clone(), branch: Some("b".into()), gone: false, ignored: Vec::new(), ignored_bytes: 0, ignored_more: false, links: Vec::new(), unpushed: u64::from(t.path.ends_with("shop-ahead")), has_remote: true, upstream_gone: false, bytes: 0 })
                .collect(),
            ..removal::Plan::default()
        });
        let l = Arc::clone(&log);
        s.effects.remove = Box::new(move |plan, _| {
            l.lock().unwrap().push(format!("remove {}", plan.items.len()));
            removal::Outcome { removed: plan.items.iter().map(|i| i.target.path.clone()).collect(), ..removal::Outcome::default() }
        });
        let l = Arc::clone(&log);
        s.effects.remove_jobs = Box::new(move |dirs| {
            l.lock().unwrap().push(format!("jobs {}", dirs.len()));
            jobs::Outcome { removed: dirs.to_vec(), bytes: dirs.len() as u64 * 2 * MB, kept: Vec::new() }
        });
        let left = found.clone();
        s.effects.leftovers = Box::new(move || left.iter().filter(|j| j.worktree).cloned().collect());
        {
            let mut w = lock(&made.worktrees);
            let scan = Scan { worktrees: vec![worktree("/home/me/shop", 0), worktree("/home/me/shop-tidy", 0), worktree("/home/me/shop-wip", 2)], errors: Vec::new() };
            w.finish(scan, &list);
            w.busy = HashSet::new();
            w.sizes = HashMap::from([(PathBuf::from("/home/me/shop-tidy"), 5 * MB)]);
        }
        *lock(&made.leftovers) = Some(found);
        (made, log)
    }

    fn line<'a>(out: &'a Value, wanted: &str) -> &'a Value {
        out["lines"].as_array().unwrap().iter().find(|l| l["key"] == wanted).unwrap()
    }

    fn no_params() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn every_line_says_what_it_holds_and_what_it_would_free() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![leftover("aaaaaaaa", false), leftover("bbbbbbbb", true)]);
        let out = overview(&state, &no_params());
        assert_eq!(line(&out, "no_reply")["items"], "2", "the open one isn't counted");
        assert_eq!(line(&out, "no_reply")["frees"], "2M");
        assert_eq!(line(&out, "folder_gone")["count"], 2);
        assert_eq!(line(&out, "large_old")["count"], 0);
        assert_eq!(line(&out, "worktrees")["frees"], "5M");
        assert_eq!(line(&out, "jobs")["items"], "2");
        assert_eq!(line(&out, "jobs")["frees"], "2M", "the one with a worktree frees nothing");
        assert_eq!(line(&out, "jobs")["jobs"][1]["note"], JOB_KEPT_NOTE);
        assert_eq!(line(&out, "no_reply")["filters"], json!(["closed", "junk"]));
        assert!(out["summary"].as_str().unwrap().starts_with("7 suggested"), "{}", out["summary"]);
    }

    #[test]
    fn a_line_still_being_checked_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), Vec::new());
        *lock(&state.leftovers) = None;
        let out = overview(&state, &no_params());
        assert_eq!(line(&out, "jobs")["items"], "…");
        assert!(line(&out, "jobs")["count"].is_null());
        assert!(out["summary"].as_str().unwrap().contains("checking…"));
    }

    #[test]
    fn a_look_at_the_screen_finds_job_folders_again() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![leftover("aaaaaaaa", false), leftover("bbbbbbbb", true)]);
        let out = overview(&state, &HashMap::from([("fresh".to_string(), "1".to_string())]));
        assert_eq!(line(&out, "jobs")["items"], "1", "the fake finder only returns the kept one");
    }

    #[test]
    fn lines_go_in_one_confirmation_and_a_conversation_on_two_goes_once() {
        let dir = tempfile::tempdir().unwrap();
        let (state, log) = state(dir.path(), Vec::new());
        let out = plan(&state, &json!({ "lines": ["no_reply", "folder_gone"] }));
        assert_eq!(out["conversations"].as_array().unwrap().len(), 3);
        assert!(log.lock().unwrap().is_empty());
        confirm(&state, &json!({ "plan_id": out["plan_id"] }));
        assert_eq!(*log.lock().unwrap(), ["delete 1,2,3"]);
        assert_eq!(read(&state.live).sessions.len(), 1, "the open one stays");
    }

    #[test]
    fn the_page_can_only_name_lines() {
        let dir = tempfile::tempdir().unwrap();
        let (state, log) = state(dir.path(), Vec::new());
        assert_eq!(plan(&state, &json!({ "lines": ["everything"], "ids": [OPEN_JUNK] }))["message"], "Nothing to delete on this line.");
        let out = plan(&state, &json!({ "lines": ["large_old"], "ids": [GONE] }));
        assert_eq!(out["message"], "Nothing to delete on this line.");
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn every_kind_goes_in_one_delete() {
        let dir = tempfile::tempdir().unwrap();
        let (state, log) = state(dir.path(), vec![leftover("aaaaaaaa", false), leftover("bbbbbbbb", true)]);
        let out = plan(&state, &json!({ "lines": ["no_reply", "worktrees", "jobs"] }));
        assert_eq!((out["removing_count"].as_u64(), out["job_count"].as_u64(), out["kept_jobs"].as_u64()), (Some(1), Some(1), Some(1)));
        assert_eq!(out["bytes"]["removing"], 5 * MB);
        assert_eq!(out["bytes"]["jobs"], 2 * MB);
        assert_eq!(out["rm_commands"], "claude rm bbbbbbbb");
        assert_eq!(out["removing"][0]["text"], "Worktrees to remove. Their branches stay, so commits are kept:");
        assert!(out["from"][0].as_str().unwrap().starts_with("Conversations Claude never replied to: 2, 2M"));
        let done = confirm(&state, &json!({ "plan_id": out["plan_id"] }));
        assert_eq!(*log.lock().unwrap(), ["delete 1,3", "remove 1", "jobs 1"]);
        assert_eq!(done["summary"], "Deleted 2 conversations, freed 2M. Removed 1 worktree. Its branch is kept. Deleted 1 job folder, freed 2M.");
        assert_eq!(line(&overview(&state, &no_params()), "worktrees")["count"], 0, "gone from the suggestion");
        assert_eq!(line(&overview(&state, &no_params()), "jobs")["count"], 1, "looked again after deleting");
    }

    #[test]
    fn a_worktree_that_gained_commits_is_skipped_rather_than_counted_down() {
        let dir = tempfile::tempdir().unwrap();
        let (state, log) = state(dir.path(), Vec::new());
        {
            let mut w = lock(&state.worktrees);
            let list = read(&state.live).sessions.clone();
            w.finish(Scan { worktrees: vec![worktree("/home/me/shop-tidy", 0), worktree("/home/me/shop-ahead", 0)], errors: Vec::new() }, &list);
        }
        let out = plan(&state, &json!({ "lines": ["worktrees"] }));
        assert_eq!(out["removing_count"], 1);
        assert!(out["seconds_left"].is_null());
        assert!(out["removing"].as_array().unwrap().iter().any(|l| l["text"].as_str().unwrap().starts_with("Skipping ~/shop-ahead: it has unpushed commits now")), "{}", out["removing"]);
        confirm(&state, &json!({ "plan_id": out["plan_id"] }));
        assert_eq!(*log.lock().unwrap(), ["remove 1"]);
    }

    #[test]
    fn jobs_with_a_worktree_can_be_listed_but_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let (state, log) = state(dir.path(), vec![leftover("bbbbbbbb", true)]);
        assert_eq!(plan(&state, &json!({ "lines": ["jobs"] }))["message"], KEPT_JOBS);
        let out = plan(&state, &json!({ "lines": ["jobs"], "list": true }));
        assert_eq!(out["nothing"], true);
        assert_eq!(out["rm_commands"], "claude rm bbbbbbbb");
        assert_eq!(confirm(&state, &json!({ "plan_id": out["plan_id"] }))["summary"], "");
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn listing_no_jobs_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), Vec::new());
        assert_eq!(plan(&state, &json!({ "lines": ["jobs"], "list": true }))["message"], "No job folders to clean up.");
        *lock(&state.leftovers) = None;
        assert_eq!(plan(&state, &json!({ "lines": ["jobs"], "list": true }))["message"], "Job folders can't be checked.");
    }

    #[test]
    fn a_line_still_checking_asks_to_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), Vec::new());
        *lock(&state.leftovers) = None;
        assert_eq!(plan(&state, &json!({ "lines": ["jobs"] }))["message"], "Still checking. Try again in a moment.");
    }
}
