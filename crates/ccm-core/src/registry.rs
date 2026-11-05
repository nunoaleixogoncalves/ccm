//! Finds open conversations. A registry file alone proves nothing: a hard kill leaves it behind,
//! and the PID can be reused, so the process start time must match too.
//!
//! Claude Code saves that start time in a per-platform form: field 22 of `/proc/<pid>/stat` on
//! Linux, and the output of `ps -o lstart=` elsewhere. It has to be read back the same way.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    pid: u32,
    session_id: String,
    proc_start: Option<String>,
    /// The folder the session is in now.
    cwd: Option<String>,
}

/// A running session and the folder it's in now, if its registry file says.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub id: String,
    pub cwd: Option<String>,
}

/// Running sessions with their current folders. A brand new one may have no transcript yet.
pub fn live_sessions(sessions_dir: &Path, proc_root: &Path) -> Vec<Live> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice::<Entry>(&std::fs::read(e.path()).ok()?).ok())
        .filter(|e| is_live(e, proc_root))
        .map(|e| Live { id: e.session_id, cwd: e.cwd })
        .collect()
}

pub fn open_sessions(sessions_dir: &Path, proc_root: &Path) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice::<Entry>(&std::fs::read(e.path()).ok()?).ok())
        .filter(|e| is_live(e, proc_root))
        .map(|e| e.session_id)
        .collect()
}

/// True if a running process has a registry file that can't be read. It could be any session, so
/// nothing that changes a conversation can safely go ahead.
pub fn has_unreadable_entry(sessions_dir: &Path, proc_root: &Path) -> bool {
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).any(|path| {
        let readable = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Entry>(&bytes).is_ok(),
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        };
        let pid = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok());
        !readable && pid.is_some_and(|pid| is_running(pid, proc_root))
    })
}

/// The text of the background jobs still running, to search for a session ID. The roster file
/// stays behind when its jobs die, so a job only counts if its process is alive. Anything that
/// can't be understood is kept, because a wrong "running" only blocks a delete.
pub fn live_roster(roster_file: &Path, proc_root: &Path) -> String {
    let text = std::fs::read_to_string(roster_file).unwrap_or_default();
    let Some(workers) = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|r| r.get("workers")?.as_object().cloned()) else {
        return text;
    };
    let mut live = String::new();
    for (key, worker) in workers {
        let dead = serde_json::from_value::<Entry>(worker.clone()).is_ok_and(|e| !is_live(&e, proc_root));
        if !dead {
            live.push_str(&format!("{key} {worker}\n"));
        }
    }
    live
}

fn is_running(pid: u32, proc_root: &Path) -> bool {
    if cfg!(any(target_os = "linux", test)) {
        return proc_root.join(pid.to_string()).exists();
    }
    let output = Command::new("ps").args(["-o", "pid=", "-p", &pid.to_string()]).output();
    output.map_or(true, |o| !o.stdout.is_empty())
}

fn is_live(e: &Entry, proc_root: &Path) -> bool {
    // Tests everywhere use a fake `/proc`. The `ps` form has its own test against a real process.
    if cfg!(any(target_os = "linux", test)) { is_live_in_proc(e, proc_root) } else { is_live_by_ps(e) }
}

fn is_live_in_proc(e: &Entry, proc_root: &Path) -> bool {
    let dir = proc_root.join(e.pid.to_string());
    match &e.proc_start {
        Some(expected) => proc_start(&dir).as_deref() == Some(expected),
        // Nothing to compare, so fall back to checking the process name.
        None => std::fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.contains("claude")),
    }
}

/// Field 22 of `/proc/<pid>/stat`. The program name in field 2 can contain spaces and brackets,
/// so counting starts after its closing bracket.
fn proc_start(dir: &Path) -> Option<String> {
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_ascii_whitespace().nth(19).map(str::to_owned)
}

/// If `ps` can't run at all, the session counts as open. A wrong "open" only blocks a delete; a
/// wrong "closed" could delete a conversation that's in use.
fn is_live_by_ps(e: &Entry) -> bool {
    let field = if e.proc_start.is_some() { "lstart=" } else { "comm=" };
    let output = Command::new("ps").env("LC_ALL", "C").env("TZ", "UTC").args(["-o", field, "-p", &e.pid.to_string()]).output();
    let Ok(output) = output else {
        return true;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    match &e.proc_start {
        Some(expected) => text.trim() == expected,
        None => text.contains("claude"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn stat_with_start(start: &str) -> String {
        let fields: Vec<String> = (3..=21).map(|i| i.to_string()).collect();
        format!("42 (cl aude) (x) {} {start} 23\n", fields.join(" "))
    }

    struct Fake {
        _dir: tempfile::TempDir,
        sessions: std::path::PathBuf,
        proc: std::path::PathBuf,
    }

    fn fake() -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let proc = dir.path().join("proc");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&proc).unwrap();
        Fake { _dir: dir, sessions, proc }
    }

    fn register(f: &Fake, pid: u32, id: &str, start: Option<&str>) {
        let start = start.map_or(String::new(), |s| format!(r#","procStart":"{s}""#));
        let body = format!(r#"{{"pid":{pid},"sessionId":"{id}","status":"idle"{start}}}"#);
        fs::write(f.sessions.join(format!("{pid}.json")), body).unwrap();
    }

    fn process(f: &Fake, pid: u32, start: &str, comm: &str) {
        let d = f.proc.join(pid.to_string());
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("stat"), stat_with_start(start)).unwrap();
        fs::write(d.join("comm"), comm).unwrap();
    }

    fn entry(pid: u32, proc_start: Option<&str>) -> Entry {
        Entry { pid, session_id: "s".into(), proc_start: proc_start.map(str::to_owned), cwd: None }
    }

    fn roster(f: &Fake, body: &str) -> String {
        let file = f.sessions.join("roster.json");
        fs::write(&file, body).unwrap();
        live_roster(&file, &f.proc)
    }

    #[test]
    fn a_roster_keeps_only_jobs_whose_process_is_alive() {
        let f = fake();
        process(&f, 10, "100", "claude");
        process(&f, 11, "999", "claude");
        let live = roster(
            &f,
            r#"{"workers":{
                "aaaaaaaa":{"pid":10,"procStart":"100","sessionId":"aaaaaaaa-1"},
                "bbbbbbbb":{"pid":11,"procStart":"100","sessionId":"bbbbbbbb-1","dispatch":"cccccccc-1"},
                "dddddddd":{"pid":12,"procStart":"100","sessionId":"dddddddd-1"}
            }}"#,
        );
        assert!(live.contains("aaaaaaaa"), "alive");
        assert!(!live.contains("bbbbbbbb"), "the PID was reused");
        assert!(!live.contains("cccccccc"), "only mentioned by a job that's gone");
        assert!(!live.contains("dddddddd"), "the process is gone");
    }

    #[test]
    fn a_roster_that_cannot_be_understood_counts_as_running() {
        let f = fake();
        assert!(roster(&f, "not json aaaaaaaa").contains("aaaaaaaa"));
        assert!(roster(&f, r#"{"jobs":["aaaaaaaa"]}"#).contains("aaaaaaaa"));
        assert!(roster(&f, r#"{"workers":{"aaaaaaaa":{"pid":"unknown"}}}"#).contains("aaaaaaaa"));
    }

    #[test]
    fn a_missing_roster_means_no_jobs() {
        let f = fake();
        assert!(live_roster(&f.sessions.join("none.json"), &f.proc).is_empty());
    }

    fn lstart_of(pid: u32) -> String {
        let output = Command::new("ps").env("LC_ALL", "C").env("TZ", "UTC").args(["-o", "lstart=", "-p", &pid.to_string()]).output().unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// Uses the real `ps`, so this form is covered on Linux too.
    #[test]
    fn ps_matches_the_start_time_of_a_running_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let start = lstart_of(child.id());
        assert!(!start.is_empty());
        assert!(is_live_by_ps(&entry(child.id(), Some(&start))));
        assert!(!is_live_by_ps(&entry(child.id(), Some("Thu Jan  1 00:00:00 1970"))), "a reused PID");
        assert!(!is_live_by_ps(&entry(child.id(), None)), "not a claude process");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_live_by_ps(&entry(child.id(), Some(&start))), "it has ended");
    }

    #[test]
    fn an_unreadable_entry_only_matters_while_its_process_runs() {
        let f = fake();
        assert!(!has_unreadable_entry(&f.sessions, &f.proc));
        std::fs::write(f.sessions.join("77.json"), "{ cut off").unwrap();
        assert!(!has_unreadable_entry(&f.sessions, &f.proc), "left behind by a dead process");
        process(&f, 77, "1", "claude");
        assert!(has_unreadable_entry(&f.sessions, &f.proc));
    }

    #[test]
    fn a_folder_that_cannot_be_read_counts_and_a_missing_one_does_not() {
        let f = fake();
        assert!(!has_unreadable_entry(Path::new("/nonexistent"), &f.proc));
        let file = f.proc.join("not-a-folder");
        std::fs::write(&file, "").unwrap();
        assert!(has_unreadable_entry(&file, &f.proc));
    }

    #[test]
    fn matching_process_is_open() {
        let f = fake();
        register(&f, 42, "a", Some("500"));
        process(&f, 42, "500", "claude\n");
        assert!(open_sessions(&f.sessions, &f.proc).contains("a"));
    }

    #[test]
    fn live_sessions_carry_their_current_folder() {
        let f = fake();
        fs::write(f.sessions.join("42.json"), r#"{"pid":42,"sessionId":"a","procStart":"500","cwd":"/w/x"}"#).unwrap();
        process(&f, 42, "500", "claude");
        register(&f, 43, "b", Some("1"));
        assert_eq!(live_sessions(&f.sessions, &f.proc), [Live { id: "a".into(), cwd: Some("/w/x".into()) }], "b's process is gone");
    }

    #[test]
    fn dead_process_is_not_open() {
        let f = fake();
        register(&f, 42, "a", Some("500"));
        assert!(open_sessions(&f.sessions, &f.proc).is_empty());
    }

    #[test]
    fn reused_pid_is_not_open() {
        let f = fake();
        register(&f, 42, "a", Some("500"));
        process(&f, 42, "999", "claude\n");
        assert!(open_sessions(&f.sessions, &f.proc).is_empty());
    }

    #[test]
    fn without_a_start_time_the_process_name_decides() {
        let f = fake();
        register(&f, 42, "a", None);
        register(&f, 43, "b", None);
        process(&f, 42, "1", "claude\n");
        process(&f, 43, "1", "firefox\n");
        let open = open_sessions(&f.sessions, &f.proc);
        assert!(open.contains("a") && !open.contains("b"));
    }

    #[test]
    fn unreadable_entries_and_missing_folder_are_ignored() {
        let f = fake();
        fs::write(f.sessions.join("1.json"), "not json").unwrap();
        fs::write(f.sessions.join("1.abc.key"), "x").unwrap();
        assert!(open_sessions(&f.sessions, &f.proc).is_empty());
        assert!(open_sessions(Path::new("/nonexistent"), &f.proc).is_empty());
    }
}
