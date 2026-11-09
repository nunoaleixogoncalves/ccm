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
use crate::model::{Session, Source};
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

impl Decision {
    /// Why it can't start, for the decisions that end there.
    pub fn refusal(&self) -> Option<&'static str> {
        match self {
            Decision::AlreadyOpen => Some("This conversation is already open."),
            Decision::NoFolder => Some("No folder is recorded for this conversation."),
            Decision::UnsafeFolder => Some("The recorded folder isn't a safe path, so it won't be used."),
            Decision::InvalidId => Some("This conversation's ID isn't valid."),
            Decision::Go(_) | Decision::FolderMissing(_) => None,
        }
    }
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

/// The command that resumes or forks a conversation of any tool. Codex picks a fork's ID
/// itself, so only Claude Code uses the one in `fork`. `-C` makes Codex use the folder without
/// asking, even when the conversation last worked elsewhere.
pub fn tool_args(source: Source, id: &str, fork: Option<&Fork>, folder: &Path) -> Vec<String> {
    match source {
        Source::ClaudeCode => claude_args(id, fork),
        Source::Codex => {
            let verb = if fork.is_some() { "fork" } else { "resume" };
            vec!["codex".into(), verb.into(), "-C".into(), folder.display().to_string(), id.into()]
        }
    }
}

/// Like `manual_command`, for any tool.
pub fn manual_command_for(source: Source, id: &str, fork: Option<&Fork>, folder: &Path) -> String {
    match source {
        Source::ClaudeCode => manual_command(id, fork, folder),
        Source::Codex => {
            let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
            let verb = if fork.is_some() { "fork" } else { "resume" };
            format!("cd {} && codex {verb} {id}", quote(&folder.display().to_string()))
        }
    }
}

/// The command to copy for resuming a conversation. This gets pasted into a shell, so a folder
/// that isn't safe is left out rather than quoted. A control character in it could end the paste
/// early, whatever the quoting.
pub fn copy_command(s: &Session) -> String {
    match (s.folder.as_deref().filter(|f| is_safe_folder(f)), s.source) {
        (Some(folder), source) => manual_command_for(source, &s.id, None, Path::new(folder)),
        (None, Source::Codex) => format!("codex resume {}", s.id),
        (None, Source::ClaudeCode) => format!("claude --resume {}", s.id),
    }
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

/// The WSL distribution `ccm` runs in, given a way to read the environment. WSL sets this in
/// every session it starts.
pub fn wsl_distro(var: impl Fn(&str) -> Option<String>) -> Option<String> {
    var("WSL_DISTRO_NAME").filter(|d| !d.is_empty() && !d.chars().any(hides_text))
}

/// Opens a Windows Terminal tab that starts the conversation inside this WSL distribution.
///
/// Windows programs don't understand Linux paths, so the folder goes to `wsl.exe --cd`, which
/// does. `--exec` starts `claude` directly, without a shell reading the command line, so
/// `claude` should be its full path: no login scripts run to find it. A Windows Terminal tab
/// doesn't take on `ccm`'s environment, so variables to carry over are set through `env`.
pub fn wsl_launch(claude: &[String], folder: &Path, distro: &str, claude_path: &str, env: &[(String, String)]) -> Launch {
    let mut inner: Vec<String> = vec!["wsl.exe".into(), "--distribution".into(), distro.into(), "--cd".into(), folder.display().to_string(), "--exec".into()];
    if !env.is_empty() {
        inner.push("/usr/bin/env".into());
        inner.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    }
    inner.push(claude_path.into());
    inner.extend(claude.iter().skip(1).cloned());
    // Windows Terminal reads a `;` anywhere as the start of its next command, unless escaped.
    let mut args = vec!["new-tab".to_string(), "--".to_string()];
    args.extend(inner.into_iter().map(|a| a.replace(';', "\\;")));
    Launch { program: "wt.exe".into(), args, cwd: folder.to_owned(), long_running: true }
}

/// The first executable called `name` in a `PATH`-style list of folders.
pub fn find_program(name: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
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

/// Starts the command detached, so it keeps running after `ccm` closes. A terminal launcher's
/// errors go to `log` when one is given, so a quick failure can say why. A file, not a pipe, as
/// a terminal still writing to a pipe after `ccm` quits could be killed by it.
pub fn start(launch: &Launch, log: Option<&Path>) -> io::Result<()> {
    let mut cmd = command(launch);

    if !launch.long_running {
        let out = cmd.stderr(Stdio::piped()).output()?;
        return if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        };
    }

    let errors = log.and_then(|p| open_log(p).ok());
    let stderr = match &errors {
        Some(file) => Stdio::from(file.try_clone()?),
        None => Stdio::null(),
    };
    let mut child = cmd.stderr(stderr).spawn()?;
    thread::sleep(LAUNCH_GRACE);
    if let Some(status) = child.try_wait()? {
        if !status.success() {
            let said = log.filter(|_| errors.is_some()).map(last_lines).unwrap_or_default();
            let said = if said.is_empty() { String::new() } else { format!(": {said}") };
            return Err(io::Error::other(format!("{} exited with {status}{said}", launch.program)));
        }
        return Ok(());
    }
    // Collects the exit status later, so a closed terminal doesn't stay a zombie.
    thread::spawn(move || child.wait());
    Ok(())
}

/// An owner-only log, emptied for each launch.
fn open_log(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
}

/// The end of what a launcher said, cleaned for the screen and kept short.
fn last_lines(path: &Path) -> String {
    let text = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(2)..].join(" ");
    let tail = sanitize(&tail);
    if tail.chars().count() > 200 { format!("{}…", tail.chars().take(200).collect::<String>()) } else { tail }
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
    fn codex_resumes_and_forks_in_the_folder_given_without_asking() {
        let folder = Path::new("/w/my proj");
        let id = "019a5a1e-7c3b-7d21-9f4e-2b6c1d8e0a11";
        assert_eq!(tool_args(Source::Codex, id, None, folder), ["codex", "resume", "-C", "/w/my proj", id]);
        let fork = Fork { id: "ignored".into(), name: "x".into() };
        assert_eq!(tool_args(Source::Codex, id, Some(&fork), folder), ["codex", "fork", "-C", "/w/my proj", id]);
        assert_eq!(tool_args(Source::ClaudeCode, id, None, folder), claude_args(id, None));
        assert_eq!(manual_command_for(Source::Codex, id, None, Path::new("/w/it's")), format!("cd '/w/it'\\''s' && codex resume {id}"));
        assert_eq!(manual_command_for(Source::Codex, id, Some(&fork), folder), format!("cd '/w/my proj' && codex fork {id}"));
    }

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
            context_window: None,
            prompts: 0,
            replies: 0,
            cost_usd: None,
            lines_added: None,
            lines_removed: None,
            pr_url: None,
            continued_in: None,
            forked_from: None,
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
        start(&l, None).unwrap();
        let ids = std::fs::read_to_string(&out).unwrap();
        let (pid, group) = ids.trim().split_once(' ').unwrap();
        assert_eq!(pid, group.trim());
    }

    #[test]
    fn a_failing_or_missing_program_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let l = |p: &str, long| Launch { program: p.into(), args: vec![], cwd: dir.path().into(), long_running: long };
        assert!(start(&l("/nonexistent/program", true), None).is_err());
        assert!(start(&l("false", true), None).is_err());
        assert!(start(&l("false", false), None).is_err());
        assert!(start(&l("true", true), None).is_ok());
    }

    #[test]
    fn a_launcher_that_fails_quickly_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("state/launch.log");
        let script = "echo 'warming up' >&2; echo 'no terminal found' >&2; exit 22";
        let l = Launch { program: "sh".into(), args: vec!["-c".into(), script.into()], cwd: dir.path().into(), long_running: true };
        let e = start(&l, Some(&log)).unwrap_err().to_string();
        assert!(e.contains("exit status: 22") && e.ends_with(": warming up no terminal found"), "{e}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);

        let quiet = Launch { program: "false".into(), args: vec![], cwd: dir.path().into(), long_running: true };
        assert!(start(&quiet, Some(&log)).unwrap_err().to_string().ends_with("exit status: 1"), "the last launch's words are gone");
    }

    #[test]
    fn wsl_is_its_distribution_name() {
        let env = |name: &'static str| move |v: &str| (v == "WSL_DISTRO_NAME").then(|| name.to_string());
        assert_eq!(wsl_distro(env("Ubuntu-24.04")), Some("Ubuntu-24.04".into()));
        assert_eq!(wsl_distro(env("")), None);
        assert_eq!(wsl_distro(env("Ub\u{1b}untu")), None);
        assert_eq!(wsl_distro(|_| None), None);
    }

    #[test]
    fn on_wsl_windows_terminal_starts_claude_in_the_linux_folder() {
        let fork = Fork { id: "f0000000-0000-4000-8000-000000000000".into(), name: "a; b (fork)".into() };
        let l = wsl_launch(&claude_args(ID, Some(&fork)), Path::new("/home/me/my project"), "Ubuntu", "/home/me/.local/bin/claude", &[]);
        assert_eq!(l.program, "wt.exe");
        assert_eq!(l.cwd, Path::new("/home/me/my project"));
        assert!(l.long_running);
        let expected: Vec<String> = [
            "new-tab", "--", "wsl.exe", "--distribution", "Ubuntu", "--cd", "/home/me/my project", "--exec", "/home/me/.local/bin/claude",
            "--resume", ID, "--fork-session", "--session-id", "f0000000-0000-4000-8000-000000000000", "--name=a\\; b (fork)",
        ]
        .map(String::from)
        .into();
        assert_eq!(l.args, expected);

        let env = [("CLAUDE_CONFIG_DIR".to_string(), "/home/me/.claude-work".to_string())];
        let l = wsl_launch(&claude_args(ID, None), Path::new("/w"), "Ubuntu", "claude", &env);
        assert_eq!(l.args[8..], ["/usr/bin/env", "CLAUDE_CONFIG_DIR=/home/me/.claude-work", "claude", "--resume", ID].map(String::from));
    }

    #[test]
    fn a_program_is_found_on_the_path_only_when_it_can_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("claude"), "").unwrap();
        std::fs::write(b.join("claude"), "").unwrap();
        std::fs::set_permissions(b.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("relative:{}:{}", a.display(), b.display());
        assert_eq!(find_program("claude", &path), Some(b.join("claude")), "not the one that can't run, nor a relative folder");
        assert_eq!(find_program("nothing", &path), None);
    }
}
