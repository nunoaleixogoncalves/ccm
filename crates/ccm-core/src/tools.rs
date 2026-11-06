//! Each coding tool whose conversations `ccm` reads is one part behind [`Tool`]. The catalog only
//! knows this interface, so a tool can be added or left out without touching the rest.

pub mod claude;
pub mod codex;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use crate::config::ToolName;
use crate::index::SearchDoc;
use crate::model::{Session, Source};
use crate::paths::Paths;
use crate::preview::{self, Preview};

/// One conversation file found by a scan, with the fingerprint that tells if it changed.
#[derive(Debug, Clone)]
pub struct Found {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    /// A name the tool keeps outside the conversation file.
    pub name: Option<String>,
}

/// What is true right now, never cached: which conversations are running or crashed, and the
/// real context window sizes the tool reported.
#[derive(Debug, Default)]
pub struct Live {
    pub open: HashSet<String>,
    pub crashed: HashSet<String>,
    pub windows: HashMap<String, u64>,
}

/// Finds and reads one tool's conversations.
pub trait Tool: Send + Sync {
    fn source(&self) -> Source;
    /// Every conversation file, at most one per ID.
    fn find(&self) -> Vec<Found>;
    /// Parses a conversation that is new or changed. `None` skips a file that can't be read.
    fn read(&self, found: &Found) -> Option<(Session, SearchDoc)>;
    /// The size on disk of everything the conversation keeps, which can grow without the
    /// conversation file changing.
    fn measure(&self, found: &Found) -> u64;
    fn live(&self) -> Live;
}

/// The tools to read: those `wanted` (all when `None`) whose folder exists, so a tool that
/// isn't installed costs nothing. Claude Code first.
pub fn detect(paths: &Paths, proc_root: &Path, codex_home: &Path, wanted: Option<&[ToolName]>) -> Vec<Box<dyn Tool>> {
    let wants = |name| wanted.is_none_or(|w| w.contains(&name));
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    if wants(ToolName::Claude) && paths.projects().is_dir() {
        tools.push(Box::new(claude::ClaudeCode::new(paths.clone(), proc_root.to_path_buf())));
    }
    if wants(ToolName::Codex) && codex::Codex::sessions(codex_home).is_dir() {
        tools.push(Box::new(codex::Codex::new(codex_home.to_path_buf())));
    }
    tools
}

/// What a tool lets `ccm` do with its conversations. Keys for what's missing are greyed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    pub resume: bool,
    pub fork: bool,
    pub delete: bool,
    pub rename: bool,
    /// Open and crashed states, and reopening after a crash.
    pub recovery: bool,
}

impl Source {
    /// The tool's name as people know it.
    pub fn label(self) -> &'static str {
        match self {
            Source::ClaudeCode => "Claude Code",
            Source::Codex => "Codex",
        }
    }

    /// The name of whoever replies, for the preview and the Tool column.
    pub fn short(self) -> &'static str {
        match self {
            Source::ClaudeCode => "Claude",
            Source::Codex => "Codex",
        }
    }

    pub fn features(self) -> Features {
        match self {
            Source::ClaudeCode => Features { resume: true, fork: true, delete: true, rename: true, recovery: true },
            Source::Codex => Features { resume: true, fork: true, delete: true, rename: false, recovery: false },
        }
    }
}

/// The latest messages of a conversation, read from its tool's files.
pub fn preview(s: &Session) -> io::Result<Preview> {
    match s.source {
        Source::ClaudeCode => preview::read(std::path::Path::new(&s.transcript_path)),
        Source::Codex => codex::preview(std::path::Path::new(&s.transcript_path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn only_tools_with_a_folder_and_wanted_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            claude: root.join("claude"),
            index: root.join("index.db"),
            config: root.join("config.toml"),
            claude_json: root.join("claude.json"),
            state: root.join("state"),
        };
        let codex = root.join("codex");
        let sources = |wanted: Option<&[ToolName]>| -> Vec<Source> {
            detect(&paths, root, &codex, wanted).iter().map(|t| t.source()).collect()
        };
        assert!(sources(None).is_empty());
        fs::create_dir_all(paths.projects()).unwrap();
        assert_eq!(sources(None), [Source::ClaudeCode]);
        fs::create_dir_all(codex.join("sessions")).unwrap();
        assert_eq!(sources(None), [Source::ClaudeCode, Source::Codex]);
        assert_eq!(sources(Some(&[ToolName::Codex])), [Source::Codex]);
        assert!(sources(Some(&[])).is_empty());
    }
}
