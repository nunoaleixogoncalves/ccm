//! Each coding tool whose conversations `ccm` reads is one part behind [`Tool`]. The catalog only
//! knows this interface, so a tool can be added or left out without touching the rest.

pub mod claude;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use crate::index::SearchDoc;
use crate::model::{Session, Source};
use crate::preview::{self, Preview};

/// One conversation file found by a scan, with the fingerprint that tells if it changed.
#[derive(Debug, Clone)]
pub struct Found {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
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
        }
    }

    pub fn features(self) -> Features {
        match self {
            Source::ClaudeCode => Features { resume: true, fork: true, delete: true, rename: true, recovery: true },
        }
    }
}

/// The latest messages of a conversation, read from its tool's files.
pub fn preview(s: &Session) -> io::Result<Preview> {
    match s.source {
        Source::ClaudeCode => preview::read(std::path::Path::new(&s.transcript_path)),
    }
}
