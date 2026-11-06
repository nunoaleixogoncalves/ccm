//! Codex: one file per conversation under `$CODEX_HOME/sessions/YYYY/MM/DD/`, named
//! `rollout-<local time>-<id>.jsonl`. Each line is `{"timestamp", "type", "payload"}`.
//!
//! Read only. Codex also keeps databases next to these files, which `ccm` never opens.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;

use super::{Found, Live, Tool};
use crate::delete::{Failure, Item, Outcome, Plan, Skipped};
use crate::format::sanitize;
use crate::index::SearchDoc;
use crate::model::{FolderVisit, Session, Source};
use crate::paths::is_uuid;
use crate::preview::{self, Preview};
use crate::transcript::label;

pub struct Codex {
    home: PathBuf,
}

impl Codex {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    /// `$CODEX_HOME`, or `~/.codex`.
    pub fn home_from(var: impl Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
        var("CODEX_HOME").filter(|v| !v.is_empty()).map_or_else(|| home.join(".codex"), PathBuf::from)
    }

    pub fn sessions(home: &Path) -> PathBuf {
        home.join("sessions")
    }
}

impl Tool for Codex {
    fn source(&self) -> Source {
        Source::Codex
    }

    fn find(&self) -> Vec<Found> {
        let names = read_names(&self.home.join("session_index.jsonl"));
        let mut by_id: HashMap<String, Found> = HashMap::new();
        for (path, meta) in rollout_files(&Self::sessions(&self.home)) {
            let Some(id) = rollout_id(&path) else { continue };
            let mut mtime_ns = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
            let name = names.get(id).map(|(name, at)| {
                // A rename in Codex only touches the index, so it must count as a change too.
                mtime_ns = mtime_ns.max(*at);
                name.clone()
            });
            let found = Found { id: id.to_string(), path: path.clone(), size: meta.len(), mtime_ns, name };
            // A rolled-back conversation gets a new file and keeps its ID. The newest is current.
            match by_id.get(id) {
                Some(existing) if existing.mtime_ns >= found.mtime_ns => {}
                _ => {
                    by_id.insert(id.to_string(), found);
                }
            }
        }
        by_id.into_values().collect()
    }

    fn read(&self, found: &Found) -> Option<(Session, SearchDoc)> {
        let file = fs::File::open(&found.path).ok()?;
        let p = parse(BufReader::with_capacity(1 << 20, file)).ok()?;
        if !p.interactive {
            return None;
        }
        let name = found.name.clone().filter(|n| !n.trim().is_empty()).or(p.first_prompt).map(|n| label(&n)).unwrap_or_default();
        let doc = SearchDoc {
            title: name.clone(),
            path: p.folder.clone().unwrap_or_default(),
            branch: p.branch.clone().unwrap_or_default(),
            prompts: p.prompt_text,
            replies: p.reply_text,
        };
        let session = Session {
            id: found.id.clone(),
            source: Source::Codex,
            transcript_path: found.path.to_string_lossy().into_owned(),
            transcript_size: found.size,
            transcript_mtime_ns: found.mtime_ns,
            size_bytes: found.size,
            name,
            folder: p.folder,
            folders: p.folders,
            branch: p.branch,
            model: p.model,
            started_at_ms: p.started_at_ms,
            last_active_ms: p.last_active_ms.or(Some(found.mtime_ns / 1_000_000)),
            context_used: p.context_used,
            context_window: p.context_window,
            prompts: p.prompts,
            replies: p.replies,
            cost_usd: None,
            lines_added: None,
            lines_removed: None,
            pr_url: None,
            continued_in: None,
            forked_from: p.forked_from,
            recaps: Vec::new(),
            bad_lines: p.bad_lines,
            open: false,
            crashed: false,
            window_size: None,
            folder_exists: true,
        };
        Some((session, doc))
    }

    fn measure(&self, found: &Found) -> u64 {
        found.size
    }

    fn live(&self) -> Live {
        Live::default()
    }
}

/// Every `rollout-*.jsonl` under `sessions/`, three folders deep. Compressed `.jsonl.zst` files
/// aren't read yet.
fn rollout_files(sessions: &Path) -> Vec<(PathBuf, fs::Metadata)> {
    let mut out = Vec::new();
    let dirs = |p: &Path| -> Vec<PathBuf> {
        fs::read_dir(p).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()
    };
    for year in dirs(sessions) {
        for month in dirs(&year) {
            for day in dirs(&month) {
                let Ok(files) = fs::read_dir(&day) else { continue };
                for file in files.flatten() {
                    let path = file.path();
                    let Ok(meta) = file.metadata() else { continue };
                    if meta.is_file() && rollout_id(&path).is_some() {
                        out.push((path, meta));
                    }
                }
            }
        }
    }
    out
}

/// The ID in `rollout-<time>-<id>.jsonl`, or `rollout-<time>-<id>_<rollout>.jsonl` after a
/// rollback.
fn rollout_id(path: &Path) -> Option<&str> {
    let stem = path.file_name()?.to_str()?.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let stem = stem.split_once('_').map_or(stem, |(id, _)| id);
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    is_uuid(id).then_some(id)
}

/// Names set with `/rename`, from Codex's append-only index, where the last line for an ID
/// wins. Each comes with when it was set, in nanoseconds.
fn read_names(path: &Path) -> HashMap<String, (String, i64)> {
    #[derive(Deserialize)]
    struct Entry {
        id: String,
        thread_name: Option<String>,
        updated_at: Option<String>,
    }
    let mut out = HashMap::new();
    let Ok(file) = fs::File::open(path) else { return out };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(e) = serde_json::from_str::<Entry>(&line) else { continue };
        let at = e.updated_at.as_deref().and_then(parse_ns).unwrap_or(0);
        match e.thread_name {
            Some(name) => out.insert(e.id, (name, at)),
            None => out.remove(&e.id),
        };
    }
    out
}

#[derive(Debug, Default)]
struct Parsed {
    interactive: bool,
    forked_from: Option<String>,
    folder: Option<String>,
    folders: Vec<FolderVisit>,
    branch: Option<String>,
    model: Option<String>,
    started_at_ms: Option<i64>,
    last_active_ms: Option<i64>,
    context_used: Option<u64>,
    context_window: Option<u64>,
    prompts: u64,
    replies: u64,
    first_prompt: Option<String>,
    prompt_text: String,
    reply_text: String,
    bad_lines: u64,
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
}

fn parse<R: BufRead>(mut reader: R) -> std::io::Result<Parsed> {
    let mut p = Parsed::default();
    let mut seen_meta = false;
    let mut records = 0u64;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(line) = serde_json::from_slice::<Line>(&buf) else {
            p.bad_lines += 1;
            continue;
        };
        let at = line.timestamp.as_deref().and_then(parse_ns).map(|ns| ns / 1_000_000);
        if at.is_some() {
            p.last_active_ms = at;
        }
        let payload = &line.payload;
        match line.kind.as_str() {
            // A fork copies its parent's lines, including its first line, so only the first
            // one describes this conversation.
            "session_meta" if !seen_meta => {
                seen_meta = true;
                p.interactive = is_interactive(&payload["source"]);
                p.started_at_ms = payload["timestamp"].as_str().and_then(parse_ns).map(|ns| ns / 1_000_000).or(at);
                p.branch = text(&payload["git"]["branch"]);
                p.forked_from = text(&payload["forked_from_id"]).filter(|id| is_uuid(id));
                if let Some(cwd) = text(&payload["cwd"]) {
                    visit(&mut p, &cwd, &mut records, at);
                }
            }
            "turn_context" => {
                if let Some(cwd) = text(&payload["cwd"]) {
                    visit(&mut p, &cwd, &mut records, at);
                }
                if let Some(model) = text(&payload["model"]) {
                    p.model = Some(model);
                }
            }
            "event_msg" => match payload["type"].as_str() {
                Some("user_message") => {
                    let message = payload["message"].as_str().unwrap_or_default();
                    p.prompts += 1;
                    if p.first_prompt.is_none() && !message.trim().is_empty() {
                        p.first_prompt = Some(message.trim().to_string());
                    }
                    push_text(&mut p.prompt_text, message);
                }
                Some("agent_message") => {
                    p.replies += 1;
                    push_text(&mut p.reply_text, payload["message"].as_str().unwrap_or_default());
                }
                Some("token_count") => {
                    let info = &payload["info"];
                    if let Some(used) = info["last_token_usage"]["total_tokens"].as_u64() {
                        p.context_used = Some(used);
                    }
                    if let Some(window) = info["model_context_window"].as_u64() {
                        p.context_window = Some(window);
                    }
                }
                Some("task_started") => {
                    if let Some(window) = payload["model_context_window"].as_u64() {
                        p.context_window = Some(window);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    p.folder = p.folders.first().map(|v| v.path.clone());
    Ok(p)
}

/// Conversations someone typed in. `codex exec`, MCP, subagent and internal ones aren't.
fn is_interactive(source: &Value) -> bool {
    match source {
        Value::String(s) => matches!(s.as_str(), "cli" | "vscode"),
        Value::Object(o) => o.contains_key("custom"),
        // Very old files have no source; they came from the terminal.
        Value::Null => true,
        _ => false,
    }
}

fn visit(p: &mut Parsed, cwd: &str, records: &mut u64, at: Option<i64>) {
    *records += 1;
    match p.folders.iter_mut().find(|v| v.path == cwd) {
        Some(v) => {
            v.records += 1;
            v.last_record = *records;
            v.last_ms = at.or(v.last_ms);
        }
        None => p.folders.push(FolderVisit { path: cwd.to_string(), records: 1, last_record: *records, last_ms: at }),
    }
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn push_text(all: &mut String, text: &str) {
    if !all.is_empty() {
        all.push('\n');
    }
    all.push_str(text);
}

fn parse_ns(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().and_then(|d| d.timestamp_nanos_opt())
}

/// The latest prompts and replies, with tool calls counted under the reply they belong to.
pub fn preview(path: &Path) -> std::io::Result<Preview> {
    preview::read_with(path, |turns, line| {
        let Ok(line) = serde_json::from_slice::<Line>(line) else { return };
        let payload = &line.payload;
        match (line.kind.as_str(), payload["type"].as_str()) {
            ("event_msg", Some("user_message")) => {
                let message = payload["message"].as_str().unwrap_or_default();
                if !message.trim().is_empty() {
                    preview::push_prompt(turns, message.to_string());
                }
            }
            ("event_msg", Some("agent_message")) => {
                preview::push_reply(turns, payload["message"].as_str(), 0);
            }
            ("response_item", Some("function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call")) => {
                preview::push_reply(turns, None, 1);
            }
            _ => {}
        }
    })
}

/// How long `codex delete` gets before `ccm` gives up on it.
const DELETE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for Codex's lock on its prompt history.
const HISTORY_LOCK_WAIT: Duration = Duration::from_secs(3);

/// What deleting these Codex conversations would remove. Codex's own `codex delete` does the
/// work, because it also clears the records Codex keeps in its databases. Without it, nothing
/// can be deleted.
pub fn plan_delete(sessions: &[&Session], program: Option<&Path>) -> Plan {
    let mut plan = Plan::default();
    for s in sessions {
        let reason = if program.is_none() {
            Some("the codex command isn't installed, and ccm deletes Codex conversations only through it")
        } else if s.open {
            Some("it's open")
        } else if !is_uuid(&s.id) {
            Some("its ID isn't valid")
        } else {
            None
        };
        match reason {
            Some(reason) => plan.skipped.push(Skipped { id: s.id.clone(), reason: reason.into() }),
            None => plan.items.push(Item {
                id: s.id.clone(),
                source: Source::Codex,
                side: Vec::new(),
                transcripts: vec![PathBuf::from(&s.transcript_path)],
                bytes: s.size_bytes,
            }),
        }
    }
    if !plan.items.is_empty() {
        plan.warnings.push("Codex conversations are deleted with codex delete, which also clears Codex's own records of them, and their prompts are taken out of Codex's history.".into());
    }
    plan
}

/// Deletes each conversation with `codex delete --force`, then removes its prompts from Codex's
/// prompt history, which that command leaves. Codex refuses a conversation that's running or
/// that a fork still uses, and its reason is reported.
pub fn delete(program: &Path, home: &Path, items: &[&Item]) -> Outcome {
    let mut outcome = Outcome::default();
    for item in items {
        if !is_uuid(&item.id) {
            outcome.skipped.push(Skipped { id: item.id.clone(), reason: "its ID isn't valid".into() });
            continue;
        }
        match run_delete(program, home, &item.id) {
            Ok(()) => outcome.deleted.push(item.id.clone()),
            Err(reason) => outcome.skipped.push(Skipped { id: item.id.clone(), reason }),
        }
    }
    if !outcome.deleted.is_empty() {
        let history = home.join("history.jsonl");
        let ids: Vec<&str> = outcome.deleted.iter().map(String::as_str).collect();
        if let Err(e) = remove_history_lines(&history, &ids) {
            outcome.failures.push(Failure { path: history, error: format!("its prompts stay in Codex's history: {e}") });
        }
    }
    outcome
}

fn run_delete(program: &Path, home: &Path, id: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(["delete", "--force", id])
        .env("CODEX_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("codex couldn't be started: {e}"))?;
    // Read as it comes, so a chatty codex can't fill the pipe and stall.
    let mut stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut said = String::new();
        if let Some(err) = stderr.as_mut() {
            let _ = err.take(64 * 1024).read_to_string(&mut said);
        }
        said
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > DELETE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("codex delete didn't finish in time".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("codex delete failed: {e}")),
        }
    };
    let said = reader.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let said = said.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_string();
    Err(if said.is_empty() { format!("codex delete failed ({status})") } else { format!("codex said: {}", sanitize(&said)) })
}

/// Rewrites Codex's `history.jsonl` without these conversations' prompts. Codex appends to it
/// and trims it in place under an exclusive lock on the file, so this holds the same lock and
/// keeps the same file. Lines that can't be read stay as they are.
fn remove_history_lines(history: &Path, ids: &[&str]) -> std::io::Result<()> {
    #[derive(Deserialize)]
    struct Entry {
        session_id: Option<String>,
    }
    let mut file = match fs::OpenOptions::new().read(true).write(true).open(history) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if started.elapsed() < HISTORY_LOCK_WAIT => std::thread::sleep(Duration::from_millis(50)),
            Err(fs::TryLockError::WouldBlock) => return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "Codex kept it locked")),
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut kept = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let drop = serde_json::from_slice::<Entry>(line).ok().and_then(|e| e.session_id).is_some_and(|s| ids.contains(&s.as_str()));
        if !drop {
            kept.extend_from_slice(line);
        }
    }
    if kept.len() != bytes.len() {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&kept)?;
        file.set_len(kept.len() as u64)?;
        file.sync_all()?;
    }
    file.unlock()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::{Role, Turn};

    const ID: &str = "019a5a1e-7c3b-7d21-9f4e-2b6c1d8e0a11";

    /// Invented, in the format Codex 0.167 writes.
    fn rollout(source: &str) -> String {
        [
            format!(r#"{{"timestamp":"2026-11-05T14:03:12.418Z","type":"session_meta","payload":{{"session_id":"{ID}","id":"{ID}","timestamp":"2026-11-05T14:03:12.418Z","cwd":"/home/dev/proj","originator":"codex-tui","cli_version":"0.167.0","source":{source},"model_provider":"openai","git":{{"branch":"main"}}}}}}"#),
            r#"{"timestamp":"2026-11-05T14:03:20.102Z","type":"turn_context","payload":{"turn_id":"t1","cwd":"/home/dev/proj","model":"gpt-5.5-codex"}}"#.into(),
            r#"{"timestamp":"2026-11-05T14:03:20.105Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1","model_context_window":272000}}"#.into(),
            r#"{"timestamp":"2026-11-05T14:03:20.110Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>injected</environment_context>"}]}}"#.into(),
            r#"{"timestamp":"2026-11-05T14:03:20.111Z","type":"event_msg","payload":{"type":"user_message","message":"Fix the failing parser test","local_images":[]}}"#.into(),
            r#"{"timestamp":"2026-11-05T14:03:30.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#.into(),
            r#"{"timestamp":"2026-11-05T14:03:41.874Z","type":"event_msg","payload":{"type":"agent_message","message":"Fixed the off-by-one in parse_header.","phase":"final_answer"}}"#.into(),
            r#"{"timestamp":"2026-11-05T14:03:41.990Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":12034,"output_tokens":412,"total_tokens":12446},"model_context_window":272000}}}"#.into(),
        ]
        .join("\n")
            + "\n"
    }

    struct Home {
        dir: tempfile::TempDir,
    }

    impl Home {
        fn new() -> Self {
            Self { dir: tempfile::tempdir().unwrap() }
        }
        fn put(&self, day: &str, file: &str, body: &str) -> PathBuf {
            let d = self.dir.path().join("sessions").join(day);
            fs::create_dir_all(&d).unwrap();
            let p = d.join(file);
            fs::write(&p, body).unwrap();
            p
        }
        fn codex(&self) -> Codex {
            Codex::new(self.dir.path().to_path_buf())
        }
        fn read_all(&self) -> Vec<Session> {
            let c = self.codex();
            c.find().iter().filter_map(|f| c.read(f)).map(|(s, _)| s).collect()
        }
    }

    fn file(id: &str) -> String {
        format!("rollout-2026-11-05T14-03-12-{id}.jsonl")
    }

    #[test]
    fn a_conversation_is_read_with_its_folder_model_branch_and_context() {
        let h = Home::new();
        h.put("2026/11/05", &file(ID), &rollout(r#""cli""#));
        let c = h.codex();
        let found = c.find();
        let (s, doc) = c.read(&found[0]).unwrap();
        assert_eq!(s.id, ID);
        assert_eq!(s.source, Source::Codex);
        assert_eq!(s.name, "Fix the failing parser test");
        assert_eq!(s.folder.as_deref(), Some("/home/dev/proj"));
        assert_eq!(s.folders.len(), 1);
        assert_eq!(s.model.as_deref(), Some("gpt-5.5-codex"));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!((s.prompts, s.replies), (1, 1));
        assert_eq!((s.context_used, s.context_window), (Some(12446), Some(272_000)));
        assert_eq!(s.started_at_ms, Some(1_793_887_392_418));
        assert_eq!(s.last_active_ms, Some(1_793_887_421_990));
        assert_eq!(s.bad_lines, 0);
        assert_eq!(doc.prompts, "Fix the failing parser test");
        assert!(!doc.prompts.contains("injected"), "context Codex adds isn't a prompt");
        assert_eq!(doc.replies, "Fixed the off-by-one in parse_header.");
    }

    #[test]
    fn only_conversations_someone_typed_in_are_listed() {
        let h = Home::new();
        let ids = ["11111111-1111-4111-8111-111111111111", "22222222-2222-4222-8222-222222222222", "33333333-3333-4333-8333-333333333333", "44444444-4444-4444-8444-444444444444", "55555555-5555-4555-8555-555555555555"];
        let sources = [r#""cli""#, r#""vscode""#, r#""exec""#, r#"{"subagent":"review"}"#, r#"{"custom":"atlas"}"#];
        for (id, source) in ids.iter().zip(sources) {
            h.put("2026/11/05", &file(id), &rollout(source).replace(ID, id));
        }
        let mut listed: Vec<String> = h.read_all().into_iter().map(|s| s.id).collect();
        listed.sort();
        assert_eq!(listed, [ids[0], ids[1], ids[4]]);
    }

    #[test]
    fn a_fork_takes_only_its_own_first_line() {
        let h = Home::new();
        let fork = "66666666-6666-4666-8666-666666666666";
        let own = rollout(r#""cli""#).replace(ID, fork).replace("/home/dev/proj", "/home/dev/fork");
        let parent = rollout(r#""exec""#);
        let first = own.lines().next().unwrap().replace(r#""id":"66666666"#, &format!(r#""forked_from_id":"{ID}","id":"66666666"#));
        h.put("2026/11/06", &file(fork), &format!("{first}\n{parent}"));
        let s = &h.read_all()[0];
        assert_eq!(s.id, fork);
        assert_eq!(s.folder.as_deref(), Some("/home/dev/fork"));
        assert_eq!(s.forked_from.as_deref(), Some(ID));
    }

    #[test]
    fn the_folder_can_change_and_the_last_one_is_known() {
        let h = Home::new();
        let moved = r#"{"timestamp":"2026-11-05T15:00:00.000Z","type":"turn_context","payload":{"cwd":"/home/dev/proj-wt","model":"gpt-5.5-codex"}}"#;
        h.put("2026/11/05", &file(ID), &format!("{}{moved}\n", rollout(r#""cli""#)));
        let s = &h.read_all()[0];
        assert_eq!(s.folder.as_deref(), Some("/home/dev/proj"));
        assert_eq!(s.last_folder(), Some("/home/dev/proj-wt"));
    }

    #[test]
    fn a_name_from_rename_wins_and_a_new_one_counts_as_a_change() {
        let h = Home::new();
        h.put("2026/11/05", &file(ID), &rollout(r#""cli""#));
        let index = h.dir.path().join("session_index.jsonl");
        let line = |name: &str, at: &str| format!("{{\"id\":\"{ID}\",\"thread_name\":\"{name}\",\"updated_at\":\"{at}\"}}\n");
        fs::write(&index, line("first name", "2030-01-01T00:00:00Z")).unwrap();
        let before = h.codex().find()[0].mtime_ns;
        assert_eq!(h.read_all()[0].name, "first name");
        fs::write(&index, line("first name", "2030-01-01T00:00:00Z") + &line("parser fix", "2030-01-02T00:00:00Z")).unwrap();
        let after = h.codex().find()[0].mtime_ns;
        assert!(after > before);
        assert_eq!(h.read_all()[0].name, "parser fix");
    }

    #[test]
    fn other_files_and_broken_lines_are_handled() {
        let h = Home::new();
        h.put("2026/11/05", "notes.txt", "x");
        h.put("2026/11/05", "rollout-2026-11-05T14-03-12-not-a-uuid.jsonl", "{}");
        h.put("2026/11/05", &format!("rollout-2026-11-05T14-03-12-{ID}.jsonl.zst"), "x");
        h.put("misplaced", &file(ID), &rollout(r#""cli""#));
        assert!(h.codex().find().is_empty());
        h.put("2026/11/05", &file(ID), &format!("{}{{broken\n", rollout(r#""cli""#)));
        assert_eq!(h.read_all()[0].bad_lines, 1);
    }

    #[test]
    fn after_a_rollback_the_newest_file_of_a_conversation_is_used() {
        let h = Home::new();
        let old = h.put("2026/11/05", &file(ID), &rollout(r#""cli""#));
        let new = h.put("2026/11/05", &format!("rollout-2026-11-05T16-00-00-{ID}_77777777-7777-4777-8777-777777777777.jsonl"), &rollout(r#""cli""#));
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        fs::File::options().write(true).open(&old).unwrap().set_modified(t).unwrap();
        let found = h.codex().find();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, new);
    }

    #[test]
    fn codex_home_comes_from_the_environment() {
        let home = Path::new("/home/dev");
        assert_eq!(Codex::home_from(|_| None, home), Path::new("/home/dev/.codex"));
        assert_eq!(Codex::home_from(|_| Some("/elsewhere".into()), home), Path::new("/elsewhere"));
        assert_eq!(Codex::home_from(|_| Some(String::new()), home), Path::new("/home/dev/.codex"));
    }

    #[test]
    fn the_preview_shows_prompts_and_replies_with_tool_calls_counted() {
        let h = Home::new();
        let p = h.put("2026/11/05", &file(ID), &rollout(r#""cli""#));
        let turns = preview(&p).unwrap().turns;
        assert_eq!(
            turns,
            [
                Turn { role: Role::You, text: "Fix the failing parser test".into(), tool_calls: 0 },
                Turn { role: Role::Agent, text: "Fixed the off-by-one in parse_header.".into(), tool_calls: 1 },
            ]
        );
    }

    /// A stand-in for `codex` that logs how it was called, and refuses one ID the way Codex
    /// refuses a running conversation.
    fn fake_codex(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("codex");
        let script = format!(
            "#!/bin/sh\necho \"$CODEX_HOME $*\" >> '{}/calls'\nif [ \"$3\" = '{BUSY}' ]; then echo 'warning: noise' >&2; echo 'Error: thread {BUSY} is in use by another process' >&2; exit 1; fi\n",
            dir.display()
        );
        fs::write(&p, script).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    const BUSY: &str = "88888888-8888-4888-8888-888888888888";

    fn item(id: &str) -> Item {
        Item { id: id.into(), source: Source::Codex, side: vec![], transcripts: vec![], bytes: 10 }
    }

    #[test]
    fn delete_runs_codex_delete_and_removes_only_those_prompts_from_its_history() {
        let h = Home::new();
        let bin = tempfile::tempdir().unwrap();
        let codex = fake_codex(bin.path());
        let history = h.dir.path().join("history.jsonl");
        let line = |id: &str, text: &str| format!("{{\"session_id\":\"{id}\",\"ts\":1,\"text\":\"{text}\"}}\n");
        let other = "99999999-9999-4999-8999-999999999999";
        fs::write(&history, [line(ID, "gone"), line(other, "kept"), "{broken\n".into(), line(BUSY, "busy stays"), line(ID, "gone too")].concat()).unwrap();

        let (ok, busy, bad) = (item(ID), item(BUSY), item("../../etc"));
        let outcome = delete(&codex, h.dir.path(), &[&ok, &busy, &bad]);
        assert_eq!(outcome.deleted, [ID]);
        assert_eq!(outcome.skipped.len(), 2);
        assert_eq!(outcome.skipped[0].reason, format!("codex said: Error: thread {BUSY} is in use by another process"));
        assert_eq!(outcome.skipped[1].reason, "its ID isn't valid");
        assert!(outcome.failures.is_empty());

        let calls = fs::read_to_string(bin.path().join("calls")).unwrap();
        let home = h.dir.path().display();
        assert_eq!(calls, format!("{home} delete --force {ID}\n{home} delete --force {BUSY}\n"));
        assert_eq!(fs::read_to_string(&history).unwrap(), [line(other, "kept"), "{broken\n".into(), line(BUSY, "busy stays")].concat());
    }

    #[test]
    fn the_history_waits_for_codex_to_let_go_of_its_lock() {
        let h = Home::new();
        let history = h.dir.path().join("history.jsonl");
        fs::write(&history, format!("{{\"session_id\":\"{ID}\",\"text\":\"x\"}}\n")).unwrap();
        let held = fs::File::open(&history).unwrap();
        held.lock().unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            held.unlock().unwrap();
        });
        remove_history_lines(&history, &[ID]).unwrap();
        releaser.join().unwrap();
        assert_eq!(fs::read_to_string(&history).unwrap(), "");
        assert!(remove_history_lines(&h.dir.path().join("none.jsonl"), &[ID]).is_ok());
    }

    #[test]
    fn nothing_is_planned_without_the_codex_command_or_while_open() {
        let h = Home::new();
        h.put("2026/11/05", &file(ID), &rollout(r#""cli""#));
        let mut s = h.read_all().remove(0);
        let with = |s: &Session, program: Option<&Path>| {
            let plan = plan_delete(&[s], program);
            (plan.items.len(), plan.skipped.first().map(|k| k.reason.clone()))
        };
        assert_eq!(with(&s, None), (0, Some("the codex command isn't installed, and ccm deletes Codex conversations only through it".into())));
        let (count, _) = with(&s, Some(Path::new("/usr/bin/codex")));
        assert_eq!(count, 1);
        let plan = plan_delete(&[&s], Some(Path::new("/usr/bin/codex")));
        assert_eq!(plan.items[0].source, Source::Codex);
        assert_eq!(plan.items[0].transcripts, [PathBuf::from(&s.transcript_path)]);
        s.open = true;
        assert_eq!(with(&s, Some(Path::new("/usr/bin/codex"))), (0, Some("it's open".into())));
    }
}
