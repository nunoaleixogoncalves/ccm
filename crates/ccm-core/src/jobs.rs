//! Background job folders left behind by conversations that no longer exist. Claude Code keeps
//! one per background session under `~/.claude/jobs`, and nothing removes it once its
//! conversation is deleted some other way.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::delete::{project_dirs, remove_path, size_of};
use crate::paths::Paths;
use crate::registry::live_roster;

/// A job touched more recently than this may still be starting, before its conversation is saved.
pub const QUIET_FOR: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq)]
pub struct Leftover {
    pub dir: PathBuf,
    /// The job's name as Claude Code shows it. Not sanitized.
    pub name: Option<String>,
    /// It may have a worktree, which only `claude rm` can remove safely, so `ccm` leaves it.
    pub worktree: bool,
    /// It has no state file, so Claude Code no longer knows it.
    pub no_state: bool,
    pub bytes: u64,
    /// When anything in it last changed.
    pub last_ms: Option<i64>,
}

impl Leftover {
    /// The folder's name, which is the ID `claude rm` takes.
    pub fn short(&self) -> String {
        self.dir.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    session_id: Option<String>,
    resume_session_id: Option<String>,
    bg_isolation: Option<String>,
    name: Option<String>,
}

/// Job folders whose conversation is gone, or that have no state file, that nothing has
/// touched for a day and that aren't running.
pub fn leftovers(paths: &Paths, proc_root: &Path, now: SystemTime) -> Vec<Leftover> {
    let Ok(entries) = fs::read_dir(paths.claude.join("jobs")) else { return Vec::new() };
    let projects = project_dirs(paths);
    let has_transcript = |id: &str| projects.iter().any(|p| fs::symlink_metadata(p.join(format!("{id}.jsonl"))).is_ok_and(|m| m.is_file()));
    let roster = live_roster(&paths.claude.join("daemon/roster.json"), proc_root);
    let mut out: Vec<Leftover> = entries
        .flatten()
        // A symlink is never followed, so this can't reach outside the jobs folder.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let dir = e.path();
            let short = e.file_name().to_str()?.to_owned();
            if short.is_empty() || short.starts_with('.') || roster.contains(&short) {
                return None;
            }
            let state = match fs::read(dir.join("state.json")) {
                Ok(bytes) => Some(serde_json::from_slice::<State>(&bytes).ok()?),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                // Can't tell whose it is.
                Err(_) => return None,
            };
            if let Some(state) = &state {
                let ids = [&state.session_id, &state.resume_session_id];
                if ids.iter().any(|id| id.as_deref().is_some_and(|id| has_transcript(id) || (id.len() >= 8 && roster.contains(&id[..8])))) {
                    return None;
                }
            }
            let last = newest(&dir)?;
            if now.duration_since(last).is_ok_and(|age| age < QUIET_FOR) || last > now {
                return None;
            }
            Some(Leftover {
                bytes: size_of(&dir),
                last_ms: last.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as i64),
                worktree: state.as_ref().is_some_and(|s| s.bg_isolation.as_deref() != Some("none")),
                no_state: state.is_none(),
                name: state.and_then(|s| s.name),
                dir,
            })
        })
        .collect();
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// The newest change time of a folder and everything in it, never following symlinks. `None`
/// when any of it can't be read, so a job that can't be checked isn't suggested.
fn newest(path: &Path) -> Option<SystemTime> {
    let meta = fs::symlink_metadata(path).ok()?;
    let mut latest = meta.modified().ok()?;
    if meta.is_dir() {
        for entry in fs::read_dir(path).ok()? {
            latest = latest.max(newest(&entry.ok()?.path())?);
        }
    }
    Some(latest)
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub removed: Vec<PathBuf>,
    pub bytes: u64,
    /// Folders kept, and why.
    pub kept: Vec<(PathBuf, String)>,
}

/// Removes these job folders, checking each again first. Ones that may have a worktree are kept.
pub fn remove(paths: &Paths, proc_root: &Path, dirs: &[PathBuf], now: SystemTime) -> Outcome {
    let current = leftovers(paths, proc_root, now);
    let mut outcome = Outcome::default();
    for dir in dirs {
        match current.iter().find(|l| &l.dir == dir) {
            None => outcome.kept.push((dir.clone(), "it changed, is running or has a conversation again".into())),
            Some(l) if l.worktree => outcome.kept.push((dir.clone(), "it may have a worktree. Remove it with claude rm".into())),
            Some(l) => match remove_path(&paths.claude.join("jobs"), dir) {
                Ok(()) => {
                    outcome.removed.push(dir.clone());
                    outcome.bytes += l.bytes;
                }
                Err(e) => outcome.kept.push((dir.clone(), e.to_string())),
            },
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const D: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: Paths,
        proc_root: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            claude: root.join("claude"),
            index: root.join("index.db"),
            config: root.join("config.toml"),
            claude_json: root.join("claude.json"),
            state: root.join("state"),
        };
        fs::create_dir_all(paths.claude.join("projects/-w-shop")).unwrap();
        fs::write(paths.claude.join(format!("projects/-w-shop/{A}.jsonl")), "{}\n").unwrap();
        let proc_root = root.join("proc");
        fs::create_dir_all(&proc_root).unwrap();
        Fixture { _dir: dir, paths, proc_root }
    }

    fn job(f: &Fixture, short: &str, state: Option<String>) -> PathBuf {
        let dir = f.paths.claude.join("jobs").join(short);
        fs::create_dir_all(dir.join("tmp")).unwrap();
        fs::write(dir.join("tmp/out.log"), "x".repeat(100)).unwrap();
        if let Some(state) = state {
            fs::write(dir.join("state.json"), state).unwrap();
        }
        dir
    }

    fn state(session: &str, isolation: &str) -> Option<String> {
        Some(format!(r#"{{"sessionId":"{session}","bgIsolation":"{isolation}","name":"tidy logs"}}"#))
    }

    fn later() -> SystemTime {
        SystemTime::now() + QUIET_FOR + Duration::from_secs(60)
    }

    #[test]
    fn only_quiet_jobs_without_a_conversation_are_leftovers() {
        let f = fixture();
        job(&f, "aaaaaaaa", state(A, "none"));
        let gone = job(&f, "bbbbbbbb", state(B, "none"));
        let isolated = job(&f, "cccccccc", state(C, "worktree"));
        let bare = job(&f, "dddddddd", None);
        let resumed = job(&f, "eeeeeeee", Some(format!(r#"{{"sessionId":"{D}","resumeSessionId":"{A}","bgIsolation":"none"}}"#)));
        fs::write(f.paths.claude.join("jobs/notes.txt"), "").unwrap();

        assert!(leftovers(&f.paths, &f.proc_root, SystemTime::now()).is_empty(), "recently touched jobs may still be starting");
        let found = leftovers(&f.paths, &f.proc_root, later());
        let dirs: Vec<&PathBuf> = found.iter().map(|l| &l.dir).collect();
        assert_eq!(dirs, [&gone, &isolated, &bare]);
        assert!(!dirs.contains(&&resumed), "a job of a resumed conversation that still exists is kept");
        assert_eq!((found[0].worktree, found[1].worktree, found[2].worktree), (false, true, false));
        assert!(found[2].no_state && found[0].name.as_deref() == Some("tidy logs"));
        assert!(found[0].bytes >= 100);
        assert_eq!(found[0].short(), "bbbbbbbb");
    }

    #[test]
    fn a_running_job_is_never_a_leftover() {
        let f = fixture();
        job(&f, "bbbbbbbb", state(B, "none"));
        fs::create_dir_all(f.paths.claude.join("daemon")).unwrap();
        fs::write(f.paths.claude.join("daemon/roster.json"), "not json bbbbbbbb").unwrap();
        assert!(leftovers(&f.paths, &f.proc_root, later()).is_empty());
    }

    #[test]
    fn an_unreadable_state_file_is_left_alone() {
        let f = fixture();
        job(&f, "bbbbbbbb", Some("{not json".into()));
        assert!(leftovers(&f.paths, &f.proc_root, later()).is_empty());
    }

    #[test]
    fn removing_checks_again_and_keeps_worktree_jobs() {
        let f = fixture();
        let gone = job(&f, "bbbbbbbb", state(B, "none"));
        let isolated = job(&f, "cccccccc", state(C, "worktree"));
        let back = job(&f, "dddddddd", state(D, "none"));
        let outside = f.paths.claude.join("projects");
        // The conversation reappears between listing and removing.
        fs::write(f.paths.claude.join(format!("projects/-w-shop/{D}.jsonl")), "{}\n").unwrap();

        let outcome = remove(&f.paths, &f.proc_root, &[gone.clone(), isolated.clone(), back.clone(), outside.clone()], later());
        assert_eq!(outcome.removed, std::slice::from_ref(&gone));
        assert!(outcome.bytes >= 100);
        assert!(!gone.exists() && isolated.exists() && back.exists() && outside.exists());
        let kept: Vec<&PathBuf> = outcome.kept.iter().map(|(p, _)| p).collect();
        assert_eq!(kept, [&isolated, &back, &outside]);
        assert!(outcome.kept[0].1.contains("claude rm"));
    }

    #[test]
    fn a_symlinked_job_folder_is_never_listed() {
        let f = fixture();
        let target = job(&f, "real", None);
        fs::remove_dir_all(target.join("tmp")).unwrap();
        std::os::unix::fs::symlink(&target, f.paths.claude.join("jobs/link")).unwrap();
        let found = leftovers(&f.paths, &f.proc_root, later());
        assert_eq!(found.iter().map(Leftover::short).collect::<Vec<_>>(), ["real"]);
    }
}
