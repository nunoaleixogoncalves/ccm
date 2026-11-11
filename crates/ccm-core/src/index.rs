//! The on-disk cache. Claude Code's files are the source of truth, so anything wrong with the
//! index is fixed by deleting it and scanning again.

use std::collections::{HashMap, HashSet};
use std::fs::{self, DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};

use crate::model::Session;
use crate::repo::Place;

/// Bump when the stored data or the parser changes, so old rows are rebuilt.
const SCHEMA_VERSION: i32 = 4;

/// Contentless, because the text already lives in the transcripts. This also keeps the index
/// small and limits how much conversation text it copies.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    data TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
    title, path, branch, prompts, replies,
    content = '', contentless_delete = 1,
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TABLE IF NOT EXISTS repos (
    folder TEXT PRIMARY KEY,
    repo TEXT,
    worktree TEXT
);
"#;

/// The searchable text of one conversation. Subagent content is never included.
#[derive(Debug, Clone, Default)]
pub struct SearchDoc {
    pub title: String,
    pub path: String,
    pub branch: String,
    pub prompts: String,
    pub replies: String,
}

pub struct Index {
    conn: Connection,
}

impl Index {
    /// Opens the index with owner-only permissions, rebuilding it if it's unusable.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        match Self::try_open(path) {
            Ok(index) => Ok(index),
            Err(_) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut p = path.as_os_str().to_owned();
                    p.push(suffix);
                    let _ = fs::remove_file(p);
                }
                Self::try_open(path)
            }
        }
    }

    fn try_open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = DirBuilder::new().recursive(true).mode(0o700).create(parent);
        }
        // SQLite gives its -wal and -shm files the same mode as the main file.
        let _ = OpenOptions::new().create(true).append(true).mode(0o600).open(path);
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // A scan can remove a deleted conversation before `forget` does, so every removal needs it.
        conn.pragma_update(None, "secure_delete", "ON")?;
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            conn.execute_batch("DROP TABLE IF EXISTS search; DROP TABLE IF EXISTS sessions;")?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        conn.execute_batch(SCHEMA)?;
        conn.query_row("SELECT count(*) FROM sessions", [], |_| Ok(()))?;
        Ok(Self { conn })
    }

    /// Rows that no longer parse are skipped; the next scan writes them again.
    pub fn load(&self) -> rusqlite::Result<Vec<Session>> {
        let mut stmt = self.conn.prepare("SELECT data FROM sessions")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.flatten().filter_map(|d| serde_json::from_str(&d).ok()).collect())
    }

    /// A `None` document leaves the existing search entry as it is.
    pub fn apply(&mut self, upsert: &[(Session, Option<SearchDoc>)], remove: &[String]) -> rusqlite::Result<()> {
        if upsert.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut put = tx.prepare(
                "INSERT INTO sessions (id, data) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET data = excluded.data RETURNING rowid",
            )?;
            let mut unindex = tx.prepare("DELETE FROM search WHERE rowid = ?1")?;
            let mut index = tx.prepare(
                "INSERT INTO search (rowid, title, path, branch, prompts, replies) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (s, doc) in upsert {
                let data = serde_json::to_string(s).expect("a session always serializes");
                let rowid: i64 = put.query_row(params![s.id, data], |r| r.get(0))?;
                if let Some(d) = doc {
                    unindex.execute(params![rowid])?;
                    index.execute(params![rowid, d.title, d.path, d.branch, d.prompts, d.replies])?;
                }
            }
            let mut unindex_id =
                tx.prepare("DELETE FROM search WHERE rowid = (SELECT rowid FROM sessions WHERE id = ?1)")?;
            let mut del = tx.prepare("DELETE FROM sessions WHERE id = ?1")?;
            for id in remove {
                unindex_id.execute(params![id])?;
                del.execute(params![id])?;
            }
        }
        tx.commit()
    }

    /// Removes deleted conversations so their words can't be recovered from the index file. A
    /// plain removal only marks them as gone, so this also rewrites the search data and the log.
    pub fn forget(&mut self, ids: &[String]) -> rusqlite::Result<()> {
        self.apply(&[], ids)?;
        self.conn.execute_batch("INSERT INTO search (search) VALUES ('optimize');")?;
        self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
    }
}

impl Index {
    /// What Git said about each folder before. Folders that were gone aren't kept, as there was
    /// nothing to learn from them.
    pub fn repos(&self) -> rusqlite::Result<HashMap<String, Place>> {
        let mut stmt = self.conn.prepare("SELECT folder, repo, worktree FROM repos")?;
        let rows = stmt.query_map([], |r| {
            let repo: Option<String> = r.get(1)?;
            let place = match repo {
                Some(repo) => Place::Repo { repo, worktree: r.get(2)? },
                None => Place::NotGit,
            };
            Ok((r.get::<_, String>(0)?, place))
        })?;
        rows.collect()
    }

    /// Saves new answers, and drops those for folders no conversation uses any more.
    pub fn save_repos(&mut self, answers: &HashMap<String, Place>, used: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut put = tx.prepare(
                "INSERT INTO repos (folder, repo, worktree) VALUES (?1, ?2, ?3)
                 ON CONFLICT(folder) DO UPDATE SET repo = excluded.repo, worktree = excluded.worktree",
            )?;
            for (folder, place) in answers {
                match place {
                    Place::Repo { repo, worktree } => put.execute(params![folder, repo, worktree])?,
                    Place::NotGit => put.execute(params![folder, None::<String>, None::<String>])?,
                    Place::Gone => 0,
                };
            }
            let used: HashSet<&str> = used.iter().map(String::as_str).collect();
            let stored: Vec<String> =
                tx.prepare("SELECT folder FROM repos")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            let mut del = tx.prepare("DELETE FROM repos WHERE folder = ?1")?;
            for folder in stored.iter().filter(|f| !used.contains(f.as_str())) {
                del.execute(params![folder])?;
            }
        }
        tx.commit()
    }
}

/// Answers searches on its own read-only connection, so typing never waits for a scan.
pub struct Searcher {
    conn: Connection,
}

impl Searcher {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        Ok(Self { conn: Connection::open_with_flags(path, flags)? })
    }

    /// IDs matching every word of the query, or `None` when the query has no words to search.
    pub fn search(&self, query: &str) -> rusqlite::Result<Option<HashSet<String>>> {
        let Some(expr) = match_expression(query) else { return Ok(None) };
        let mut stmt = self.conn.prepare_cached(
            "SELECT s.id FROM search JOIN sessions s ON s.rowid = search.rowid WHERE search MATCH ?1",
        )?;
        let rows = stmt.query_map(params![expr], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<HashSet<_>, _>>().map(Some)
    }
}

/// Every word becomes a quoted prefix term, so results appear while a word is still being typed
/// and nothing typed can be read as FTS5 syntax.
fn match_expression(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;

    fn session(id: &str) -> Session {
        Session {
            id: id.into(),
            source: Source::ClaudeCode,
            transcript_path: String::new(),
            transcript_size: 0,
            transcript_mtime_ns: 0,
            size_bytes: 0,
            name: String::new(),
            folder: None,
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
            pinned: false,
            tags: Vec::new(),
        }
    }

    fn setup() -> (tempfile::TempDir, Searcher) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut index = Index::open(&path).unwrap();
        let doc = |title: &str, path: &str, branch: &str, prompts: &str, replies: &str| {
            Some(SearchDoc {
                title: title.into(),
                path: path.into(),
                branch: branch.into(),
                prompts: prompts.into(),
                replies: replies.into(),
            })
        };
        index
            .apply(
                &[
                    (session("a"), doc("Payment gateway", "/work/cc_manager", "fix/KOI-1547", "orçamento do projeto", "")),
                    (session("b"), doc("Other", "/work/rentbook", "main", "", "the payment failed")),
                ],
                &[],
            )
            .unwrap();
        let s = Searcher::open(&path).unwrap();
        (dir, s)
    }

    fn ids(s: &Searcher, q: &str) -> Vec<String> {
        let mut v: Vec<String> = s.search(q).unwrap().unwrap().into_iter().collect();
        v.sort();
        v
    }

    #[test]
    fn every_field_is_searchable() {
        let (_d, s) = setup();
        assert_eq!(ids(&s, "gateway"), ["a"]);
        assert_eq!(ids(&s, "rentbook"), ["b"]);
        assert_eq!(ids(&s, "KOI-1547"), ["a"]);
        assert_eq!(ids(&s, "orçamento"), ["a"]);
        assert_eq!(ids(&s, "failed"), ["b"]);
        assert_eq!(ids(&s, "payment"), ["a", "b"]);
    }

    #[test]
    fn matching_ignores_case_and_accents_and_accepts_partial_words() {
        let (_d, s) = setup();
        assert_eq!(ids(&s, "ORCAMENTO"), ["a"]);
        assert_eq!(ids(&s, "orca"), ["a"]);
        assert_eq!(ids(&s, "cc_manager"), ["a"]);
    }

    #[test]
    fn all_words_must_match() {
        let (_d, s) = setup();
        assert_eq!(ids(&s, "payment failed"), ["b"]);
        assert!(ids(&s, "gateway failed").is_empty());
    }

    #[test]
    fn anything_typed_is_safe() {
        let (_d, s) = setup();
        for q in ["\"", "a\"b", "title:x", "NOT", "a OR", "(", "*", "-x", "a AND", "{x}", "^", "日本", "x'y;--"] {
            assert!(s.search(q).is_ok(), "{q}");
        }
    }

    #[test]
    fn a_query_with_no_words_means_no_search() {
        let (_d, s) = setup();
        assert_eq!(s.search("").unwrap(), None);
        assert_eq!(s.search("   ").unwrap(), None);
        assert_eq!(s.search("-- *").unwrap(), None);
    }

    #[test]
    fn repository_answers_are_kept_until_no_conversation_uses_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut index = Index::open(&path).unwrap();
        let shop = Place::Repo { repo: "/w/shop".into(), worktree: Some("/w/shop-login".into()) };
        let answers = HashMap::from([
            ("/w/shop-login/src".to_string(), shop.clone()),
            ("/w/notes".to_string(), Place::NotGit),
            ("/w/gone".to_string(), Place::Gone),
        ]);
        let used = ["/w/shop-login/src".to_string(), "/w/notes".to_string(), "/w/gone".to_string()];
        index.save_repos(&answers, &used).unwrap();
        drop(index);
        let mut index = Index::open(&path).unwrap();
        let saved = index.repos().unwrap();
        assert_eq!(saved.len(), 2, "a gone folder taught nothing: {saved:?}");
        assert_eq!(saved["/w/shop-login/src"], shop);
        assert_eq!(saved["/w/notes"], Place::NotGit);

        index.save_repos(&HashMap::new(), &used[..1]).unwrap();
        assert_eq!(index.repos().unwrap().into_keys().collect::<Vec<_>>(), ["/w/shop-login/src"]);
    }

    #[test]
    fn an_older_index_is_rebuilt_from_scratch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, data TEXT NOT NULL); INSERT INTO sessions VALUES ('old', '{}'); PRAGMA user_version = 1;").unwrap();
        drop(c);
        let index = Index::open(&path).unwrap();
        assert!(index.load().unwrap().is_empty());
    }

    #[test]
    fn forgetting_a_conversation_leaves_none_of_its_words_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut index = Index::open(&path).unwrap();
        let doc = |words: &str| Some(SearchDoc { title: words.into(), prompts: words.into(), ..SearchDoc::default() });
        let mut secret = session("gone");
        secret.name = "zebrafishname".into();
        index
            .apply(&[(secret, doc("zebrafishword")), (session("kept"), doc("otterword"))], &[])
            .unwrap();

        index.forget(&["gone".to_string()]).unwrap();
        drop(index);

        let searcher = Searcher::open(&path).unwrap();
        assert!(searcher.search("zebrafishword").unwrap().unwrap().is_empty());
        assert_eq!(searcher.search("otterword").unwrap().unwrap().len(), 1);
        for entry in fs::read_dir(dir.path()).unwrap().flatten() {
            let bytes = fs::read(entry.path()).unwrap();
            assert!(!bytes.windows(9).any(|w| w == b"zebrafish"), "found in {:?}", entry.path());
        }
    }
}
