//! `ccm web`: the conversation list in a browser, for this computer only. See `guard` for how
//! the server is kept private. The page is built into the program, so nothing else is installed.

mod guard;
mod list;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;

use anyhow::{anyhow, Result};
use ccm_core::config::Config;
use ccm_core::forks;
use ccm_core::index::Searcher;
use ccm_core::model::{Session, Source};
use ccm_core::paths::Paths;
use ccm_core::resume;
use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

use crate::repos::{self, Repos};
use guard::{Guard, Headers, Verdict};

const INDEX: &str = include_str!("web/page/index.html");
const SCRIPT: &str = include_str!("web/page/app.js");
const STYLE: &str = include_str!("web/page/style.css");
const ICON: &str = include_str!("web/page/icon.svg");

/// Previews and Git run while other requests wait, so one slow answer doesn't hold up the page.
const WORKERS: usize = 4;

/// The page only ever loads its own files, which limits what text in a conversation could do
/// even if it got past the page's own handling.
const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    /// 0 picks a free port.
    pub port: u16,
    pub open_browser: bool,
}

impl Options {
    pub fn parse(args: &[&str]) -> Option<Self> {
        let mut options = Self { port: 0, open_browser: true };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match *arg {
                "--port" => options.port = args.next()?.parse().ok()?,
                "--no-browser" => options.open_browser = false,
                _ => return None,
            }
        }
        Some(options)
    }
}

/// The latest conversations.
struct Live {
    sessions: Arc<Vec<Session>>,
    /// Each fork and the conversation it copied.
    forks: Arc<HashMap<String, String>>,
    /// Changes when the list does, so the page knows to ask for it again.
    version: u64,
    /// True until the first scan finishes.
    loading: bool,
    /// Why the last scan failed.
    problem: Option<String>,
}

pub(crate) struct State {
    live: RwLock<Live>,
    repos: Mutex<Repos>,
    searcher: Mutex<Searcher>,
    config: Config,
    config_error: Option<String>,
    home: String,
    looked_in: String,
    tools: Vec<Source>,
    index: PathBuf,
}

// A panic in one request must not take the page down for the others.
fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Scans come back in no fixed order, and a new order alone isn't a change.
fn by_id(mut sessions: Vec<Session>) -> Vec<Session> {
    sessions.sort_by(|a, b| a.id.cmp(&b.id));
    sessions
}

fn forks_of(paths: &Paths, sessions: &[Session]) -> HashMap<String, String> {
    let mut forks = forks::load(paths);
    for s in sessions {
        if let Some(from) = &s.forked_from {
            forks.entry(s.id.clone()).or_insert_with(|| from.clone());
        }
    }
    forks
}

impl State {
    fn apply(&self, update: crate::Update, paths: &Paths) {
        let mut live = write(&self.live);
        match update {
            Ok(sessions) => {
                let sessions = by_id(sessions);
                if live.loading || *live.sessions != sessions {
                    live.forks = Arc::new(forks_of(paths, &sessions));
                    live.sessions = Arc::new(sessions);
                    live.version += 1;
                }
                live.loading = false;
                live.problem = None;
            }
            Err(e) => {
                live.problem = Some(format!("Refresh failed: {e}"));
                live.version += 1;
            }
        }
    }

    /// Starts asking Git about folders it hasn't answered for. The list follows when it's done.
    fn look_up_repos(self: &Arc<Self>, sessions: &[Session], again: bool) {
        let request = {
            let mut repos = lock(&self.repos);
            if again {
                repos.ask_again();
            }
            repos.request(sessions)
        };
        let Some(request) = request else { return };
        let state = Arc::clone(self);
        thread::spawn(move || {
            let answers = repos::look_up(&state.index, &request);
            lock(&state.repos).finish(answers);
            write(&state.live).version += 1;
        });
    }
}

pub fn serve(paths: Paths, options: Options) -> Result<()> {
    let start = crate::start(&paths)?;
    let server = Server::http(("127.0.0.1", options.port)).map_err(|e| anyhow!("Couldn't start on port {}: {e}", options.port))?;
    let port = server.server_addr().to_ip().map(|a| a.port()).ok_or_else(|| anyhow!("Couldn't tell which port it's on"))?;
    let (guard, login) = Guard::new(port)?;

    let sessions = by_id(start.cached);
    let state = Arc::new(State {
        live: RwLock::new(Live {
            forks: Arc::new(forks_of(&paths, &sessions)),
            sessions: Arc::new(sessions),
            version: 1,
            loading: true,
            problem: None,
        }),
        repos: Mutex::new(Repos::default()),
        searcher: Mutex::new(start.searcher),
        config: start.config,
        config_error: start.config_error,
        home: crate::home(),
        looked_in: start.looked_in,
        tools: start.tools,
        index: paths.index.clone(),
    });

    let updates = crate::spawn_refresher(start.catalog);
    {
        let state = Arc::clone(&state);
        thread::spawn(move || {
            for update in updates {
                state.apply(update, &paths);
            }
        });
    }

    let link = format!("http://127.0.0.1:{port}/?token={login}");
    println!("ccm web is running on this computer only.");
    println!("This link works once. To use another browser, start ccm web again:\n\n  {link}\n");
    println!("Press Ctrl+C to stop.");
    if options.open_browser && !open_browser(&link) {
        println!("Couldn't open a browser, so open the link yourself.");
    }

    run(Arc::new(server), state, Arc::new(guard));
    Ok(())
}

fn run(server: Arc<Server>, state: Arc<State>, guard: Arc<Guard>) {
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (server, state, guard) = (Arc::clone(&server), Arc::clone(&state), Arc::clone(&guard));
            thread::spawn(move || {
                while let Ok(request) = server.recv() {
                    answer(request, &state, &guard);
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
}

/// Opens the system's browser, without waiting for it. A false return means it couldn't start.
fn open_browser(link: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    let in_wsl = resume::wsl_distro(|v| std::env::var(v).ok()).is_some();
    let program = match () {
        _ if cfg!(target_os = "macos") => "open",
        _ if in_wsl && resume::find_program("wslview", &path).is_some() => "wslview",
        _ if in_wsl => "explorer.exe",
        _ => "xdg-open",
    };
    let started = Command::new(program).arg(link).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    match started {
        Ok(mut child) => {
            thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
    }
}

/// The part of a reply that doesn't depend on the HTTP library.
#[derive(Debug, PartialEq)]
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    location: Option<&'static str>,
    cookie: Option<String>,
}

impl Reply {
    fn file(content_type: &'static str, body: &str) -> Self {
        Self { status: 200, content_type, body: body.as_bytes().to_vec(), location: None, cookie: None }
    }

    fn text(status: u16, message: &str) -> Self {
        Self { status, content_type: "text/plain; charset=utf-8", body: message.as_bytes().to_vec(), location: None, cookie: None }
    }

    fn json(value: &Value) -> Self {
        Self { status: 200, content_type: "application/json", body: value.to_string().into_bytes(), location: None, cookie: None }
    }

    fn home(cookie: Option<String>) -> Self {
        Self { status: 303, content_type: "text/plain; charset=utf-8", body: Vec::new(), location: Some("/"), cookie }
    }
}

fn answer(request: tiny_http::Request, state: &Arc<State>, guard: &Guard) {
    let find = |name: &'static str| request.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str().to_string());
    let headers = Headers { host: find("Host"), origin: find("Origin"), fetch_site: find("Sec-Fetch-Site"), cookie: find("Cookie") };
    let method = request.method().as_str().to_string();
    let reply = handle(state, guard, &method, request.url(), &headers);
    let mut response = Response::from_data(reply.body).with_status_code(reply.status);
    let mut add = |name: &str, value: &str| {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    };
    add("Content-Type", reply.content_type);
    add("Content-Security-Policy", POLICY);
    add("X-Content-Type-Options", "nosniff");
    add("Referrer-Policy", "no-referrer");
    add("Cross-Origin-Resource-Policy", "same-origin");
    add("Cache-Control", "no-store");
    if let Some(location) = reply.location {
        add("Location", location);
    }
    if let Some(cookie) = &reply.cookie {
        add("Set-Cookie", cookie);
    }
    let _ = request.respond(response);
}

fn handle(state: &Arc<State>, guard: &Guard, method: &str, url: &str, headers: &Headers) -> Reply {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let params = list::params(query);
    match guard.check(headers, path, params.get("token").map(String::as_str)) {
        Verdict::Refuse(status, message) => return Reply::text(status, message),
        Verdict::LoggedIn(cookie) => return Reply::home(Some(cookie)),
        Verdict::DropToken => return Reply::home(None),
        Verdict::Pass => {}
    }
    if method != "GET" {
        return Reply::text(405, "Method not allowed");
    }
    match path {
        "/" => Reply::file("text/html; charset=utf-8", INDEX),
        "/app.js" => Reply::file("text/javascript; charset=utf-8", SCRIPT),
        "/style.css" => Reply::file("text/css; charset=utf-8", STYLE),
        "/icon.svg" => Reply::file("image/svg+xml", ICON),
        "/api/version" => {
            let live = read(&state.live);
            Reply::json(&json!({ "version": live.version, "loading": live.loading }))
        }
        "/api/list" => Reply::json(&list_reply(state, &params)),
        "/api/details" => match details_reply(state, params.get("id").map_or("", String::as_str)) {
            Some(value) => Reply::json(&value),
            None => Reply::text(404, "That conversation is gone."),
        },
        _ => Reply::text(404, "Not found"),
    }
}

fn now_ms() -> i64 {
    crate::now_ms()
}

fn list_reply(state: &Arc<State>, params: &HashMap<String, String>) -> Value {
    let (sessions, version, loading, problem) = {
        let live = read(&state.live);
        (Arc::clone(&live.sessions), live.version, live.loading, live.problem.clone())
    };
    let query = list::Query::parse(params, &state.tools);
    if query.grouping == crate::app::Grouping::Repo {
        state.look_up_repos(&sessions, params.contains_key("fresh"));
    }
    let matches = lock(&state.searcher).search(&query.text);
    let (matches, search_error) = match matches {
        Ok(found) => (found, None),
        Err(e) => (None, Some(format!("Search failed: {e}"))),
    };
    let places = lock(&state.repos).places.clone();
    let view = list::View {
        sessions: &sessions,
        matches: matches.as_ref(),
        places: &places,
        config: &state.config,
        home: &state.home,
        looked_in: &state.looked_in,
        tools: &state.tools,
        now_ms: now_ms(),
    };
    let notice = problem.or(search_error).or_else(|| state.config_error.clone());
    list::render(&view, &query, version, loading, notice.as_deref())
}

fn details_reply(state: &State, id: &str) -> Option<Value> {
    let (sessions, forks) = {
        let live = read(&state.live);
        (Arc::clone(&live.sessions), Arc::clone(&live.forks))
    };
    let session = sessions.iter().find(|s| s.id == id)?;
    Some(list::details(session, &sessions, &forks, ccm_core::tools::preview(session).map_err(|e| e.to_string())))
}

#[cfg(test)]
impl State {
    fn for_tests(root: &std::path::Path, sessions: Vec<Session>) -> Arc<Self> {
        let index = root.join("index.db");
        drop(ccm_core::index::Index::open(&index).unwrap());
        Arc::new(State {
            live: RwLock::new(Live { sessions: Arc::new(by_id(sessions)), forks: Arc::default(), version: 1, loading: false, problem: None }),
            repos: Mutex::new(Repos::default()),
            searcher: Mutex::new(Searcher::open(&index).unwrap()),
            config: Config::default(),
            config_error: None,
            home: "/home/me".into(),
            looked_in: "/home/me/.claude".into(),
            tools: vec![Source::ClaudeCode],
            index,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use super::*;
    use guard::cookie_pair;

    #[test]
    fn options() {
        assert_eq!(Options::parse(&[]), Some(Options { port: 0, open_browser: true }));
        assert_eq!(Options::parse(&["--port", "9000", "--no-browser"]), Some(Options { port: 9000, open_browser: false }));
        assert_eq!(Options::parse(&["--port"]), None);
        assert_eq!(Options::parse(&["--port", "many"]), None);
        assert_eq!(Options::parse(&["--bind", "0.0.0.0"]), None);
    }

    fn exchange(port: u16, request: String) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    }

    fn cookie_from(response: &str) -> String {
        let line = response.lines().find(|l| l.to_lowercase().starts_with("set-cookie:")).expect("a cookie");
        line.split_once(':').unwrap().1.trim().split(';').next().unwrap().to_string()
    }

    #[test]
    fn over_a_real_connection_nothing_works_without_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::for_tests(dir.path(), Vec::new());
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let (guard, login) = Guard::new(port).unwrap();
        thread::spawn(move || run(Arc::new(server), state, Arc::new(guard)));
        let host = format!("127.0.0.1:{port}");
        let get = |path: &str, extra: &str| exchange(port, format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{extra}\r\n"));

        assert!(get("/api/list", "").starts_with("HTTP/1.1 401"));
        assert!(get("/?token=wrong", "").starts_with("HTTP/1.1 401"));
        let refused = exchange(port, format!("GET /api/list HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n"));
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");

        let signed_in = get(&format!("/?token={login}"), "");
        assert!(signed_in.starts_with("HTTP/1.1 303"), "{signed_in}");
        assert!(signed_in.to_lowercase().contains("httponly"));
        assert!(signed_in.contains("SameSite=Strict"));
        assert!(get(&format!("/?token={login}"), "").starts_with("HTTP/1.1 401"), "the link works once");

        let cookie = cookie_from(&signed_in);
        let page = get("/", &format!("Cookie: {cookie}\r\n"));
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("Content-Security-Policy"));
        assert!(get("/api/list", &format!("Cookie: {cookie}\r\n")).starts_with("HTTP/1.1 200"));

        let other_site = get("/api/list", &format!("Cookie: {cookie}\r\nOrigin: http://evil.example\r\n"));
        assert!(other_site.starts_with("HTTP/1.1 403"), "{other_site}");
    }

    #[test]
    fn only_get_is_answered_for_now() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::for_tests(dir.path(), Vec::new());
        let (guard, _) = Guard::new(4000).unwrap();
        let headers = Headers { host: Some("127.0.0.1:4000".into()), origin: None, fetch_site: None, cookie: Some(cookie_pair(&guard)) };
        assert_eq!(handle(&state, &guard, "POST", "/api/list", &headers).status, 405);
        assert_eq!(handle(&state, &guard, "DELETE", "/api/list", &headers).status, 405);
    }

    #[test]
    fn an_unknown_conversation_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::for_tests(dir.path(), Vec::new());
        let (guard, _) = Guard::new(4000).unwrap();
        let headers = Headers { host: Some("127.0.0.1:4000".into()), origin: None, fetch_site: None, cookie: Some(cookie_pair(&guard)) };
        assert_eq!(handle(&state, &guard, "GET", "/api/details?id=nope", &headers).status, 404);
        assert_eq!(handle(&state, &guard, "GET", "/etc/passwd", &headers).status, 404);
    }

    #[test]
    fn the_page_inserts_conversation_text_only_as_text() {
        assert!(!SCRIPT.contains("innerHTML"));
        assert!(!SCRIPT.contains("insertAdjacentHTML"));
        assert!(!SCRIPT.contains("document.write"));
        assert!(!SCRIPT.contains("eval("));
        assert!(!INDEX.contains("<script>"), "the policy would block an inline script");
    }

    #[test]
    fn a_new_order_alone_is_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let session = |id: &str| list::tests::session(id);
        let state = State::for_tests(dir.path(), vec![session("a"), session("b")]);
        let paths = Paths { claude: dir.path().join("c"), index: dir.path().join("i"), config: dir.path().join("t"), claude_json: dir.path().join("j"), state: dir.path().join("s") };
        state.apply(Ok(vec![session("b"), session("a")]), &paths);
        assert_eq!(read(&state.live).version, 1);
        state.apply(Ok(vec![session("a")]), &paths);
        assert_eq!(read(&state.live).version, 2);
    }

    #[test]
    fn the_first_scan_tells_the_page_it_finished() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::for_tests(dir.path(), Vec::new());
        write(&state.live).loading = true;
        let paths = Paths { claude: dir.path().join("c"), index: dir.path().join("i"), config: dir.path().join("t"), claude_json: dir.path().join("j"), state: dir.path().join("s") };
        state.apply(Ok(Vec::new()), &paths);
        let live = read(&state.live);
        assert_eq!((live.version, live.loading), (2, false));
    }

}
