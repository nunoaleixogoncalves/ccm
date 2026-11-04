//! `ccm setup`: shows what it would change in Claude Code's config and applies it only after a yes.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::delete::replace_file;
use crate::paths::Paths;

const EVENTS: [&str; 2] = ["SessionStart", "SessionEnd"];
const STATUS_INPUT: &str = "input=$(cat)";
const STATUS_COMMENT: &str = "# ccm: saves the context window size.";
/// Long enough for a busy machine, and a hook that fails doesn't stop Claude Code.
const HOOK_TIMEOUT: u64 = 5;

/// One file's proposed new contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub changes: Vec<Change>,
    /// What setup couldn't do, and what to do instead.
    pub notes: Vec<String>,
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

fn hook_command(exe: &Path) -> String {
    format!("{} hook", quote(exe))
}

fn status_command(exe: &Path) -> String {
    format!("printf '%s' \"$input\" | {} hook status-line >/dev/null 2>&1 &", quote(exe))
}

/// An older hook of ours, such as from before the binary moved.
fn is_ours(command: &str) -> bool {
    command.ends_with("/ccm' hook")
}

pub fn plan(paths: &Paths, exe: &Path) -> Result<Plan, String> {
    let mut plan = Plan::default();
    let settings_path = paths.claude.join("settings.json");
    let before = read_or_empty(&settings_path)?;
    let mut settings: Value = if before.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&before).map_err(|e| format!("{} isn't valid JSON, so nothing was changed: {e}", settings_path.display()))?
    };
    add_hooks(&mut settings, &hook_command(exe)).map_err(|e| format!("{}: {e}", settings_path.display()))?;
    let mut after = serde_json::to_string_pretty(&settings).expect("JSON always serializes");
    if before.ends_with('\n') || before.is_empty() {
        after.push('\n');
    }
    if !same_json(&before, &after) {
        plan.changes.push(Change { path: settings_path, before, after });
    }
    plan_status_line(paths, exe, &settings, &mut plan)?;
    if others_can_replace(exe) {
        plan.notes.push(format!(
            "Claude Code will run {} in every session, and other users can replace it there. Install ccm somewhere only you can write to.",
            exe.display()
        ));
    }
    Ok(plan)
}

fn others_can_replace(exe: &Path) -> bool {
    let world_writable = |path: &Path| fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o002 != 0);
    world_writable(exe) || exe.parent().is_some_and(world_writable)
}

fn same_json(a: &str, b: &str) -> bool {
    matches!((serde_json::from_str::<Value>(a), serde_json::from_str::<Value>(b)), (Ok(x), Ok(y)) if x == y)
}

fn read_or_empty(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("Couldn't read {}: {e}", path.display())),
    }
}

/// Adds `ccm`'s hook beside whatever is there. Other people's hooks are never touched.
fn add_hooks(settings: &mut Value, command: &str) -> Result<(), String> {
    let root = settings.as_object_mut().ok_or("the top level isn't an object")?;
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().ok_or("\"hooks\" isn't an object")?;
    for event in EVENTS {
        let entries = hooks.entry(event).or_insert_with(|| json!([]));
        let entries = entries.as_array_mut().ok_or(format!("\"hooks.{event}\" isn't a list"))?;
        let mut present = false;
        for entry in entries.iter_mut() {
            let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else { continue };
            list.retain(|hook| {
                let current = hook.get("command").and_then(Value::as_str).unwrap_or_default();
                present |= current == command;
                current == command || !is_ours(current)
            });
        }
        entries.retain(|entry| entry.get("hooks").and_then(Value::as_array).is_none_or(|list| !list.is_empty()));
        if !present {
            entries.push(json!({
                "matcher": "*",
                "hooks": [{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT }],
            }));
        }
    }
    Ok(())
}

fn plan_status_line(paths: &Paths, exe: &Path, settings: &Value, plan: &mut Plan) -> Result<(), String> {
    let line = status_command(exe);
    let manual = format!("To save real window sizes, add this after your status line reads its input into $input:\n  {line}");
    let configured = settings
        .pointer("/statusLine/command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.ends_with("statusline.sh"));
    let path = paths.claude.join("statusline.sh");
    if !configured || !path.exists() {
        plan.notes.push(format!("No statusline.sh is in use. {manual}"));
        return Ok(());
    }
    let before = fs::read_to_string(&path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    let Some(after) = with_status_line(&before, &line) else {
        plan.notes.push(format!("statusline.sh has no \"{STATUS_INPUT}\" line. {manual}"));
        return Ok(());
    };
    if after != before {
        plan.changes.push(Change { path, before, after });
    }
    Ok(())
}

/// Puts our line right after the script reads its input, replacing any older one.
fn with_status_line(script: &str, line: &str) -> Option<String> {
    let mut out = String::with_capacity(script.len() + line.len() + STATUS_COMMENT.len());
    let mut placed = false;
    for current in script.split_inclusive('\n') {
        let text = current.trim_end_matches('\n');
        if text == STATUS_COMMENT || text.contains(" hook status-line ") {
            continue;
        }
        out.push_str(current);
        if !placed && text.trim() == STATUS_INPUT {
            if !current.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("{STATUS_COMMENT}\n{line}\n"));
            placed = true;
        }
    }
    placed.then_some(out)
}

/// Writes every change, keeping a copy of each original. Nothing is written if a file has changed
/// since the plan was shown.
pub fn apply(paths: &Paths, plan: &Plan, stamp: i64) -> Result<(), String> {
    for change in &plan.changes {
        if read_or_empty(&change.path)? != change.before {
            return Err(format!("{} changed in the meantime, so nothing was applied. Run ccm setup again.", change.path.display()));
        }
    }
    let backups = paths.state.join("backups");
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&backups).map_err(|e| format!("Couldn't create {}: {e}", backups.display()))?;
    for change in &plan.changes {
        let name = change.path.file_name().map_or("file".into(), |n| n.to_string_lossy().into_owned());
        if change.path.exists() {
            let copy = backups.join(format!("{name}.{stamp}"));
            fs::copy(&change.path, &copy).map_err(|e| format!("Couldn't back up {}: {e}", change.path.display()))?;
        }
        replace_file(&change.path, change.after.as_bytes(), || true).map_err(|e| format!("Couldn't write {}: {e}", change.path.display()))?;
    }
    Ok(())
}

/// The removed and added lines, each marked with `-` or `+`.
pub fn diff(before: &str, after: &str) -> Vec<String> {
    let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    let mut lengths = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lengths[i][j] = if a[i] == b[j] { lengths[i + 1][j + 1] + 1 } else { lengths[i + 1][j].max(lengths[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lengths[i][j + 1] >= lengths[i + 1][j]) {
            out.push(format!("+ {}", b[j]));
            j += 1;
        } else {
            out.push(format!("- {}", a[i]));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const EXE: &str = "/opt/my tools/ccm";
    const SCRIPT: &str = "#!/bin/bash\n# Status.\ninput=$(cat)\nMODEL=$(echo \"$input\")\nprintf 'x'\n";

    fn tree() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            claude: root.join("claude"),
            index: root.join("index.db"),
            config: root.join("config.toml"),
            claude_json: root.join("claude.json"),
            state: root.join("state/ccm"),
        };
        fs::create_dir_all(&paths.claude).unwrap();
        (dir, paths)
    }

    fn write(p: &Paths, name: &str, body: &str) -> PathBuf {
        let path = p.claude.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    fn settings_with_status() -> String {
        r#"{"statusLine":{"type":"command","command":"~/.claude/statusline.sh"}}"#.into()
    }

    fn after_plan(p: &Paths) -> Value {
        let plan = plan(p, Path::new(EXE)).unwrap();
        let change = plan.changes.iter().find(|c| c.path.ends_with("settings.json")).unwrap();
        serde_json::from_str(&change.after).unwrap()
    }

    #[test]
    fn existing_hooks_and_settings_are_preserved_in_order() {
        let (_d, p) = tree();
        write(&p, "settings.json", r#"{"zebra":1,"hooks":{"SessionStart":[{"matcher":"*","hooks":[{"type":"command","command":"mine.sh","timeout":10}]}],"Stop":[{"hooks":[{"type":"command","command":"stop.sh"}]}]},"apple":{"x":"é"}}"#);
        let v = after_plan(&p);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zebra", "hooks", "apple"]);
        assert_eq!(v["apple"]["x"], "é");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "stop.sh");
        let start = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start[0]["hooks"][0]["command"], "mine.sh");
        assert_eq!(start[1]["hooks"][0]["command"], "'/opt/my tools/ccm' hook");
        assert_eq!(v["hooks"]["SessionEnd"][0]["hooks"][0]["command"], "'/opt/my tools/ccm' hook");
    }

    #[test]
    fn a_missing_settings_file_is_created_on_apply() {
        let (_d, p) = tree();
        let plan = plan(&p, Path::new(EXE)).unwrap();
        apply(&p, &plan, 1).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(p.claude.join("settings.json")).unwrap()).unwrap();
        assert!(v["hooks"]["SessionEnd"].is_array());
    }

    #[test]
    fn broken_settings_are_refused_without_touching_anything() {
        let (_d, p) = tree();
        let path = write(&p, "settings.json", "{ not json");
        assert!(plan(&p, Path::new(EXE)).unwrap_err().contains("nothing was changed"));
        assert_eq!(fs::read_to_string(path).unwrap(), "{ not json");
        for odd in [r#"[]"#, r#"{"hooks":[]}"#, r#"{"hooks":{"SessionStart":{}}}"#] {
            write(&p, "settings.json", odd);
            assert!(plan(&p, Path::new(EXE)).is_err(), "{odd}");
        }
    }

    #[test]
    fn running_setup_twice_changes_nothing_more() {
        let (_d, p) = tree();
        write(&p, "settings.json", &settings_with_status());
        write(&p, "statusline.sh", SCRIPT);
        let first = plan(&p, Path::new(EXE)).unwrap();
        assert_eq!(first.changes.len(), 2);
        apply(&p, &first, 1).unwrap();
        assert_eq!(plan(&p, Path::new(EXE)).unwrap(), Plan::default());
    }

    #[test]
    fn a_moved_binary_replaces_the_old_hook_and_line() {
        let (_d, p) = tree();
        write(&p, "settings.json", &settings_with_status());
        write(&p, "statusline.sh", SCRIPT);
        apply(&p, &plan(&p, Path::new(EXE)).unwrap(), 1).unwrap();
        apply(&p, &plan(&p, Path::new("/new/ccm")).unwrap(), 2).unwrap();
        let text = fs::read_to_string(p.claude.join("settings.json")).unwrap();
        assert!(!text.contains("my tools") && text.matches("'/new/ccm' hook").count() == 2, "{text}");
        let script = fs::read_to_string(p.claude.join("statusline.sh")).unwrap();
        assert!(!script.contains("my tools") && script.matches("hook status-line").count() == 1, "{script}");
        assert_eq!(script.matches(STATUS_COMMENT).count(), 1);
    }

    #[test]
    fn another_tools_hook_with_a_similar_name_stays() {
        let (_d, p) = tree();
        write(&p, "settings.json", r#"{"hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":"'/x/notccm-helper' hook"},{"type":"command","command":"bash ccm.sh hook"}]}]}}"#);
        let v = after_plan(&p);
        let first = &v["hooks"]["SessionEnd"][0]["hooks"];
        assert_eq!(first.as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_status_line_goes_right_after_the_input_is_read() {
        let (_d, p) = tree();
        write(&p, "settings.json", &settings_with_status());
        let path = write(&p, "statusline.sh", SCRIPT);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        apply(&p, &plan(&p, Path::new(EXE)).unwrap(), 1).unwrap();
        let script = fs::read_to_string(&path).unwrap();
        assert_eq!(
            script,
            format!("#!/bin/bash\n# Status.\ninput=$(cat)\n{STATUS_COMMENT}\nprintf '%s' \"$input\" | '/opt/my tools/ccm' hook status-line >/dev/null 2>&1 &\nMODEL=$(echo \"$input\")\nprintf 'x'\n")
        );
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755, "still executable");
    }

    #[test]
    fn a_status_line_it_cannot_edit_gets_instructions_instead() {
        let (_d, p) = tree();
        write(&p, "settings.json", &settings_with_status());
        let path = write(&p, "statusline.sh", "#!/bin/sh\njq . <&0\n");
        let result = plan(&p, Path::new(EXE)).unwrap();
        assert!(result.changes.iter().all(|c| c.path != path));
        assert!(result.notes[0].contains("hook status-line"));

        write(&p, "settings.json", "{}");
        let unused = plan(&p, Path::new(EXE)).unwrap();
        assert!(unused.notes[0].contains("No statusline.sh"));
    }

    #[test]
    fn a_file_changed_after_the_plan_stops_everything() {
        let (_d, p) = tree();
        let settings = write(&p, "settings.json", &settings_with_status());
        let script = write(&p, "statusline.sh", SCRIPT);
        let plan = plan(&p, Path::new(EXE)).unwrap();
        fs::write(&script, "#!/bin/sh\ninput=$(cat)\nedited\n").unwrap();
        assert!(apply(&p, &plan, 1).unwrap_err().contains("nothing was applied"));
        assert_eq!(fs::read_to_string(settings).unwrap(), settings_with_status());
    }

    #[test]
    fn originals_are_backed_up() {
        let (_d, p) = tree();
        write(&p, "settings.json", &settings_with_status());
        write(&p, "statusline.sh", SCRIPT);
        apply(&p, &plan(&p, Path::new(EXE)).unwrap(), 42).unwrap();
        assert_eq!(fs::read_to_string(p.state.join("backups/settings.json.42")).unwrap(), settings_with_status());
        assert_eq!(fs::read_to_string(p.state.join("backups/statusline.sh.42")).unwrap(), SCRIPT);
    }

    #[test]
    fn a_program_others_could_replace_gets_a_warning() {
        let (dir, p) = tree();
        let shared = dir.path().join("shared");
        fs::create_dir_all(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(plan(&p, &shared.join("ccm")).unwrap().notes.iter().any(|n| n.contains("other users")));
        let private = dir.path().join("private");
        fs::create_dir_all(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!plan(&p, &private.join("ccm")).unwrap().notes.iter().any(|n| n.contains("other users")));
    }

    #[test]
    fn planning_writes_nothing() {
        let (_d, p) = tree();
        let settings = write(&p, "settings.json", "{}");
        plan(&p, Path::new(EXE)).unwrap();
        assert_eq!(fs::read_to_string(settings).unwrap(), "{}");
        assert!(!p.state.exists());
    }

    #[test]
    fn a_quote_in_the_path_stays_one_word() {
        assert_eq!(hook_command(Path::new("/a'b/ccm")), r"'/a'\''b/ccm' hook");
    }

    #[test]
    fn diff_shows_only_what_changed() {
        assert_eq!(diff("a\nb\nc\n", "a\nx\nc\nd\n"), ["+ x", "- b", "+ d"]);
        assert!(diff("same\n", "same\n").is_empty());
    }
}
