//! Claude Code: transcripts under `~/.claude/projects/<folder>/<id>.jsonl`.

use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{Found, Live, Tool};
use crate::index::SearchDoc;
use crate::model::{Session, Source};
use crate::paths::{session_id_of, Paths};
use crate::recovery;
use crate::registry::{live_roster, open_sessions};
use crate::transcript;

pub struct ClaudeCode {
    paths: Paths,
    proc_root: PathBuf,
}

impl ClaudeCode {
    pub fn new(paths: Paths, proc_root: PathBuf) -> Self {
        Self { paths, proc_root }
    }
}

impl Tool for ClaudeCode {
    fn source(&self) -> Source {
        Source::ClaudeCode
    }

    fn find(&self) -> Vec<Found> {
        find_transcripts(&self.paths.projects())
    }

    fn read(&self, found: &Found) -> Option<(Session, SearchDoc)> {
        read_session(found)
    }

    fn measure(&self, found: &Found) -> u64 {
        found.size + dir_size(&companion_dir(&found.path))
    }

    fn live(&self) -> Live {
        let open = open_sessions(&self.paths.sessions(), &self.proc_root);
        let roster = live_roster(&self.paths.claude.join("daemon/roster.json"), &self.proc_root);
        let crashed = recovery::crashed(&self.paths, &open, &roster, chrono::Utc::now().timestamp_millis());
        let windows = recovery::saved_windows(&self.paths);
        Live { open, crashed, windows }
    }
}

/// When the same ID is in two project folders, the newest transcript wins.
fn find_transcripts(projects: &Path) -> Vec<Found> {
    let mut by_id: HashMap<String, Found> = HashMap::new();
    let Ok(dirs) = fs::read_dir(projects) else {
        return Vec::new();
    };
    for dir in dirs.flatten() {
        let Ok(files) = fs::read_dir(dir.path()) else { continue };
        for file in files.flatten() {
            let path = file.path();
            let Some(id) = session_id_of(&path).map(str::to_owned) else { continue };
            let Ok(meta) = file.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let found = Found {
                id: id.clone(),
                path,
                size: meta.len(),
                mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                name: None,
            };
            match by_id.get(&id) {
                Some(existing) if existing.mtime_ns >= found.mtime_ns => {}
                _ => {
                    by_id.insert(id, found);
                }
            }
        }
    }
    by_id.into_values().collect()
}

fn read_session(f: &Found) -> Option<(Session, SearchDoc)> {
    let file = fs::File::open(&f.path).ok()?;
    let p = transcript::parse(BufReader::with_capacity(1 << 20, file)).ok()?;
    let doc = SearchDoc {
        title: p.name.clone(),
        path: p.folder.clone().unwrap_or_default(),
        branch: p.branch.clone().unwrap_or_default(),
        prompts: p.prompt_text,
        replies: p.reply_text,
    };
    let session = Session {
        id: f.id.clone(),
        source: Source::ClaudeCode,
        transcript_path: f.path.to_string_lossy().into_owned(),
        transcript_size: f.size,
        transcript_mtime_ns: f.mtime_ns,
        size_bytes: f.size + dir_size(&companion_dir(&f.path)),
        name: p.name,
        folder: p.folder,
        folders: p.folders,
        branch: p.branch,
        model: p.model,
        started_at_ms: p.started_at_ms,
        last_active_ms: p.last_active_ms.or(Some(f.mtime_ns / 1_000_000)),
        context_used: p.context_used,
        context_window: None,
        prompts: p.prompts,
        replies: p.replies,
        cost_usd: p.cost_usd,
        lines_added: p.lines_added,
        lines_removed: p.lines_removed,
        pr_url: p.pr_url,
        continued_in: p.continued_in,
        recaps: p.recaps,
        bad_lines: p.bad_lines,
        open: false,
        crashed: false,
        window_size: None,
        folder_exists: true,
    };
    Some((session, doc))
}

/// `<id>.jsonl` keeps its subagents and tool results in `<id>/`.
fn companion_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("")
}

/// Total size of regular files, never following symlinks.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}
