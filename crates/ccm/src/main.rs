mod app;
mod clipboard;
mod repos;
mod ui;
mod web;
mod worktrees;

use std::collections::{HashMap, HashSet};
use std::io::{stdin, stdout, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use ccm_core::catalog::Catalog;
use ccm_core::config::ToolName;
use ccm_core::model::Source;
use ccm_core::tools::{self, codex::{self, Codex}};
use ccm_core::config::Config;
use ccm_core::delete::{self, Failure, Outcome, Plan, Skipped};
use ccm_core::index::{Index, Searcher};
use ccm_core::jobs;
use ccm_core::model::Session;
use ccm_core::paths::Paths;
use ccm_core::recovery;
use ccm_core::registry;
use ccm_core::removal;
use ccm_core::repo::Place;
use ccm_core::rename;
use ccm_core::forks;
use ccm_core::resume::{self, Fork, Launch};
use ccm_core::setup;
use ccm_core::worktree::{self, Scan};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, KeyModifiers};
use crossterm::execute;
use ratatui::DefaultTerminal;

use app::App;

/// Often enough to feel live, and cheap: a poll only stats files unless something changed.
/// Polling is needed anyway, because a killed process produces no file event.
const POLL: Duration = Duration::from_secs(2);
/// Subagent folders can grow without touching the transcript, so re-measure them less often.
const RESIZE_EVERY: u32 = 30;
const PROC: &str = "/proc";

const USAGE: &str = "Usage: ccm          browse your conversations
       ccm web      browse them in a browser, on this computer only
                    [--port N] choose the port  [--no-browser] only print the link
       ccm setup    install the hooks crash recovery needs, after showing the changes
       ccm --version";

fn main() -> Result<()> {
    let paths = Paths::from_env().ok_or_else(|| anyhow!("HOME is not set"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => browse(paths),
        ["web", rest @ ..] => match web::Options::parse(rest) {
            Some(options) => web::serve(paths, options),
            None => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        },
        ["setup"] => run_setup(&paths),
        ["hook"] => {
            run_hook(|input| recovery::record_hook(&paths, input, now_ms()));
            Ok(())
        }
        ["hook", "status-line"] => {
            run_hook(|input| recovery::record_status_line(&paths, input));
            Ok(())
        }
        ["--version" | "-V"] => {
            println!("ccm {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ["--help" | "-h"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

/// Runs inside every Claude Code session, so it must stay fast and silent: anything a
/// `SessionStart` hook prints goes into the conversation. A failure only costs crash information.
fn run_hook(record: impl FnOnce(&[u8]) -> std::io::Result<()>) {
    const MAX_INPUT: u64 = 1 << 20;
    let mut input = Vec::new();
    if stdin().take(MAX_INPUT).read_to_end(&mut input).is_ok() {
        let _ = record(&input);
    }
}

fn run_setup(paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe().context("finding this program's path")?;
    let plan = setup::plan(paths, &exe).map_err(|e| anyhow!(e))?;
    for note in &plan.notes {
        println!("{note}\n");
    }
    if plan.changes.is_empty() {
        println!("Nothing to change. ccm is already set up.");
        return Ok(());
    }
    for change in &plan.changes {
        println!("{}", change.path.display());
        for line in setup::diff(&change.before, &change.after) {
            println!("  {line}");
        }
        println!();
    }
    println!("Sessions already open aren't tracked until they're resumed. New sessions are.");
    print!("Apply these changes? [y/N] ");
    stdout().flush()?;
    let mut answer = String::new();
    stdin().read_line(&mut answer)?;
    if answer.trim() != "y" {
        println!("Nothing was changed.");
        return Ok(());
    }
    setup::apply(paths, &plan, now_ms()).map_err(|e| anyhow!(e))?;
    println!("Done. The originals are in {}.", paths.state.join("backups").display());
    Ok(())
}

/// What every front end starts from.
struct Start {
    config: Config,
    config_error: Option<String>,
    codex_home: PathBuf,
    /// Where conversations are looked for, for an empty list.
    looked_in: String,
    tools: Vec<Source>,
    catalog: Catalog,
    cached: Vec<Session>,
    searcher: Searcher,
}

fn start(paths: &Paths) -> Result<Start> {
    let (config, config_error) = Config::load(&paths.config);
    let codex_home = Codex::home_from(|v| std::env::var(v).ok(), Path::new(&home()));
    let tools = tools::detect(paths, Path::new(PROC), &codex_home, config.tools.as_deref());
    let sources: Vec<Source> = tools.iter().map(|t| t.source()).collect();
    let catalog = Catalog::with_tools(&paths.index, tools).context("opening the index")?;
    let cached = catalog.cached();
    let searcher = Searcher::open(&paths.index).context("opening the search index")?;
    let looked_in = match config.tools.as_deref() {
        Some(wanted) if !wanted.contains(&ToolName::Codex) => paths.claude.display().to_string(),
        Some(wanted) if !wanted.contains(&ToolName::Claude) => codex_home.display().to_string(),
        _ => format!("{} or {}", paths.claude.display(), codex_home.display()),
    };
    let tools = if sources.is_empty() { vec![Source::ClaudeCode] } else { sources };
    Ok(Start { config, config_error, codex_home, looked_in, tools, catalog, cached, searcher })
}

fn browse(paths: Paths) -> Result<()> {
    let Start { config, config_error, codex_home, looked_in, tools, catalog, cached, searcher } = start(&paths)?;
    let config_terminal = config.terminal.clone();
    let mut app = App::new(config, looked_in, home());
    app.tools = tools;
    app.status = config_error;
    app.resume = Some(Box::new(resumer(&paths, config_terminal)));
    app.forks = forks::load(&paths);
    app.search = Some(Box::new(move |q| searcher.search(q).map_err(|e| e.to_string())));
    let plan_paths = paths.clone();
    app.planner = Some(Box::new(move |sessions| plan_delete(&plan_paths, sessions)));
    let delete_paths = paths.clone();
    let delete_codex_home = codex_home.clone();
    app.deleter = Some(Box::new(move |plan, notes| delete_conversations(&delete_paths, &delete_codex_home, plan, notes)));
    app.renamer = Some(Box::new(renamer(&paths, &codex_home)));
    app.copier = Some(Box::new(clipboard::copy));
    app.dismisser = Some(Box::new(dismisser(&paths)));
    let remove_plan_paths = paths.clone();
    app.remove_planner = Some(Box::new(move |targets, sessions| removal::plan(&remove_plan_paths, Path::new(PROC), targets, sessions)));
    let offer_paths = paths.clone();
    app.worktree_offer = Some(Box::new(move |ids, sessions| removal::offer(&offer_paths, Path::new(PROC), ids, sessions)));
    let remove_paths = paths.clone();
    app.remover = Some(Box::new(move |plan, sessions| removal::execute(&remove_paths, Path::new(PROC), plan, sessions)));
    let jobs_paths = paths.clone();
    app.leftovers_finder = Some(Box::new(move || jobs::leftovers(&jobs_paths, Path::new(PROC), SystemTime::now())));
    let remove_jobs_paths = paths.clone();
    app.job_remover = Some(Box::new(move |dirs| jobs::remove(&remove_jobs_paths, Path::new(PROC), dirs, SystemTime::now())));
    app.set_sessions(cached);

    let updates = spawn_refresher(catalog);

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app, updates, &paths);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Opens a conversation, or a fork of it, the way the configuration and the surroundings ask.
fn resumer(paths: &Paths, terminal: Option<Vec<String>>) -> impl Fn(&str, Source, Option<&Fork>, &Path) -> Result<String, String> + Send + Sync + 'static {
    let paths = paths.clone();
    let log = paths.state.join("launch.log");
    move |id, source, fork, folder| {
        // Codex records the original in the fork itself.
        if let (Some(fork), Source::ClaudeCode) = (fork, source) {
            forks::record(&paths, &fork.id, id, now_ms()).map_err(|e| format!("Couldn't note the fork, so it wasn't started: {e}"))?;
        }
        resume_conversation(id, source, fork, folder, terminal.as_deref(), &paths.claude_json, &log)
    }
}

fn renamer(paths: &Paths, codex_home: &Path) -> impl Fn(&str, Source, &Path, &str) -> Result<String, String> + Send + Sync + 'static {
    let (paths, codex_home) = (paths.clone(), codex_home.to_path_buf());
    move |id, source, transcript, name| match source {
        Source::ClaudeCode => rename::rename(&paths, Path::new(PROC), id, transcript, name),
        Source::Codex => codex::rename(&codex_home, id, transcript, name),
    }
}

fn dismisser(paths: &Paths) -> impl Fn(&[String]) -> Result<(), String> + Send + Sync + 'static {
    let paths = paths.clone();
    move |ids| recovery::dismiss(&paths, ids, now_ms()).map_err(|e| e.to_string())
}

/// Each tool's conversations are planned by that tool's code.
fn plan_delete(paths: &Paths, sessions: &[Session]) -> Plan {
    let (codex, claude): (Vec<&Session>, Vec<&Session>) = sessions.iter().partition(|s| s.source == Source::Codex);
    let ids: Vec<String> = claude.iter().map(|s| s.id.clone()).collect();
    let mut plan = if ids.is_empty() { Plan::default() } else { delete::plan(paths, Path::new(PROC), &ids) };
    if !codex.is_empty() {
        let program = codex_program();
        let more = codex::plan_delete(&codex, program.as_deref());
        plan.items.extend(more.items);
        plan.skipped.extend(more.skipped);
        plan.warnings.extend(more.warnings);
    }
    plan
}

fn codex_program() -> Option<PathBuf> {
    resume::find_program("codex", &std::env::var("PATH").unwrap_or_default())
}

/// Also removes them from the search index, whose copy of their words would otherwise linger.
fn delete_conversations(paths: &Paths, codex_home: &Path, plan: &Plan, notes: bool) -> Result<Outcome, String> {
    let codex: Vec<&delete::Item> = plan.items.iter().filter(|i| i.source == Source::Codex).collect();
    let mut outcome = if codex.len() == plan.items.len() { Outcome::default() } else { delete::execute(paths, Path::new(PROC), plan, notes)? };
    if !codex.is_empty() {
        let done = match codex_program() {
            Some(program) => codex::delete(&program, codex_home, &codex),
            None => Outcome {
                skipped: codex.iter().map(|i| Skipped { id: i.id.clone(), reason: "the codex command isn't installed".into() }).collect(),
                ..Outcome::default()
            },
        };
        outcome.deleted.extend(done.deleted);
        outcome.skipped.extend(done.skipped);
        outcome.failures.extend(done.failures);
    }
    if !outcome.deleted.is_empty() {
        let forgotten = Index::open(&paths.index).and_then(|mut index| index.forget(&outcome.deleted));
        if let Err(e) = forgotten {
            outcome.failures.push(Failure { path: PathBuf::from(&paths.index), error: format!("search entries stay until the next scan: {e}") });
        }
    }
    Ok(outcome)
}

/// Opens the conversation in `herdr-app` when running inside it, otherwise in a new terminal.
fn resume_conversation(
    id: &str,
    source: Source,
    fork: Option<&Fork>,
    folder: &Path,
    terminal: Option<&[String]>,
    claude_json: &Path,
    log: &Path,
) -> Result<String, String> {
    let var = |v: &str| std::env::var(v).ok();
    let claude = resume::tool_args(source, id, fork, folder);
    let (launch, place) = if resume::in_herdr(var) {
        herdr_plan(&claude, folder)?
    } else if let Some(launch) = terminal.is_none().then(|| wsl_plan(&claude, folder)).flatten() {
        (launch, "a new Windows Terminal tab")
    } else {
        (resume::terminal_launch(&claude, folder, terminal), "a new terminal")
    };
    if let Err(e) = resume::start(&launch, Some(log)) {
        let why = if e.kind() == std::io::ErrorKind::NotFound {
            format!("{} isn't installed. Set terminal in ccm's config.toml to your terminal", ccm_core::format::sanitize(&launch.program))
        } else {
            e.to_string()
        };
        return Err(format!("Couldn't start it ({why}). Run: {}", resume::manual_command_for(source, id, fork, folder)));
    }
    // Claude Code asks this itself, in the new terminal. Only the user may answer it.
    let warning = match (source, resume::is_trusted(claude_json, folder)) {
        (Source::ClaudeCode, Some(false)) => " Claude Code will ask you to trust this folder there first.",
        _ => "",
    };
    Ok(format!("Opened in {place}.{warning}"))
}

/// Inside WSL, with Windows Terminal there to open, unless `terminal` says otherwise.
fn wsl_plan(claude: &[String], folder: &Path) -> Option<Launch> {
    let distro = resume::wsl_distro(|v| std::env::var(v).ok())?;
    let path = std::env::var("PATH").unwrap_or_default();
    resume::find_program("wt.exe", &path)?;
    let name = claude.first().map_or("claude", String::as_str);
    let claude_path = resume::find_program(name, &path).map_or_else(|| name.to_string(), |p| p.display().to_string());
    // The one setting that decides which conversations exist, so the resumed one is found.
    let var = if name == "codex" { "CODEX_HOME" } else { "CLAUDE_CONFIG_DIR" };
    let env: Vec<(String, String)> = std::env::var(var).ok().filter(|v| !v.is_empty()).map(|v| (var.to_string(), v)).into_iter().collect();
    Some(resume::wsl_launch(claude, folder, &distro, &claude_path, &env))
}

fn herdr_plan(claude: &[String], folder: &Path) -> Result<(Launch, &'static str), String> {
    let terminals = resume::herdr_terminals().map_err(|e| format!("Couldn't read herdr's terminals: {e}"))?;
    let space = folder.to_str().and_then(|f| resume::find_space(&terminals, f));
    let new_id = |prefix| resume::new_herdr_id(prefix).map_err(|e| e.to_string());
    let place = if space.is_some() { "a new tab of its workspace" } else { "a new workspace" };
    let launch = resume::herdr_launch(claude, folder, space.as_deref(), &new_id("tb")?, &new_id("sp")?);
    Ok((launch, place))
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

type Update = Result<Vec<Session>, String>;
/// A worktree scan, the sizes it measured when asked to, and the worktrees running sessions are in.
type ScanResult = (Scan, Option<HashMap<PathBuf, u64>>, HashSet<PathBuf>);

fn spawn_refresher(mut catalog: Catalog) -> Receiver<Update> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut tick = 0u32;
        loop {
            let update = catalog.refresh(tick.is_multiple_of(RESIZE_EVERY)).map_err(|e| e.to_string());
            if tx.send(update).is_err() {
                return;
            }
            tick = tick.wrapping_add(1);
            thread::sleep(POLL);
        }
    });
    rx
}

/// Sizes of the worktrees removing could lose nothing from, for the clean-up screen. Which of
/// them no conversation used is only known later, so this measures a few more than needed.
fn measure_worktrees(scan: &Scan) -> HashMap<PathBuf, u64> {
    scan.worktrees
        .iter()
        .filter(|w| w.is_linked() && !w.prunable && !w.locked && w.state.is_some_and(|s| s.changes == 0 && s.unpushed == 0))
        .map(|w| (w.path.clone(), delete::size_of(&w.path)))
        .collect()
}

/// Linked worktrees a running session is working in, which may have no transcript yet.
fn busy_worktrees(scan: &Scan, sessions_dir: &Path) -> HashSet<PathBuf> {
    let cwds: Vec<PathBuf> = registry::live_sessions(sessions_dir, Path::new(PROC)).into_iter().filter_map(|l| l.cwd.map(PathBuf::from)).collect();
    scan.worktrees.iter().filter(|w| w.is_linked() && cwds.iter().any(|c| c.starts_with(&w.path))).map(|w| w.path.clone()).collect()
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, updates: Receiver<Update>, paths: &Paths) -> Result<()> {
    let index = paths.index.clone();
    let mut dirty = true;
    let mut last_draw = Instant::now();
    let mut worktree_scan: Option<Receiver<ScanResult>> = None;
    let mut repo_lookup: Option<Receiver<HashMap<String, Place>>> = None;
    while !app.quit {
        if repo_lookup.is_none()
            && let Some(request) = app.repo_request()
        {
            let (tx, rx) = mpsc::channel();
            let index = index.clone();
            thread::spawn(move || {
                let _ = tx.send(repos::look_up(&index, &request));
            });
            repo_lookup = Some(rx);
        }
        if let Some(rx) = &repo_lookup {
            let finished = match rx.try_recv() {
                Ok(answers) => Some(answers),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(HashMap::new()),
            };
            if let Some(answers) = finished {
                app.finish_repo_lookup(answers);
                repo_lookup = None;
                dirty = true;
            }
        }
        if worktree_scan.is_none()
            && let Some(folders) = app.worktree_request()
        {
            let measure = app.worktree_sizes_wanted();
            let sessions_dir = paths.sessions();
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let scan = worktree::scan(&folders);
                let sizes = measure.then(|| measure_worktrees(&scan));
                let busy = busy_worktrees(&scan, &sessions_dir);
                let _ = tx.send((scan, sizes, busy));
            });
            worktree_scan = Some(rx);
            dirty = true;
        }
        if let Some(rx) = &worktree_scan {
            let finished = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some((Scan { errors: vec!["Looking for worktrees stopped unexpectedly.".into()], ..Scan::default() }, None, HashSet::new()))
                }
            };
            if let Some((scan, sizes, busy)) = finished {
                app.finish_worktree_scan(scan);
                app.set_busy_worktrees(busy);
                if let Some(sizes) = sizes {
                    app.finish_worktree_sizes(sizes);
                }
                worktree_scan = None;
                dirty = true;
            }
        }
        for update in updates.try_iter() {
            match update {
                Ok(sessions) => {
                    app.set_sessions(sessions);
                    app.loading = false;
                }
                Err(e) => app.status = Some(format!("Refresh failed: {e}")),
            }
            dirty = true;
        }
        // Keeps the "last active" ages current while nothing else changes.
        if dirty || app.counting_down() || last_draw.elapsed() >= Duration::from_secs(30) {
            app.ensure_preview();
            terminal.draw(|f| ui::draw(f, app, now_ms()))?;
            last_draw = Instant::now();
            dirty = false;
        }
        if app.has_pending_delete() {
            app.perform_delete();
            dirty = true;
            continue;
        }
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if k.modifiers.contains(KeyModifiers::CONTROL) {
                        app.on_ctrl(k.code);
                    } else {
                        app.on_key(k.code);
                    }
                }
                Event::Mouse(m) => app.on_mouse(m.kind, m.column, m.row),
                Event::Resize(..) => {}
                _ => continue,
            }
            dirty = true;
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
