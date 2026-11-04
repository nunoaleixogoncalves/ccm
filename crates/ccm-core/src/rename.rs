//! Renames a conversation the way Claude Code's `/rename` does, so the name also shows in its own
//! `/resume`: a `custom-title` record in the transcript and a `custom-title.json` beside it.
//! Claude Code prefers the record while it's near the end of the transcript and reads the file
//! once it isn't, so leaving either one out would bring an old name back later.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use serde_json::json;

use crate::delete::replace_file;
use crate::format::sanitize;
use crate::paths::{session_id_of, Paths};
use crate::registry::{has_unreadable_entry, open_sessions};

/// Longer than this isn't a name, and wouldn't fit anywhere it's shown.
pub const MAX_NAME: usize = 200;

/// Returns the name as saved.
pub fn rename(paths: &Paths, proc_root: &Path, id: &str, transcript: &Path, name: &str) -> Result<String, String> {
    let name: String = sanitize(name).trim().chars().take(MAX_NAME).collect();
    if name.is_empty() {
        return Err("The name is empty.".into());
    }
    if session_id_of(transcript) != Some(id) || !transcript.starts_with(paths.projects()) {
        return Err("This conversation's path isn't valid.".into());
    }
    let is_file = fs::symlink_metadata(transcript).is_ok_and(|m| m.is_file());
    if !is_file {
        return Err("Its transcript isn't a regular file.".into());
    }
    if open_sessions(&paths.sessions(), proc_root).contains(id) {
        return Err("This conversation is open, so rename it from there with /rename.".into());
    }
    if has_unreadable_entry(&paths.sessions(), proc_root) {
        return Err("A running session's registry file can't be read, so this one may be open.".into());
    }
    append_record(transcript, id, &name).map_err(|e| format!("Couldn't rename it: {e}"))?;
    write_sidecar(transcript, &name).map_err(|e| format!("Renamed, but Claude Code may show the old name later: {e}"))?;
    Ok(name)
}

/// Written in one call on an append-only handle, so a line from another writer can't land inside.
fn append_record(transcript: &Path, id: &str, name: &str) -> io::Result<()> {
    let record = json!({ "type": "custom-title", "customTitle": name, "sessionId": id });
    let mut line = serde_json::to_string(&record)?;
    line.push('\n');
    if !ends_with_newline(transcript)? {
        line.insert(0, '\n');
    }
    OpenOptions::new().append(true).open(transcript)?.write_all(line.as_bytes())
}

fn ends_with_newline(path: &Path) -> io::Result<bool> {
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

fn write_sidecar(transcript: &Path, name: &str) -> io::Result<()> {
    let dir = transcript.with_extension("");
    DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let body = serde_json::to_vec(&json!({ "customTitle": name }))?;
    replace_file(&dir.join("custom-title.json"), &body, || true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript;
    use std::io::BufReader;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    struct Tree {
        _dir: tempfile::TempDir,
        paths: Paths,
        proc: PathBuf,
        transcript: PathBuf,
    }

    fn tree(body: &str) -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            claude: root.join("claude"),
            index: root.join("index.db"),
            config: root.join("config.toml"),
            claude_json: root.join("claude.json"),
            state: root.join("state/ccm"),
        };
        fs::create_dir_all(paths.projects().join("-p")).unwrap();
        fs::create_dir_all(paths.sessions()).unwrap();
        let proc = root.join("proc");
        fs::create_dir_all(&proc).unwrap();
        let transcript = paths.projects().join(format!("-p/{A}.jsonl"));
        fs::write(&transcript, body).unwrap();
        Tree { _dir: dir, paths, proc, transcript }
    }

    fn run(t: &Tree, name: &str) -> Result<String, String> {
        rename(&t.paths, &t.proc, A, &t.transcript, name)
    }

    fn parsed_name(t: &Tree) -> String {
        transcript::parse(BufReader::new(fs::File::open(&t.transcript).unwrap())).unwrap().name
    }

    #[test]
    fn writes_the_record_and_the_file_claude_code_reads() {
        let t = tree("{\"type\":\"user\"}\n");
        assert_eq!(run(&t, "my name").unwrap(), "my name");
        let text = fs::read_to_string(&t.transcript).unwrap();
        assert_eq!(
            text,
            format!("{{\"type\":\"user\"}}\n{{\"type\":\"custom-title\",\"customTitle\":\"my name\",\"sessionId\":\"{A}\"}}\n")
        );
        let sidecar = t.transcript.with_extension("").join("custom-title.json");
        assert_eq!(fs::read_to_string(&sidecar).unwrap(), r#"{"customTitle":"my name"}"#);
        assert_eq!(fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(parsed_name(&t), "my name");
    }

    #[test]
    fn the_newest_name_wins_and_replaces_the_file() {
        let t = tree("");
        run(&t, "first").unwrap();
        run(&t, "second").unwrap();
        assert_eq!(parsed_name(&t), "second");
        let sidecar = t.transcript.with_extension("").join("custom-title.json");
        assert_eq!(fs::read_to_string(sidecar).unwrap(), r#"{"customTitle":"second"}"#);
    }

    #[test]
    fn a_transcript_cut_off_mid_line_does_not_swallow_the_record() {
        let t = tree("{\"type\":\"user\"");
        run(&t, "safe").unwrap();
        assert_eq!(parsed_name(&t), "safe");
    }

    #[test]
    fn quotes_and_control_characters_cannot_break_the_line() {
        let t = tree("");
        let saved = run(&t, "a\"b\\c\u{1b}[31md\ne").unwrap();
        assert!(!saved.contains('\u{1b}') && !saved.contains('\n'));
        let text = fs::read_to_string(&t.transcript).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert_eq!(parsed_name(&t), saved);
    }

    #[test]
    fn long_names_are_cut_and_blank_ones_refused() {
        let t = tree("");
        assert_eq!(run(&t, &"é".repeat(500)).unwrap().chars().count(), MAX_NAME);
        assert!(run(&t, "   ").is_err());
    }

    #[test]
    fn an_open_conversation_is_refused() {
        let t = tree("{}\n");
        let fields: Vec<String> = (3..=21).map(|i| i.to_string()).collect();
        let proc = t.proc.join("7");
        fs::create_dir_all(&proc).unwrap();
        fs::write(proc.join("stat"), format!("7 (claude) {} 5 1\n", fields.join(" "))).unwrap();
        fs::write(t.paths.sessions().join("7.json"), format!(r#"{{"pid":7,"sessionId":"{A}","procStart":"5"}}"#)).unwrap();
        assert!(run(&t, "x").unwrap_err().contains("open"));
        assert_eq!(fs::read_to_string(&t.transcript).unwrap(), "{}\n");
    }

    #[test]
    fn a_running_session_with_an_unreadable_registry_file_stops_the_rename() {
        let t = tree("{}\n");
        fs::create_dir_all(t.proc.join("7")).unwrap();
        fs::write(t.paths.sessions().join("7.json"), "{ cut off").unwrap();
        assert!(run(&t, "x").unwrap_err().contains("can't be read"));
        assert_eq!(fs::read_to_string(&t.transcript).unwrap(), "{}\n");
    }

    #[test]
    fn a_path_that_does_not_match_the_id_is_refused() {
        let t = tree("{}\n");
        let other = t.paths.projects().join("-p/other.jsonl");
        fs::write(&other, "{}\n").unwrap();
        assert!(rename(&t.paths, &t.proc, A, &other, "x").is_err());
        assert_eq!(fs::read_to_string(other).unwrap(), "{}\n");
    }
}
