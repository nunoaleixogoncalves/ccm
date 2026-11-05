//! Works out how to reopen a conversation. Decisions are plain functions, and only `start` and
//! `herdr_terminals` touch the outside world.

use std::collections::HashSet;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde::Deserialize;

use crate::format::hides_text;
use crate::model::Session;
use crate::paths::is_uuid;

/// Claude Code's own session variables. A resumed session must not think it's a child of the
/// session `ccm` happened to be started from. Settings such as `CLAUDE_CONFIG_DIR` stay.
const SESSION_VARS: [&str; 10] = [
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
];

/// How long a terminal launcher gets to fail before it counts as started.
const LAUNCH_GRACE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Go(PathBuf),
    AlreadyOpen,
    NoFolder,
    /// The folder is gone, and can be recreated at this path.
    FolderMissing(PathBuf),
    /// The recorded folder isn't a clean absolute path, so `ccm` won't create or enter it.
    UnsafeFolder,
    InvalidId,
}

/// Transcript content is untrusted, so the folder is checked before it's used for anything.
pub fn decide(s: &Session) -> Decision {
    if !is_uuid(&s.id) {
        return Decision::InvalidId;
    }
    if s.open {
        return Decision::AlreadyOpen;
    }
    let Some(folder) = s.folder.as_deref() else { return Decision::NoFolder };
    let path = Path::new(folder);
    if !is_safe_folder(folder) {
        return Decision::UnsafeFolder;
    }
    if s.is_folder_missing() {
        Decision::FolderMissing(path.to_owned())
    } else {
        Decision::Go(path.to_owned())
    }
}

/// An absolute path that shows on screen exactly as it is, so what you're asked about is what's used.
pub fn is_safe_folder(folder: &str) -> bool {
    let path = Path::new(folder);
    path.is_absolute() && path.components().all(|c| !matches!(c, Component::ParentDir)) && !folder.chars().any(hides_text)
}

/// A command to start. Arguments stay separate and no shell is involved, so a folder name can
/// never be read as a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// False for a program that returns once its work is done, whose exit code is the result.
    pub long_running: bool,
}

fn claude_args(id: &str) -> Vec<String> {
    vec!["claude".into(), "--resume".into(), id.into()]
}

/// Opens a new terminal window, using `command` from the config if it's set.
pub fn terminal_launch(id: &str, folder: &Path, command: Option<&[String]>) -> Launch {
    let (program, mut args) = match command {
        Some([program, rest @ ..]) => (program.clone(), rest.to_vec()),
        _ => ("xdg-terminal-exec".to_string(), vec![format!("--dir={}", folder.display()), "--".to_string()]),
    };
    args.extend(claude_args(id));
    Launch { program, args, cwd: folder.to_owned(), long_running: true }
}

/// A live terminal in `herdr-app`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrTerminal {
    pub space: String,
    pub created_ms: i64,
    pub folder: String,
}

/// The workspace on `folder`, judged by the first terminal of each workspace. If several are on
/// it, the oldest wins so the choice doesn't change between runs.
pub fn find_space(terminals: &[HerdrTerminal], folder: &str) -> Option<String> {
    let mut sorted: Vec<&HerdrTerminal> = terminals.iter().collect();
    sorted.sort_by_key(|t| t.created_ms);
    let mut seen = HashSet::new();
    sorted
        .into_iter()
        .filter(|t| seen.insert(t.space.as_str()))
        .find(|t| t.folder == folder)
        .map(|t| t.space.clone())
}

/// Adds a tab to `space`, or creates a workspace when there's none. Labels `herdr-app` doesn't
/// know yet become a new tab or workspace.
pub fn herdr_launch(id: &str, folder: &Path, space: Option<&str>, tab_id: &str, new_space_id: &str) -> Launch {
    let mut args: Vec<String> = ["run", "--bg", "--keep", "--no-parent", "--outlive-parent", "--no-notify", "--cwd"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    args.push(folder.display().to_string());
    args.push("--label".into());
    args.push(format!("herdr.space={}", space.unwrap_or(new_space_id)));
    args.push("--label".into());
    args.push(format!("herdr.tab={tab_id}"));
    args.push("--".into());
    args.extend(claude_args(id));
    Launch { program: "hc".into(), args, cwd: folder.to_owned(), long_running: false }
}

#[derive(Deserialize)]
struct HcList {
    terminals: Vec<HcListed>,
}

#[derive(Deserialize)]
struct HcListed {
    address: String,
}

#[derive(Deserialize)]
struct HcTerminal {
    cwd: Option<String>,
    created_ms: Option<i64>,
    foreground: Option<HcForeground>,
    #[serde(default)]
    labels: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct HcForeground {
    cwd: Option<String>,
}

/// `cwd` can lag behind a program that changed folder, so the foreground process comes first.
fn parse_terminal(json: &[u8]) -> Option<HerdrTerminal> {
    let t: HcTerminal = serde_json::from_slice(json).ok()?;
    let folder = t.foreground.and_then(|f| f.cwd).or(t.cwd)?;
    Some(HerdrTerminal {
        space: t.labels.get("herdr.space")?.clone(),
        created_ms: t.created_ms.unwrap_or(i64::MAX),
        folder,
    })
}

fn hc_output(args: &[&str]) -> io::Result<Vec<u8>> {
    let out = Command::new("hc").args(args).stdin(Stdio::null()).output()?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

pub fn herdr_terminals() -> io::Result<Vec<HerdrTerminal>> {
    let list: HcList = serde_json::from_slice(&hc_output(&["ls", "--json"])?).map_err(io::Error::other)?;
    Ok(list
        .terminals
        .iter()
        .filter_map(|t| parse_terminal(&hc_output(&["get", &t.address, "--json"]).ok()?))
        .collect())
}

/// Whether `ccm` is running inside a `herdr-app` terminal, given a way to read the environment.
pub fn in_herdr(var: impl Fn(&str) -> Option<String>) -> bool {
    ["HC_TERMINAL_ID", "HC_TERMINAL_SOCKET"].iter().all(|v| var(v).is_some_and(|x| !x.is_empty()))
}

/// A random ID in the form `herdr-app` uses, such as `tb` followed by 32 hex digits.
pub fn new_herdr_id(prefix: &str) -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{prefix}{hex}"))
}

fn command(launch: &Launch) -> Command {
    let mut cmd = Command::new(&launch.program);
    cmd.args(&launch.args)
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    for var in SESSION_VARS {
        cmd.env_remove(var);
    }
    // A session of its own, so closing the terminal `ccm` runs in can't take this down with it.
    // SAFETY: `setsid` is async-signal-safe and touches no memory of this process.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// Starts the command detached, so it keeps running after `ccm` closes.
pub fn start(launch: &Launch) -> io::Result<()> {
    let mut cmd = command(launch);

    if !launch.long_running {
        let out = cmd.stderr(Stdio::piped()).output()?;
        return if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        };
    }

    let mut child = cmd.stderr(Stdio::null()).spawn()?;
    thread::sleep(LAUNCH_GRACE);
    if let Some(status) = child.try_wait()? {
        if !status.success() {
            return Err(io::Error::other(format!("{} exited with {status}", launch.program)));
        }
        return Ok(());
    }
    // Collects the exit status later, so a closed terminal doesn't stay a zombie.
    thread::spawn(move || child.wait());
    Ok(())
}

/// What a user can run by hand if starting a terminal fails.
pub fn manual_command(id: &str, folder: &Path) -> String {
    format!("cd '{}' && claude --resume {id}", folder.display().to_string().replace('\'', r"'\''"))
}

/// Whether Claude Code has accepted its trust prompt for this folder or a parent of it. An
/// unreadable file gives `None`, because that says nothing either way.
pub fn is_trusted(claude_json: &Path, folder: &Path) -> Option<bool> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        projects: std::collections::HashMap<String, Project>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Project {
        #[serde(default)]
        has_trust_dialog_accepted: bool,
    }
    let file: File = serde_json::from_slice(&std::fs::read(claude_json).ok()?).ok()?;
    Some(folder.ancestors().any(|p| {
        p.to_str()
            .and_then(|p| file.projects.get(p))
            .is_some_and(|project| project.has_trust_dialog_accepted)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_that_hide_what_they_are_are_not_safe() {
        assert!(is_safe_folder("/work/two  spaces/olá 日本"));
        for bad in ["relative", "/a/../b", "/x\u{1b}]/tmp/evil", "/tmp/evil\nx", "/x\u{3}y", "/a\u{202e}b", "/a\u{200b}b"] {
            assert!(!is_safe_folder(bad), "{bad:?}");
        }
    }
    use crate::model::Source;

    const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    fn session(folder: Option<&str>) -> Session {
        Session {
            id: ID.into(),
            source: Source::ClaudeCode,
            transcript_path: String::new(),
            transcript_size: 0,
            transcript_mtime_ns: 0,
            size_bytes: 0,
            name: String::new(),
            folder: folder.map(str::to_owned),
            folders: Vec::new(),
            branch: None,
            model: None,
            started_at_ms: None,
            last_active_ms: None,
            context_used: None,
            prompts: 0,
            replies: 0,
            cost_usd: None,
            lines_added: None,
            lines_removed: None,
            pr_url: None,
            continued_in: None,
            recaps: Vec::new(),
            bad_lines: 0,
            open: false,
            crashed: false,
            window_size: None,
            folder_exists: true,
        }
    }

    #[test]
    fn a_closed_conversation_goes_to_its_recorded_folder() {
        assert_eq!(decide(&session(Some("/work/a"))), Decision::Go("/work/a".into()));
    }

    #[test]
    fn an_open_conversation_is_blocked_before_anything_else() {
        let mut s = session(None);
        s.open = true;
        assert_eq!(decide(&s), Decision::AlreadyOpen);
    }

    #[test]
    fn a_missing_folder_is_offered_for_recreation() {
        let mut s = session(Some("/gone/a"));
        s.folder_exists = false;
        assert_eq!(decide(&s), Decision::FolderMissing("/gone/a".into()));
    }

    #[test]
    fn no_folder_is_reported() {
        assert_eq!(decide(&session(None)), Decision::NoFolder);
    }

    #[test]
    fn untrusted_folders_and_ids_are_refused() {
        for bad in ["relative/path", "/work/../../etc", "../up", ""] {
            assert_eq!(decide(&session(Some(bad))), Decision::UnsafeFolder, "{bad}");
        }
        let mut s = session(Some("/work/a"));
        s.id = "--dangerous".into();
        assert_eq!(decide(&s), Decision::InvalidId);
    }

    #[test]
    fn a_new_terminal_keeps_every_argument_separate() {
        let l = terminal_launch(ID, Path::new("/work/my dir; rm -rf"), None);
        assert_eq!(l.program, "xdg-terminal-exec");
        assert_eq!(l.args, ["--dir=/work/my dir; rm -rf", "--", "claude", "--resume", ID]);
        assert_eq!(l.cwd, Path::new("/work/my dir; rm -rf"));
        assert!(l.long_running);
    }

    #[test]
    fn a_configured_terminal_replaces_the_default() {
        let cmd = ["foot".to_string(), "-e".to_string()];
        let l = terminal_launch(ID, Path::new("/w"), Some(&cmd));
        assert_eq!(l.program, "foot");
        assert_eq!(l.args, ["-e", "claude", "--resume", ID]);
        assert_eq!(terminal_launch(ID, Path::new("/w"), Some(&[])).program, "xdg-terminal-exec");
    }

    fn term(space: &str, created_ms: i64, folder: &str) -> HerdrTerminal {
        HerdrTerminal { space: space.into(), created_ms, folder: folder.into() }
    }

    #[test]
    fn a_workspace_is_matched_by_its_first_terminal_only() {
        let ts = [term("a", 2, "/x"), term("a", 1, "/first"), term("b", 3, "/other")];
        assert_eq!(find_space(&ts, "/first").as_deref(), Some("a"));
        assert_eq!(find_space(&ts, "/x"), None, "a later tab elsewhere doesn't define the workspace");
        assert_eq!(find_space(&ts, "/nope"), None);
    }

    #[test]
    fn the_oldest_workspace_on_a_folder_wins() {
        let ts = [term("new", 9, "/x"), term("old", 1, "/x")];
        assert_eq!(find_space(&ts, "/x").as_deref(), Some("old"));
    }

    #[test]
    fn herdr_adds_a_tab_or_a_workspace_and_outlives_ccm() {
        let tab = herdr_launch(ID, Path::new("/w x"), Some("sp1"), "tb9", "spnew");
        assert_eq!(tab.program, "hc");
        assert!(!tab.long_running);
        assert_eq!(
            tab.args,
            ["run", "--bg", "--keep", "--no-parent", "--outlive-parent", "--no-notify", "--cwd", "/w x",
             "--label", "herdr.space=sp1", "--label", "herdr.tab=tb9", "--", "claude", "--resume", ID]
        );
        let new = herdr_launch(ID, Path::new("/w"), None, "tb9", "spnew");
        assert!(new.args.contains(&"herdr.space=spnew".to_string()));
    }

    #[test]
    fn hc_output_prefers_the_foreground_folder() {
        let json = br#"{"cwd":"/stale","created_ms":5,"foreground":{"cwd":"/real"},"labels":{"herdr.space":"sp1"}}"#;
        assert_eq!(parse_terminal(json), Some(term("sp1", 5, "/real")));
        let json = br#"{"cwd":"/only","created_ms":5,"foreground":null,"labels":{"herdr.space":"sp1"}}"#;
        assert_eq!(parse_terminal(json).unwrap().folder, "/only");
    }

    #[test]
    fn terminals_outside_herdr_are_ignored() {
        assert_eq!(parse_terminal(br#"{"cwd":"/x","created_ms":1,"labels":{}}"#), None);
        assert_eq!(parse_terminal(b"nonsense"), None);
    }

    #[test]
    fn herdr_needs_both_variables() {
        let both = |v: &str| Some(format!("value-{v}"));
        assert!(in_herdr(both));
        assert!(!in_herdr(|v: &str| (v == "HC_TERMINAL_ID").then(|| "t1".to_string())));
        assert!(!in_herdr(|_: &str| Some(String::new())));
        assert!(!in_herdr(|_: &str| None));
    }

    #[test]
    fn herdr_ids_have_the_expected_form() {
        let id = new_herdr_id("tb").unwrap();
        assert_eq!(id.len(), 34);
        assert!(id.starts_with("tb"));
        assert_ne!(id, new_herdr_id("tb").unwrap());
    }

    #[test]
    fn the_manual_command_is_safe_to_paste() {
        assert_eq!(manual_command(ID, Path::new("/a b")), format!("cd '/a b' && claude --resume {ID}"));
        assert_eq!(manual_command(ID, Path::new("/it's")), format!(r"cd '/it'\''s' && claude --resume {ID}"));
    }

    #[test]
    fn trust_counts_the_folder_and_its_parents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude.json");
        std::fs::write(&p, r#"{"projects":{"/yes":{"hasTrustDialogAccepted":true},"/no":{"hasTrustDialogAccepted":false},"/bare":{}}}"#).unwrap();
        assert_eq!(is_trusted(&p, Path::new("/yes")), Some(true));
        assert_eq!(is_trusted(&p, Path::new("/yes/deep/down")), Some(true));
        assert_eq!(is_trusted(&p, Path::new("/no")), Some(false));
        assert_eq!(is_trusted(&p, Path::new("/bare")), Some(false));
        assert_eq!(is_trusted(&p, Path::new("/other")), Some(false));
        assert_eq!(is_trusted(Path::new("/nonexistent"), Path::new("/yes")), None);
    }

    #[test]
    fn session_variables_are_cleared_and_settings_kept() {
        let cmd = command(&terminal_launch(ID, Path::new("/w"), None));
        let removed: Vec<_> = cmd.get_envs().filter(|(_, v)| v.is_none()).map(|(k, _)| k.to_owned()).collect();
        assert!(removed.iter().any(|k| k == "CLAUDECODE"));
        assert!(removed.iter().any(|k| k == "CLAUDE_CODE_SESSION_ID"));
        assert!(!removed.iter().any(|k| k == "CLAUDE_CONFIG_DIR"));
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/w")));
    }

    #[test]
    fn a_started_program_runs_in_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("ids");
        let script = dir.path().join("s.sh");
        // A new session makes it the leader of a new process group. `sid` isn't in macOS's `ps`.
        std::fs::write(&script, format!("#!/bin/sh\necho $$ $(ps -o pgid= -p $$) > '{}'\n", out.display())).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let l = Launch { program: script.display().to_string(), args: vec![], cwd: dir.path().into(), long_running: false };
        start(&l).unwrap();
        let ids = std::fs::read_to_string(&out).unwrap();
        let (pid, group) = ids.trim().split_once(' ').unwrap();
        assert_eq!(pid, group.trim());
    }

    #[test]
    fn a_failing_or_missing_program_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let l = |p: &str, long| Launch { program: p.into(), args: vec![], cwd: dir.path().into(), long_running: long };
        assert!(start(&l("/nonexistent/program", true)).is_err());
        assert!(start(&l("false", true)).is_err());
        assert!(start(&l("false", false)).is_err());
        assert!(start(&l("true", true)).is_ok());
    }
}
