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

use crate::format::{hides_text, sanitize};
use crate::rename::MAX_NAME;
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
    if s.open && is_uuid(&s.id) {
        return Decision::AlreadyOpen;
    }
    decide_fork(s)
}

/// Like `decide`, but an open conversation can be forked: the copy is a new conversation and the
/// original carries on untouched.
pub fn decide_fork(s: &Session) -> Decision {
    if !is_uuid(&s.id) {
        return Decision::InvalidId;
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

/// A new conversation that starts as a copy of another. `ccm` picks its ID, so it knows which
/// conversation is the fork; Claude Code records no link between the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fork {
    pub id: String,
    pub name: String,
}

impl Fork {
    /// A fork named after the original, with a fresh random ID.
    pub fn of(original_name: &str) -> io::Result<Self> {
        let name = sanitize(original_name);
        let name = name.trim();
        let name = if name.is_empty() { "(fork)".to_string() } else { format!("{} (fork)", name.chars().take(MAX_NAME - 7).collect::<String>()) };
        Ok(Self { id: new_session_id()?, name })
    }
}

/// A random version 4 UUID, the form Claude Code's session IDs take.
pub fn new_session_id() -> io::Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

/// The command that resumes a conversation, or starts a fork of it. The name is joined to its
/// option, so one starting with `-` can't be read as another option.
pub fn claude_args(id: &str, fork: Option<&Fork>) -> Vec<String> {
    let mut args = vec!["claude".to_string(), "--resume".into(), id.into()];
    if let Some(f) = fork {
        args.extend(["--fork-session".into(), "--session-id".into(), f.id.clone(), format!("--name={}", f.name)]);
    }
    args
}

/// Opens a new terminal window, using `command` from the config if it's set. `claude` comes from
/// `claude_args`.
pub fn terminal_launch(claude: &[String], folder: &Path, command: Option<&[String]>) -> Launch {
    let (program, mut args) = match command {
        Some([program, rest @ ..]) => (program.clone(), rest.to_vec()),
        _ => ("xdg-terminal-exec".to_string(), vec![format!("--dir={}", folder.display()), "--".to_string()]),
    };
    args.extend_from_slice(claude);
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
pub fn herdr_launch(claude: &[String], folder: &Path, space: Option<&str>, tab_id: &str, new_space_id: &str) -> Launch {
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
    args.extend_from_slice(claude);
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
pub fn manual_command(id: &str, fork: Option<&Fork>, folder: &Path) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let args: Vec<String> = claude_args(id, fork).iter().map(|a| if a.starts_with("--name=") { quote(a) } else { a.clone() }).collect();
    format!("cd {} && {}", quote(&folder.display().to_string()), args.join(" "))
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
        let l = terminal_launch(&claude_args(ID, None), Path::new("/work/my dir; rm -rf"), None);
        assert_eq!(l.program, "xdg-terminal-exec");
        assert_eq!(l.args, ["--dir=/work/my dir; rm -rf", "--", "claude", "--resume", ID]);
        assert_eq!(l.cwd, Path::new("/work/my dir; rm -rf"));
        assert!(l.long_running);
    }

    #[test]
    fn a_fork_gets_a_new_id_and_a_name_that_cannot_pass_for_an_option() {
        let fork = Fork::of("-rf\u{1b}[2J Fix it").unwrap();
        assert!(is_uuid(&fork.id) && fork.id.as_bytes()[14] == b'4', "{}", fork.id);
        assert_ne!(Fork::of("x").unwrap().id, fork.id);
        assert!(!fork.name.contains('\u{1b}') && fork.name.ends_with("Fix it (fork)"), "{}", fork.name);
        assert_eq!(Fork::of("  ").unwrap().name, "(fork)");
        assert_eq!(Fork::of(&"n".repeat(500)).unwrap().name.chars().count(), MAX_NAME);
        let args = claude_args(ID, Some(&fork));
        assert_eq!(args[..5], ["claude", "--resume", ID, "--fork-session", "--session-id"]);
        assert_eq!(args[5], fork.id);
        assert_eq!(args[6], format!("--name={}", fork.name));
        let l = terminal_launch(&args, Path::new("/w"), None);
        assert_eq!(l.args[2..], args[..]);
    }

    #[test]
    fn the_manual_command_quotes_the_folder_and_the_name() {
        assert_eq!(manual_command(ID, None, Path::new("/w/it's")), format!("cd '/w/it'\\''s' && claude --resume {ID}"));
        let fork = Fork { id: "new".into(), name: "a'b (fork)".into() };
        assert_eq!(
            manual_command(ID, Some(&fork), Path::new("/w")),
            format!("cd '/w' && claude --resume {ID} --fork-session --session-id new '--name=a'\\''b (fork)'")
        );
    }

    #[test]
    fn an_open_conversation_can_be_forked_but_not_resumed() {
        let mut s = session(Some("/work/a"));
        s.open = true;
        assert_eq!(decide(&s), Decision::AlreadyOpen);
        assert_eq!(decide_fork(&s), Decision::Go("/work/a".into()));
        s.id = "--dangerous".into();
        assert_eq!(decide_fork(&s), Decision::InvalidId);
    }

    #[test]
    fn a_configured_terminal_replaces_the_default() {
        let cmd = ["foot".to_string(), "-e".to_string()];
        let l = terminal_launch(&claude_args(ID, None), Path::new("/w"), Some(&cmd));
        assert_eq!(l.program, "foot");
        assert_eq!(l.args, ["-e", "claude", "--resume", ID]);
        assert_eq!(terminal_launch(&claude_args(ID, None), Path::new("/w"), Some(&[])).program, "xdg-terminal-exec");
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
        let tab = herdr_launch(&claude_args(ID, None), Path::new("/w x"), Some("sp1"), "tb9", "spnew");
        assert_eq!(tab.program, "hc");
        assert!(!tab.long_running);
        assert_eq!(
            tab.args,
            ["run", "--bg", "--keep", "--no-parent", "--outlive-parent", "--no-notify", "--cwd", "/w x",
             "--label", "herdr.space=sp1", "--label", "herdr.tab=tb9", "--", "claude", "--resume", ID]
        );
        let new = herdr_launch(&claude_args(ID, None), Path::new("/w"), None, "tb9", "spnew");
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
        assert_eq!(manual_command(ID, None, Path::new("/a b")), format!("cd '/a b' && claude --resume {ID}"));
        assert_eq!(manual_command(ID, None, Path::new("/it's")), format!(r"cd '/it'\''s' && claude --resume {ID}"));
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
        let cmd = command(&terminal_launch(&claude_args(ID, None), Path::new("/w"), None));
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
        std::fs::write(&script, format!("echo $$ $(ps -o pgid= -p $$) > '{}'\n", out.display())).unwrap();
        // Run through `sh`, because executing a file just written can fail with "text file busy"
        // while another test's fork still holds it open.
        let l = Launch { program: "sh".into(), args: vec![script.display().to_string()], cwd: dir.path().into(), long_running: false };
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
