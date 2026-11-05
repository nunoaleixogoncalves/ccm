//! Removes worktrees with `git worktree remove`, never with `--force` and never by deleting the
//! folder itself. The branch stays, so its commits are kept. Git refuses a worktree with changes
//! on its own, but `ccm` checks first so it can say why, and also refuses what Git would allow
//! and shouldn't: a worktree a conversation is open in, or one that's only partly known.
//!
//! A plan is built and shown first. Every worktree is checked again just before it goes.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::delete::{jobs, written_since, Job, RECENT_WRITE};
use crate::format::sanitize;
use crate::model::Session;
use crate::paths::Paths;
use crate::registry::{has_unreadable_entry, live_roster, live_sessions, Live};
use crate::worktree::{connect, git, inspect, parse_list, run, text, Link};

/// Entries counted when measuring ignored files, so a huge `node_modules` can't stall the popup.
const MAX_COUNTED: usize = 200_000;
/// Ignored entries named in the popup. The rest are only counted.
pub const NAMED_IGNORED: usize = 8;

/// A worktree to remove, as the user picked it.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    /// Its repository's main worktree, where Git is run from.
    pub repo: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub target: Target,
    pub branch: Option<String>,
    /// Its folder was already gone, so this only clears Git's record of it.
    pub gone: bool,
    /// Files and folders Git ignores, which go with the worktree, as Git names them.
    pub ignored: Vec<String>,
    pub ignored_bytes: u64,
    /// Measuring stopped early, so the size is a minimum.
    pub ignored_more: bool,
    /// Conversations that worked here.
    pub links: Vec<Link>,
    /// Commits on no remote branch, or with no remote, on no other branch. The branch stays, so
    /// they're kept, but only on this machine. The popup always warns about them.
    pub unpushed: u64,
    pub has_remote: bool,
    pub upstream_gone: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub items: Vec<Planned>,
    pub refused: Vec<Refused>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub removed: Vec<PathBuf>,
    /// Became unsafe after the plan was shown.
    pub skipped: Vec<Refused>,
    /// Git refused or failed.
    pub failures: Vec<Refused>,
}

/// What's running now. Read once per plan, and again just before each removal.
struct Busy {
    live: Vec<Live>,
    roster: String,
    unreadable: bool,
    jobs: Vec<Job>,
    own_folder: Option<PathBuf>,
}

impl Busy {
    fn read(paths: &Paths, proc_root: &Path) -> Self {
        Self {
            live: live_sessions(&paths.sessions(), proc_root),
            roster: live_roster(&paths.claude.join("daemon/roster.json"), proc_root),
            unreadable: has_unreadable_entry(&paths.sessions(), proc_root),
            jobs: jobs(paths),
            own_folder: std::env::current_dir().ok(),
        }
    }
}

/// True if the folder is the worktree or inside it, by the path as written or its real path.
fn inside(folder: &Path, root: &Path) -> bool {
    let real_root = fs::canonicalize(root).ok();
    let roots: Vec<&Path> = std::iter::once(root).chain(real_root.as_deref()).collect();
    let real = fs::canonicalize(folder).ok();
    std::iter::once(folder).chain(real.as_deref()).any(|f| roots.iter().any(|r| f.starts_with(r)))
}

fn name_of(sessions: &[Session], id: &str) -> String {
    match sessions.iter().find(|s| s.id == id) {
        Some(s) if !s.name.is_empty() => format!("\"{}\"", sanitize(&s.name)),
        _ => format!("conversation {}", &id[..id.len().min(8)]),
    }
}

/// Why a conversation or job stops this worktree going, if one does.
fn busy_reason(busy: &Busy, root: &Path, sessions: &[Session], links: &[Link]) -> Option<String> {
    if busy.unreadable {
        return Some("a running session's registry file can't be read, so one may be open in it".into());
    }
    if busy.own_folder.as_deref().is_some_and(|f| inside(f, root)) {
        return Some("ccm was started inside it".into());
    }
    for live in &busy.live {
        if live.cwd.as_deref().is_some_and(|c| inside(Path::new(c), root)) {
            return Some(format!("{} is open in it", name_of(sessions, &live.id)));
        }
        let last = sessions.iter().find(|s| s.id == live.id).and_then(Session::last_folder);
        if last.is_some_and(|f| inside(Path::new(f), root)) {
            return Some(format!("{} is open and was last working in it", name_of(sessions, &live.id)));
        }
    }
    let now = SystemTime::now();
    for link in links {
        let Some(s) = sessions.iter().find(|s| s.id == link.id) else { continue };
        if written_since(Path::new(&s.transcript_path), now, RECENT_WRITE) {
            return Some(format!("{} worked in it and was written to in the last minute, so it may still be running", name_of(sessions, &s.id)));
        }
        if s.id.len() >= 8 && busy.roster.contains(&s.id[..8]) {
            return Some(format!("{} worked in it and is running in the background", name_of(sessions, &s.id)));
        }
    }
    let job = busy.jobs.iter().find(|j| j.isolation.as_deref() != Some("none") && j.cwd.as_deref().is_some_and(|c| inside(Path::new(c), root)));
    if job.is_some() {
        return Some("a background job may own it. Remove that job with: claude rm".into());
    }
    None
}

/// Total size of regular files, never following symlinks, counting at most `budget` entries.
fn size_within(path: &Path, budget: &mut usize) -> u64 {
    if *budget == 0 {
        return 0;
    }
    *budget -= 1;
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return if meta.is_file() { meta.len() } else { 0 };
    }
    let Ok(entries) = fs::read_dir(path) else { return 0 };
    entries.flatten().map(|e| size_within(&e.path(), budget)).sum()
}

/// What Git ignores in the worktree, which its removal deletes without asking.
fn ignored(path: &Path) -> Option<(Vec<String>, u64, bool)> {
    let out = git(path, &["status", "--porcelain=v2", "-z", "--ignored=matching"]).ok()?;
    let names: Vec<String> =
        out.split(|b| *b == 0).filter_map(|f| f.strip_prefix(b"! ")).map(|n| String::from_utf8_lossy(n).into_owned()).collect();
    let mut budget = MAX_COUNTED;
    let bytes = names.iter().map(|n| size_within(&path.join(n.trim_end_matches('/')), &mut budget)).sum();
    Some((names, bytes, budget == 0))
}

/// Checks one worktree as it is now. Reads only.
fn check(target: &Target, busy: &Busy, sessions: &[Session]) -> Result<Planned, String> {
    if !target.path.is_absolute() || !target.repo.is_absolute() {
        return Err("its path isn't absolute".into());
    }
    let list = git(&target.repo, &["worktree", "list", "--porcelain", "-z"]).map_err(|_| "Git couldn't list its repository's worktrees".to_string())?;
    let list = parse_list(&list);
    let Some(w) = list.iter().find(|w| w.path == target.path) else {
        return Err("Git no longer lists it".into());
    };
    if !w.is_linked() {
        return Err("it's the repository's main worktree".into());
    }
    if w.locked {
        return Err("it's locked in Git. Unlock it with: git worktree unlock".into());
    }
    let links = connect(&list, sessions).into_iter().nth(list.iter().position(|x| x.path == w.path).unwrap_or(0)).unwrap_or_default();
    if let Some(reason) = busy_reason(busy, &w.path, sessions, &links) {
        return Err(reason);
    }
    let mut planned = Planned {
        target: target.clone(),
        branch: w.branch.clone(),
        gone: w.prunable,
        ignored: Vec::new(),
        ignored_bytes: 0,
        ignored_more: false,
        links,
        unpushed: 0,
        has_remote: false,
        upstream_gone: false,
    };
    if w.prunable {
        return Ok(planned);
    }
    let state = inspect(w).ok_or("Git couldn't read whether it has changes or unpushed commits")?;
    if state.changes > 0 {
        let plural = if state.changes == 1 { "file" } else { "files" };
        return Err(format!("it has {} uncommitted {plural}", state.changes));
    }
    if w.detached() {
        // No branch stays to hold these, so removing the worktree would strand them.
        let out = git(&w.path, &["rev-list", "--count", "HEAD", "--not", "--branches", "--tags", "--remotes"]).map_err(|_| "Git couldn't count its commits".to_string())?;
        match text(&out).parse::<u64>() {
            Ok(0) => {}
            Ok(1) => return Err("it has 1 commit that's on no branch. Create a branch for it first".into()),
            Ok(n) => return Err(format!("it has {n} commits that are on no branch. Create a branch for them first")),
            Err(_) => return Err("Git couldn't count its commits".into()),
        }
    }
    planned.unpushed = state.unpushed;
    planned.has_remote = state.has_remote;
    planned.upstream_gone = state.upstream_gone;
    if fs::symlink_metadata(w.path.join(".gitmodules")).is_ok() {
        return Err("it has submodules, which Git won't remove".into());
    }
    let (names, bytes, more) = ignored(&w.path).ok_or("Git couldn't list its ignored files")?;
    planned.ignored = names;
    planned.ignored_bytes = bytes;
    planned.ignored_more = more;
    Ok(planned)
}

/// Works out what removing these worktrees would do, without changing anything.
pub fn plan(paths: &Paths, proc_root: &Path, targets: &[Target], sessions: &[Session]) -> Plan {
    let busy = Busy::read(paths, proc_root);
    let mut plan = Plan::default();
    for target in targets {
        match check(target, &busy, sessions) {
            Ok(planned) => plan.items.push(planned),
            Err(reason) => plan.refused.push(Refused { path: target.path.clone(), reason }),
        }
    }
    plan
}

/// Removes what the plan lists, checking each worktree again first. One that changed in a way
/// that matters, including new ignored files the popup didn't show, is skipped.
pub fn execute(paths: &Paths, proc_root: &Path, plan: &Plan, sessions: &[Session]) -> Outcome {
    let mut outcome = Outcome::default();
    for item in &plan.items {
        let path = item.target.path.clone();
        let busy = Busy::read(paths, proc_root);
        let now = match check(&item.target, &busy, sessions) {
            Ok(now) => now,
            Err(reason) => {
                outcome.skipped.push(Refused { path, reason });
                continue;
            }
        };
        if now.unpushed > item.unpushed {
            outcome.skipped.push(Refused { path, reason: "it has more unpushed commits than when you confirmed".into() });
            continue;
        }
        let shown: HashSet<&String> = item.ignored.iter().collect();
        if now.ignored.iter().any(|n| !shown.contains(n)) {
            outcome.skipped.push(Refused { path, reason: "new ignored files appeared after you confirmed".into() });
            continue;
        }
        match run(&item.target.repo, &[std::ffi::OsStr::new("worktree"), "remove".as_ref(), path.as_os_str()]) {
            Ok((true, _, _)) => outcome.removed.push(path),
            Ok((false, _, err)) => {
                let message = text(&err).lines().next().unwrap_or("").trim_start_matches("fatal: ").to_owned();
                let reason = if message.is_empty() { "Git refused".to_owned() } else { format!("Git refused: {message}") };
                outcome.failures.push(Refused { path, reason });
            }
            Err(_) => outcome.failures.push(Refused { path, reason: "Git couldn't be run".into() }),
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FolderVisit, Source};
    use std::process::Command;
    use std::time::Duration;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "init.defaultBranch=main", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        repo: PathBuf,
        paths: Paths,
        proc: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(dir.path()).unwrap();
            let paths = Paths {
                claude: root.join("claude"),
                index: root.join("index.db"),
                config: root.join("config.toml"),
                claude_json: root.join("claude.json"),
                state: root.join("state"),
            };
            fs::create_dir_all(paths.sessions()).unwrap();
            let proc = root.join("proc");
            fs::create_dir_all(&proc).unwrap();
            sh(&root, &["init", "-q", "--bare", "origin.git"]);
            sh(&root, &["init", "-q", "app"]);
            let repo = root.join("app");
            fs::write(repo.join(".gitignore"), ".env\ntarget/\n").unwrap();
            sh(&repo, &["add", "."]);
            sh(&repo, &["commit", "-qm", "one"]);
            sh(&repo, &["remote", "add", "origin", "../origin.git"]);
            sh(&repo, &["push", "-q", "-u", "origin", "main"]);
            Self { _dir: dir, root, repo, paths, proc }
        }

        /// A clean worktree beside the repository, on a branch at the remote's commit.
        fn worktree(&self, name: &str) -> Target {
            sh(&self.repo, &["worktree", "add", "-q", &format!("../{name}"), "-b", name]);
            Target { path: self.root.join(name), repo: self.repo.clone() }
        }

        fn plan(&self, targets: &[Target], sessions: &[Session]) -> Plan {
            plan(&self.paths, &self.proc, targets, sessions)
        }

        fn execute(&self, plan: &Plan, sessions: &[Session]) -> Outcome {
            execute(&self.paths, &self.proc, plan, sessions)
        }

        /// A running session, as Claude Code registers one.
        fn running(&self, pid: u32, id: &str, cwd: &Path) {
            let body = format!(r#"{{"pid":{pid},"sessionId":"{id}","procStart":"500","cwd":"{}"}}"#, cwd.display());
            fs::write(self.paths.sessions().join(format!("{pid}.json")), body).unwrap();
            let d = self.proc.join(pid.to_string());
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("stat"), format!("{pid} (claude) S {} 500 1\n", vec!["0"; 18].join(" "))).unwrap();
        }

        /// A conversation's transcript, written long enough ago to count as finished.
        fn session(&self, id: &str, name: &str, folders: &[&Path]) -> Session {
            let transcript = self.root.join(format!("{id}.jsonl"));
            fs::write(&transcript, "{}").unwrap();
            let old = SystemTime::now() - Duration::from_secs(3600);
            fs::File::options().write(true).open(&transcript).unwrap().set_modified(old).unwrap();
            Session {
                id: id.into(),
                source: Source::ClaudeCode,
                transcript_path: transcript.display().to_string(),
                transcript_size: 0,
                transcript_mtime_ns: 0,
                size_bytes: 0,
                name: name.into(),
                folder: folders.first().map(|f| f.display().to_string()),
                folders: folders
                    .iter()
                    .enumerate()
                    .map(|(n, f)| FolderVisit { path: f.display().to_string(), records: 1, last_record: n as u64 + 1, last_ms: None })
                    .collect(),
                branch: None,
                model: None,
                started_at_ms: None,
                last_active_ms: None,
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
    }

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    fn reason(plan: &Plan) -> &str {
        assert!(plan.items.is_empty(), "expected a refusal, got {plan:?}");
        &plan.refused[0].reason
    }

    #[test]
    fn a_clean_worktree_is_removed_and_its_branch_kept() {
        let f = Fixture::new();
        let t = f.worktree("clean");
        let s = f.session(A, "Work", &[&t.path]);
        let plan = f.plan(std::slice::from_ref(&t), std::slice::from_ref(&s));
        assert!(plan.refused.is_empty(), "{:?}", plan.refused);
        assert_eq!(plan.items[0].links.len(), 1, "the conversation is listed");
        let outcome = f.execute(&plan, &[s]);
        assert_eq!(outcome.removed, std::slice::from_ref(&t.path));
        assert!(!t.path.exists());
        let branches = Command::new("git").arg("-C").arg(&f.repo).args(["branch", "--list", "clean"]).output().unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).contains("clean"));
    }

    #[test]
    fn ignored_files_are_listed_with_their_size_because_they_go_too() {
        let f = Fixture::new();
        let t = f.worktree("ign");
        fs::write(t.path.join(".env"), "secret=1").unwrap();
        fs::create_dir_all(t.path.join("target/debug")).unwrap();
        fs::write(t.path.join("target/debug/big"), vec![0u8; 1000]).unwrap();
        let plan = f.plan(&[t], &[]);
        let item = &plan.items[0];
        assert_eq!(item.ignored, [".env", "target/"]);
        assert_eq!((item.ignored_bytes, item.ignored_more), (1008, false));
    }

    #[test]
    fn new_ignored_files_after_the_plan_stop_the_removal() {
        let f = Fixture::new();
        let t = f.worktree("late");
        let plan = f.plan(std::slice::from_ref(&t), &[]);
        fs::write(t.path.join(".env"), "secret").unwrap();
        let outcome = f.execute(&plan, &[]);
        assert!(outcome.removed.is_empty());
        assert!(outcome.skipped[0].reason.contains("new ignored files"));
        assert!(t.path.join(".env").exists());
    }

    #[test]
    fn changes_locks_and_submodules_refuse() {
        let f = Fixture::new();
        let dirty = f.worktree("dirty");
        fs::write(dirty.path.join("new"), "x").unwrap();
        assert_eq!(reason(&f.plan(&[dirty], &[])), "it has 1 uncommitted file");

        let held = f.worktree("held");
        sh(&f.repo, &["worktree", "lock", "../held"]);
        assert!(reason(&f.plan(&[held], &[])).starts_with("it's locked"));

        let sub = f.worktree("sub");
        fs::write(sub.path.join(".gitmodules"), "").unwrap();
        sh(&sub.path, &["add", ".gitmodules"]);
        sh(&sub.path, &["commit", "-qm", "m"]);
        sh(&sub.path, &["push", "-q", "-u", "origin", "sub"]);
        assert!(reason(&f.plan(&[sub], &[])).contains("submodules"));
    }

    fn commit(dir: &Path, file: &str) {
        fs::write(dir.join(file), file).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-qm", file]);
    }

    #[test]
    fn unpushed_commits_are_allowed_and_counted_for_the_warning() {
        let f = Fixture::new();
        let ahead = f.worktree("ahead");
        commit(&ahead.path, "b");
        commit(&ahead.path, "c");
        let plan = f.plan(std::slice::from_ref(&ahead), &[]);
        assert!(plan.refused.is_empty(), "{:?}", plan.refused);
        let item = &plan.items[0];
        assert_eq!((item.unpushed, item.has_remote, item.upstream_gone), (2, true, false));
        assert_eq!(f.execute(&plan, &[]).removed, std::slice::from_ref(&ahead.path));
        let kept = Command::new("git").arg("-C").arg(&f.repo).args(["rev-list", "--count", "ahead"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&kept.stdout).trim(), "3", "the branch keeps them");
    }

    #[test]
    fn a_new_unpushed_commit_after_the_plan_stops_the_removal() {
        let f = Fixture::new();
        let ahead = f.worktree("ahead");
        commit(&ahead.path, "b");
        let plan = f.plan(std::slice::from_ref(&ahead), &[]);
        commit(&ahead.path, "c");
        let outcome = f.execute(&plan, &[]);
        assert_eq!(outcome.skipped[0].reason, "it has more unpushed commits than when you confirmed");
        assert!(ahead.path.exists());
    }

    #[test]
    fn a_detached_worktree_refuses_only_when_its_commits_are_on_no_branch() {
        let f = Fixture::new();
        sh(&f.repo, &["worktree", "add", "-q", "--detach", "../loose"]);
        let loose = Target { path: f.root.join("loose"), repo: f.repo.clone() };
        assert!(f.plan(std::slice::from_ref(&loose), &[]).refused.is_empty(), "at a commit main has");
        commit(&loose.path, "b");
        assert_eq!(reason(&f.plan(std::slice::from_ref(&loose), &[])), "it has 1 commit that's on no branch. Create a branch for it first");
        commit(&loose.path, "c");
        assert_eq!(reason(&f.plan(std::slice::from_ref(&loose), &[])), "it has 2 commits that are on no branch. Create a branch for them first");
        sh(&f.repo, &["branch", "keep", "HEAD"]);
        sh(&loose.path, &["branch", "-f", "keep", "HEAD"]);
        let plan = f.plan(&[loose], &[]);
        assert!(plan.refused.is_empty(), "a branch holds them now: {:?}", plan.refused);
        assert_eq!(plan.items[0].unpushed, 2, "still only on this machine");
    }

    #[test]
    fn the_main_worktree_and_unknown_paths_refuse() {
        let f = Fixture::new();
        let main = Target { path: f.repo.clone(), repo: f.repo.clone() };
        assert_eq!(reason(&f.plan(&[main], &[])), "it's the repository's main worktree");
        let stranger = Target { path: f.root.join("nowhere"), repo: f.repo.clone() };
        assert_eq!(reason(&f.plan(&[stranger], &[])), "Git no longer lists it");
        let relative = Target { path: "app-x".into(), repo: f.repo.clone() };
        assert_eq!(reason(&f.plan(&[relative], &[])), "its path isn't absolute");
    }

    #[test]
    fn a_session_open_in_it_refuses_even_without_a_transcript() {
        let f = Fixture::new();
        let t = f.worktree("busy");
        let sub = t.path.join("src");
        fs::create_dir(&sub).unwrap();
        f.running(42, A, &sub);
        assert_eq!(reason(&f.plan(&[t], &[])), "conversation aaaaaaaa is open in it");
    }

    #[test]
    fn an_open_conversation_last_working_in_it_refuses_but_one_that_only_passed_through_does_not() {
        let f = Fixture::new();
        let t = f.worktree("wt");
        let elsewhere = f.root.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        // The registry can still name the folder the session started in.
        f.running(42, A, &elsewhere);
        let last_here = f.session(A, "Last here", &[&elsewhere, &t.path]);
        assert_eq!(reason(&f.plan(std::slice::from_ref(&t), &[last_here])), "\"Last here\" is open and was last working in it");

        let passed = f.session(A, "Passed", &[&t.path, &elsewhere]);
        let plan = f.plan(&[t], &[passed]);
        assert!(plan.refused.is_empty(), "{:?}", plan.refused);
        assert_eq!(plan.items[0].links.len(), 1, "still listed");
    }

    #[test]
    fn a_recent_write_a_background_run_or_an_unreadable_registry_refuses() {
        let f = Fixture::new();
        let t = f.worktree("wt");
        let s = f.session(B, "Recent", &[&t.path]);
        fs::write(&s.transcript_path, "{}\n{}").unwrap();
        assert!(reason(&f.plan(std::slice::from_ref(&t), std::slice::from_ref(&s))).contains("written to in the last minute"));

        let s = f.session(B, "Background", &[&t.path]);
        let roster = f.paths.claude.join("daemon/roster.json");
        fs::create_dir_all(roster.parent().unwrap()).unwrap();
        fs::write(&roster, "not json bbbbbbbb").unwrap();
        assert!(reason(&f.plan(std::slice::from_ref(&t), std::slice::from_ref(&s))).contains("running in the background"));
        fs::remove_file(&roster).unwrap();

        fs::write(f.paths.sessions().join("77.json"), "{ cut off").unwrap();
        fs::create_dir_all(f.proc.join("77")).unwrap();
        assert!(reason(&f.plan(&[t], &[s])).contains("registry file can't be read"));
    }

    #[test]
    fn a_background_job_that_may_own_it_refuses() {
        let f = Fixture::new();
        let t = f.worktree("job");
        let job = f.paths.claude.join("jobs/cccccccc");
        fs::create_dir_all(&job).unwrap();
        fs::write(job.join("state.json"), format!(r#"{{"sessionId":"{B}","bgIsolation":"worktree","cwd":"{}"}}"#, t.path.display())).unwrap();
        assert!(reason(&f.plan(&[t], &[])).contains("claude rm"));
    }

    #[test]
    fn a_worktree_whose_folder_is_gone_only_clears_gits_record() {
        let f = Fixture::new();
        let t = f.worktree("gone");
        fs::remove_dir_all(&t.path).unwrap();
        let plan = f.plan(std::slice::from_ref(&t), &[]);
        assert!(plan.items[0].gone);
        assert_eq!(f.execute(&plan, &[]).removed, std::slice::from_ref(&t.path));
        let list = Command::new("git").arg("-C").arg(&f.repo).args(["worktree", "list"]).output().unwrap();
        assert!(!String::from_utf8_lossy(&list.stdout).contains("gone"));
    }

    #[test]
    fn a_change_after_the_plan_is_caught_before_removing() {
        let f = Fixture::new();
        let t = f.worktree("race");
        let plan = f.plan(std::slice::from_ref(&t), &[]);
        fs::write(t.path.join("file"), "new work").unwrap();
        let outcome = f.execute(&plan, &[]);
        assert_eq!(outcome.skipped[0].reason, "it has 1 uncommitted file");
        assert!(t.path.join("file").exists());
    }

    #[test]
    fn a_git_refusal_after_the_checks_is_reported_with_its_reason() {
        let f = Fixture::new();
        let t = f.worktree("refuse");
        let plan = f.plan(std::slice::from_ref(&t), &[]);
        assert!(plan.refused.is_empty());
        // Git won't remove a worktree with initialized submodules, which it tracks in the
        // worktree's own admin folder. Nothing in the worktree shows it, so ccm's checks pass.
        fs::create_dir_all(f.repo.join(".git/worktrees/refuse/modules")).unwrap();
        let outcome = f.execute(&plan, &[]);
        assert!(outcome.removed.is_empty() && outcome.skipped.is_empty());
        assert!(outcome.failures[0].reason.starts_with("Git refused: ") && outcome.failures[0].reason.contains("submodules"), "{outcome:?}");
        assert!(t.path.exists());
    }
}
