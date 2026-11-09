//! Deleting from the page. The plan stays here and the page only refers to it, so nothing it
//! sends can add to what goes. The countdown is also kept here, as a page can be changed.

use std::path::PathBuf;
use std::sync::Arc;

use ccm_core::delete::{Plan, Skipped};
use ccm_core::format::{sanitize, short_path};
use ccm_core::jobs::Leftover;
use ccm_core::model::Session;
use ccm_core::removal;
use serde_json::{json, Value};

use super::{lock, read, write, State};
use crate::app::Countdown;
use crate::wording::{delete_report, jobs_report, planned_lines, removal_report, Tone};

const OUT_OF_DATE: &str = "This confirmation is out of date, so nothing was deleted. Start the delete again.";

/// A delete waiting for an answer. Nothing has changed yet.
pub struct Pending {
    id: String,
    plan: Plan,
    offered: removal::Plan,
    include_worktrees: bool,
    countdown: Countdown,
    /// Worktrees going for their own sake, from the worktree view or the clean-up screen.
    remove: Option<removal::Plan>,
    /// Job folders from the clean-up screen. Ones that may have a worktree are only listed.
    jobs: Vec<Leftover>,
}

/// What a new confirmation is about.
#[derive(Default)]
pub(super) struct Held {
    pub plan: Plan,
    pub offered: removal::Plan,
    pub remove: Option<removal::Plan>,
    pub jobs: Vec<Leftover>,
    pub countdown: Countdown,
}

pub(super) fn refused(message: &str) -> Value {
    json!({ "ok": false, "message": sanitize(message) })
}

fn name_in(sessions: &[Session], id: &str) -> String {
    sessions.iter().find(|s| s.id == id).map_or_else(|| sanitize(id), |s| sanitize(&s.display_name()))
}

fn tone(t: Tone) -> &'static str {
    match t {
        Tone::Plain => "plain",
        Tone::Bold => "bold",
        Tone::Warn => "warn",
        Tone::Danger => "danger",
    }
}

/// What deleting these would do, leaving out any just resumed.
pub(super) fn plan_for(state: &State, sessions: &[Session], ids: &[String]) -> Plan {
    let picked = |s: &&Session| ids.contains(&s.id);
    let (targets, unsupported): (Vec<&Session>, Vec<&Session>) = sessions.iter().filter(picked).partition(|s| s.source.features().delete);
    let starting_now = lock(&state.starting);
    let (starting, targets): (Vec<&Session>, Vec<&Session>) = targets.into_iter().partition(|s| starting_now.recent(&s.id));
    drop(starting_now);
    let targets: Vec<Session> = targets.into_iter().cloned().collect();
    let mut plan = if targets.is_empty() { Plan::default() } else { (state.effects.plan_delete)(&targets) };
    plan.skipped.extend(starting.into_iter().map(|s| Skipped { id: s.id.clone(), reason: "it was just resumed".into() }));
    plan.skipped.extend(unsupported.into_iter().map(|s| Skipped { id: s.id.clone(), reason: format!("ccm can't delete {} conversations yet", s.source.label()) }));
    plan
}

pub fn plan(state: &State, body: &Value) -> Value {
    let ids: Vec<String> = body["ids"].as_array().map_or(Vec::new(), |list| list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
    let sessions = Arc::clone(&read(&state.live).sessions);
    let plan = plan_for(state, &sessions, &ids);
    if plan.items.is_empty() {
        let reason = plan.skipped.first().map_or("nothing to delete", |s| s.reason.as_str());
        return refused(&format!("Can't delete: {reason}."));
    }
    let deleting: Vec<String> = plan.items.iter().map(|i| i.id.clone()).collect();
    let offered = (state.effects.offer)(&deleting, &sessions);
    hold(state, &sessions, Held { plan, offered, ..Held::default() }, |_| {})
}

/// Keeps a confirmation for its answer, replacing any older one, and says what's in it.
pub(super) fn hold(state: &State, sessions: &[Session], held: Held, extra: impl FnOnce(&mut Value)) -> Value {
    let id = match ccm_core::resume::new_session_id() {
        Ok(id) => id,
        Err(e) => return refused(&format!("Couldn't start the delete: {e}")),
    };
    let Held { plan, offered, remove, jobs, countdown } = held;
    let mut reply = describe(state, sessions, &id, &plan, &offered);
    reply["seconds_left"] = json!(countdown.seconds_left());
    extra(&mut reply);
    *lock(&state.pending_delete) = Some(Pending { id, plan, offered, include_worktrees: false, countdown, remove, jobs });
    reply
}

pub(super) fn toned(lines: Vec<(String, Tone)>) -> Vec<Value> {
    lines.into_iter().filter(|(text, _)| !text.is_empty()).map(|(text, t)| json!({ "text": text, "tone": tone(t) })).collect()
}

/// What goes with each worktree of a plan, then the ones that can't go and why.
pub(super) fn removal_lines(state: &State, sessions: &[Session], plan: &removal::Plan) -> Vec<Value> {
    let mut lines: Vec<(String, Tone)> = plan.items.iter().flat_map(|item| planned_lines(item, sessions, &state.home)).collect();
    lines.extend(plan.refused.iter().map(|r| (format!("Skipping {}: {}", sanitize(&short_path(&r.path.to_string_lossy(), &state.home)), sanitize(&r.reason)), Tone::Plain)));
    toned(lines)
}

/// Lets a test act as if the wait has run out.
#[cfg(test)]
pub(super) fn run_out(state: &State) {
    lock(&state.pending_delete).as_mut().unwrap().countdown.run_out();
}

/// One line each with a short ID, so a long or repeated name can't pass for other rows.
fn describe(state: &State, sessions: &[Session], id: &str, plan: &Plan, offered: &removal::Plan) -> Value {
    let conversations: Vec<Value> = plan
        .items
        .iter()
        .map(|i| json!({ "name": name_in(sessions, &i.id), "short": &i.id[..i.id.len().min(8)], "bytes": i.bytes }))
        .collect();
    let skipped: Vec<String> = plan.skipped.iter().map(|s| format!("Skipping {}: {}", name_in(sessions, &s.id), sanitize(&s.reason))).collect();
    let warnings: Vec<String> = plan.warnings.iter().map(|w| sanitize(w)).collect();
    let notes: Vec<String> = plan.notes.iter().map(|n| sanitize(&n.name())).collect();
    let worktrees = toned(
        offered
            .items
            .iter()
            .flat_map(|item| {
                // Only the conversations being deleted worked there, and they're listed above.
                let item = removal::Planned { links: Vec::new(), ..item.clone() };
                planned_lines(&item, sessions, &state.home)
            })
            .collect(),
    );
    let stays: Vec<String> = offered.refused.iter().map(|r| format!("{} stays: {}", sanitize(&short_path(&r.path.to_string_lossy(), &state.home)), sanitize(&r.reason))).collect();
    json!({
        "ok": true,
        "plan_id": id,
        "kind": "delete",
        "head": [],
        "from": [],
        "removing": [],
        "removing_count": 0,
        "jobs": [],
        "job_count": 0,
        "kept_jobs": 0,
        "rm_commands": null,
        "nothing": false,
        "conversations": conversations,
        "skipped": skipped,
        "warnings": warnings,
        "notes": notes,
        "bytes": {
            "conversations": plan.bytes(),
            "notes": plan.note_bytes(),
            "worktrees": offered.items.iter().map(|i| i.bytes).sum::<u64>(),
            "removing": 0,
            "jobs": 0,
        },
        "worktree_count": offered.items.len(),
        "worktrees": worktrees,
        "stays": stays,
    })
}

fn is_plan(pending: &Pending, body: &Value) -> bool {
    body["plan_id"].as_str() == Some(pending.id.as_str())
}

/// Adds the offered worktrees, or leaves them out. Either starts the countdown afresh.
pub fn toggle_worktrees(state: &State, body: &Value) -> Value {
    let mut pending = lock(&state.pending_delete);
    let Some(p) = pending.as_mut().filter(|p| is_plan(p, body)) else { return refused(OUT_OF_DATE) };
    p.include_worktrees = body["include"].as_bool().unwrap_or(false) && !p.offered.items.is_empty();
    p.countdown = Countdown::new(p.include_worktrees && p.offered.items.iter().any(|i| i.unpushed > 0));
    json!({ "ok": true, "include": p.include_worktrees, "seconds_left": p.countdown.seconds_left() })
}

pub fn cancel(state: &State, body: &Value) -> Value {
    let mut pending = lock(&state.pending_delete);
    if pending.as_ref().is_some_and(|p| is_plan(p, body)) {
        *pending = None;
    }
    json!({ "ok": true })
}

/// Carries out the waiting plan, whose every item is checked again as it goes.
pub fn confirm(state: &State, body: &Value) -> Value {
    // Held to the end, so two deletes can't run at once.
    let mut guard = lock(&state.pending_delete);
    let Some(p) = guard.as_mut().filter(|p| is_plan(p, body)) else { return refused(OUT_OF_DATE) };
    if body["worktrees"].as_bool().unwrap_or(false) != p.include_worktrees {
        return refused(OUT_OF_DATE);
    }
    if p.countdown.seconds_left().is_some() {
        p.countdown.restart();
        return json!({ "ok": false, "restarted": true, "seconds_left": p.countdown.seconds_left() });
    }
    let Some(Pending { plan, offered, include_worktrees, remove, jobs, .. }) = guard.take() else { return refused(OUT_OF_DATE) };
    let notes = body["notes"].as_bool().unwrap_or(false) && !plan.notes.is_empty();

    let mut summaries = Vec::new();
    let mut lines = Vec::new();
    let mut deleted = Vec::new();
    if plan.items.is_empty() {
        let sessions = Arc::clone(&read(&state.live).sessions);
        lines.extend(plan.skipped.iter().map(|s| format!("Skipped {}: {}", name_in(&sessions, &s.id), s.reason)));
    } else {
        match (state.effects.delete)(&plan, notes) {
            Ok(outcome) => {
                let sessions = Arc::clone(&read(&state.live).sessions);
                let (summary, more) = delete_report(&plan, notes, &outcome, |id| name_in(&sessions, id));
                summaries.push(summary);
                lines.extend(more);
                let mut live = write(&state.live);
                Arc::make_mut(&mut live.sessions).retain(|s| !outcome.deleted.contains(&s.id));
                live.version += 1;
                deleted = outcome.deleted;
            }
            Err(e) => lines.push(sanitize(&e)),
        }
    }
    if let Some(going) = remove.or(include_worktrees.then_some(offered)) {
        // After the conversations, so what's still here is what a worktree is checked against.
        let sessions = Arc::clone(&read(&state.live).sessions);
        let outcome = if going.items.is_empty() { removal::Outcome::default() } else { (state.effects.remove)(&going, &sessions) };
        lock(&state.worktrees).forget(&outcome.removed, &sessions);
        let (summary, more) = removal_report(&going, &outcome);
        if !going.items.is_empty() {
            summaries.push(summary);
        }
        lines.extend(more);
    }
    let dirs: Vec<PathBuf> = jobs.iter().filter(|j| !j.worktree).map(|j| j.dir.clone()).collect();
    if !dirs.is_empty() {
        let (summary, more) = jobs_report(&(state.effects.remove_jobs)(&dirs));
        summaries.push(summary);
        lines.extend(more);
        let found = (state.effects.leftovers)();
        *lock(&state.leftovers) = Some(found);
    }
    json!({ "ok": true, "summary": summaries.join(" "), "lines": lines, "deleted": deleted })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use ccm_core::delete::{Item, Note, Outcome};
    use ccm_core::model::Source;
    use ccm_core::removal::{Planned, Refused, Target};

    use super::super::list::tests::session;
    use super::*;

    const A: &str = "11111111-2222-4333-8444-555555555555";
    const B: &str = "21111111-2222-4333-8444-555555555555";

    type Calls = Arc<Mutex<Vec<String>>>;

    fn item(id: &str) -> Item {
        Item { id: id.into(), source: Source::ClaudeCode, side: Vec::new(), transcripts: vec![PathBuf::from(format!("/x/{id}.jsonl"))], bytes: 2048 }
    }

    fn worktree(unpushed: u64) -> Planned {
        Planned {
            target: Target { path: "/home/me/wt".into(), repo: "/home/me/repo".into() },
            branch: Some("feature".into()),
            gone: false,
            ignored: Vec::new(),
            ignored_bytes: 0,
            ignored_more: false,
            links: Vec::new(),
            unpushed,
            has_remote: false,
            upstream_gone: false,
            bytes: 4096,
        }
    }

    /// Plans the conversations asked for, writing down anything carried out.
    fn state(dir: &Path, sessions: Vec<Session>, offered: removal::Plan) -> (Arc<State>, Calls) {
        let calls: Calls = Arc::default();
        let mut made = State::for_tests(dir, sessions);
        let s = Arc::get_mut(&mut made).unwrap();
        s.effects.plan_delete = Box::new(|sessions| Plan {
            items: sessions.iter().map(|x| item(&x.id)).collect(),
            notes: vec![Note { path: "/x/memory/note.md".into(), bytes: 100 }],
            skipped: Vec::new(),
            warnings: Vec::new(),
        });
        s.effects.offer = Box::new(move |_, _| offered.clone());
        let log = Arc::clone(&calls);
        s.effects.delete = Box::new(move |plan, notes| {
            log.lock().unwrap().push(format!("delete {} notes={notes}", plan.items.len()));
            Ok(Outcome { deleted: plan.items.iter().map(|i| i.id.clone()).collect(), ..Outcome::default() })
        });
        let log = Arc::clone(&calls);
        s.effects.remove = Box::new(move |plan, sessions| {
            log.lock().unwrap().push(format!("remove {} with {} left", plan.items.len(), sessions.len()));
            removal::Outcome { removed: plan.items.iter().map(|i| i.target.path.clone()).collect(), ..removal::Outcome::default() }
        });
        (made, calls)
    }

    fn offered(unpushed: u64) -> removal::Plan {
        removal::Plan { items: vec![worktree(unpushed)], refused: vec![Refused { path: "/home/me/other".into(), reason: "it has changes".into() }], unused_only: true }
    }

    fn planned(state: &State, ids: &[&str]) -> (Value, String) {
        let out = plan(state, &json!({ "ids": ids }));
        let id = out["plan_id"].as_str().unwrap_or_default().to_string();
        (out, id)
    }

    #[test]
    fn a_plan_changes_nothing_and_says_what_would_go() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A), session(B)], offered(0));
        let (out, _) = planned(&state, &[A]);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(out["conversations"].as_array().unwrap().len(), 1);
        assert_eq!(out["conversations"][0]["name"], format!("name {A}"));
        assert_eq!(out["conversations"][0]["short"], "11111111");
        assert_eq!(out["notes"][0], "note.md");
        assert_eq!(out["bytes"]["conversations"], 2048);
        assert_eq!(out["bytes"]["worktrees"], 4096);
        assert_eq!(out["worktrees"][0]["text"], "~/wt  feature");
        assert_eq!(out["stays"][0], "~/other stays: it has changes");
    }

    #[test]
    fn confirming_deletes_keeps_notes_and_leaves_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A), session(B)], offered(0));
        let (_, id) = planned(&state, &[A]);
        let out = confirm(&state, &json!({ "plan_id": id }));
        assert_eq!(out["summary"], "Deleted 1 conversation, freed 2K.");
        assert_eq!(out["deleted"][0], A);
        assert_eq!(*calls.lock().unwrap(), ["delete 1 notes=false"]);
        assert_eq!(read(&state.live).sessions.len(), 1);
    }

    #[test]
    fn notes_go_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A)], removal::Plan::default());
        let (_, id) = planned(&state, &[A]);
        confirm(&state, &json!({ "plan_id": id, "notes": true }));
        assert_eq!(*calls.lock().unwrap(), ["delete 1 notes=true"]);
    }

    #[test]
    fn the_page_cannot_add_to_a_plan() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A), session(B)], removal::Plan::default());
        let (_, id) = planned(&state, &[A]);
        confirm(&state, &json!({ "plan_id": id, "ids": [A, B], "items": [B] }));
        assert_eq!(*calls.lock().unwrap(), ["delete 1 notes=false"]);
        assert_eq!(read(&state.live).sessions[0].id, B);
    }

    #[test]
    fn a_plan_works_once_and_only_by_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A)], removal::Plan::default());
        let (_, id) = planned(&state, &[A]);
        assert_eq!(confirm(&state, &json!({ "plan_id": "wrong" }))["message"], OUT_OF_DATE);
        assert_eq!(confirm(&state, &json!({}))["message"], OUT_OF_DATE);
        assert!(calls.lock().unwrap().is_empty());
        confirm(&state, &json!({ "plan_id": id }));
        assert_eq!(confirm(&state, &json!({ "plan_id": id }))["message"], OUT_OF_DATE);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_newer_popup_replaces_an_older_one() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A), session(B)], removal::Plan::default());
        let (_, old) = planned(&state, &[A]);
        planned(&state, &[B]);
        assert_eq!(confirm(&state, &json!({ "plan_id": old }))["message"], OUT_OF_DATE);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_cancelled_plan_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A)], removal::Plan::default());
        let (_, id) = planned(&state, &[A]);
        cancel(&state, &json!({ "plan_id": id }));
        assert_eq!(confirm(&state, &json!({ "plan_id": id }))["message"], OUT_OF_DATE);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn worktrees_go_after_the_conversations_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A), session(B)], offered(0));
        let (_, id) = planned(&state, &[A]);
        assert!(toggle_worktrees(&state, &json!({ "plan_id": id, "include": true }))["seconds_left"].is_null());
        let out = confirm(&state, &json!({ "plan_id": id, "worktrees": true }));
        assert_eq!(*calls.lock().unwrap(), ["delete 1 notes=false", "remove 1 with 1 left"]);
        assert_eq!(out["summary"], "Deleted 1 conversation, freed 2K. Removed 1 worktree. Its branch is kept.");
    }

    #[test]
    fn a_worktree_not_asked_for_cannot_be_slipped_in() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A)], offered(0));
        let (_, id) = planned(&state, &[A]);
        assert_eq!(confirm(&state, &json!({ "plan_id": id, "worktrees": true }))["message"], OUT_OF_DATE);
        assert!(calls.lock().unwrap().is_empty());
        confirm(&state, &json!({ "plan_id": id, "worktrees": false }));
        assert_eq!(*calls.lock().unwrap(), ["delete 1 notes=false"]);
    }

    #[test]
    fn unpushed_commits_lock_the_delete_here_and_trying_early_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A)], offered(2));
        let (out, id) = planned(&state, &[A]);
        assert_eq!(out["worktrees"][1]["text"], "  No remote, so 2 commits are only on this machine.");
        assert_eq!(out["worktrees"][1]["tone"], "danger");
        assert_eq!(toggle_worktrees(&state, &json!({ "plan_id": id, "include": true }))["seconds_left"], 5);
        let early = confirm(&state, &json!({ "plan_id": id, "worktrees": true }));
        assert_eq!((early["ok"].as_bool(), early["restarted"].as_bool()), (Some(false), Some(true)));
        assert_eq!(early["seconds_left"], 5);
        assert!(calls.lock().unwrap().is_empty());
        run_out(&state);
        assert_eq!(confirm(&state, &json!({ "plan_id": id, "worktrees": true }))["ok"], true);
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn leaving_the_worktrees_out_ends_the_countdown() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A)], offered(2));
        let (_, id) = planned(&state, &[A]);
        toggle_worktrees(&state, &json!({ "plan_id": id, "include": true }));
        assert!(toggle_worktrees(&state, &json!({ "plan_id": id, "include": false }))["seconds_left"].is_null());
        assert_eq!(confirm(&state, &json!({ "plan_id": id }))["ok"], true);
        assert_eq!(*calls.lock().unwrap(), ["delete 1 notes=false"]);
    }

    #[test]
    fn worktrees_cannot_be_included_when_none_are_offered() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![session(A)], removal::Plan::default());
        let (_, id) = planned(&state, &[A]);
        assert_eq!(toggle_worktrees(&state, &json!({ "plan_id": id, "include": true }))["include"], false);
    }

    #[test]
    fn just_resumed_and_nothing_left_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(A);
        s.folder = Some(dir.path().display().to_string());
        let (state, calls) = state(dir.path(), vec![s, session(B)], removal::Plan::default());
        super::super::actions::resume(&state, &json!({ "id": A }));
        assert_eq!(plan(&state, &json!({ "ids": [A] }))["message"], "Can't delete: it was just resumed.");
        let (out, _) = planned(&state, &[A, B]);
        assert_eq!(out["skipped"][0], format!("Skipping name {A}: it was just resumed"));
        assert_eq!(out["conversations"].as_array().unwrap().len(), 1);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_ids_plan_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![session(A)], removal::Plan::default());
        assert_eq!(plan(&state, &json!({ "ids": [B] }))["message"], "Can't delete: nothing to delete.");
        assert_eq!(plan(&state, &json!({}))["message"], "Can't delete: nothing to delete.");
    }

    #[test]
    fn what_a_delete_could_not_do_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (mut made, _) = state(dir.path(), vec![session(A), session(B)], removal::Plan::default());
        let s = Arc::get_mut(&mut made).unwrap();
        s.effects.delete = Box::new(|_, _| {
            Ok(Outcome { deleted: vec![A.into()], skipped: vec![Skipped { id: B.into(), reason: "it became open".into() }], failures: vec![ccm_core::delete::Failure { path: "/x/f".into(), error: "denied".into() }] })
        });
        let (_, id) = planned(&made, &[A, B]);
        let out = confirm(&made, &json!({ "plan_id": id }));
        assert_eq!(out["lines"][0], "Not removed: /x/f (denied)");
        assert_eq!(out["lines"][1], format!("Skipped name {B}: it became open"));
        assert_eq!(read(&made.live).sessions.len(), 1, "only what really went leaves the list");
    }

    #[test]
    fn a_delete_that_fails_outright_leaves_the_worktrees_checked_as_usual() {
        let dir = tempfile::tempdir().unwrap();
        let (mut made, _) = state(dir.path(), vec![session(A)], removal::Plan::default());
        Arc::get_mut(&mut made).unwrap().effects.delete = Box::new(|_, _| Err("locked".into()));
        let (_, id) = planned(&made, &[A]);
        let out = confirm(&made, &json!({ "plan_id": id }));
        assert_eq!(out["lines"][0], "locked");
        assert_eq!(read(&made.live).sessions.len(), 1);
    }
}
