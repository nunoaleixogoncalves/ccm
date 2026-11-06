mod app;
mod clipboard;
mod ui;
mod worktrees;

use std::collections::{HashMap, HashSet};
use std::io::{stdin, stdout, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use ccm_core::catalog::Catalog;
use ccm_core::config::Config;
use ccm_core::delete::{self, Failure, Outcome, Plan};
use ccm_core::index::{Index, Searcher};
use ccm_core::model::Session;
use ccm_core::paths::Paths;
use ccm_core::recovery;
use ccm_core::registry;
use ccm_core::removal;
use ccm_core::repo::{self, Place};
use ccm_core::rename;
use ccm_core::forks;
use ccm_core::resume::{self, Fork, Launch};
use ccm_core::setup;
use ccm_core::worktree::{self, Scan};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, KeyModifiers};
use crossterm::execute;
use ratatui::DefaultTerminal;

use app::{App, RepoRequest};

/// Often enough to feel live, and cheap: a poll only stats files unless something changed.
/// Polling is needed anyway, because a killed process produces no file event.
const POLL: Duration = Duration::from_secs(2);
/// Subagent folders can grow without touching the transcript, so re-measure them less often.
const RESIZE_EVERY: u32 = 30;
const PROC: &str = "/proc";

const USAGE: &str = "Usage: ccm          browse your conversations
       ccm setup    install the hooks crash recovery needs, after showing the changes
       ccm --version";

fn main() -> Result<()> {
    let paths = Paths::from_env().ok_or_else(|| anyhow!("HOME is not set"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => browse(paths),
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

fn browse(paths: Paths) -> Result<()> {
    let (config, config_error) = Config::load(&paths.config);
    let catalog = Catalog::open(paths.clone()).context("opening the index")?;
    let cached = catalog.cached();
    let searcher = Searcher::open(&paths.index).context("opening the search index")?;

    let config_terminal = config.terminal.clone();
    let mut app = App::new(config, paths.claude.display().to_string(), home());
    app.status = config_error;
    let terminal_command = config_terminal.clone();
    let claude_json = paths.claude_json.clone();
    let fork_paths = paths.clone();
    app.resume = Some(Box::new(move |id, fork, folder| {
        if let Some(fork) = fork {
            forks::record(&fork_paths, &fork.id, id, now_ms()).map_err(|e| format!("Couldn't note the fork, so it wasn't started: {e}"))?;
        }
        resume_conversation(id, fork, folder, terminal_command.as_deref(), &claude_json)
    }));
    app.forks = forks::load(&paths);
    app.search = Some(Box::new(move |q| searcher.search(q).map_err(|e| e.to_string())));
    let plan_paths = paths.clone();
    app.planner = Some(Box::new(move |ids| delete::plan(&plan_paths, Path::new(PROC), ids)));
    let delete_paths = paths.clone();
    app.deleter = Some(Box::new(move |plan, notes| delete_conversations(&delete_paths, plan, notes)));
    let rename_paths = paths.clone();
    app.renamer = Some(Box::new(move |id, transcript, name| rename::rename(&rename_paths, Path::new(PROC), id, transcript, name)));
    app.copier = Some(Box::new(clipboard::copy));
    let dismiss_paths = paths.clone();
    app.dismisser = Some(Box::new(move |ids| recovery::dismiss(&dismiss_paths, ids, now_ms()).map_err(|e| e.to_string())));
    let remove_plan_paths = paths.clone();
    app.remove_planner = Some(Box::new(move |targets, sessions| removal::plan(&remove_plan_paths, Path::new(PROC), targets, sessions)));
    let remove_paths = paths.clone();
    app.remover = Some(Box::new(move |plan, sessions| removal::execute(&remove_paths, Path::new(PROC), plan, sessions)));
    app.set_sessions(cached);

    let updates = spawn_refresher(catalog);

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app, updates, &paths);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Also removes them from the search index, whose copy of their words would otherwise linger.
fn delete_conversations(paths: &Paths, plan: &Plan, notes: bool) -> Result<Outcome, String> {
    let mut outcome = delete::execute(paths, Path::new(PROC), plan, notes)?;
    if !outcome.deleted.is_empty() {
        let forgotten = Index::open(&paths.index).and_then(|mut index| index.forget(&outcome.deleted));
        if let Err(e) = forgotten {
            outcome.failures.push(Failure { path: PathBuf::from(&paths.index), error: format!("search entries stay until the next scan: {e}") });
        }
    }
    Ok(outcome)
}

/// Opens the conversation in `herdr-app` when running inside it, otherwise in a new terminal.
fn resume_conversation(id: &str, fork: Option<&Fork>, folder: &Path, terminal: Option<&[String]>, claude_json: &Path) -> Result<String, String> {
    let in_herdr = resume::in_herdr(|v| std::env::var(v).ok());
    let claude = resume::claude_args(id, fork);
    let (launch, place) = if in_herdr { herdr_plan(&claude, folder)? } else { (resume::terminal_launch(&claude, folder, terminal), "a new terminal") };
    if let Err(e) = resume::start(&launch) {
        return Err(format!("Couldn't start it ({e}). Run: {}", resume::manual_command(id, fork, folder)));
    }
    // Claude Code asks this itself, in the new terminal. Only the user may answer it.
    let warning = match resume::is_trusted(claude_json, folder) {
        Some(false) => " Claude Code will ask you to trust this folder there first.",
        _ => "",
    };
    Ok(format!("Opened in {place}.{warning}"))
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

/// Answers what each folder's repository is, reusing what the index saved before. Folders that
/// weren't in Git are asked about again on a full request, in case one has become a repository.
fn look_up_repos(index: &Path, request: &RepoRequest) -> HashMap<String, Place> {
    let mut index = Index::open(index).ok();
    let saved = index.as_ref().and_then(|i| i.repos().ok()).unwrap_or_default();
    let reusable = |f: &String| match saved.get(f) {
        Some(Place::Repo { .. }) => true,
        Some(_) => !request.full,
        None => false,
    };
    let ask: Vec<String> = request.folders.iter().filter(|f| !reusable(f)).cloned().collect();
    let answers = repo::look_up_all(&ask);
    if let Some(index) = index.as_mut() {
        let _ = index.save_repos(&answers, &request.used);
    }
    let mut out: HashMap<String, Place> =
        request.folders.iter().filter_map(|f| saved.get(f).filter(|_| reusable(f)).map(|p| (f.clone(), p.clone()))).collect();
    out.extend(answers);
    out
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
                let _ = tx.send(look_up_repos(&index, &request));
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
