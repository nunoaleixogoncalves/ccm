//! Permanently deletes conversations. Nothing here can be undone, so a plan is built first and
//! shown to the user, and every path is checked again just before it goes.
//!
//! The order matters. `history.jsonl` is edited first, because that's the step most likely to be
//! refused, and a refusal then leaves everything intact. The transcript goes last, so a
//! conversation that couldn't be fully removed stays in the list and can be retried.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rayon::prelude::*;
use serde::Deserialize;

use crate::paths::{is_uuid, Paths};
use crate::recovery;
use crate::registry::{has_unreadable_entry, live_roster, open_sessions};
use crate::transcript::read_capped_line;

/// A transcript written this recently may belong to a session the registry doesn't list.
pub(crate) const RECENT_WRITE: Duration = Duration::from_secs(60);
/// Claude Code's own settings for this lock. A holder refreshes the lock's time well inside this.
const LOCK_STALE: Duration = Duration::from_secs(10);
const LOCK_WAIT: Duration = Duration::from_secs(3);
const MEMORY_INDEX: &str = "MEMORY.md";

/// The files of one conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    /// Removed first.
    pub side: Vec<PathBuf>,
    /// Removed last, and only if everything in `side` went.
    pub transcripts: Vec<PathBuf>,
    pub bytes: u64,
}

/// A memory note that only conversations in this delete wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub path: PathBuf,
    pub bytes: u64,
}

impl Note {
    pub fn name(&self) -> String {
        self.path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub items: Vec<Item>,
    pub notes: Vec<Note>,
    pub skipped: Vec<Skipped>,
    /// Things left behind on purpose, which the user should know about.
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).sum()
    }

    pub fn note_bytes(&self) -> u64 {
        self.notes.iter().map(|n| n.bytes).sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// Conversations whose transcripts are gone.
    pub deleted: Vec<String>,
    /// Conversations that became unsafe to delete after the plan was shown.
    pub skipped: Vec<Skipped>,
    /// Everything that could not be removed.
    pub failures: Vec<Failure>,
}

/// Works out what deleting these conversations would remove. Reads only.
pub fn plan(paths: &Paths, proc_root: &Path, ids: &[String]) -> Plan {
    let mut plan = Plan::default();
    let busy = Busy::read(paths, proc_root);
    let now = SystemTime::now();
    let projects = project_dirs(paths);
    let jobs = jobs(paths);

    for id in ids {
        let skip = |reason: &str| Skipped { id: id.clone(), reason: reason.into() };
        if !is_uuid(id) {
            plan.skipped.push(skip("its ID isn't valid"));
            continue;
        }
        if let Some(reason) = busy.reason(id) {
            plan.skipped.push(skip(reason));
            continue;
        }
        let transcripts: Vec<PathBuf> = projects
            .iter()
            .map(|p| p.join(format!("{id}.jsonl")))
            .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))
            .collect();
        if transcripts.is_empty() {
            plan.skipped.push(skip("its transcript isn't in a normal project folder"));
            continue;
        }
        if transcripts.iter().any(|t| written_since(t, now, RECENT_WRITE)) {
            plan.skipped.push(skip("it was written to in the last minute, so it may still be running"));
            continue;
        }
        let side = side_paths(paths, id, &projects, &jobs, &mut plan.warnings);
        let bytes = side.iter().chain(&transcripts).map(|p| size_of(p)).sum();
        plan.items.push(Item { id: id.clone(), side, transcripts, bytes });
    }
    plan.notes = sole_notes(&plan.items.iter().collect::<Vec<_>>(), &projects);
    plan
}

struct Busy {
    open: HashSet<String>,
    roster: String,
    unreadable: bool,
}

impl Busy {
    fn read(paths: &Paths, proc_root: &Path) -> Self {
        Self {
            open: open_sessions(&paths.sessions(), proc_root),
            roster: live_roster(&paths.claude.join("daemon/roster.json"), proc_root),
            unreadable: has_unreadable_entry(&paths.sessions(), proc_root),
        }
    }

    fn reason(&self, id: &str) -> Option<&'static str> {
        if self.open.contains(id) {
            Some("it's open")
        } else if self.roster.contains(&id[..8]) {
            Some("it's running in the background")
        } else if self.unreadable {
            Some("a running session's registry file can't be read, so it may be open")
        } else {
            None
        }
    }
}

pub(crate) fn written_since(path: &Path, now: SystemTime, window: Duration) -> bool {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age < window))
}

/// Real folders only: following a symlink could reach outside `~/.claude`.
pub(crate) fn project_dirs(paths: &Paths) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(paths.projects()) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

pub(crate) struct Job {
    pub(crate) dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) isolation: Option<String>,
    pub(crate) cwd: Option<String>,
}

pub(crate) fn jobs(paths: &Paths) -> Vec<Job> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct State {
        session_id: Option<String>,
        bg_isolation: Option<String>,
        cwd: Option<String>,
    }
    let Ok(entries) = fs::read_dir(paths.claude.join("jobs")) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let state: State = serde_json::from_slice(&fs::read(e.path().join("state.json")).ok()?).ok()?;
            Some(Job { dir: e.path(), session_id: state.session_id?, isolation: state.bg_isolation, cwd: state.cwd })
        })
        .collect()
}

/// Everything of this conversation's except its transcripts. Names are exact, built from a
/// validated ID, so another conversation's files can't match.
fn side_paths(paths: &Paths, id: &str, projects: &[PathBuf], jobs: &[Job], warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let claude = &paths.claude;
    let mut out = vec![
        claude.join("file-history").join(id),
        claude.join("session-env").join(id),
        claude.join("tasks").join(id),
        claude.join("debug").join(format!("{id}.txt")),
    ];
    let prefix = format!("{id}.");
    let transcript = format!("{id}.jsonl");
    for project in projects {
        out.push(project.join(id));
        // Claude Code keeps files like `<id>.cast` and `<id>.precompact.json` beside the transcript.
        if let Ok(entries) = fs::read_dir(project) {
            out.extend(
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix) && n != transcript))
                    .map(|e| e.path()),
            );
        }
    }
    if let Ok(entries) = fs::read_dir(claude.join("telemetry")) {
        out.extend(
            entries
                .flatten()
                .filter(|e| e.file_name().to_str().is_some_and(|n| n.split('.').nth(1) == Some(id)))
                .map(|e| e.path()),
        );
    }
    for job in jobs.iter().filter(|j| j.session_id == id) {
        // `claude rm` is what can safely clean up a job's worktree, and it needs the job's files.
        if job.isolation.as_deref() == Some("none") {
            out.push(job.dir.clone());
        } else {
            warnings.push(format!("Background job {} was kept because it may have a worktree. Remove it with: claude rm", job.dir.display()));
        }
    }
    out.retain(|p| fs::symlink_metadata(p).is_ok());
    out
}

/// Total size of what removing this path would free, never following symlinks.
pub fn size_of(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return if meta.is_file() { meta.len() } else { 0 };
    }
    let Ok(entries) = fs::read_dir(path) else { return 0 };
    entries.flatten().map(|e| size_of(&e.path())).sum()
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    content: Option<serde_json::Value>,
}

/// Paths under a `memory/` folder that this transcript tried to write with a tool. Anything in it
/// could be claimed by text Claude read, so this only ever keeps a note from being deleted.
fn memory_write_attempts(transcript: &Path) -> HashSet<String> {
    memory_writes(transcript).0.into_iter().map(|(path, _)| path).collect()
}

/// The paths that a reply wrote and whose tool reported success, as a denied write changes nothing.
fn memory_writes_that_worked(transcript: &Path) -> HashSet<String> {
    let (attempts, results) = memory_writes(transcript);
    attempts
        .into_iter()
        .filter(|(_, reply)| reply.as_ref().is_some_and(|id| results.get(id) == Some(&true)))
        .map(|(path, _)| path)
        .collect()
}

/// Each write's path and, if a reply made it, its tool call ID, plus whether each call succeeded.
#[allow(clippy::type_complexity)]
fn memory_writes(transcript: &Path) -> (Vec<(String, Option<String>)>, HashMap<String, bool>) {
    let (mut attempts, mut results) = (Vec::new(), HashMap::new());
    let Ok(file) = fs::File::open(transcript) else { return (attempts, results) };
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::new();
    while let Ok(Some(_)) = read_capped_line(&mut reader, &mut line) {
        if !contains(&line, b"/memory/") && !contains(&line, b"tool_result") {
            continue;
        }
        let Ok(parsed) = serde_json::from_slice::<Line>(&line) else { continue };
        let from_reply = parsed.kind.as_deref() == Some("assistant");
        let Some(blocks) = parsed.message.and_then(|m| m.content) else { continue };
        for block in blocks.as_array().into_iter().flatten() {
            let text = |key| block.get(key).and_then(|v| v.as_str());
            if text("type") == Some("tool_result") {
                if let Some(id) = text("tool_use_id") {
                    let failed = block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                    results.insert(id.to_owned(), !failed);
                }
            } else if text("type") == Some("tool_use")
                && matches!(text("name"), Some("Write" | "Edit" | "MultiEdit"))
                && let Some(path) = block.get("input").and_then(|i| i.get("file_path")).and_then(|p| p.as_str())
            {
                attempts.push((path.to_owned(), text("id").filter(|_| from_reply).map(str::to_owned)));
            }
        }
    }
    (attempts, results)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The same path written differently, such as with `/./` or `..`, is still the same note.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A transcript plus the files of its subagents, which can write notes too.
fn with_subagents(transcript: &Path) -> Vec<PathBuf> {
    let mut out = vec![transcript.to_path_buf()];
    let dir = transcript.with_extension("").join("subagents");
    collect_jsonl(&dir, &mut out);
    out
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            collect_jsonl(&entry.path(), out);
        } else if kind.is_file() && entry.path().extension().is_some_and(|x| x == "jsonl") {
            out.push(entry.path());
        }
    }
}

/// Notes in these conversations' own project folders that they wrote and nobody else did. A note
/// with no recorded writer, such as one edited by hand, is never offered.
fn sole_notes(items: &[&Item], projects: &[PathBuf]) -> Vec<Note> {
    let targets: HashSet<&Path> = items.iter().flat_map(|i| i.transcripts.iter().map(PathBuf::as_path)).collect();
    let mut candidates: HashMap<PathBuf, PathBuf> = HashMap::new();
    for transcript in &targets {
        let Some(project) = transcript.parent() else { continue };
        let memory = project.join("memory");
        // A linked folder holds files from somewhere else, which aren't this conversation's own.
        if !fs::symlink_metadata(&memory).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        for file in with_subagents(transcript) {
            for written in memory_writes_that_worked(&file) {
                let path = PathBuf::from(&written);
                let in_memory = path.parent() == Some(memory.as_path());
                let is_note = path.extension().is_some_and(|x| x == "md") && path.file_name().is_some_and(|n| n != MEMORY_INDEX);
                if in_memory && is_note && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                    candidates.insert(lexical(&path), path);
                }
            }
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    // Only reached when a note is at stake, because this reads every other transcript.
    let mut others = Vec::new();
    for project in projects {
        let Ok(entries) = fs::read_dir(project) else { continue };
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_some_and(|x| x == "jsonl") && !targets.contains(path.as_path()) {
                others.extend(with_subagents(&path));
            }
        }
    }
    let shared: HashSet<PathBuf> =
        others.par_iter().flat_map_iter(|t| memory_write_attempts(t)).map(|p| lexical(Path::new(&p))).collect();
    let mut notes: Vec<Note> = candidates
        .into_iter()
        .filter(|(key, _)| !shared.contains(key))
        .map(|(_, path)| Note { bytes: size_of(&path), path })
        .collect();
    notes.sort_by(|a, b| a.path.cmp(&b.path));
    notes
}

/// Carries out a plan. `Err` means nothing was changed.
pub fn execute(paths: &Paths, proc_root: &Path, plan: &Plan, include_notes: bool) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    // The plan may have sat on screen for a while.
    let busy = Busy::read(paths, proc_root);
    let mut items = Vec::new();
    for item in &plan.items {
        let reason = busy.reason(&item.id).map(str::to_owned).or_else(|| {
            item.transcripts
                .iter()
                .any(|t| written_since(t, SystemTime::now(), RECENT_WRITE))
                .then(|| "it was written to in the last minute, so it may still be running".to_owned())
        });
        match reason {
            Some(reason) => outcome.skipped.push(Skipped { id: item.id.clone(), reason }),
            None => items.push(item),
        }
    }
    if items.is_empty() {
        return Ok(outcome);
    }
    // Every removal is checked up front, because the history edit that comes first can't be undone.
    let every_path = items.iter().flat_map(|i| i.side.iter().chain(&i.transcripts));
    if let Some(path) = every_path.chain(plan.notes.iter().map(|n| &n.path)).find(|p| !is_inside(&paths.claude, p)) {
        return Err(format!("{} is outside the Claude Code folder, so nothing was deleted.", path.display()));
    }
    // A conversation may have been skipped above or started since, so notes are worked out again.
    let notes = if include_notes { sole_notes(&items, &project_dirs(paths)) } else { Vec::new() };

    let ids: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let pastes = remove_history_lines(&paths.claude.join("history.jsonl"), &ids)
        .map_err(|e| format!("history.jsonl could not be edited, so nothing was deleted: {e}"))?;
    remove_unreferenced_pastes(paths, &pastes, &items, &mut outcome.failures);

    if include_notes {
        for note in &plan.notes {
            if notes.iter().any(|n| n.path == note.path) {
                remove_note(paths, note, &mut outcome.failures);
            } else {
                outcome.failures.push(Failure { path: note.path.clone(), error: "kept, because another conversation may be using it".into() });
            }
        }
    }
    for item in items {
        remove_item(paths, item, &mut outcome);
    }
    Ok(outcome)
}

fn remove_item(paths: &Paths, item: &Item, outcome: &mut Outcome) {
    let before = outcome.failures.len();
    for path in &item.side {
        remove(paths, path, &mut outcome.failures);
    }
    // Keep the transcript while anything is left, so the row stays and the delete can be retried.
    if outcome.failures.len() != before {
        outcome.failures.extend(item.transcripts.iter().map(|t| Failure {
            path: t.clone(),
            error: "kept, so this delete can be retried".into(),
        }));
        return;
    }
    for path in &item.transcripts {
        remove(paths, path, &mut outcome.failures);
        if let Some(project) = path.parent() {
            // Fails, as intended, while anything else such as `memory/` is in the folder.
            let _ = fs::remove_dir(project);
        }
    }
    if outcome.failures.len() == before {
        recovery::forget(paths, &item.id);
        outcome.deleted.push(item.id.clone());
    }
}

fn remove(paths: &Paths, path: &Path, failures: &mut Vec<Failure>) {
    if let Err(e) = remove_path(&paths.claude, path) {
        failures.push(Failure { path: path.to_path_buf(), error: e.to_string() });
    }
}

/// Refuses anything outside `root`. A symlink is removed itself, and never followed.
pub(crate) fn remove_path(root: &Path, path: &Path) -> io::Result<()> {
    if !is_inside(root, path) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "outside the Claude Code folder"));
    }
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn is_inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root) && path.components().all(|c| !matches!(c, Component::ParentDir))
}

/// Removes this conversation's lines while holding Claude Code's own lock, so a prompt typed
/// meanwhile isn't lost. Returns the pasted-content hashes that no remaining line uses.
fn remove_history_lines(history: &Path, ids: &HashSet<&str>) -> io::Result<HashSet<String>> {
    let matches_any = |bytes: &[u8]| ids.iter().any(|id| bytes.windows(id.len()).any(|w| w == id.as_bytes()));
    match fs::read(history) {
        Ok(bytes) if matches_any(&bytes) => {}
        Ok(_) => return Ok(HashSet::new()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(e),
    }
    let lock = HistoryLock::acquire(history)?;
    let bytes = fs::read(history)?;
    let mut kept = Vec::with_capacity(bytes.len());
    let mut removed = HashSet::new();
    let mut remaining = HashSet::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let entry = serde_json::from_slice::<HistoryLine>(line).ok();
        match entry {
            Some(e) if e.session_id.as_deref().is_some_and(|s| ids.contains(s)) => removed.extend(e.pastes()),
            // Anything unreadable stays exactly as it was.
            other => {
                remaining.extend(other.map(|e| e.pastes()).into_iter().flatten());
                kept.extend_from_slice(line);
            }
        }
    }
    if kept.len() != bytes.len() {
        replace_file(history, &kept, || lock.still_held())?;
    }
    drop(lock);
    Ok(removed.difference(&remaining).cloned().collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryLine {
    session_id: Option<String>,
    #[serde(default)]
    pasted_contents: HashMap<String, Pasted>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pasted {
    content_hash: Option<String>,
}

impl HistoryLine {
    fn pastes(self) -> impl Iterator<Item = String> {
        self.pasted_contents.into_values().filter_map(|p| p.content_hash)
    }
}

/// The same lock Claude Code takes around `history.jsonl`: a `.lock` folder that counts as
/// abandoned once its time is older than `LOCK_STALE`.
struct HistoryLock {
    dir: PathBuf,
    taken_at: SystemTime,
}

impl HistoryLock {
    fn acquire(file: &Path) -> io::Result<Self> {
        let mut name = file.as_os_str().to_owned();
        name.push(".lock");
        let dir = PathBuf::from(name);
        let start = Instant::now();
        loop {
            match fs::create_dir(&dir) {
                Ok(()) => return Ok(Self { taken_at: fs::metadata(&dir)?.modified()?, dir }),
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                Err(_) => {}
            }
            let abandoned = fs::metadata(&dir)
                .and_then(|m| m.modified())
                .is_ok_and(|t| SystemTime::now().duration_since(t).is_ok_and(|age| age > LOCK_STALE));
            if abandoned {
                let _ = fs::remove_dir(&dir);
                continue;
            }
            if start.elapsed() > LOCK_WAIT {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "Claude Code is holding its lock on it. Try again."));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// False if someone decided this lock was abandoned and took it over.
    fn still_held(&self) -> bool {
        fs::metadata(&self.dir).and_then(|m| m.modified()).is_ok_and(|t| t == self.taken_at)
    }

}

/// Also runs when an error ends the edit early, so Claude Code isn't left waiting for the lock.
impl Drop for HistoryLock {
    fn drop(&mut self) {
        if self.still_held() {
            let _ = fs::remove_dir(&self.dir);
        }
    }
}

/// Replaces a file in one step, keeping its permissions. The temporary name is one Claude Code's
/// own cleanup recognises, in case this is interrupted.
pub(crate) fn replace_file(path: &Path, bytes: &[u8], ready: impl FnOnce() -> bool) -> io::Result<()> {
    let mode = match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")),
        Ok(meta) => meta.permissions().mode() & 0o777,
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0o600,
        Err(e) => return Err(e),
    };
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".tmp.{:08x}", random_u32()?));
    let tmp = PathBuf::from(name);
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if !ready() {
            return Err(io::Error::other("lost the lock while writing"));
        }
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn random_u32() -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    io::Read::read_exact(&mut fs::File::open("/dev/urandom")?, &mut bytes)?;
    Ok(u32::from_ne_bytes(bytes))
}

/// A pasted file goes only when nothing still points at it. The hash is checked first, because it
/// comes from a file and is about to become part of a path.
fn remove_unreferenced_pastes(paths: &Paths, hashes: &HashSet<String>, deleting: &[&Item], failures: &mut Vec<Failure>) {
    let candidates: Vec<&String> = hashes.iter().filter(|h| is_paste_hash(h)).collect();
    if candidates.is_empty() {
        return;
    }
    let targets: HashSet<&Path> = deleting.iter().flat_map(|i| i.transcripts.iter().map(PathBuf::as_path)).collect();
    let mut others = Vec::new();
    for project in project_dirs(paths) {
        let Ok(entries) = fs::read_dir(project) else { continue };
        others.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && !targets.contains(p.as_path())),
        );
    }
    let used: HashSet<&String> = others.par_iter().flat_map_iter(|t| hashes_in_file(t, &candidates)).collect();
    for hash in candidates.iter().filter(|h| !used.contains(*h)) {
        remove(paths, &paths.claude.join("paste-cache").join(format!("{hash}.txt")), failures);
    }
}

/// Reads in pieces, because transcripts can be huge and several are read at once. A file that can't
/// be read might mention any of them, so it counts as using them all.
fn hashes_in_file<'a>(path: &Path, hashes: &[&'a String]) -> Vec<&'a String> {
    const PIECE: usize = 1 << 20;
    let overlap = hashes.iter().map(|h| h.len()).max().unwrap_or(0);
    let Ok(mut file) = fs::File::open(path) else { return hashes.to_vec() };
    let mut found: HashSet<&'a String> = HashSet::new();
    let mut buffer = vec![0; PIECE + overlap];
    let mut kept = 0;
    loop {
        let read = match io::Read::read(&mut file, &mut buffer[kept..]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return hashes.to_vec(),
        };
        let filled = kept + read;
        found.extend(hashes.iter().copied().filter(|h| contains(&buffer[..filled], h.as_bytes())));
        kept = overlap.min(filled);
        buffer.copy_within(filled - kept..filled, 0);
    }
    found.into_iter().collect()
}

fn is_paste_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Removes a note and any line in `MEMORY.md` that links to it.
fn remove_note(paths: &Paths, note: &Note, failures: &mut Vec<Failure>) {
    let before = failures.len();
    remove(paths, &note.path, failures);
    if failures.len() != before {
        return;
    }
    let Some(index) = note.path.parent().map(|d| d.join(MEMORY_INDEX)) else { return };
    let Ok(text) = fs::read(&index) else { return };
    let link = format!("]({})", note.name());
    let kept: Vec<u8> = text
        .split_inclusive(|b| *b == b'\n')
        .filter(|line| !line.windows(link.len()).any(|w| w == link.as_bytes()))
        .flatten()
        .copied()
        .collect();
    if kept.len() != text.len()
        && let Err(e) = replace_file(&index, &kept, || true)
    {
        failures.push(Failure { path: index, error: e.to_string() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::unix::fs::symlink;

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const HOUR: Duration = Duration::from_secs(3600);

    struct Tree {
        dir: tempfile::TempDir,
        paths: Paths,
        proc: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let paths = Paths {
                claude: root.join("claude"),
                index: root.join("cache/index.db"),
                config: root.join("config.toml"),
                claude_json: root.join("claude.json"),
                state: root.join("state/ccm"),
            };
            fs::create_dir_all(paths.projects()).unwrap();
            fs::create_dir_all(paths.sessions()).unwrap();
            let proc = root.join("proc");
            fs::create_dir_all(&proc).unwrap();
            Self { dir, paths, proc }
        }

        fn claude(&self) -> &Path {
            &self.paths.claude
        }

        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.claude().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        }

        /// Old enough that the "may still be running" check doesn't apply.
        fn transcript(&self, project: &str, id: &str, body: &str) -> PathBuf {
            let path = self.write(&format!("projects/{project}/{id}.jsonl"), body);
            age(&path);
            path
        }

        fn files(&self) -> BTreeSet<String> {
            fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
                for e in fs::read_dir(dir).unwrap().flatten() {
                    if e.file_type().unwrap().is_dir() {
                        walk(root, &e.path(), out);
                    } else {
                        out.insert(e.path().strip_prefix(root).unwrap().display().to_string());
                    }
                }
            }
            let mut out = BTreeSet::new();
            walk(self.claude(), self.claude(), &mut out);
            out
        }

        fn plan(&self, ids: &[&str]) -> Plan {
            let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
            plan(&self.paths, &self.proc, &ids)
        }

        fn run(&self, plan: &Plan, notes: bool) -> Outcome {
            execute(&self.paths, &self.proc, plan, notes).unwrap()
        }

        /// A registry entry and matching process, which is what makes a session count as open.
        fn open(&self, id: &str) {
            let fields: Vec<String> = (3..=21).map(|i| i.to_string()).collect();
            let proc = self.proc.join("4242");
            fs::create_dir_all(&proc).unwrap();
            fs::write(proc.join("stat"), format!("4242 (claude) {} 777 23\n", fields.join(" "))).unwrap();
            fs::write(
                self.paths.sessions().join("4242.json"),
                format!(r#"{{"pid":4242,"sessionId":"{id}","procStart":"777"}}"#),
            )
            .unwrap();
        }
    }

    fn age(path: &Path) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - HOUR).unwrap();
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn write_tool(path: &Path) -> String {
        format!(
            concat!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Write","input":{{"file_path":"{}"}}}}]}}}}"#,
                "\n",
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":"ok"}}]}}}}"#
            ),
            path.display()
        )
    }

    fn history(id: &str, text: &str, hash: Option<&str>) -> String {
        let pasted = hash.map_or("{}".to_string(), |h| format!(r#"{{"1":{{"id":1,"type":"text","contentHash":"{h}"}}}}"#));
        format!(r#"{{"display":"{text}","pastedContents":{pasted},"timestamp":1,"project":"/p","sessionId":"{id}"}}"#) + "\n"
    }

    #[test]
    fn removes_exactly_this_conversations_files_and_nothing_else() {
        let t = Tree::new();
        let (a, b) = (A, B);
        for id in [a, b] {
            t.transcript("-p", id, "{}\n");
            t.write(&format!("projects/-p/{id}/subagents/agent-1.jsonl"), "{}\n");
            t.write(&format!("projects/-p/{id}/tool-results/r.txt"), "x");
            t.write(&format!("projects/-p/{id}/custom-title.json"), "{}");
            t.write(&format!("projects/-p/{id}.cast"), "x");
            t.write(&format!("projects/-p/{id}.precompact.json"), "x");
            t.write(&format!("file-history/{id}/f@v1"), "x");
            t.write(&format!("session-env/{id}/env"), "x");
            t.write(&format!("tasks/{id}/.lock"), "");
            t.write(&format!("debug/{id}.txt"), "x");
            t.write(&format!("telemetry/1p_failed_events.{id}.0f577fe6-6f09-42ec-b922-5c464f7cd0a3.json"), "x");
            let short = &id[..8];
            t.write(&format!("jobs/{short}/state.json"), &format!(r#"{{"sessionId":"{id}","bgIsolation":"none"}}"#));
            t.write(&format!("jobs/{short}/timeline.jsonl"), "{}");
        }
        t.write("projects/-p/memory/MEMORY.md", "- [n](n.md)\n");
        t.write("projects/-p/memory/n.md", "note");
        t.write("agent-memory/x/y.md", "keep");
        t.write("plans/a-plan.md", "keep");
        t.write("shell-snapshots/s.sh", "keep");
        t.write("settings.json", "{}");
        // Another conversation's telemetry whose second ID happens to be this one's.
        t.write(&format!("telemetry/1p_failed_events.{b}.{a}.json"), "x");
        let before = t.files();

        let plan = t.plan(&[a]);
        assert_eq!(plan.skipped, vec![]);
        assert_eq!(plan.items.len(), 1);
        let outcome = t.run(&plan, false);

        assert_eq!(outcome.failures, vec![]);
        assert_eq!(outcome.deleted, vec![a.to_string()]);
        let gone: BTreeSet<String> = before.difference(&t.files()).cloned().collect();
        let expected: BTreeSet<String> = [
            format!("projects/-p/{a}.jsonl"),
            format!("projects/-p/{a}/subagents/agent-1.jsonl"),
            format!("projects/-p/{a}/tool-results/r.txt"),
            format!("projects/-p/{a}/custom-title.json"),
            format!("projects/-p/{a}.cast"),
            format!("projects/-p/{a}.precompact.json"),
            format!("file-history/{a}/f@v1"),
            format!("session-env/{a}/env"),
            format!("tasks/{a}/.lock"),
            format!("debug/{a}.txt"),
            format!("telemetry/1p_failed_events.{a}.0f577fe6-6f09-42ec-b922-5c464f7cd0a3.json"),
            format!("jobs/{}/state.json", &a[..8]),
            format!("jobs/{}/timeline.jsonl", &a[..8]),
        ]
        .into_iter()
        .collect();
        assert_eq!(gone, expected);
        assert!(!t.claude().join("file-history").join(a).exists(), "empty folders go too");
        assert!(!t.claude().join("projects/-p").join(a).exists());
    }

    #[test]
    fn the_space_shown_counts_everything_that_goes() {
        let t = Tree::new();
        t.transcript("-p", A, "12345");
        t.write(&format!("projects/-p/{A}/subagents/a.jsonl"), "1234567890");
        t.write(&format!("file-history/{A}/f"), "123");
        let plan = t.plan(&[A]);
        assert_eq!(plan.bytes(), 18);
    }

    #[test]
    fn open_conversations_are_refused_when_planning() {
        let t = Tree::new();
        let transcript = t.transcript("-p", A, "{}\n");
        t.open(A);
        let plan = t.plan(&[A]);
        assert!(plan.items.is_empty());
        assert_eq!(plan.skipped[0].reason, "it's open");
        assert!(transcript.exists());
    }

    #[test]
    fn a_conversation_opened_after_the_plan_is_refused_too() {
        let t = Tree::new();
        let transcript = t.transcript("-p", A, "{}\n");
        let side = t.write(&format!("file-history/{A}/f"), "x");
        let plan = t.plan(&[A]);
        assert_eq!(plan.items.len(), 1);
        t.open(A);
        let outcome = t.run(&plan, false);
        assert_eq!(outcome.deleted, Vec::<String>::new());
        assert_eq!(outcome.skipped[0].reason, "it's open");
        assert!(transcript.exists() && side.exists());
    }

    #[test]
    fn a_stale_registry_entry_does_not_block_a_delete() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        fs::write(t.paths.sessions().join("99.json"), format!(r#"{{"pid":99,"sessionId":"{A}","procStart":"1"}}"#)).unwrap();
        assert_eq!(t.plan(&[A]).items.len(), 1);
    }

    #[test]
    fn a_recently_written_transcript_may_still_be_running() {
        let t = Tree::new();
        t.write(&format!("projects/-p/{A}.jsonl"), "{}\n");
        let plan = t.plan(&[A]);
        assert!(plan.items.is_empty());
        assert!(plan.skipped[0].reason.contains("last minute"));
    }

    #[test]
    fn a_background_job_that_is_running_is_refused() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let proc = t.proc.join("4242");
        fs::create_dir_all(&proc).unwrap();
        let fields: Vec<String> = (3..=21).map(|i| i.to_string()).collect();
        fs::write(proc.join("stat"), format!("4242 (claude) {} 777 23\n", fields.join(" "))).unwrap();
        t.write("daemon/roster.json", &format!(r#"{{"workers":{{"{}":{{"pid":4242,"procStart":"777","sessionId":"{A}"}}}}}}"#, &A[..8]));
        let plan = t.plan(&[A]);
        assert!(plan.items.is_empty());
        assert!(plan.skipped[0].reason.contains("background"));
    }

    #[test]
    fn a_background_job_that_has_died_does_not_block_a_delete() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        t.transcript("-p", B, "{}\n");
        t.write(
            "daemon/roster.json",
            &format!(r#"{{"workers":{{"{}":{{"pid":4242,"procStart":"777","sessionId":"{A}","dispatch":"{B}"}}}}}}"#, &A[..8]),
        );
        let plan = t.plan(&[A, B]);
        assert_eq!(plan.items.len(), 2, "{:?}", plan.skipped.iter().map(|s| &s.reason).collect::<Vec<_>>());
    }

    #[test]
    fn a_job_that_may_have_a_worktree_is_kept_and_reported() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let state = t.write("jobs/aaaaaaaa/state.json", &format!(r#"{{"sessionId":"{A}","bgIsolation":"worktree"}}"#));
        let plan = t.plan(&[A]);
        assert!(plan.warnings[0].contains("claude rm"));
        t.run(&plan, false);
        assert!(state.exists());
    }

    #[test]
    fn a_job_for_another_conversation_is_kept() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let state = t.write("jobs/bbbbbbbb/state.json", &format!(r#"{{"sessionId":"{B}","resumeSessionId":"{A}","bgIsolation":"none"}}"#));
        t.run(&t.plan(&[A]), false);
        assert!(state.exists());
    }

    #[test]
    fn invalid_ids_never_reach_the_filesystem() {
        let t = Tree::new();
        let outside = t.dir.path().join("precious");
        fs::write(&outside, "x").unwrap();
        for bad in ["../../precious", "", "*", "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA"] {
            let plan = t.plan(&[bad]);
            assert!(plan.items.is_empty(), "{bad}");
            assert_eq!(plan.skipped[0].reason, "its ID isn't valid");
        }
        assert!(outside.exists());
    }

    #[test]
    fn nothing_outside_claude_is_ever_removed() {
        let t = Tree::new();
        let outside = t.dir.path().join("precious");
        fs::write(&outside, "x").unwrap();
        assert!(remove_path(t.claude(), &outside).is_err());
        assert!(remove_path(t.claude(), &t.claude().join("../precious")).is_err());
        assert!(outside.exists());
    }

    #[test]
    fn symlinks_are_removed_without_following_them() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let target = t.dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("precious"), "x").unwrap();
        fs::create_dir_all(t.claude().join("file-history")).unwrap();
        symlink(&target, t.claude().join("file-history").join(A)).unwrap();
        t.run(&t.plan(&[A]), false);
        assert!(target.join("precious").exists());
        assert!(fs::symlink_metadata(t.claude().join("file-history").join(A)).is_err());
    }

    #[test]
    fn a_symlinked_project_folder_is_refused() {
        let t = Tree::new();
        let elsewhere = t.dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let transcript = elsewhere.join(format!("{A}.jsonl"));
        fs::write(&transcript, "{}\n").unwrap();
        age(&transcript);
        symlink(&elsewhere, t.paths.projects().join("-linked")).unwrap();
        let plan = t.plan(&[A]);
        assert!(plan.items.is_empty());
        assert!(plan.skipped[0].reason.contains("normal project folder"));
        assert!(transcript.exists());
    }

    #[test]
    fn the_same_id_in_two_project_folders_goes_from_both() {
        let t = Tree::new();
        let one = t.transcript("-p", A, "{}\n");
        let two = t.transcript("-q", A, "{}\n");
        t.transcript("-q", B, "{}\n");
        let outcome = t.run(&t.plan(&[A]), false);
        assert_eq!(outcome.deleted, vec![A.to_string()]);
        assert!(!one.exists() && !two.exists());
    }

    #[test]
    fn an_emptied_project_folder_goes_but_one_with_memory_stays() {
        let t = Tree::new();
        t.transcript("-empty", A, "{}\n");
        t.transcript("-mem", B, "{}\n");
        t.write("projects/-mem/memory/n.md", "x");
        t.run(&t.plan(&[A, B]), false);
        assert!(!t.paths.projects().join("-empty").exists());
        assert!(t.paths.projects().join("-mem/memory/n.md").exists());
    }

    #[test]
    fn a_fork_keeps_its_transcript() {
        let t = Tree::new();
        t.transcript("-p", A, r#"{"uuid":"1","sessionId":"parent"}"#);
        let fork = t.transcript("-p", B, &format!(r#"{{"uuid":"1","sessionId":"{B}","forkedFrom":{{"sessionId":"{A}"}}}}"#));
        t.run(&t.plan(&[A]), false);
        assert!(fork.exists());
    }

    #[test]
    fn a_failure_reports_what_stayed_and_keeps_the_transcript_to_retry() {
        let t = Tree::new();
        let transcript = t.transcript("-p", A, "{}\n");
        let stuck = t.write(&format!("file-history/{A}/f"), "x");
        let parent = t.claude().join("file-history").join(A);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();

        let outcome = t.run(&t.plan(&[A]), false);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(outcome.deleted, Vec::<String>::new());
        let paths: Vec<&PathBuf> = outcome.failures.iter().map(|f| &f.path).collect();
        assert_eq!(paths, vec![&parent, &transcript]);
        assert!(transcript.exists() && stuck.exists());
    }

    #[test]
    fn history_loses_only_this_conversations_lines() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let (other, garbage) = (history(B, "other", None), "not json at all\n");
        let mine_mentions_b = history(A, B, None);
        let path = t.write(
            "history.jsonl",
            &format!("{}{other}{garbage}{mine_mentions_b}{}", history(A, "mine", None), history(C, A, None)),
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        t.run(&t.plan(&[A]), false);

        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{other}{garbage}{}", history(C, A, None)));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!t.claude().join("history.jsonl.lock").exists());
    }

    #[test]
    fn a_last_line_with_no_newline_survives() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let last = history(B, "last", None);
        let last = last.trim_end();
        let path = t.write("history.jsonl", &format!("{}{last}", history(A, "mine", None)));
        t.run(&t.plan(&[A]), false);
        assert_eq!(fs::read_to_string(path).unwrap(), last);
    }

    #[test]
    fn a_held_history_lock_stops_the_delete_before_anything_changes() {
        let t = Tree::new();
        let transcript = t.transcript("-p", A, "{}\n");
        let side = t.write(&format!("file-history/{A}/f"), "x");
        let history_path = t.write("history.jsonl", &history(A, "mine", None));
        let lock = t.claude().join("history.jsonl.lock");
        fs::create_dir(&lock).unwrap();

        let error = execute(&t.paths, &t.proc, &t.plan(&[A]), false).unwrap_err();

        assert!(error.contains("nothing was deleted"), "{error}");
        assert!(transcript.exists() && side.exists());
        assert!(fs::read_to_string(history_path).unwrap().contains("mine"));
        assert!(lock.exists(), "someone else's lock is left alone");
    }

    #[test]
    fn an_abandoned_history_lock_is_taken_over() {
        let t = Tree::new();
        let transcript = t.transcript("-p", A, "{}\n");
        t.write("history.jsonl", &history(A, "mine", None));
        let lock = t.claude().join("history.jsonl.lock");
        fs::create_dir(&lock).unwrap();
        fs::File::open(&lock).unwrap().set_modified(SystemTime::now() - HOUR).unwrap();

        t.run(&t.plan(&[A]), false);

        assert!(!transcript.exists());
        assert!(!lock.exists());
    }

    #[test]
    fn a_lock_taken_over_midway_stops_the_rewrite() {
        let t = Tree::new();
        let path = t.write("history.jsonl", "old");
        let error = replace_file(&path, b"new", || false).unwrap_err();
        assert!(error.to_string().contains("lost the lock"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(fs::read_dir(t.claude()).unwrap().count(), 3, "no temporary file left");
    }

    const ONLY: &str = "00eef5709c97a5fe";
    const SHARED: &str = "02fc1b293516fe97";
    const IN_TRANSCRIPT: &str = "09f1719ddc447b71";

    #[test]
    fn a_pasted_file_goes_only_when_nothing_references_it() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        t.transcript("-p", B, &format!(r#"{{"note":"{IN_TRANSCRIPT}"}}"#));
        t.write(
            "history.jsonl",
            &[
                history(A, "1", Some(ONLY)),
                history(A, "2", Some(SHARED)),
                history(A, "3", Some(IN_TRANSCRIPT)),
                history(B, "4", Some(SHARED)),
            ]
            .concat(),
        );
        for hash in [ONLY, SHARED, IN_TRANSCRIPT] {
            t.write(&format!("paste-cache/{hash}.txt"), "pasted");
        }
        t.run(&t.plan(&[A]), false);
        let left: BTreeSet<String> = fs::read_dir(t.claude().join("paste-cache"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, set(&[&format!("{SHARED}.txt"), &format!("{IN_TRANSCRIPT}.txt")]));
    }

    #[test]
    fn a_hostile_paste_hash_cannot_reach_another_file() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let settings = t.write("settings.json", "{}");
        t.write("paste-cache/x.txt", "x");
        t.write("history.jsonl", &history(A, "1", Some("../settings.json\u{0}")));
        t.run(&t.plan(&[A]), false);
        assert!(settings.exists());
    }

    struct Notes {
        t: Tree,
        memory: PathBuf,
        only_a: PathBuf,
        by_subagent: PathBuf,
        shared: PathBuf,
        by_hand: PathBuf,
    }

    fn notes() -> Notes {
        let t = Tree::new();
        let memory = t.claude().join("projects/-p/memory");
        let only_a = t.write("projects/-p/memory/only-a.md", "x");
        let by_subagent = t.write("projects/-p/memory/by-subagent.md", "x");
        let shared = t.write("projects/-p/memory/shared.md", "x");
        let by_hand = t.write("projects/-p/memory/by-hand.md", "x");
        t.write(
            "projects/-p/memory/MEMORY.md",
            "# Index\n- [Only](only-a.md) — hook\n- [Sub](by-subagent.md)\n- [Shared](shared.md)\n- [Hand](by-hand.md)\n",
        );
        t.transcript("-p", A, &format!("{}\n{}\n", write_tool(&only_a), write_tool(&shared)));
        t.write(&format!("projects/-p/{A}/subagents/agent.jsonl"), &write_tool(&by_subagent));
        t.transcript("-p", B, &write_tool(&shared));
        Notes { t, memory, only_a, by_subagent, shared, by_hand }
    }

    fn open_process(t: &Tree, pid: u32, id: &str) {
        let proc = t.proc.join(pid.to_string());
        fs::create_dir_all(&proc).unwrap();
        let fields: Vec<String> = (3..=21).map(|i| i.to_string()).collect();
        fs::write(proc.join("stat"), format!("{pid} (claude) {} 777 1\n", fields.join(" "))).unwrap();
        fs::write(t.paths.sessions().join(format!("{pid}.json")), format!(r#"{{"pid":{pid},"sessionId":"{id}","procStart":"777"}}"#)).unwrap();
    }

    #[test]
    fn a_conversation_skipped_at_the_last_moment_keeps_its_notes() {
        let t = Tree::new();
        fs::create_dir_all(t.paths.sessions()).unwrap();
        let note_a = t.write("projects/-p/memory/a.md", "x");
        let note_b = t.write("projects/-p/memory/b.md", "x");
        t.transcript("-p", A, &write_tool(&note_a));
        t.transcript("-p", B, &write_tool(&note_b));
        let plan = t.plan(&[A, B]);
        assert_eq!(plan.notes.len(), 2);
        open_process(&t, 4300, B);
        let outcome = t.run(&plan, true);
        assert_eq!(outcome.deleted, vec![A.to_string()]);
        assert!(!note_a.exists());
        assert!(note_b.exists(), "B is open now, so its note stays");
    }

    #[test]
    fn a_running_session_with_an_unreadable_registry_file_stops_deletes() {
        let t = Tree::new();
        fs::create_dir_all(t.paths.sessions()).unwrap();
        t.transcript("-p", A, "{}\n");
        fs::create_dir_all(t.proc.join("4400")).unwrap();
        fs::write(t.paths.sessions().join("4400.json"), "{ cut off").unwrap();
        let plan = t.plan(&[A]);
        assert!(plan.items.is_empty());
        assert!(plan.skipped[0].reason.contains("can't be read"));
    }

    #[test]
    fn a_session_with_an_unreadable_registry_file_that_appears_later_stops_the_delete() {
        let t = Tree::new();
        fs::create_dir_all(t.paths.sessions()).unwrap();
        let transcript = t.transcript("-p", A, "{}\n");
        let plan = t.plan(&[A]);
        fs::create_dir_all(t.proc.join("4400")).unwrap();
        fs::write(t.paths.sessions().join("4400.json"), "{ cut off").unwrap();
        let outcome = t.run(&plan, false);
        assert!(outcome.deleted.is_empty());
        assert!(transcript.exists());
    }

    #[test]
    fn the_same_note_written_differently_still_counts_as_shared() {
        let t = Tree::new();
        let note = t.write("projects/-p/memory/shared.md", "x");
        let odd = PathBuf::from(note.to_str().unwrap().replace("/memory/", "/memory/./"));
        t.transcript("-p", A, &write_tool(&odd));
        t.transcript("-p", B, &write_tool(&note));
        assert!(t.plan(&[A]).notes.is_empty());
    }

    #[test]
    fn a_write_that_was_denied_does_not_make_a_note_this_conversations() {
        let t = Tree::new();
        let note = t.write("projects/-p/memory/by-hand.md", "x");
        let denied = format!(
            concat!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Write","input":{{"file_path":"{}"}}}}]}}}}"#,
                "\n",
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"denied"}}]}}}}"#
            ),
            note.display()
        );
        t.transcript("-p", A, &denied);
        assert!(t.plan(&[A]).notes.is_empty());
    }

    #[test]
    fn a_write_with_no_recorded_result_does_not_count() {
        let t = Tree::new();
        let note = t.write("projects/-p/memory/n.md", "x");
        let line = format!(r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Write","input":{{"file_path":"{}"}}}}]}}}}"#, note.display());
        t.transcript("-p", A, &line);
        assert!(t.plan(&[A]).notes.is_empty());
    }

    #[test]
    fn a_linked_memory_folder_is_never_offered() {
        let t = Tree::new();
        let elsewhere = t.dir.path().join("synced");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("n.md"), "x").unwrap();
        fs::create_dir_all(t.claude().join("projects/-p")).unwrap();
        symlink(&elsewhere, t.claude().join("projects/-p/memory")).unwrap();
        let through = t.claude().join("projects/-p/memory/n.md");
        t.transcript("-p", A, &write_tool(&through));
        assert!(t.plan(&[A]).notes.is_empty());
        assert!(elsewhere.join("n.md").exists());
    }

    #[test]
    fn the_lock_is_released_when_the_history_edit_fails() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let real = t.dir.path().join("real-history");
        fs::write(&real, format!("{}\n", history(A, "x", None))).unwrap();
        symlink(&real, t.claude().join("history.jsonl")).unwrap();
        let plan = t.plan(&[A]);
        assert!(execute(&t.paths, &t.proc, &plan, false).is_err());
        assert!(!t.claude().join("history.jsonl.lock").exists());
    }

    #[test]
    fn a_folder_path_with_dots_refuses_before_history_is_touched() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let history_path = t.write("history.jsonl", &format!("{}\n", history(A, "x", None)));
        let before = fs::read(&history_path).unwrap();
        let mut paths = t.paths.clone();
        paths.claude = t.dir.path().join("x/../claude");
        fs::create_dir_all(t.dir.path().join("x")).unwrap();
        let plan = plan(&paths, &t.proc, &[A.to_string()]);
        assert_eq!(plan.items.len(), 1);
        assert!(execute(&paths, &t.proc, &plan, false).unwrap_err().contains("nothing was deleted"));
        assert_eq!(fs::read(&history_path).unwrap(), before);
    }

    #[test]
    fn hashes_across_a_piece_boundary_are_found_and_an_unreadable_file_uses_them_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jsonl");
        let hash = "0123456789abcdef".to_string();
        let mut body = vec![b'x'; (1 << 20) - 8];
        body.extend_from_slice(hash.as_bytes());
        fs::write(&path, &body).unwrap();
        assert_eq!(hashes_in_file(&path, &[&hash]), vec![&hash]);
        assert_eq!(hashes_in_file(&dir.path().join("gone"), &[&hash]), vec![&hash]);
        fs::write(&path, "nothing here").unwrap();
        assert!(hashes_in_file(&path, &[&hash]).is_empty());
    }

    #[test]
    fn only_notes_this_conversation_alone_wrote_are_offered() {
        let n = notes();
        let names: Vec<String> = n.t.plan(&[A]).notes.iter().map(Note::name).collect();
        assert_eq!(names, vec!["by-subagent.md", "only-a.md"]);
    }

    #[test]
    fn memory_is_kept_by_default() {
        let n = notes();
        let index = fs::read_to_string(n.memory.join(MEMORY_INDEX)).unwrap();
        n.t.run(&n.t.plan(&[A]), false);
        assert!(n.only_a.exists() && n.by_subagent.exists() && n.shared.exists() && n.by_hand.exists());
        assert_eq!(fs::read_to_string(n.memory.join(MEMORY_INDEX)).unwrap(), index);
    }

    #[test]
    fn choosing_memory_removes_sole_notes_and_their_index_lines() {
        let n = notes();
        n.t.run(&n.t.plan(&[A]), true);
        assert!(!n.only_a.exists() && !n.by_subagent.exists());
        assert!(n.shared.exists() && n.by_hand.exists());
        assert_eq!(
            fs::read_to_string(n.memory.join(MEMORY_INDEX)).unwrap(),
            "# Index\n- [Shared](shared.md)\n- [Hand](by-hand.md)\n"
        );
    }

    #[test]
    fn a_note_written_by_everyone_being_deleted_is_offered() {
        let n = notes();
        let names: Vec<String> = n.t.plan(&[A, B]).notes.iter().map(Note::name).collect();
        assert_eq!(names, vec!["by-subagent.md", "only-a.md", "shared.md"]);
    }

    #[test]
    fn an_open_conversations_notes_stay_when_it_is_in_the_selection() {
        let n = notes();
        n.t.open(B);
        let plan = n.t.plan(&[A, B]);
        assert_eq!(plan.items.len(), 1);
        assert!(!plan.notes.iter().any(|note| note.name() == "shared.md"));
    }

    #[test]
    fn the_memory_index_and_files_elsewhere_are_never_offered() {
        let t = Tree::new();
        let index = t.write("projects/-p/memory/MEMORY.md", "x");
        let elsewhere = t.write("projects/-q/memory/other.md", "x");
        let sub = t.write("projects/-p/memory/deep/x.md", "x");
        let outside = t.dir.path().join("memory/x.md");
        let lines = [&index, &elsewhere, &sub, &outside].map(|p| write_tool(p)).join("\n");
        t.transcript("-p", A, &lines);
        assert_eq!(t.plan(&[A]).notes, vec![]);
    }

    #[test]
    fn a_batch_rewrites_history_once_and_deletes_them_all() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        t.transcript("-p", B, "{}\n");
        let keep = t.transcript("-p", C, "{}\n");
        let path = t.write("history.jsonl", &[history(A, "1", None), history(B, "2", None), history(C, "3", None)].concat());
        let plan = t.plan(&[A, B]);
        let outcome = t.run(&plan, false);
        assert_eq!(outcome.deleted, vec![A.to_string(), B.to_string()]);
        assert_eq!(fs::read_to_string(path).unwrap(), history(C, "3", None));
        assert!(keep.exists());
    }

    #[test]
    fn missing_folders_are_not_an_error() {
        let t = Tree::new();
        t.transcript("-p", A, "{}\n");
        let outcome = t.run(&t.plan(&[A]), false);
        assert_eq!(outcome.failures, vec![]);
        assert_eq!(outcome.deleted, vec![A.to_string()]);
    }
}
