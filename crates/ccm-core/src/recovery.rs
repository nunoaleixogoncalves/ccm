//! Works out what was open before a crash. Claude Code's `SessionEnd` hook fires on a clean exit
//! or a closed terminal but not on a hard kill, so a start with no end and no running process
//! means the session died. The hooks only append to `ccm`'s own log, which holds IDs and times.

use std::collections::{HashMap, HashSet};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::delete::replace_file;
use crate::paths::{is_uuid, Paths};

/// Soon after a start, the session may not be in the registry yet.
const START_GRACE_MS: i64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Start,
    End,
    /// The user chose not to reopen it, so it stops showing as crashed.
    Dismissed,
}

#[derive(Debug, Serialize, Deserialize)]
struct Event {
    event: Kind,
    id: String,
    ts: i64,
}

fn log_path(paths: &Paths) -> PathBuf {
    paths.state.join("sessions.jsonl")
}

fn windows_dir(paths: &Paths) -> PathBuf {
    paths.state.join("windows")
}

fn append(paths: &Paths, events: &[Event]) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(&paths.state)?;
    let mut text = String::new();
    for event in events {
        text.push_str(&serde_json::to_string(event)?);
        text.push('\n');
    }
    // Sessions log at the same time. An append-only handle keeps their lines whole.
    OpenOptions::new().create(true).append(true).mode(0o600).open(log_path(paths))?.write_all(text.as_bytes())
}

/// Handles the JSON Claude Code passes to the `SessionStart` and `SessionEnd` hooks.
pub fn record_hook(paths: &Paths, input: &[u8], now_ms: i64) -> io::Result<()> {
    #[derive(Deserialize)]
    struct Input {
        session_id: String,
        hook_event_name: String,
    }
    let input: Input = serde_json::from_slice(input)?;
    let event = match input.hook_event_name.as_str() {
        "SessionStart" => Kind::Start,
        "SessionEnd" => Kind::End,
        _ => return Ok(()),
    };
    if !is_uuid(&input.session_id) {
        return Ok(());
    }
    append(paths, &[Event { event, id: input.session_id, ts: now_ms }])
}

/// Handles the JSON Claude Code passes to the status line. Only writes when the size changed,
/// because this runs on every status line refresh.
pub fn record_status_line(paths: &Paths, input: &[u8]) -> io::Result<()> {
    #[derive(Deserialize)]
    struct Window {
        context_window_size: Option<u64>,
    }
    #[derive(Deserialize)]
    struct Input {
        session_id: Option<String>,
        context_window: Option<Window>,
    }
    let input: Input = serde_json::from_slice(input)?;
    let (Some(id), Some(size)) = (input.session_id, input.context_window.and_then(|w| w.context_window_size)) else {
        return Ok(());
    };
    if !is_uuid(&id) || size == 0 {
        return Ok(());
    }
    let dir = windows_dir(paths);
    let path = dir.join(&id);
    if fs::read_to_string(&path).is_ok_and(|saved| saved == size.to_string()) {
        return Ok(());
    }
    DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    replace_file(&path, size.to_string().as_bytes(), || true)
}

/// Window sizes saved by the status line.
pub fn saved_windows(paths: &Paths) -> HashMap<String, u64> {
    let Ok(entries) = fs::read_dir(windows_dir(paths)) else { return HashMap::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_str().filter(|n| is_uuid(n))?.to_owned();
            let size = fs::read_to_string(e.path()).ok()?.trim().parse().ok()?;
            Some((id, size))
        })
        .collect()
}

/// Sessions whose last event is a start, with no process running them. `open` holds the sessions
/// with a live process, and `roster` is the background job list.
pub fn crashed(paths: &Paths, open: &HashSet<String>, roster: &str, now_ms: i64) -> HashSet<String> {
    let Ok(text) = fs::read_to_string(log_path(paths)) else { return HashSet::new() };
    let mut last: HashMap<String, (Kind, i64)> = HashMap::new();
    for event in text.lines().filter_map(|l| serde_json::from_str::<Event>(l).ok()) {
        last.insert(event.id, (event.event, event.ts));
    }
    last.into_iter()
        .filter(|(id, (kind, ts))| {
            *kind == Kind::Start
                && now_ms - ts >= START_GRACE_MS
                && is_uuid(id)
                && !open.contains(id)
                && !roster.contains(&id[..8])
        })
        .map(|(id, _)| id)
        .collect()
}

/// When each conversation was last dismissed, in ms, from the log in `state`. For tools whose
/// crashes `ccm` sees some other way.
pub fn dismissals(state: &Path) -> HashMap<String, i64> {
    let Ok(text) = fs::read_to_string(state.join("sessions.jsonl")) else { return HashMap::new() };
    let mut out = HashMap::new();
    for event in text.lines().filter_map(|l| serde_json::from_str::<Event>(l).ok()) {
        if event.event == Kind::Dismissed {
            out.insert(event.id, event.ts);
        }
    }
    out
}

/// Stops these showing as crashed. The next time one of them starts, it's tracked again.
pub fn dismiss(paths: &Paths, ids: &[String], now_ms: i64) -> io::Result<()> {
    let events: Vec<Event> = ids.iter().map(|id| Event { event: Kind::Dismissed, id: id.clone(), ts: now_ms }).collect();
    append(paths, &events)
}

/// Removes what `ccm` itself holds about a deleted conversation. Its log lines hold only an ID.
pub fn forget(paths: &Paths, id: &str) {
    if is_uuid(id) {
        let _ = fs::remove_file(windows_dir(paths).join(Path::new(id)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const LATER: i64 = 1_000_000;

    fn tree() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            claude: root.join("claude"),
            index: root.join("index.db"),
            config: root.join("config.toml"),
            claude_json: root.join("claude.json"),
            state: root.join("state/ccm"),
        };
        (dir, paths)
    }

    fn hook(paths: &Paths, name: &str, id: &str, ts: i64) {
        let input = format!(r#"{{"session_id":"{id}","hook_event_name":"{name}","reason":"other","cwd":"/secret"}}"#);
        record_hook(paths, input.as_bytes(), ts).unwrap();
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn a_start_with_no_end_and_no_process_is_a_crash() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        assert_eq!(crashed(&p, &none(), "", LATER), HashSet::from([A.to_string()]));
    }

    #[test]
    fn a_clean_end_is_not_a_crash() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        hook(&p, "SessionEnd", A, 1);
        assert!(crashed(&p, &none(), "", LATER).is_empty());
    }

    #[test]
    fn a_resume_after_an_end_that_then_dies_is_a_crash() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        hook(&p, "SessionEnd", A, 1);
        hook(&p, "SessionStart", A, 2);
        assert_eq!(crashed(&p, &none(), "", LATER).len(), 1);
    }

    #[test]
    fn a_running_session_is_not_a_crash() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        assert!(crashed(&p, &HashSet::from([A.to_string()]), "", LATER).is_empty());
    }

    #[test]
    fn a_running_background_job_is_not_a_crash() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        assert!(crashed(&p, &none(), r#"{"workers":{"aaaaaaaa":{}}}"#, LATER).is_empty());
    }

    #[test]
    fn a_session_that_just_started_gets_time_to_register() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        assert!(crashed(&p, &none(), "", START_GRACE_MS - 1).is_empty());
        assert_eq!(crashed(&p, &none(), "", START_GRACE_MS).len(), 1);
    }

    #[test]
    fn dismissing_lasts_until_the_next_start() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        hook(&p, "SessionStart", B, 0);
        dismiss(&p, &[A.to_string()], 1).unwrap();
        assert_eq!(crashed(&p, &none(), "", LATER), HashSet::from([B.to_string()]));
        hook(&p, "SessionStart", A, 2);
        assert_eq!(crashed(&p, &none(), "", LATER).len(), 2);
    }

    #[test]
    fn without_a_log_nothing_is_crashed() {
        let (_d, p) = tree();
        assert!(crashed(&p, &none(), "", LATER).is_empty());
    }

    #[test]
    fn bad_lines_in_the_log_are_skipped() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        let log = log_path(&p);
        let mut text = fs::read_to_string(&log).unwrap();
        text.push_str("not json\n{\"event\":\"start\",\"id\":\"short\",\"ts\":0}\n{\"event\":\"start\",\"id\":\"é\",\"ts\":0}\n");
        fs::write(&log, text).unwrap();
        assert_eq!(crashed(&p, &none(), "", LATER), HashSet::from([A.to_string()]));
    }

    #[test]
    fn the_log_is_private_and_holds_no_folder() {
        let (_d, p) = tree();
        hook(&p, "SessionStart", A, 0);
        let log = log_path(&p);
        assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&p.state).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(!fs::read_to_string(log).unwrap().contains("secret"));
    }

    #[test]
    fn other_hook_events_and_bad_input_write_nothing() {
        let (_d, p) = tree();
        hook(&p, "Stop", A, 0);
        hook(&p, "SessionStart", "../../etc", 0);
        assert!(record_hook(&p, b"not json", 0).is_err());
        assert!(!log_path(&p).exists());
    }

    fn status(p: &Paths, id: &str, size: u64) {
        let input = format!(r#"{{"session_id":"{id}","model":{{"id":"m"}},"context_window":{{"context_window_size":{size},"used_percentage":4}}}}"#);
        record_status_line(p, input.as_bytes()).unwrap();
    }

    #[test]
    fn the_status_line_saves_the_real_window_size() {
        let (_d, p) = tree();
        status(&p, A, 1_000_000);
        status(&p, B, 200_000);
        assert_eq!(saved_windows(&p), HashMap::from([(A.to_string(), 1_000_000), (B.to_string(), 200_000)]));
        status(&p, B, 1_000_000);
        assert_eq!(saved_windows(&p)[B], 1_000_000);
    }

    #[test]
    fn an_unchanged_size_does_not_rewrite_the_file() {
        let (_d, p) = tree();
        status(&p, A, 1000);
        let file = windows_dir(&p).join(A);
        let before = fs::metadata(&file).unwrap().modified().unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(before - std::time::Duration::from_secs(60)).unwrap();
        let aged = fs::metadata(&file).unwrap().modified().unwrap();
        status(&p, A, 1000);
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), aged);
    }

    #[test]
    fn missing_fields_and_bad_ids_save_nothing() {
        let (_d, p) = tree();
        record_status_line(&p, br#"{"session_id":"x"}"#).unwrap();
        record_status_line(&p, br#"{"context_window":{"context_window_size":5}}"#).unwrap();
        status(&p, "../../evil", 5);
        status(&p, A, 0);
        assert!(saved_windows(&p).is_empty());
        assert!(!p.state.exists());
    }

    #[test]
    fn deleting_a_conversation_removes_its_saved_size() {
        let (_d, p) = tree();
        status(&p, A, 5);
        status(&p, B, 5);
        forget(&p, A);
        forget(&p, "../../etc");
        assert_eq!(saved_windows(&p).len(), 1);
    }
}
