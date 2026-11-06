//! Recent messages for the preview pane. Reads only the end of the transcript, so the cost
//! doesn't grow with its length, and makes no AI calls or network requests.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use crate::format::sanitize_lines;
use crate::transcript::{prompt_kind, prompt_label, PromptKind, Record, SYNTHETIC_MODEL};

const TAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TURNS: usize = 30;
const MAX_TURN_CHARS: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    You,
    /// The tool that replied, such as Claude or Codex.
    Agent,
}

/// A prompt, or everything the agent did in reply to one, with its tool calls collapsed to a count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    pub tool_calls: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    pub turns: Vec<Turn>,
    /// Older messages exist that aren't shown.
    pub truncated: bool,
}

pub fn read(path: &Path) -> io::Result<Preview> {
    read_with(path, |turns, line| {
        if let Ok(r) = serde_json::from_slice::<Record>(line) {
            add_record(turns, r);
        }
    })
}

/// Reads the end of a JSONL file, handing each whole line to `add`, and keeps the latest turns.
pub(crate) fn read_with(path: &Path, mut add: impl FnMut(&mut Vec<Turn>, &[u8])) -> io::Result<Preview> {
    let mut file = File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut line = Vec::new();
    if start > 0 {
        // The first line is probably cut in half.
        reader.read_until(b'\n', &mut line)?;
    }

    let mut turns: Vec<Turn> = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        add(&mut turns, &line);
    }

    let hidden = turns.len().saturating_sub(MAX_TURNS);
    turns.drain(..hidden);
    for t in &mut turns {
        t.text = limit(sanitize_lines(t.text.trim()));
    }
    Ok(Preview { turns, truncated: start > 0 || hidden > 0 })
}

/// Adds reply text and tool calls to the agent's turn, starting one if a prompt came last.
pub(crate) fn push_reply(turns: &mut Vec<Turn>, text: Option<&str>, tool_calls: u32) {
    if !matches!(turns.last(), Some(t) if t.role == Role::Agent) {
        turns.push(Turn { role: Role::Agent, text: String::new(), tool_calls: 0 });
    }
    let turn = turns.last_mut().expect("just ensured");
    turn.tool_calls += tool_calls;
    if let Some(text) = text {
        if !turn.text.is_empty() {
            turn.text.push('\n');
        }
        turn.text.push_str(text);
    }
}

fn add_record(turns: &mut Vec<Turn>, r: Record) {
    if r.is_sidechain || r.is_meta || r.is_compact_summary {
        return;
    }
    let Some(m) = r.message else { return };
    match r.kind.as_deref() {
        Some("user") => {
            let content = m.content.unwrap_or_default();
            if content.has_tool_result {
                return;
            }
            let text = content.texts.join("\n");
            match prompt_kind(&text) {
                PromptKind::CommandOutput => {}
                PromptKind::SlashCommand => push_prompt(turns, prompt_label(&text)),
                PromptKind::Typed if text.trim().is_empty() => {}
                PromptKind::Typed => push_prompt(turns, text),
            }
        }
        Some("assistant") if m.model.as_deref() != Some(SYNTHETIC_MODEL) => {
            let content = m.content.unwrap_or_default();
            push_reply(turns, None, content.tool_uses);
            for text in content.texts {
                push_reply(turns, Some(&text), 0);
            }
        }
        _ => {}
    }
}

pub(crate) fn push_prompt(turns: &mut Vec<Turn>, text: String) {
    turns.push(Turn { role: Role::You, text, tool_calls: 0 });
}

fn limit(text: String) -> String {
    match text.char_indices().nth(MAX_TURN_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn preview_of(body: &str) -> Preview {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        fs::write(&p, body).unwrap();
        read(&p).unwrap()
    }

    fn user(t: &str) -> String {
        format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{t}\"}}}}\n")
    }

    const TOOL_RESULT: &str = "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"output\"}]}}\n";

    fn reply(t: &str, tools: usize) -> String {
        let tools = ",{\"type\":\"tool_use\",\"input\":{}}".repeat(tools);
        format!("{{\"type\":\"assistant\",\"message\":{{\"id\":\"m\",\"model\":\"x\",\"content\":[{{\"type\":\"text\",\"text\":\"{t}\"}}{tools}]}}}}\n")
    }

    #[test]
    fn a_tool_loop_collapses_into_one_turn_with_a_count() {
        let body = [user("go"), reply("first", 2), TOOL_RESULT.into(), reply("second", 1), TOOL_RESULT.into(), user("thanks")].concat();
        let p = preview_of(&body);
        assert_eq!(
            p.turns,
            [
                Turn { role: Role::You, text: "go".into(), tool_calls: 0 },
                Turn { role: Role::Agent, text: "first\nsecond".into(), tool_calls: 3 },
                Turn { role: Role::You, text: "thanks".into(), tool_calls: 0 },
            ]
        );
        assert!(!p.truncated);
    }

    #[test]
    fn hidden_records_are_left_out_and_commands_read_cleanly() {
        let body = [
            "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"meta\"}}\n".to_string(),
            "{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"content\":\"sub\"}}\n".into(),
            "{\"type\":\"user\",\"isCompactSummary\":true,\"message\":{\"content\":\"summary\"}}\n".into(),
            "{\"type\":\"assistant\",\"message\":{\"model\":\"<synthetic>\",\"content\":[{\"type\":\"text\",\"text\":\"err\"}]}}\n".into(),
            "{\"type\":\"user\",\"message\":{\"content\":\"<local-command-stdout>x</local-command-stdout>\"}}\n".into(),
            user("<command-name>/mcp</command-name> y"),
            "{broken\n".into(),
        ]
        .concat();
        let p = preview_of(&body);
        assert_eq!(p.turns, [Turn { role: Role::You, text: "/mcp".into(), tool_calls: 0 }]);
    }

    #[test]
    fn terminal_escapes_are_removed_and_lines_kept() {
        let p = preview_of(&user("a\\u001b[31mb\\nc\\u001b]0;pwn\\u0007d"));
        assert_eq!(p.turns[0].text, "ab\ncd");
    }

    #[test]
    fn only_the_latest_turns_are_kept() {
        let body: String = (0..40).map(|i| [user(&format!("q{i}")), reply("a", 0)].concat()).collect();
        let p = preview_of(&body);
        assert_eq!(p.turns.len(), MAX_TURNS);
        assert_eq!(p.turns.last().unwrap().text, "a");
        assert!(p.truncated);
    }

    #[test]
    fn a_huge_transcript_only_has_its_end_read() {
        let filler = user(&"x".repeat(1024 * 1024));
        let body = [filler.repeat(6), user("the end")].concat();
        let p = preview_of(&body);
        assert!(p.truncated);
        assert_eq!(p.turns.last().unwrap().text, "the end");
        assert!(p.turns.len() < 6);
    }

    #[test]
    fn very_long_messages_are_cut() {
        let p = preview_of(&user(&"日".repeat(MAX_TURN_CHARS + 5)));
        assert_eq!(p.turns[0].text.chars().count(), MAX_TURN_CHARS + 1);
        assert!(p.turns[0].text.ends_with('…'));
    }

    #[test]
    fn a_missing_file_is_an_error_and_an_empty_one_is_not() {
        assert!(read(Path::new("/nonexistent/t.jsonl")).is_err());
        assert_eq!(preview_of(""), Preview::default());
    }
}
