//! Parses a transcript. The format is undocumented and can change, so unknown records are
//! skipped and bad lines are counted rather than treated as errors.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::BufRead;

use chrono::DateTime;
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::Deserialize;

use crate::format::{sanitize, sanitize_lines};
use crate::model::FolderVisit;

pub(crate) const SYNTHETIC_MODEL: &str = "<synthetic>";
/// A limit per field, so one enormous transcript can't bloat the index.
const MAX_SEARCH_TEXT: usize = 4 * 1024 * 1024;
/// A longer line is skipped, so one enormous line can't use up all the memory.
const MAX_LINE: usize = 64 * 1024 * 1024;
/// Names and labels are drawn for every row, so one enormous value would slow the whole list.
const MAX_LABEL: usize = 1024;
/// The longest path Linux and macOS accept. A longer folder can't be real.
const MAX_FOLDER: usize = 4096;
const MAX_RECAPS: usize = 3;
/// Folders kept per conversation. Later ones are ignored, so a script that visits thousands
/// can't bloat the index.
const MAX_FOLDERS: usize = 256;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Parsed {
    pub name: String,
    pub folder: Option<String>,
    pub folders: Vec<FolderVisit>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub started_at_ms: Option<i64>,
    pub last_active_ms: Option<i64>,
    pub context_used: Option<u64>,
    pub prompts: u64,
    pub replies: u64,
    pub cost_usd: Option<f64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub pr_url: Option<String>,
    pub continued_in: Option<String>,
    /// Newest first.
    pub recaps: Vec<String>,
    pub bad_lines: u64,
    pub prompt_text: String,
    pub reply_text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Record {
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
    subtype: Option<String>,
    content: Option<Content>,
    #[serde(default)]
    pub(crate) is_sidechain: bool,
    #[serde(default)]
    pub(crate) is_meta: bool,
    #[serde(default)]
    pub(crate) is_compact_summary: bool,
    cwd: Option<String>,
    git_branch: Option<String>,
    timestamp: Option<String>,
    pub(crate) message: Option<Message>,
    ai_title: Option<String>,
    custom_title: Option<String>,
    last_prompt: Option<String>,
    #[serde(rename = "totalCostUSD")]
    total_cost_usd: Option<f64>,
    total_lines_added: Option<u64>,
    total_lines_removed: Option<u64>,
    pr_url: Option<String>,
    continued_in_session_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct Message {
    id: Option<String>,
    pub(crate) model: Option<String>,
    usage: Option<Usage>,
    pub(crate) content: Option<Content>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

/// The parts of a message's content that `ccm` uses. Hand-written so that tool output, often the
/// bulk of a transcript, is skipped rather than copied.
#[derive(Default)]
pub(crate) struct Content {
    pub(crate) texts: Vec<String>,
    pub(crate) has_tool_result: bool,
    pub(crate) tool_uses: u32,
}

/// What a user record's text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptKind {
    Typed,
    SlashCommand,
    CommandOutput,
}

pub(crate) fn prompt_kind(text: &str) -> PromptKind {
    let text = text.trim_start();
    if text.starts_with("<local-command") {
        PromptKind::CommandOutput
    } else if text.starts_with("<command-") {
        PromptKind::SlashCommand
    } else {
        PromptKind::Typed
    }
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Content;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or a list of content blocks")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Content, E> {
                Ok(Content { texts: vec![s.to_owned()], ..Content::default() })
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
                let mut c = Content::default();
                while let Some(b) = seq.next_element::<Block>()? {
                    match b.kind.as_deref() {
                        Some("tool_result") => c.has_tool_result = true,
                        Some("tool_use") => c.tool_uses += 1,
                        Some("text") => c.texts.extend(b.text),
                        _ => {}
                    }
                }
                Ok(c)
            }
        }
        d.deserialize_any(V)
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

/// Reads one line into `line`, replacing what's there. Returns `None` at the end of the file, and
/// `Some(false)` with an empty `line` if the line was too long to keep.
pub(crate) fn read_capped_line<R: BufRead>(reader: &mut R, line: &mut Vec<u8>) -> std::io::Result<Option<bool>> {
    line.clear();
    let (mut fits, mut any) = (true, false);
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(any.then_some(fits));
        }
        any = true;
        let end = available.iter().position(|b| *b == b'\n');
        let piece = &available[..end.map_or(available.len(), |at| at + 1)];
        if fits && line.len() + piece.len() <= MAX_LINE {
            line.extend_from_slice(piece);
        } else {
            fits = false;
            line.clear();
        }
        let used = piece.len();
        reader.consume(used);
        if end.is_some() {
            return Ok(Some(fits));
        }
    }
}

fn label(text: &str) -> String {
    let short: String = text.chars().take(MAX_LABEL).collect();
    sanitize(&short)
}

pub fn parse<R: BufRead>(mut reader: R) -> std::io::Result<Parsed> {
    let mut p = Parsed::default();
    let (mut custom, mut ai, mut last_prompt, mut first_prompt) = (None, None, None, None);
    let mut reply_ids = HashSet::new();
    let mut anonymous_replies = 0;
    let mut recaps = Vec::new();
    let mut line = Vec::new();
    let mut visits: HashMap<String, usize> = HashMap::new();
    let mut with_folder = 0u64;

    while let Some(fits) = read_capped_line(&mut reader, &mut line)? {
        if !fits {
            p.bad_lines += 1;
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(r) = serde_json::from_slice::<Record>(&line) else {
            p.bad_lines += 1;
            continue;
        };

        custom = non_empty(r.custom_title).or(custom);
        ai = non_empty(r.ai_title).or(ai);
        last_prompt = non_empty(r.last_prompt).or(last_prompt);
        p.cost_usd = r.total_cost_usd.or(p.cost_usd);
        p.lines_added = r.total_lines_added.or(p.lines_added);
        p.lines_removed = r.total_lines_removed.or(p.lines_removed);
        p.pr_url = r.pr_url.or(p.pr_url);
        p.continued_in = r.continued_in_session_id.or(p.continued_in);

        if r.is_sidechain {
            continue;
        }
        let kind = r.kind.as_deref();
        if let Some(cwd) = r.cwd.filter(|f| f.len() <= MAX_FOLDER) {
            with_folder += 1;
            let at = r.timestamp.as_deref().and_then(parse_ms);
            match visits.get(&cwd) {
                Some(&i) => {
                    let v = &mut p.folders[i];
                    v.records += 1;
                    v.last_record = with_folder;
                    v.last_ms = at.or(v.last_ms);
                }
                None if p.folders.len() < MAX_FOLDERS => {
                    visits.insert(cwd.clone(), p.folders.len());
                    p.folders.push(FolderVisit { path: cwd.clone(), records: 1, last_record: with_folder, last_ms: at });
                }
                None => {}
            }
            p.folder.get_or_insert(cwd);
        }
        if matches!(kind, Some("user" | "assistant" | "system")) {
            if let Some(t) = r.timestamp.as_deref().and_then(parse_ms) {
                p.started_at_ms.get_or_insert(t);
                p.last_active_ms = Some(p.last_active_ms.map_or(t, |v| v.max(t)));
            }
            p.branch = non_empty(r.git_branch).or(p.branch.take());
        }

        match (kind, r.message) {
            (Some("user"), Some(m)) if !r.is_meta && !r.is_compact_summary => {
                let content = m.content.unwrap_or_default();
                if !content.has_tool_result {
                    let text = content.texts.join("\n");
                    let kind = prompt_kind(&text);
                    if kind == PromptKind::Typed {
                        p.prompts += 1;
                        push_search_text(&mut p.prompt_text, &text);
                    }
                    if first_prompt.is_none() && kind != PromptKind::CommandOutput {
                        first_prompt = non_empty(Some(text));
                    }
                }
            }
            (Some("assistant"), Some(m)) if m.model.as_deref() != Some(SYNTHETIC_MODEL) => {
                // One response is split into a record per content block, so count IDs.
                match m.id {
                    Some(id) => {
                        reply_ids.insert(id);
                    }
                    None => anonymous_replies += 1,
                }
                p.model = m.model.or(p.model.take());
                for text in m.content.unwrap_or_default().texts {
                    push_search_text(&mut p.reply_text, &text);
                }
                if let Some(u) = m.usage {
                    let used = u.input_tokens.saturating_add(u.cache_read_input_tokens).saturating_add(u.cache_creation_input_tokens);
                    if used > 0 {
                        p.context_used = Some(used);
                    }
                }
            }
            _ => {}
        }
        if kind == Some("system") && r.subtype.as_deref() == Some("away_summary") {
            let text = r.content.unwrap_or_default().texts.join("\n");
            if !text.trim().is_empty() {
                if recaps.len() == MAX_RECAPS {
                    recaps.remove(0);
                }
                recaps.push(sanitize_lines(&text));
            }
        }
    }

    recaps.reverse();
    p.recaps = recaps;
    p.replies = reply_ids.len() as u64 + anonymous_replies;
    let name = custom.or(ai).or(last_prompt.or(first_prompt).map(|t| prompt_label(&t))).unwrap_or_default();
    p.name = label(&name);
    p.branch = p.branch.map(|b| label(&b));
    p.model = p.model.map(|m| label(&m));
    p.pr_url = p.pr_url.map(|u| label(&u));
    Ok(p)
}

fn push_search_text(into: &mut String, text: &str) {
    if into.len() + text.len() < MAX_SEARCH_TEXT {
        into.push_str(text);
        into.push('\n');
    }
}

/// A slash command is recorded as `<command-name>/mcp</command-name>...`, so name it `/mcp`.
pub(crate) fn prompt_label(text: &str) -> String {
    let inner = text
        .split_once("<command-name>")
        .and_then(|(_, rest)| rest.split_once("</command-name>"))
        .map(|(name, _)| name);
    inner.unwrap_or(text).to_owned()
}

fn parse_ms(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Parsed {
        parse(lines.join("\n").as_bytes()).unwrap()
    }

    const USER: &str = r#"{"type":"user","cwd":"/work/a","gitBranch":"main","timestamp":"2026-11-01T10:00:00.000Z","message":{"role":"user","content":"fix the bug"}}"#;
    const REPLY: &str = r#"{"type":"assistant","cwd":"/work/b","gitBranch":"dev","timestamp":"2026-11-01T10:00:05.000Z","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"cache_read_input_tokens":80000,"cache_creation_input_tokens":7000,"output_tokens":99}}}"#;

    #[test]
    fn reads_a_normal_conversation() {
        let p = run(&[USER, REPLY]);
        assert_eq!(p.folder.as_deref(), Some("/work/a"), "first folder wins");
        assert_eq!(p.branch.as_deref(), Some("dev"), "last branch wins");
        assert_eq!(p.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(p.context_used, Some(87010), "output tokens are not context");
        assert_eq!((p.prompts, p.replies), (1, 1));
        assert_eq!(p.name, "fix the bug");
        assert_eq!(p.last_active_ms.unwrap() - p.started_at_ms.unwrap(), 5000);
    }

    #[test]
    fn every_folder_is_kept_with_how_much_happened_there_and_when() {
        let side = r#"{"type":"assistant","isSidechain":true,"cwd":"/sub","message":{"id":"s1"}}"#;
        let summary = r#"{"type":"summary","cwd":"/work/a"}"#;
        let p = run(&[USER, REPLY, side, summary, REPLY]);
        let seen: Vec<(&str, u64, u64)> = p.folders.iter().map(|v| (v.path.as_str(), v.records, v.last_record)).collect();
        assert_eq!(seen, [("/work/a", 2, 3), ("/work/b", 2, 4)], "first-seen order, and subagents don't count");
        assert_eq!(p.folders[0].last_ms, Some(p.started_at_ms.unwrap()), "a record without a time keeps the last one");
        assert_eq!(p.folders[1].last_ms, p.last_active_ms);
    }

    #[test]
    fn folders_are_capped() {
        let lines: Vec<String> = (0..MAX_FOLDERS + 10).map(|i| format!(r#"{{"type":"user","cwd":"/f{i}"}}"#)).collect();
        let p = run(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(p.folders.len(), MAX_FOLDERS);
        assert_eq!(p.folder.as_deref(), Some("/f0"));
    }

    #[test]
    fn a_response_split_across_records_counts_once() {
        assert_eq!(run(&[USER, REPLY, REPLY]).replies, 1);
    }

    #[test]
    fn name_priority_is_custom_then_ai_then_last_prompt() {
        let custom = r#"{"type":"custom-title","customTitle":"mine"}"#;
        let ai = r#"{"type":"ai-title","aiTitle":"generated"}"#;
        let last = r#"{"type":"last-prompt","lastPrompt":"latest"}"#;
        assert_eq!(run(&[custom, ai, last, USER]).name, "mine");
        assert_eq!(run(&[ai, last, USER]).name, "generated");
        assert_eq!(run(&[last, USER]).name, "latest");
        assert_eq!(run(&[USER]).name, "fix the bug");
    }

    #[test]
    fn the_latest_title_of_a_kind_wins() {
        let a = r#"{"type":"custom-title","customTitle":"old"}"#;
        let b = r#"{"type":"custom-title","customTitle":"new"}"#;
        assert_eq!(run(&[a, b]).name, "new");
    }

    #[test]
    fn subagent_records_do_not_count() {
        let side = r#"{"type":"assistant","isSidechain":true,"cwd":"/other","message":{"id":"s1","model":"claude-haiku-5-5","usage":{"input_tokens":999999}}}"#;
        let p = run(&[side, USER, REPLY, side]);
        assert_eq!(p.folder.as_deref(), Some("/work/a"));
        assert_eq!(p.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(p.context_used, Some(87010));
        assert_eq!(p.replies, 1);
    }

    #[test]
    fn tool_results_meta_and_compact_summaries_are_not_prompts() {
        let tool = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"huge"}]}}"#;
        let meta = r#"{"type":"user","isMeta":true,"message":{"content":"hidden"}}"#;
        let compact = r#"{"type":"user","isCompactSummary":true,"message":{"content":"summary"}}"#;
        let blocks = r#"{"type":"user","message":{"content":[{"type":"image"},{"type":"text","text":"look"}]}}"#;
        let p = run(&[tool, meta, compact, blocks]);
        assert_eq!(p.prompts, 1);
        assert_eq!(p.name, "look");
    }

    #[test]
    fn slash_commands_are_named_cleanly_and_are_not_prompts() {
        let cmd = r#"{"type":"user","message":{"content":"<command-name>/mcp</command-name> <command-message>mcp</command-message>"}}"#;
        let out = r#"{"type":"user","message":{"content":"<local-command-stdout>done</local-command-stdout>"}}"#;
        let p = run(&[out, cmd, out]);
        assert_eq!(p.name, "/mcp");
        assert_eq!(p.prompts, 0);
        let last = r#"{"type":"last-prompt","lastPrompt":"<command-name>/clear</command-name> x"}"#;
        assert_eq!(run(&[last]).name, "/clear");
    }

    #[test]
    fn search_text_holds_prompts_and_replies_only() {
        let reply = r#"{"type":"assistant","message":{"id":"a","model":"m","content":[{"type":"thinking","thinking":"secret plan"},{"type":"text","text":"visible answer"},{"type":"tool_use","input":{"cmd":"rm tool-input"}}]}}"#;
        let tool = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"tool-output"}]}}"#;
        let side = r#"{"type":"assistant","isSidechain":true,"message":{"id":"b","content":[{"type":"text","text":"subagent words"}]}}"#;
        let cmd = r#"{"type":"user","message":{"content":"<command-name>/mcp</command-name>"}}"#;
        let p = run(&[USER, reply, tool, side, cmd]);
        assert_eq!(p.prompt_text, "fix the bug\n");
        assert_eq!(p.reply_text, "visible answer\n");
    }

    #[test]
    fn recaps_are_newest_first_and_limited() {
        let recap = |t: &str| format!(r#"{{"type":"system","subtype":"away_summary","content":"{t}"}}"#);
        let lines: Vec<String> = ["one", "two", "three", "four"].iter().map(|t| recap(t)).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(run(&refs).recaps, ["four", "three", "two"]);
    }

    #[test]
    fn synthetic_replies_do_not_make_a_conversation_real() {
        let synth = r#"{"type":"assistant","message":{"id":"e1","model":"<synthetic>","usage":{"input_tokens":5}}}"#;
        let p = run(&[USER, synth]);
        assert_eq!(p.replies, 0);
        assert_eq!(p.model, None);
        assert_eq!(p.context_used, None);
    }

    #[test]
    fn empty_usage_keeps_the_previous_figure() {
        let zero = r#"{"type":"assistant","message":{"id":"m2","model":"claude-opus-5-5","usage":{"input_tokens":0}}}"#;
        assert_eq!(run(&[USER, REPLY, zero]).context_used, Some(87010));
    }

    #[test]
    fn bad_lines_and_unknown_records_are_survivable() {
        let p = run(&["{not json", r#"{"type":"from-the-future","x":[1,2]}"#, "", USER, r#"{"type":"user","cwd":5}"#]);
        assert_eq!(p.bad_lines, 2);
        assert_eq!(p.prompts, 1);
    }

    #[test]
    fn a_truncated_last_line_from_a_crash_is_survivable() {
        let cut = &REPLY[..REPLY.len() / 2];
        let p = run(&[USER, cut]);
        assert_eq!(p.bad_lines, 1);
        assert_eq!(p.prompts, 1);
    }

    #[test]
    fn a_line_too_long_to_keep_is_counted_and_the_rest_still_read() {
        let mut input = vec![b'x'; MAX_LINE + 1];
        input.push(b'\n');
        input.extend_from_slice(USER.as_bytes());
        let p = parse(input.as_slice()).unwrap();
        assert_eq!(p.bad_lines, 1);
        assert_eq!(p.prompts, 1);
    }

    #[test]
    fn read_capped_line_handles_every_ending() {
        let mut reader = "one\ntwo".as_bytes();
        let mut line = Vec::new();
        assert_eq!(read_capped_line(&mut reader, &mut line).unwrap(), Some(true));
        assert_eq!(line, b"one\n");
        assert_eq!(read_capped_line(&mut reader, &mut line).unwrap(), Some(true));
        assert_eq!(line, b"two");
        assert_eq!(read_capped_line(&mut reader, &mut line).unwrap(), None);
    }

    #[test]
    fn enormous_labels_are_cut_and_an_impossible_folder_is_ignored() {
        let huge = "n".repeat(MAX_LABEL * 2);
        let title = format!(r#"{{"type":"custom-title","customTitle":"{huge}"}}"#);
        let path = "/".to_string() + &"f".repeat(MAX_FOLDER + 1);
        let user = USER.replace("/work/a", &path);
        let p = run(&[&title, &user]);
        assert_eq!(p.name.len(), MAX_LABEL);
        assert_eq!(p.folder, None);
    }

    #[test]
    fn token_counts_that_would_overflow_stay_at_the_maximum() {
        let reply = format!(r#"{{"type":"assistant","message":{{"id":"m","model":"m","usage":{{"input_tokens":{0},"cache_read_input_tokens":{0}}}}}}}"#, u64::MAX);
        assert_eq!(run(&[&reply]).context_used, Some(u64::MAX));
    }

    #[test]
    fn only_the_newest_recaps_are_kept() {
        let lines: Vec<String> = (0..MAX_RECAPS + 3).map(|i| format!(r#"{{"type":"system","subtype":"away_summary","content":"recap {i}"}}"#)).collect();
        let p = run(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(p.recaps.len(), MAX_RECAPS);
        assert_eq!(p.recaps[0], format!("recap {}", MAX_RECAPS + 2));
    }

    #[test]
    fn empty_file_gives_an_empty_conversation() {
        let p = run(&[]);
        assert_eq!(p, Parsed::default());
    }

    #[test]
    fn reads_detail_records() {
        let cost = r#"{"type":"cost-state","totalCostUSD":1.5,"totalLinesAdded":10,"totalLinesRemoved":2}"#;
        let cost2 = r#"{"type":"cost-state","totalCostUSD":2,"totalLinesAdded":12,"totalLinesRemoved":3}"#;
        let pr = r#"{"type":"pr-link","prUrl":"https://example.test/pr/1"}"#;
        let cont = r#"{"type":"continued-in","continuedInSessionId":"abc"}"#;
        let p = run(&[cost, cost2, pr, cont]);
        assert_eq!(p.cost_usd, Some(2.0));
        assert_eq!((p.lines_added, p.lines_removed), (Some(12), Some(3)));
        assert_eq!(p.pr_url.as_deref(), Some("https://example.test/pr/1"));
        assert_eq!(p.continued_in.as_deref(), Some("abc"));
    }

    #[test]
    fn the_folder_stays_exact_so_resume_finds_it() {
        let p = run(&[r#"{"type":"user","cwd":"/work/two  spaces","message":{"content":"x"}}"#]);
        assert_eq!(p.folder.as_deref(), Some("/work/two  spaces"));
    }

    #[test]
    fn terminal_escapes_in_text_are_removed() {
        let evil = r#"{"type":"custom-title","customTitle":"a\u001b[31mb\u001b]0;pwn\u0007c\nd"}"#;
        assert_eq!(run(&[evil]).name, "abc d");
    }
}
