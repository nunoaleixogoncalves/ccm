use serde::{Deserialize, Serialize};

/// Which tool wrote a conversation. See [`crate::tools`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    ClaudeCode,
    Codex,
}

/// One conversation. Everything except `open` and `folder_exists` is cached in the index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub source: Source,
    pub transcript_path: String,
    /// Fingerprint of the transcript when it was parsed.
    pub transcript_size: u64,
    pub transcript_mtime_ns: i64,
    /// Transcript plus the sibling folder holding subagents and tool results.
    pub size_bytes: u64,
    pub name: String,
    /// First recorded folder, which is where a resume must start. Kept exact, so sanitize it
    /// before display.
    pub folder: Option<String>,
    /// Every folder the conversation worked in, in the order first seen. A conversation can
    /// change folder, and often only passes through a worktree.
    #[serde(default)]
    pub folders: Vec<FolderVisit>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub started_at_ms: Option<i64>,
    pub last_active_ms: Option<i64>,
    /// Tokens used by the last main-thread response.
    pub context_used: Option<u64>,
    /// The context window the tool recorded, for tools that record it.
    #[serde(default)]
    pub context_window: Option<u64>,
    pub prompts: u64,
    pub replies: u64,
    pub cost_usd: Option<f64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub pr_url: Option<String>,
    pub continued_in: Option<String>,
    /// Recaps saved by Claude Code, newest first.
    #[serde(default)]
    pub recaps: Vec<String>,
    /// Lines that could not be parsed.
    pub bad_lines: u64,
    #[serde(skip)]
    pub open: bool,
    /// The log shows it started and never ended, and nothing is running it.
    #[serde(skip)]
    pub crashed: bool,
    /// The real size, saved by the status line.
    #[serde(skip)]
    pub window_size: Option<u64>,
    #[serde(skip, default = "yes")]
    pub folder_exists: bool,
}

/// How much of a conversation happened in one folder. Kept exact, so sanitize before display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderVisit {
    pub path: String,
    /// Records written in this folder.
    pub records: u64,
    /// Position of the last record written here, counting only records with a folder. The
    /// highest one across visits is the folder the conversation was last in.
    pub last_record: u64,
    pub last_ms: Option<i64>,
}

fn yes() -> bool {
    true
}

impl Session {
    pub fn messages(&self) -> u64 {
        self.prompts + self.replies
    }

    /// Junk means Claude never replied. It only filters and never deletes anything by itself.
    pub fn is_junk(&self) -> bool {
        self.replies == 0
    }

    pub fn is_folder_missing(&self) -> bool {
        self.folder.is_some() && !self.folder_exists
    }

    /// All records that recorded a folder.
    pub fn folder_records(&self) -> u64 {
        self.folders.iter().map(|v| v.records).sum()
    }

    /// The folder the conversation was last working in.
    pub fn last_folder(&self) -> Option<&str> {
        self.folders.iter().max_by_key(|v| v.last_record).map(|v| v.path.as_str())
    }
}
