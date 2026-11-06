//! Finds the Git worktrees of the repositories conversations ran in, reads their state, and works
//! out which conversations used each one. Reads only.
//!
//! Git is the source of truth. Many people keep worktrees beside the main repository, so a
//! folder's name can't say whether it is one.
//!
//! Git runs here in folders `ccm` doesn't control, so it runs with care: the `GIT_*` variables of
//! `ccm`'s own environment are cleared, a repository's `core.fsmonitor` setting can't start a
//! program, nothing takes optional locks or fetches missing objects, and a Git that hangs is
//! stopped. A state that couldn't be read stays unknown, never "clean".

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::model::Session;

/// Long enough for `git status` on a large repository, short enough not to leave the view hanging.
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The repository's main worktree, which names the group.
    pub repo: PathBuf,
    /// The repository's main worktree. Never a candidate for removal.
    pub main: bool,
    pub bare: bool,
    /// Short name. `None` with a commit checked out directly.
    pub branch: Option<String>,
    pub locked: bool,
    /// Its folder is gone.
    pub prunable: bool,
    /// `None` when Git couldn't tell, which must count as unsafe.
    pub state: Option<State>,
}

impl Worktree {
    pub fn detached(&self) -> bool {
        self.branch.is_none() && !self.bare
    }

    /// The worktrees the view lists. A main worktree is never one of them.
    pub fn is_linked(&self) -> bool {
        !self.main && !self.bare
    }

    /// Recorded by Git for Windows, with a path like `C:/Users/...` that Git in WSL can't open.
    /// Git then calls it prunable, but its folder may well exist on the Windows side. Depending
    /// on the version, Git shows the path as it is or after the admin folder's own path.
    pub fn from_windows(&self) -> bool {
        self.path.to_string_lossy().split('/').any(|part| {
            let b = part.as_bytes();
            b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || b[2] == b'\\')
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    /// Changed, staged, conflicted and untracked files. Ignored files aren't counted.
    pub changes: u64,
    /// With a remote, commits on no remote-tracking branch. Without one, commits on no other
    /// local branch. Remote branches are as of the last fetch, as nothing here uses the network.
    pub unpushed: u64,
    pub has_remote: bool,
    /// The branch's upstream was deleted, which usually means it was merged.
    pub upstream_gone: bool,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Scan {
    pub worktrees: Vec<Worktree>,
    /// Problems worth showing, such as Git not being installed.
    pub errors: Vec<String>,
}

/// A conversation that worked in a worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub id: String,
    /// Its records written in this worktree, and in all folders.
    pub records: u64,
    pub total: u64,
    pub last_ms: Option<i64>,
    /// The folder it was last in is inside this worktree.
    pub last_here: bool,
    /// Its first folder, where a resume starts, is inside this worktree.
    pub started_here: bool,
}

#[derive(Debug)]
pub(crate) enum Failure {
    /// Git isn't installed.
    Missing,
    Failed,
}

/// Runs Git and returns its output. `dir` is passed with `-C`, so a folder that's gone is a Git
/// error, not a missing Git.
pub(crate) fn git<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Vec<u8>, Failure> {
    match run(dir, args)? {
        (true, out, _) => Ok(out),
        _ => Err(Failure::Failed),
    }
}

/// Runs Git and returns whether it succeeded, its output and its error text.
pub(crate) fn run<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<(bool, Vec<u8>, Vec<u8>), Failure> {
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.as_bytes().starts_with(b"GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { Failure::Missing } else { Failure::Failed })?;
    // Read meanwhile, so a large output can't fill a pipe and stall Git.
    let read = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut out = Vec::new();
            pipe.read_to_end(&mut out).map(|_| out)
        })
    };
    let reader = read(Box::new(child.stdout.take().ok_or(Failure::Failed)?));
    let errors = read(Box::new(child.stderr.take().ok_or(Failure::Failed)?));
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().map_err(|_| Failure::Failed)?.map_err(|_| Failure::Failed)?;
                let err = errors.join().map_err(|_| Failure::Failed)?.unwrap_or_default();
                return Ok((status.success(), out, err));
            }
            Ok(None) if started.elapsed() < TIMEOUT => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Failed);
            }
        }
    }
}

pub(crate) fn text(out: &[u8]) -> &str {
    std::str::from_utf8(out).unwrap_or("").trim_end_matches('\n')
}

pub(crate) fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

/// The repository folder shared by all of a repository's worktrees, as a real path.
fn common_dir(folder: &Path) -> Option<PathBuf> {
    let out = git(folder, &["rev-parse", "--git-common-dir"]).ok()?;
    let out = out.strip_suffix(b"\n").unwrap_or(&out);
    let dir = path_of(out);
    fs::canonicalize(if dir.is_absolute() { dir } else { folder.join(dir) }).ok()
}

/// Parses `git worktree list --porcelain -z`. The first entry is the main worktree.
pub(crate) fn parse_list(out: &[u8]) -> Vec<Worktree> {
    let mut list: Vec<Worktree> = Vec::new();
    let mut current: Option<Worktree> = None;
    for field in out.split(|b| *b == 0) {
        if field.is_empty() {
            list.extend(current.take());
            continue;
        }
        let (key, value) = match field.iter().position(|b| *b == b' ') {
            Some(at) => (&field[..at], &field[at + 1..]),
            None => (field, &[][..]),
        };
        if key == b"worktree" {
            list.extend(current.take());
            current = Some(Worktree {
                path: path_of(value),
                repo: PathBuf::new(),
                main: list.is_empty(),
                bare: false,
                branch: None,
                locked: false,
                prunable: false,
                state: None,
            });
            continue;
        }
        let Some(w) = current.as_mut() else { continue };
        match key {
            b"bare" => w.bare = true,
            b"branch" => {
                let name = String::from_utf8_lossy(value);
                w.branch = Some(name.strip_prefix("refs/heads/").unwrap_or(&name).to_owned());
            }
            b"locked" => w.locked = true,
            // Not gone, only out of reach: see `from_windows`.
            b"prunable" => w.prunable = !w.from_windows(),
            _ => {}
        }
    }
    list.extend(current);
    let repo = list.first().map(|w| w.path.clone()).unwrap_or_default();
    for w in &mut list {
        w.repo = repo.clone();
    }
    list
}

/// Counts entries of `git status --porcelain=v2 -z`. A rename carries its old path as an extra
/// field, which is skipped so it can't be mistaken for an entry.
fn count_changes(out: &[u8]) -> u64 {
    let mut fields = out.split(|b| *b == 0);
    let mut count = 0;
    while let Some(field) = fields.next() {
        match field.first() {
            Some(b'1' | b'u' | b'?') => count += 1,
            Some(b'2') => {
                count += 1;
                fields.next();
            }
            _ => {}
        }
    }
    count
}

fn count(out: &[u8]) -> Option<u64> {
    text(out).parse().ok()
}

pub(crate) fn inspect(w: &Worktree) -> Option<State> {
    let dir = &w.path;
    let changes = count_changes(&git(dir, &["status", "--porcelain=v2", "-z"]).ok()?);
    let has_remote = !text(&git(dir, &["remote"]).ok()?).is_empty();
    let unpushed = match (has_remote, &w.branch) {
        (true, _) => count(&git(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"]).ok()?)?,
        (false, Some(branch)) => {
            // The pattern is relative to refs/heads/. Branch names can't hold glob characters.
            let own = format!("--exclude={branch}");
            count(&git(dir, &["rev-list", "--count", "HEAD", "--not", &own, "--branches"]).ok()?)?
        }
        (false, None) => count(&git(dir, &["rev-list", "--count", "HEAD", "--not", "--branches"]).ok()?)?,
    };
    let upstream_gone = match &w.branch {
        Some(branch) => {
            let out = git(dir, &["for-each-ref", "--format=%(upstream:track)", &format!("refs/heads/{branch}")]).ok()?;
            text(&out) == "[gone]"
        }
        None => false,
    };
    Some(State { changes, unpushed, has_remote, upstream_gone })
}

/// Lists the worktrees of every repository that one of these folders is in. Folders that are gone
/// or outside a repository are skipped, so a repository no conversation ran in isn't listed.
pub fn scan(folders: &[String]) -> Scan {
    let mut scan = Scan::default();
    if let Err(Failure::Missing) = git(Path::new("/"), &["--version"]) {
        scan.errors.push("Git wasn't found, so worktrees can't be listed.".into());
        return scan;
    }
    let mut folders: Vec<&str> = folders.iter().map(String::as_str).collect();
    folders.sort_unstable();
    folders.dedup();
    let mut repos: Vec<PathBuf> =
        folders.par_iter().filter(|f| Path::new(f).is_dir()).filter_map(|f| common_dir(Path::new(f))).collect();
    repos.sort();
    repos.dedup();

    let lists: Vec<Option<Vec<Worktree>>> =
        repos.par_iter().map(|r| git(r, &["worktree", "list", "--porcelain", "-z"]).ok().map(|out| parse_list(&out))).collect();
    let failed = lists.iter().filter(|l| l.is_none()).count();
    if failed > 0 {
        let plural = if failed == 1 { "y" } else { "ies" };
        scan.errors.push(format!("Couldn't list the worktrees of {failed} repositor{plural}. Worktrees need Git 2.36 or later."));
    }
    scan.worktrees = lists.into_iter().flatten().flatten().collect();
    scan.worktrees.par_iter_mut().filter(|w| w.is_linked() && !w.prunable && !w.from_windows()).for_each(|w| w.state = inspect(w));
    scan
}

/// The worktree a folder is in. With one worktree inside another, the deepest wins.
fn owner(worktrees: &[Worktree], folder: &Path) -> Option<usize> {
    let real = fs::canonicalize(folder).ok();
    let candidates = std::iter::once(folder).chain(real.as_deref());
    candidates
        .flat_map(|f| worktrees.iter().enumerate().filter(move |(_, w)| f.starts_with(&w.path)))
        .max_by_key(|(_, w)| w.path.components().count())
        .map(|(i, _)| i)
}

/// For each worktree, in the same order, the conversations that worked in it.
pub fn connect(worktrees: &[Worktree], sessions: &[Session]) -> Vec<Vec<Link>> {
    let mut links = vec![Vec::new(); worktrees.len()];
    let mut owners: HashMap<&str, Option<usize>> = HashMap::new();
    for s in sessions {
        let mut here: BTreeMap<usize, Link> = BTreeMap::new();
        let last = s.last_folder();
        let total = s.folder_records();
        for visit in &s.folders {
            let path = visit.path.as_str();
            let Some(w) = *owners.entry(path).or_insert_with(|| owner(worktrees, Path::new(path))) else { continue };
            let link = here.entry(w).or_insert_with(|| Link {
                id: s.id.clone(),
                records: 0,
                total,
                last_ms: None,
                last_here: false,
                started_here: false,
            });
            link.records += visit.records;
            link.last_ms = link.last_ms.max(visit.last_ms);
            link.last_here |= last == Some(path);
            link.started_here |= s.folder.as_deref() == Some(path);
        }
        for (w, link) in here {
            links[w].push(link);
        }
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FolderVisit, Source};

    /// Test repositories are made with an empty configuration, so the developer's own can't
    /// change them.
    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "init.defaultBranch=main", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn commit(dir: &Path, file: &str) {
        fs::write(dir.join(file), file).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-qm", file]);
    }

    /// A repository with a remote, and its folder, under a real path.
    fn repo(root: &Path) -> PathBuf {
        let root = fs::canonicalize(root).unwrap();
        sh(&root, &["init", "-q", "--bare", "origin.git"]);
        let repo = root.join("app");
        sh(&root, &["init", "-q", "app"]);
        commit(&repo, "a");
        sh(&repo, &["remote", "add", "origin", "../origin.git"]);
        sh(&repo, &["push", "-q", "-u", "origin", "main"]);
        repo
    }

    fn find<'a>(scan: &'a Scan, path: &Path) -> &'a Worktree {
        scan.worktrees.iter().find(|w| w.path == path).unwrap_or_else(|| panic!("{path:?} not in {scan:?}"))
    }

    fn folder(p: &Path) -> String {
        p.to_str().unwrap().to_owned()
    }

    #[test]
    fn lists_worktrees_beside_the_repository_with_their_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        let root = repo.parent().unwrap();
        let wt = |name: &str| root.join(name);
        sh(&repo, &["worktree", "add", "-q", "../clean", "-b", "clean"]);
        sh(&repo, &["worktree", "add", "-q", "../dirty", "-b", "dirty"]);
        fs::write(wt("dirty").join("a"), "changed").unwrap();
        fs::write(wt("dirty").join("new"), "untracked").unwrap();
        fs::write(wt("dirty").join(".gitignore"), "").unwrap();
        sh(&repo, &["worktree", "add", "-q", "../ahead", "-b", "ahead"]);
        commit(&wt("ahead"), "b");
        sh(&repo, &["worktree", "add", "-q", "--detach", "../loose"]);
        sh(&repo, &["worktree", "add", "-q", "../held", "-b", "held"]);
        sh(&repo, &["worktree", "lock", "../held"]);
        sh(&repo, &["worktree", "add", "-q", "../gone", "-b", "gone"]);
        fs::remove_dir_all(wt("gone")).unwrap();

        // Found from a worktree's own subfolder, without naming the repository.
        let sub = wt("clean").join("src");
        fs::create_dir(&sub).unwrap();
        let scan = scan(&[folder(&sub)]);
        assert!(scan.errors.is_empty(), "{:?}", scan.errors);
        assert_eq!(scan.worktrees.len(), 7);

        let main = find(&scan, &repo);
        assert!(main.main && !main.is_linked() && main.state.is_none(), "the main worktree is never inspected");
        assert!(scan.worktrees.iter().all(|w| w.repo == repo));

        let state = |name: &str| find(&scan, &wt(name)).state.unwrap();
        assert_eq!(state("clean"), State { changes: 0, unpushed: 0, has_remote: true, upstream_gone: false });
        assert_eq!(state("dirty").changes, 3, "changed and untracked files count, each once");
        assert_eq!(state("ahead").unpushed, 1);
        assert_eq!(find(&scan, &wt("ahead")).branch.as_deref(), Some("ahead"));
        assert!(find(&scan, &wt("loose")).detached());
        assert!(find(&scan, &wt("held")).locked);
        let gone = find(&scan, &wt("gone"));
        assert!(gone.prunable && gone.state.is_none());
    }

    #[test]
    fn a_deleted_upstream_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        let wt = repo.parent().unwrap().join("merged");
        sh(&repo, &["worktree", "add", "-q", "../merged", "-b", "merged"]);
        commit(&wt, "b");
        sh(&wt, &["push", "-q", "-u", "origin", "merged"]);
        assert_eq!(find(&scan(&[folder(&wt)]), &wt).state.unwrap().unpushed, 0);
        sh(&wt, &["push", "-q", "origin", "--delete", "merged"]);
        sh(&wt, &["fetch", "-q", "--prune", "origin"]);
        let state = find(&scan(&[folder(&wt)]), &wt).state.unwrap();
        assert!(state.upstream_gone);
        assert_eq!(state.unpushed, 1, "its commits are on no remote branch any more");
    }

    #[test]
    fn without_a_remote_only_commits_on_no_other_branch_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        sh(&root, &["init", "-q", "solo"]);
        let repo = root.join("solo");
        commit(&repo, "a");
        commit(&repo, "b");
        sh(&repo, &["worktree", "add", "-q", "../solo-x", "-b", "x"]);
        let wt = root.join("solo-x");
        let state = find(&scan(&[folder(&repo)]), &wt).state.unwrap();
        assert_eq!((state.has_remote, state.unpushed), (false, 0), "main's history isn't this worktree's");
        commit(&wt, "c");
        assert_eq!(find(&scan(&[folder(&repo)]), &wt).state.unwrap().unpushed, 1);
    }

    #[test]
    fn folders_outside_a_repository_or_gone_are_skipped_and_repositories_are_listed_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        let plain = dir.path().join("plain");
        fs::create_dir(&plain).unwrap();
        let folders = [folder(&repo), folder(&repo.join(".git")), folder(&plain), "/does/not/exist".into()];
        let scan = scan(&folders);
        assert_eq!(scan.worktrees.len(), 1);
        assert!(scan.errors.is_empty());
    }

    #[test]
    fn a_repository_cannot_make_status_run_a_program() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        sh(&repo, &["worktree", "add", "-q", "../wt", "-b", "wt"]);
        let ran = dir.path().join("ran");
        let script = dir.path().join("monitor.sh");
        fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
        fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        sh(&repo, &["config", "core.fsmonitor", script.to_str().unwrap()]);
        let scan = scan(&[folder(&repo)]);
        assert!(scan.worktrees.iter().any(|w| w.state.is_some()));
        assert!(!ran.exists(), "core.fsmonitor ran");
    }

    #[test]
    fn a_worktree_made_by_git_for_windows_is_not_gone() {
        for windows in ["/r/.git/worktrees/w/C:/Users/me/w", "C:/Users/me/w", "/r/.git/worktrees/w/d:\\Users\\me\\w\\.git", "/home/me/C:"] {
            let out = format!("worktree /r\0HEAD 1\0branch refs/heads/main\0\0worktree {windows}\0HEAD 1\0branch refs/heads/w\0prunable gitdir file points to non-existent location\0\0");
            let list = parse_list(out.as_bytes());
            assert!(list[1].from_windows() && !list[1].prunable, "{windows}");
        }
        for linux in ["/home/me/w", "/home/me/a:b", "/home/me/ab:/w", "/mnt/c/Users/me/w"] {
            let out = format!("worktree /r\0HEAD 1\0\0worktree {linux}\0HEAD 1\0prunable gone\0\0");
            let list = parse_list(out.as_bytes());
            assert!(!list[1].from_windows() && list[1].prunable, "{linux}");
        }
    }

    #[test]
    fn porcelain_paths_may_hold_spaces_and_newlines() {
        let out = b"worktree /r/main\0HEAD 1\0branch refs/heads/main\0\0worktree /r/odd name\nx\0HEAD 2\0detached\0locked\0prunable gone\0\0";
        let list = parse_list(out);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].path, Path::new("/r/odd name\nx"));
        assert!(list[0].main && !list[1].main && list[1].detached() && list[1].locked && list[1].prunable);
        assert_eq!(list[1].repo, Path::new("/r/main"));
    }

    #[test]
    fn a_rename_counts_once() {
        assert_eq!(count_changes(b"2 R. N... 100644 100644 100644 a b R100 new\x001 old\0? 1 x\0"), 2);
    }

    fn worktree(path: &str, main: bool) -> Worktree {
        Worktree {
            path: path.into(),
            repo: "/r".into(),
            main,
            bare: false,
            branch: Some("b".into()),
            locked: false,
            prunable: false,
            state: None,
        }
    }

    fn session(id: &str, visits: &[(&str, u64, u64)]) -> Session {
        Session {
            id: id.into(),
            source: Source::ClaudeCode,
            transcript_path: String::new(),
            transcript_size: 0,
            transcript_mtime_ns: 0,
            size_bytes: 0,
            name: String::new(),
            folder: visits.first().map(|v| v.0.into()),
            folders: visits
                .iter()
                .map(|&(path, records, last_record)| FolderVisit { path: path.into(), records, last_record, last_ms: Some(last_record as i64) })
                .collect(),
            branch: None,
            model: None,
            started_at_ms: None,
            last_active_ms: None,
            context_used: None,
            prompts: 0,
            replies: 0,
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

    #[test]
    fn any_folder_connects_and_the_deepest_worktree_wins() {
        let trees = [worktree("/r", true), worktree("/r/.claude/worktrees/x", false), worktree("/r-y", false)];
        let sessions = [
            // Starts in the repository, passes through x's subfolder, ends in y.
            session("a", &[("/r", 6, 7), ("/r/.claude/worktrees/x/src", 2, 5), ("/r-y", 2, 10)]),
            session("b", &[("/r-y", 1, 1)]),
            // A sibling whose name only starts the same isn't inside.
            session("c", &[("/r-yz", 1, 1)]),
        ];
        let links = connect(&trees, &sessions);
        let ids = |w: usize| links[w].iter().map(|l| l.id.as_str()).collect::<Vec<_>>();
        assert_eq!((ids(0), ids(1), ids(2)), (vec!["a"], vec!["a"], vec!["a", "b"]));
        let x = &links[1][0];
        assert_eq!((x.records, x.total, x.last_ms, x.last_here, x.started_here), (2, 10, Some(5), false, false));
        let y = &links[2][0];
        assert!(y.last_here && !y.started_here);
        assert!(links[2][1].last_here && links[2][1].started_here);
    }

    #[test]
    fn a_folder_reached_through_a_symlink_connects() {
        let dir = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(dir.path()).unwrap().join("wt");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let trees = [worktree(real.to_str().unwrap(), false)];
        let links = connect(&trees, &[session("a", &[(link.to_str().unwrap(), 1, 1)])]);
        assert_eq!(links[0].len(), 1);
    }
}
