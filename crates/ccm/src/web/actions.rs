//! What the page can start or change. The page only ever names a conversation: every folder and
//! path comes from what `ccm` read itself, and every check is made again here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ccm_core::delete;
use ccm_core::jobs::{self, Leftover};
use ccm_core::labels::{self, Change, Labels};
use ccm_core::format::{sanitize, short_path};
use ccm_core::model::{Session, Source};
use ccm_core::removal::{self, Target};
use ccm_core::resume::{decide, decide_fork, Decision, Focus, Fork, FOCUS_NOT_CONTROLLED};
use ccm_core::tools::Features;
use serde_json::{json, Value};

use super::{lock, read, write, State};

/// A resumed session takes a moment to register as open. Until then, block a second launch.
const COOLDOWN: Duration = Duration::from_secs(20);
const NOTHING_TO_REOPEN: &str = "Nothing to reopen. Crash information needs ccm setup, and only covers sessions started after it.";

type ResumeFn = Box<dyn Fn(&str, Source, Option<&Fork>, &Path, Focus) -> Result<String, String> + Send + Sync>;
type ReadFocusFn = Box<dyn Fn() -> bool + Send + Sync>;
type SaveFocusFn = Box<dyn Fn(bool) -> Result<(), String> + Send + Sync>;
type RenameFn = Box<dyn Fn(&str, Source, &Path, &str) -> Result<String, String> + Send + Sync>;
type DismissFn = Box<dyn Fn(&[String]) -> Result<(), String> + Send + Sync>;
type PlanDeleteFn = Box<dyn Fn(&[Session]) -> delete::Plan + Send + Sync>;
type DeleteFn = Box<dyn Fn(&delete::Plan, bool) -> Result<delete::Outcome, String> + Send + Sync>;
type OfferFn = Box<dyn Fn(&[String], &[Session]) -> removal::Plan + Send + Sync>;
type RemoveFn = Box<dyn Fn(&removal::Plan, &[Session]) -> removal::Outcome + Send + Sync>;
type ScanFn = Box<dyn Fn(&[String], bool) -> crate::ScanResult + Send + Sync>;
type PlanRemoveFn = Box<dyn Fn(&[Target], &[Session]) -> removal::Plan + Send + Sync>;
type LeftoversFn = Box<dyn Fn() -> Vec<Leftover> + Send + Sync>;
type RemoveJobsFn = Box<dyn Fn(&[PathBuf]) -> jobs::Outcome + Send + Sync>;
type ReadLabelsFn = Box<dyn Fn() -> Result<Labels, String> + Send + Sync>;
type LabelFn = Box<dyn Fn(&[String], &Change) -> Result<Labels, String> + Send + Sync>;

/// What actually touches the system. Tests swap these.
pub struct Effects {
    pub resume: ResumeFn,
    pub read_focus: ReadFocusFn,
    pub save_focus: SaveFocusFn,
    /// False where nothing can steer the focus, so the setting would do nothing.
    pub controls_focus: bool,
    pub rename: RenameFn,
    pub dismiss: DismissFn,
    pub plan_delete: PlanDeleteFn,
    pub delete: DeleteFn,
    pub offer: OfferFn,
    pub remove: RemoveFn,
    pub scan: ScanFn,
    pub plan_remove: PlanRemoveFn,
    pub leftovers: LeftoversFn,
    pub remove_jobs: RemoveJobsFn,
    pub read_labels: ReadLabelsFn,
    /// Changes the pins or tags of these conversations, and returns them all as saved.
    pub change_labels: LabelFn,
}

/// Conversations just started, as they don't show as open at once.
#[derive(Default)]
pub struct Starting(HashMap<String, Instant>);

impl Starting {
    pub(super) fn recent(&self, key: &str) -> bool {
        self.0.get(key).is_some_and(|t| t.elapsed() < COOLDOWN)
    }

    fn note(&mut self, key: String) {
        self.0.retain(|_, t| t.elapsed() < COOLDOWN);
        self.0.insert(key, Instant::now());
    }
}

fn fork_key(id: &str) -> String {
    format!("fork:{id}")
}

fn done(message: &str) -> Value {
    json!({ "ok": true, "message": sanitize(message) })
}

fn refused(message: &str) -> Value {
    json!({ "ok": false, "message": sanitize(message) })
}

fn find(state: &State, id: &str) -> Option<Session> {
    read(&state.live).sessions.iter().find(|s| s.id == id).cloned()
}

fn unsupported(s: &Session, what: &str, supported: fn(Features) -> bool) -> Option<Value> {
    (!supported(s.source.features())).then(|| refused(&format!("ccm can't {what} {} conversations yet.", s.source.label())))
}

/// Which buttons work for this conversation right now.
pub fn abilities(state: &State, s: &Session) -> Value {
    let features = s.source.features();
    let starting = lock(&state.starting);
    let just_resumed = starting.recent(&s.id);
    json!({
        "resume": features.resume && !s.open && !just_resumed,
        "fork": features.fork,
        "rename": features.rename && !s.open && !just_resumed,
        "delete": features.delete && !s.open && !just_resumed,
    })
}

/// Starts it, or a fork of it. A folder that's gone is only recreated when the page asks after
/// being told, and it's always the folder `ccm` read itself.
pub fn resume(state: &State, body: &Value) -> Value {
    let id = body["id"].as_str().unwrap_or_default();
    let fork = body["fork"].as_bool().unwrap_or(false);
    let recreate = body["recreate"].as_bool().unwrap_or(false);
    let Some(s) = find(state, id) else { return refused("That conversation is gone.") };
    let what = if fork { "fork" } else { "resume" };
    let check: fn(Features) -> bool = if fork { |f| f.fork } else { |f| f.resume };
    if let Some(no) = unsupported(&s, what, check) {
        return no;
    }
    // Held through the launch, so a double click can't start two.
    let mut starting = lock(&state.starting);
    let key = if fork { fork_key(id) } else { id.to_string() };
    if starting.recent(&key) {
        return refused(if fork { "A fork of this conversation is already starting." } else { "Already starting. It will show as open in a moment." });
    }
    let decision = if fork { decide_fork(&s) } else { decide(&s) };
    let folder = match decision {
        Decision::Go(folder) => folder,
        Decision::FolderMissing(folder) if recreate => {
            if let Err(e) = std::fs::create_dir_all(&folder) {
                return refused(&format!("Couldn't create {}: {e}", folder.display()));
            }
            folder
        }
        Decision::FolderMissing(folder) => return json!({ "ok": false, "ask_folder": sanitize(&folder.display().to_string()) }),
        other => return refused(other.refusal().unwrap_or("It can't be started.")),
    };
    if fork { start_fork(state, &s, &folder, &mut starting, key) } else { start_resume(state, &s, &folder, &mut starting, key) }
}

fn failed(s: &Session, error: &str) -> Value {
    json!({ "ok": false, "message": sanitize(error), "command": ccm_core::resume::copy_command(s) })
}

fn start_resume(state: &State, s: &Session, folder: &Path, starting: &mut Starting, key: String) -> Value {
    match (state.effects.resume)(&s.id, s.source, None, folder, Focus::AsSet) {
        Ok(message) => {
            starting.note(key);
            done(&message)
        }
        Err(e) => failed(s, &e),
    }
}

fn start_fork(state: &State, s: &Session, folder: &Path, starting: &mut Starting, key: String) -> Value {
    let fork = match Fork::of(&s.name) {
        Ok(fork) => fork,
        Err(e) => return refused(&format!("Couldn't make an ID for the fork: {e}")),
    };
    match (state.effects.resume)(&s.id, s.source, Some(&fork), folder, Focus::AsSet) {
        Ok(message) => {
            starting.note(key);
            // Codex picks the fork's ID itself.
            let id = (s.source == Source::ClaudeCode).then_some(fork.id);
            json!({ "ok": true, "message": sanitize(&format!("Started a fork. {message} It shows here once {} saves it.", s.source.label())), "fork_id": id })
        }
        Err(e) => failed(s, &e),
    }
}

pub fn rename(state: &State, body: &Value) -> Value {
    let id = body["id"].as_str().unwrap_or_default();
    let name = body["name"].as_str().unwrap_or_default();
    let Some(s) = find(state, id) else { return refused("That conversation is gone.") };
    if let Some(no) = unsupported(&s, "rename", |f| f.rename) {
        return no;
    }
    if s.open || lock(&state.starting).recent(id) {
        return refused("This conversation is open, so rename it from there with /rename.");
    }
    match (state.effects.rename)(id, s.source, Path::new(&s.transcript_path), name) {
        Ok(saved) => {
            let message = format!("Renamed to {saved}.");
            let mut live = write(&state.live);
            if let Some(session) = Arc::make_mut(&mut live.sessions).iter_mut().find(|x| x.id == id) {
                session.name = saved;
            }
            live.version += 1;
            done(&message)
        }
        Err(e) => refused(&e),
    }
}

/// The conversations of these IDs that are still there, in the order asked for.
fn found(state: &State, ids: &[String]) -> Vec<Session> {
    let sessions = Arc::clone(&read(&state.live).sessions);
    ids.iter().filter_map(|id| sessions.iter().find(|s| &s.id == id).cloned()).collect()
}

fn change_labels(state: &State, ids: &[String], change: &Change, message: &str) -> Value {
    match (state.effects.change_labels)(ids, change) {
        Ok(labels) => {
            state.relabel(labels);
            done(message)
        }
        Err(e) => refused(&e),
    }
}

/// Pins them all, unless they're all pinned already, which unpins them.
pub fn pin(state: &State, body: &Value) -> Value {
    let targets = found(state, &ids(&body["ids"]));
    if targets.is_empty() {
        return refused("That conversation is gone.");
    }
    let pin = targets.iter().any(|s| !s.pinned);
    let ids: Vec<String> = targets.iter().map(|s| s.id.clone()).collect();
    change_labels(state, &ids, &Change::Pin(pin), &crate::wording::pinned(pin, ids.len()))
}

/// Saves typed tags. `before` holds the tags the page showed, which they all shared: only those
/// can be removed, so a tag added elsewhere meanwhile stays.
pub fn tags(state: &State, body: &Value) -> Value {
    let targets = found(state, &ids(&body["ids"]));
    if targets.is_empty() {
        return refused("That conversation is gone.");
    }
    let (before, after) = match (labels::parse(body["before"].as_str().unwrap_or_default()), labels::parse(body["tags"].as_str().unwrap_or_default())) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(e), _) | (_, Err(e)) => return refused(&e),
    };
    let ids: Vec<String> = targets.iter().map(|s| s.id.clone()).collect();
    change_labels(state, &ids, &labels::edit(&before, &after), &crate::wording::tags_saved(&after, ids.len()))
}

/// "Focus new conversation" as the page shows it. `note` says why it can't work here.
pub fn focus_state(state: &State) -> Value {
    json!({ "on": (state.effects.read_focus)(), "note": (!state.effects.controls_focus).then_some(FOCUS_NOT_CONTROLLED) })
}

pub fn set_focus(state: &State, body: &Value) -> Value {
    if !state.effects.controls_focus {
        return refused(FOCUS_NOT_CONTROLLED);
    }
    let Some(on) = body["on"].as_bool() else { return refused("That isn't a setting ccm knows.") };
    match (state.effects.save_focus)(on) {
        Ok(()) => json!({ "ok": true, "on": on, "message": crate::wording::focus_saved(on) }),
        Err(e) => refused(&crate::wording::focus_not_saved(&e)),
    }
}

/// The crashed conversations, newest first. One whose folder is gone starts unticked, so a folder
/// isn't recreated without someone asking for it.
pub fn reopen_list(state: &State) -> Value {
    let sessions = Arc::clone(&read(&state.live).sessions);
    let mut crashed: Vec<&Session> = sessions.iter().filter(|s| s.crashed).collect();
    if crashed.is_empty() {
        return json!({ "message": NOTHING_TO_REOPEN, "rows": [] });
    }
    crashed.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms).then_with(|| a.id.cmp(&b.id)));
    let rows: Vec<Value> = crashed
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": sanitize(&s.display_name()),
                "folder": s.folder.as_deref().map_or("-".to_string(), |p| sanitize(&short_path(p, &state.home))),
                "missing": s.is_folder_missing(),
            })
        })
        .collect();
    json!({ "rows": rows })
}

fn reopen_one(state: &State, s: &Session) -> Result<String, String> {
    if !s.crashed {
        return Err("It's no longer crashed.".into());
    }
    let mut starting = lock(&state.starting);
    if starting.recent(&s.id) {
        return Err("It's already starting.".into());
    }
    let folder = match decide(s) {
        Decision::Go(folder) => folder,
        Decision::FolderMissing(folder) => {
            std::fs::create_dir_all(&folder).map_err(|e| format!("Couldn't create its folder: {e}"))?;
            folder
        }
        other => return Err(other.refusal().unwrap_or("It can't be started.").into()),
    };
    let message = (state.effects.resume)(&s.id, s.source, None, &folder, Focus::Stay)?;
    starting.note(s.id.clone());
    Ok(message)
}

fn ids(value: &Value) -> Vec<String> {
    value.as_array().map_or(Vec::new(), |list| list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
}

/// Resumes what's ticked the normal way, and stops showing the rest as crashed.
pub fn reopen(state: &State, body: &Value) -> Value {
    let mut lines = Vec::new();
    for id in ids(&body["ticked"]) {
        let Some(s) = find(state, &id) else {
            lines.push(format!("{}: not reopened. It's gone.", sanitize(&id)));
            continue;
        };
        let name = sanitize(&s.display_name());
        lines.push(match reopen_one(state, &s) {
            Ok(message) => format!("{name}: {}", sanitize(&message)),
            Err(e) => format!("{name}: not reopened. {}", sanitize(&e)),
        });
    }
    let crashed = |id: &String| find(state, id).is_some_and(|s| s.crashed);
    let unticked: Vec<String> = ids(&body["unticked"]).into_iter().filter(crashed).collect();
    if !unticked.is_empty() {
        match (state.effects.dismiss)(&unticked) {
            Ok(()) => {
                let mut live = write(&state.live);
                Arc::make_mut(&mut live.sessions).iter_mut().filter(|s| unticked.contains(&s.id)).for_each(|s| s.crashed = false);
                live.version += 1;
                lines.push(format!("{} unticked no longer show as crashed.", unticked.len()));
            }
            Err(e) => lines.push(format!("Unticked ones still show as crashed: {}", sanitize(&e))),
        }
    }
    json!({ "lines": lines })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::list::tests::session;
    use super::*;

    const A: &str = "11111111-2222-4333-8444-555555555555";
    const B: &str = "21111111-2222-4333-8444-555555555555";

    type Calls = Arc<Mutex<Vec<String>>>;

    /// A state whose launches and renames are only written down.
    fn state(dir: &Path, sessions: Vec<Session>, result: Result<String, String>) -> (Arc<State>, Calls) {
        let calls: Calls = Arc::default();
        let mut made = State::for_tests(dir, sessions);
        let state = Arc::get_mut(&mut made).unwrap();
        let log = Arc::clone(&calls);
        state.effects.resume = Box::new(move |id, source, fork, folder, focus| {
            let stays = if focus == Focus::Stay { " stays" } else { "" };
            log.lock().unwrap().push(format!("{} {id} {source:?} fork={} in {}{stays}", "start", fork.is_some(), folder.display()));
            result.clone()
        });
        let log = Arc::clone(&calls);
        state.effects.rename = Box::new(move |id, _, _, name| {
            log.lock().unwrap().push(format!("rename {id} {name}"));
            Ok(format!("saved {name}"))
        });
        let log = Arc::clone(&calls);
        state.effects.dismiss = Box::new(move |ids| {
            log.lock().unwrap().push(format!("dismiss {}", ids.join(",")));
            Ok(())
        });
        (made, calls)
    }

    fn fine() -> Result<String, String> {
        Ok("Opened in a new terminal.".into())
    }

    fn real(dir: &Path, id: &str) -> Session {
        let mut s = session(id);
        s.folder = Some(dir.display().to_string());
        s
    }

    #[test]
    fn resumes_in_the_folder_ccm_read_whatever_the_page_sends() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![real(dir.path(), A)], fine());
        let out = resume(&state, &json!({ "id": A, "folder": "/somewhere/else" }));
        assert_eq!(out["ok"], true);
        assert_eq!(*calls.lock().unwrap(), [format!("start {A} ClaudeCode fork=false in {}", dir.path().display())]);
    }

    #[test]
    fn a_second_resume_is_refused_while_starting() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![real(dir.path(), A)], fine());
        resume(&state, &json!({ "id": A }));
        let again = resume(&state, &json!({ "id": A }));
        assert_eq!(again["ok"], false);
        assert_eq!(again["message"], "Already starting. It will show as open in a moment.");
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_start_gives_the_command_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![real(dir.path(), A)], Err("no terminal".into()));
        let out = resume(&state, &json!({ "id": A }));
        assert_eq!(out["ok"], false);
        assert_eq!(out["message"], "no terminal");
        assert!(out["command"].as_str().unwrap().contains(A));
        resume(&state, &json!({ "id": A }));
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn an_open_conversation_is_not_resumed_but_can_be_forked() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = real(dir.path(), A);
        s.open = true;
        let (state, calls) = state(dir.path(), vec![s], fine());
        assert_eq!(resume(&state, &json!({ "id": A }))["message"], "This conversation is already open.");
        assert!(calls.lock().unwrap().is_empty());
        let forked = resume(&state, &json!({ "id": A, "fork": true }));
        assert_eq!(forked["ok"], true);
        assert!(forked["fork_id"].is_string());
        assert_eq!(resume(&state, &json!({ "id": A, "fork": true }))["message"], "A fork of this conversation is already starting.");
    }

    #[test]
    fn a_missing_folder_is_only_made_after_asking() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone/deep");
        let mut s = session(A);
        s.folder = Some(gone.display().to_string());
        s.folder_exists = false;
        let (state, calls) = state(dir.path(), vec![s], fine());
        let asked = resume(&state, &json!({ "id": A }));
        assert_eq!(asked["ask_folder"], gone.display().to_string());
        assert!(!gone.exists());
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(resume(&state, &json!({ "id": A, "recreate": true }))["ok"], true);
        assert!(gone.is_dir());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unsafe_folder_is_never_used_even_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(A);
        s.folder = Some("/tmp/../etc".into());
        s.folder_exists = false;
        let (state, calls) = state(dir.path(), vec![s], fine());
        let out = resume(&state, &json!({ "id": A, "recreate": true }));
        assert_eq!(out["message"], "The recorded folder isn't a safe path, so it won't be used.");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unknown_or_invalid_conversation_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![real(dir.path(), "not-a-uuid")], fine());
        assert_eq!(resume(&state, &json!({ "id": B }))["message"], "That conversation is gone.");
        assert_eq!(resume(&state, &json!({}))["message"], "That conversation is gone.");
        assert_eq!(resume(&state, &json!({ "id": "not-a-uuid" }))["message"], "This conversation's ID isn't valid.");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_codex_fork_has_no_id_to_wait_for() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = real(dir.path(), A);
        s.source = Source::Codex;
        let (state, _) = state(dir.path(), vec![s], fine());
        let out = resume(&state, &json!({ "id": A, "fork": true }));
        assert_eq!(out["ok"], true);
        assert!(out["fork_id"].is_null());
        assert!(out["message"].as_str().unwrap().contains("once Codex saves it"));
    }

    #[test]
    fn rename_shows_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![session(A)], fine());
        let before = read(&state.live).version;
        let out = rename(&state, &json!({ "id": A, "name": "new" }));
        assert_eq!(out["message"], "Renamed to saved new.");
        assert_eq!(calls.lock().unwrap()[0], format!("rename {A} new"));
        assert_eq!(read(&state.live).sessions[0].name, "saved new");
        assert_eq!(read(&state.live).version, before + 1);
    }

    #[test]
    fn an_open_or_just_started_conversation_is_not_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let mut open = real(dir.path(), A);
        open.open = true;
        let (state, calls) = state(dir.path(), vec![open, real(dir.path(), B)], fine());
        assert!(rename(&state, &json!({ "id": A, "name": "x" }))["message"].as_str().unwrap().contains("/rename"));
        resume(&state, &json!({ "id": B }));
        assert!(rename(&state, &json!({ "id": B, "name": "x" }))["message"].as_str().unwrap().contains("/rename"));
        assert!(calls.lock().unwrap().iter().all(|c| !c.starts_with("rename")));
    }

    #[test]
    fn pin_pins_them_all_unless_all_are_pinned_and_shows_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![session(A), session(B)], fine());
        let before = read(&state.live).version;
        let out = pin(&state, &json!({ "ids": [B] }));
        assert_eq!(out, json!({ "ok": true, "message": "Pinned. It stays at the top of the list." }));
        assert!(read(&state.live).sessions.iter().any(|s| s.id == B && s.pinned));
        assert_eq!(read(&state.live).version, before + 1);
        assert_eq!(pin(&state, &json!({ "ids": [A, B] }))["message"], "Pinned 2 conversations. They stay at the top of the list.");
        assert_eq!(pin(&state, &json!({ "ids": [A, B] }))["message"], "Unpinned 2 conversations.");
        assert!(ccm_core::labels::load(&crate::web::test_paths(dir.path())).unwrap().pinned.is_empty(), "saved in the file");
        assert_eq!(pin(&state, &json!({ "ids": ["gone"] }))["ok"], false);
    }

    #[test]
    fn tags_change_only_what_the_page_showed() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![session(A), session(B)], fine());
        let out = tags(&state, &json!({ "ids": [A], "before": "", "tags": "#Work bug" }));
        assert_eq!(out["message"], "Tags saved: #work #bug. Search for one with #.");
        // Added elsewhere after the page drew its tags, so it isn't the page's to remove.
        state.relabel((state.effects.change_labels)(&[A.to_string()], &Change::Tags { add: vec!["meanwhile".into()], remove: vec![] }).unwrap());
        tags(&state, &json!({ "ids": [A, B], "before": "#work", "tags": "#shared" }));
        let live = read(&state.live);
        let of = |id: &str| live.sessions.iter().find(|s| s.id == id).unwrap().tags.clone();
        assert_eq!(of(A), ["bug", "meanwhile", "shared"]);
        assert_eq!(of(B), ["shared"]);
        drop(live);
        let out = tags(&state, &json!({ "ids": [A], "before": "", "tags": "no!" }));
        assert_eq!(out["ok"], false);
        assert!(out["message"].as_str().unwrap().contains("can't be a tag"));
    }

    #[test]
    fn abilities_follow_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut open = real(dir.path(), A);
        open.open = true;
        let (state, _) = state(dir.path(), vec![open.clone(), real(dir.path(), B)], fine());
        assert_eq!(abilities(&state, &open), json!({ "resume": false, "fork": true, "rename": false, "delete": false }));
        let closed = find(&state, B).unwrap();
        assert_eq!(abilities(&state, &closed), json!({ "resume": true, "fork": true, "rename": true, "delete": true }));
        resume(&state, &json!({ "id": B }));
        assert_eq!(abilities(&state, &closed)["resume"], false);
    }

    fn crashed(dir: &Path, id: &str, ms: i64) -> Session {
        let mut s = real(dir, id);
        s.crashed = true;
        s.last_active_ms = Some(ms);
        s
    }

    #[test]
    fn the_reopen_list_is_newest_first_and_says_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![crashed(dir.path(), A, 1), crashed(dir.path(), B, 2), session("plain")], fine());
        let rows = reopen_list(&state);
        assert_eq!(rows["rows"].as_array().unwrap().len(), 2);
        assert_eq!(rows["rows"][0]["id"], B);
        let dir2 = tempfile::tempdir().unwrap();
        let (none, _) = super::tests::state(dir2.path(), vec![session("plain")], fine());
        assert!(reopen_list(&none)["message"].as_str().unwrap().starts_with("Nothing to reopen."));
    }

    #[test]
    fn reopen_starts_the_ticked_and_dismisses_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![crashed(dir.path(), A, 1), crashed(dir.path(), B, 2)], fine());
        let out = reopen(&state, &json!({ "ticked": [A], "unticked": [B] }));
        assert_eq!(out["lines"][0], format!("name {A}: Opened in a new terminal."));
        assert_eq!(out["lines"][1], "1 unticked no longer show as crashed.");
        let calls = calls.lock().unwrap();
        assert!(calls[0].starts_with(&format!("start {A}")));
        assert!(calls[0].ends_with(" stays"), "reopening never takes the focus: {}", calls[0]);
        assert_eq!(calls[1], format!("dismiss {B}"));
        assert!(!find(&state, B).unwrap().crashed);
    }

    #[test]
    fn reopen_leaves_alone_what_is_not_crashed() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = state(dir.path(), vec![real(dir.path(), A), real(dir.path(), B)], fine());
        let out = reopen(&state, &json!({ "ticked": [A], "unticked": [B] }));
        assert_eq!(out["lines"][0], format!("name {A}: not reopened. It's no longer crashed."));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_reopen_is_reported_for_that_one() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = state(dir.path(), vec![crashed(dir.path(), A, 1)], Err("no terminal".into()));
        let out = reopen(&state, &json!({ "ticked": [A, B] }));
        assert_eq!(out["lines"][0], format!("name {A}: not reopened. no terminal"));
        assert!(out["lines"][1].as_str().unwrap().contains("It's gone."));
    }

    #[test]
    fn the_focus_setting_is_saved_and_a_bad_value_or_no_control_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (mut made, _) = state(dir.path(), Vec::new(), fine());
        let saved: Arc<Mutex<Vec<bool>>> = Arc::default();
        let log = Arc::clone(&saved);
        Arc::get_mut(&mut made).unwrap().effects.save_focus = Box::new(move |on| {
            log.lock().unwrap().push(on);
            Ok(())
        });
        let out = set_focus(&made, &json!({ "on": false }));
        assert_eq!((out["ok"].as_bool(), out["on"].as_bool()), (Some(true), Some(false)));
        assert_eq!(out["message"], "Focus new conversation is off: focus stays on ccm.");
        assert_eq!(set_focus(&made, &json!({ "on": "no" }))["ok"], false);
        assert_eq!(*saved.lock().unwrap(), [false]);
        assert!(focus_state(&made)["note"].is_null());

        Arc::get_mut(&mut made).unwrap().effects.controls_focus = false;
        assert_eq!(set_focus(&made, &json!({ "on": true }))["message"], FOCUS_NOT_CONTROLLED);
        assert_eq!(focus_state(&made)["note"], FOCUS_NOT_CONTROLLED);
        assert_eq!(*saved.lock().unwrap(), [false], "nothing saved where it would do nothing");
    }

    #[test]
    fn a_setting_that_cant_be_saved_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let (mut made, _) = state(dir.path(), Vec::new(), fine());
        Arc::get_mut(&mut made).unwrap().effects.save_focus = Box::new(|_| Err("read-only".into()));
        assert_eq!(set_focus(&made, &json!({ "on": false }))["message"], "Couldn't save the setting: read-only");
    }
}
