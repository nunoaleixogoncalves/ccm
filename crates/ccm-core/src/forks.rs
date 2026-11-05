//! Which conversations are forks of which. Claude Code records no link between a fork and its
//! original, so `ccm` keeps a log of the forks it starts. Each line holds only two IDs and a time.

use std::collections::HashMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::{is_uuid, Paths};

#[derive(Serialize, Deserialize)]
struct Line {
    fork: String,
    from: String,
    ts: i64,
}

fn log_path(paths: &Paths) -> PathBuf {
    paths.state.join("forks.jsonl")
}

/// Notes that `fork` started as a copy of `from`. Written before the fork starts, so it's known
/// as soon as it appears. A fork that never starts leaves a line about an ID nothing uses.
pub fn record(paths: &Paths, fork: &str, from: &str, now_ms: i64) -> io::Result<()> {
    if !is_uuid(fork) || !is_uuid(from) {
        return Err(io::Error::other("not a conversation ID"));
    }
    DirBuilder::new().recursive(true).mode(0o700).create(&paths.state)?;
    let mut text = serde_json::to_string(&Line { fork: fork.into(), from: from.into(), ts: now_ms })?;
    text.push('\n');
    OpenOptions::new().create(true).append(true).mode(0o600).open(log_path(paths))?.write_all(text.as_bytes())
}

/// Each fork's original. Unreadable lines are skipped.
pub fn load(paths: &Paths) -> HashMap<String, String> {
    let Ok(text) = fs::read_to_string(log_path(paths)) else { return HashMap::new() };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Line>(l).ok())
        .filter(|l| is_uuid(&l.fork) && is_uuid(&l.from) && l.fork != l.from)
        .map(|l| (l.fork, l.from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

    fn paths(root: &std::path::Path) -> Paths {
        Paths {
            claude: root.join("claude"),
            index: root.join("index.db"),
            config: root.join("config.toml"),
            claude_json: root.join("claude.json"),
            state: root.join("state/ccm"),
        }
    }

    #[test]
    fn forks_are_remembered_in_an_owner_only_log() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        assert!(load(&p).is_empty());
        record(&p, B, A, 1).unwrap();
        record(&p, C, B, 2).unwrap();
        assert_eq!(load(&p), HashMap::from([(B.to_string(), A.to_string()), (C.to_string(), B.to_string())]));
        let mode = fs::metadata(log_path(&p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn only_conversation_ids_are_written_or_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        assert!(record(&p, "../x", A, 1).is_err());
        fs::create_dir_all(&p.state).unwrap();
        fs::write(log_path(&p), format!("garbage\n{{\"fork\":\"../x\",\"from\":\"{A}\",\"ts\":1}}\n{{\"fork\":\"{A}\",\"from\":\"{A}\",\"ts\":1}}\n")).unwrap();
        assert!(load(&p).is_empty());
    }
}
