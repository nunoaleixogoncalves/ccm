//! Keeps the index in step with Claude Code's files.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufReader;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::index::{Index, SearchDoc};
use crate::model::{Session, Source};
use crate::paths::{session_id_of, Paths};
use crate::recovery;
use crate::registry::{live_roster, open_sessions};
use crate::transcript;

pub struct Catalog {
    paths: Paths,
    proc_root: PathBuf,
    index: Index,
    known: HashMap<String, Session>,
}

struct Found {
    id: String,
    path: PathBuf,
    size: u64,
    mtime_ns: i64,
}

impl Catalog {
    pub fn open(paths: Paths) -> rusqlite::Result<Self> {
        Self::with_proc_root(paths, PathBuf::from("/proc"))
    }

    pub fn with_proc_root(paths: Paths, proc_root: PathBuf) -> rusqlite::Result<Self> {
        let index = Index::open(&paths.index)?;
        let known = index.load()?.into_iter().map(|s| (s.id.clone(), s)).collect();
        Ok(Self { paths, proc_root, index, known })
    }

    /// What the index held at the last scan, for an instant first screen.
    pub fn cached(&self) -> Vec<Session> {
        self.annotated()
    }

    /// Stats every transcript, parses those that changed and returns the full list.
    /// `resize_all` also re-measures unchanged conversations, whose subagent folders can grow
    /// after the transcript's last write.
    pub fn refresh(&mut self, resize_all: bool) -> rusqlite::Result<Vec<Session>> {
        let found = find_transcripts(&self.paths.projects());
        let present: HashSet<&str> = found.iter().map(|f| f.id.as_str()).collect();
        let remove: Vec<String> =
            self.known.keys().filter(|id| !present.contains(id.as_str())).cloned().collect();

        let changed: Vec<&Found> = found
            .iter()
            .filter(|f| {
                self.known.get(&f.id).is_none_or(|s| {
                    s.transcript_size != f.size
                        || s.transcript_mtime_ns != f.mtime_ns
                        || s.transcript_path != f.path.to_string_lossy()
                })
            })
            .collect();
        let mut upsert: Vec<(Session, Option<SearchDoc>)> = changed
            .par_iter()
            .filter_map(|f| read_session(f))
            .map(|(s, doc)| (s, Some(doc)))
            .collect();

        if resize_all {
            let changed_ids: HashSet<&str> = changed.iter().map(|f| f.id.as_str()).collect();
            let resized: Vec<(Session, Option<SearchDoc>)> = found
                .par_iter()
                .filter(|f| !changed_ids.contains(f.id.as_str()))
                .filter_map(|f| {
                    let s = self.known.get(&f.id)?;
                    let size = f.size + dir_size(&companion_dir(&f.path));
                    (size != s.size_bytes).then(|| (Session { size_bytes: size, ..s.clone() }, None))
                })
                .collect();
            upsert.extend(resized);
        }

        self.index.apply(&upsert, &remove)?;
        for id in &remove {
            self.known.remove(id);
        }
        for (s, _) in upsert {
            self.known.insert(s.id.clone(), s);
        }
        Ok(self.annotated())
    }

    fn annotated(&self) -> Vec<Session> {
        let open = open_sessions(&self.paths.sessions(), &self.proc_root);
        let roster = live_roster(&self.paths.claude.join("daemon/roster.json"), &self.proc_root);
        let crashed = recovery::crashed(&self.paths, &open, &roster, chrono::Utc::now().timestamp_millis());
        let windows = recovery::saved_windows(&self.paths);
        let mut folders: HashMap<&str, bool> = HashMap::new();
        self.known
            .values()
            .map(|s| {
                let mut s = s.clone();
                s.open = open.contains(&s.id);
                s.crashed = crashed.contains(&s.id);
                s.window_size = windows.get(&s.id).copied();
                s.folder_exists = match self.known[&s.id].folder.as_deref() {
                    Some(f) => *folders.entry(f).or_insert_with(|| Path::new(f).is_dir()),
                    None => true,
                };
                s
            })
            .collect()
    }
}

/// When the same ID is in two project folders, the newest transcript wins.
fn find_transcripts(projects: &Path) -> Vec<Found> {
    let mut by_id: HashMap<String, Found> = HashMap::new();
    let Ok(dirs) = fs::read_dir(projects) else {
        return Vec::new();
    };
    for dir in dirs.flatten() {
        let Ok(files) = fs::read_dir(dir.path()) else { continue };
        for file in files.flatten() {
            let path = file.path();
            let Some(id) = session_id_of(&path).map(str::to_owned) else { continue };
            let Ok(meta) = file.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let found = Found {
                id: id.clone(),
                path,
                size: meta.len(),
                mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            };
            match by_id.get(&id) {
                Some(existing) if existing.mtime_ns >= found.mtime_ns => {}
                _ => {
                    by_id.insert(id, found);
                }
            }
        }
    }
    by_id.into_values().collect()
}

fn read_session(f: &Found) -> Option<(Session, SearchDoc)> {
    let file = fs::File::open(&f.path).ok()?;
    let p = transcript::parse(BufReader::with_capacity(1 << 20, file)).ok()?;
    let doc = SearchDoc {
        title: p.name.clone(),
        path: p.folder.clone().unwrap_or_default(),
        branch: p.branch.clone().unwrap_or_default(),
        prompts: p.prompt_text,
        replies: p.reply_text,
    };
    let session = Session {
        id: f.id.clone(),
        source: Source::ClaudeCode,
        transcript_path: f.path.to_string_lossy().into_owned(),
        transcript_size: f.size,
        transcript_mtime_ns: f.mtime_ns,
        size_bytes: f.size + dir_size(&companion_dir(&f.path)),
        name: p.name,
        folder: p.folder,
        branch: p.branch,
        model: p.model,
        started_at_ms: p.started_at_ms,
        last_active_ms: p.last_active_ms.or(Some(f.mtime_ns / 1_000_000)),
        context_used: p.context_used,
        prompts: p.prompts,
        replies: p.replies,
        cost_usd: p.cost_usd,
        lines_added: p.lines_added,
        lines_removed: p.lines_removed,
        pr_url: p.pr_url,
        continued_in: p.continued_in,
        recaps: p.recaps,
        bad_lines: p.bad_lines,
        open: false,
        crashed: false,
        window_size: None,
        folder_exists: true,
    };
    Some((session, doc))
}

/// `<id>.jsonl` keeps its subagents and tool results in `<id>/`.
fn companion_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("")
}

/// Total size of regular files, never following symlinks.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Searcher;
    use std::time::{Duration, SystemTime};

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    struct Tree {
        dir: tempfile::TempDir,
        paths: Paths,
        proc: PathBuf,
    }

    fn tree() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            claude: root.join("claude"),
            index: root.join("cache/ccm/index.db"),
            config: root.join("config.toml"),
            claude_json: root.join("claude.json"),
            state: root.join("state/ccm"),
        };
        fs::create_dir_all(paths.projects()).unwrap();
        fs::create_dir_all(paths.sessions()).unwrap();
        let proc = root.join("proc");
        fs::create_dir_all(&proc).unwrap();
        Tree { dir, paths, proc }
    }

    impl Tree {
        fn catalog(&self) -> Catalog {
            Catalog::with_proc_root(self.paths.clone(), self.proc.clone()).unwrap()
        }

        fn write(&self, project: &str, id: &str, title: &str, folder: &str) -> PathBuf {
            let d = self.paths.projects().join(project);
            fs::create_dir_all(&d).unwrap();
            let p = d.join(format!("{id}.jsonl"));
            let body = format!(
                "{{\"type\":\"custom-title\",\"customTitle\":\"{title}\"}}\n{{\"type\":\"user\",\"cwd\":\"{folder}\",\"message\":{{\"content\":\"hi\"}}}}\n"
            );
            fs::write(&p, body).unwrap();
            p
        }
    }

    fn get<'a>(list: &'a [Session], id: &str) -> &'a Session {
        list.iter().find(|s| s.id == id).unwrap()
    }

    #[test]
    fn finds_conversations_in_all_project_folders_and_skips_others() {
        let t = tree();
        t.write("-p1", A, "one", "/x");
        t.write("-p2", B, "two", "/y");
        fs::write(t.paths.projects().join("-p1/agent-123.jsonl"), "{}").unwrap();
        fs::write(t.paths.projects().join("-p1/notes.txt"), "x").unwrap();
        let list = t.catalog().refresh(true).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(get(&list, A).name, "one");
        assert_eq!(get(&list, B).folder.as_deref(), Some("/y"));
    }

    #[test]
    fn size_includes_subagents_and_tool_results() {
        let t = tree();
        let p = t.write("-p1", A, "one", "/x");
        let sub = t.paths.projects().join(format!("-p1/{A}/subagents"));
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent-1.jsonl"), vec![0u8; 5000]).unwrap();
        let list = t.catalog().refresh(true).unwrap();
        assert_eq!(get(&list, A).size_bytes, fs::metadata(&p).unwrap().len() + 5000);
    }

    #[test]
    fn subagent_growth_after_the_last_write_is_found_by_a_full_resize() {
        let t = tree();
        t.write("-p1", A, "one", "/x");
        let mut c = t.catalog();
        let before = get(&c.refresh(true).unwrap(), A).size_bytes;
        let sub = t.paths.projects().join(format!("-p1/{A}"));
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("late"), vec![0u8; 100]).unwrap();
        assert_eq!(get(&c.refresh(false).unwrap(), A).size_bytes, before);
        assert_eq!(get(&c.refresh(true).unwrap(), A).size_bytes, before + 100);
    }

    #[test]
    fn changes_are_picked_up_and_removed_files_disappear() {
        let t = tree();
        let p = t.write("-p1", A, "one", "/x");
        t.write("-p1", B, "two", "/y");
        let mut c = t.catalog();
        c.refresh(false).unwrap();
        let mut body = fs::read_to_string(&p).unwrap();
        body.push_str("{\"type\":\"custom-title\",\"customTitle\":\"renamed\"}\n");
        fs::write(&p, body).unwrap();
        fs::remove_file(t.paths.projects().join(format!("-p1/{B}.jsonl"))).unwrap();
        let list = c.refresh(false).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(get(&list, A).name, "renamed");
    }

    #[test]
    fn same_size_edit_is_detected_by_mtime() {
        let t = tree();
        let p = t.write("-p1", A, "aaa", "/x");
        let mut c = t.catalog();
        c.refresh(false).unwrap();
        t.write("-p1", A, "bbb", "/x");
        let f = fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(get(&c.refresh(false).unwrap(), A).name, "bbb");
    }

    #[test]
    fn the_cache_survives_a_restart_and_unchanged_files_are_not_parsed_again() {
        let t = tree();
        let p = t.write("-p1", A, "one", "/x");
        t.catalog().refresh(false).unwrap();
        let mut c = t.catalog();
        assert_eq!(get(&c.cached(), A).name, "one");
        // Unreadable content with the same fingerprint proves the cached row is reused.
        let meta = fs::metadata(&p).unwrap();
        let modified = meta.modified().unwrap();
        fs::write(&p, "x".repeat(meta.len() as usize)).unwrap();
        fs::File::options().write(true).open(&p).unwrap().set_modified(modified).unwrap();
        assert_eq!(get(&c.refresh(false).unwrap(), A).name, "one");
    }

    #[test]
    fn a_corrupt_index_is_rebuilt() {
        let t = tree();
        t.write("-p1", A, "one", "/x");
        fs::create_dir_all(t.paths.index.parent().unwrap()).unwrap();
        fs::write(&t.paths.index, "this is not a database, not even close").unwrap();
        let list = t.catalog().refresh(false).unwrap();
        assert_eq!(get(&list, A).name, "one");
    }

    #[test]
    fn the_index_is_private() {
        let t = tree();
        t.write("-p1", A, "one", "/x");
        t.catalog().refresh(false).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().mode() & 0o777;
        assert_eq!(mode(&t.paths.index), 0o600);
        assert_eq!(mode(t.paths.index.parent().unwrap()), 0o700);
        let wal = t.paths.index.with_extension("db-wal");
        if wal.exists() {
            assert_eq!(mode(&wal), 0o600);
        }
    }

    #[test]
    fn open_and_missing_folder_state_follow_the_machine() {
        let t = tree();
        let real = t.dir.path().join("real");
        fs::create_dir(&real).unwrap();
        t.write("-p1", A, "one", real.to_str().unwrap());
        t.write("-p1", B, "two", "/definitely/not/here");
        fs::write(
            t.paths.sessions().join("42.json"),
            format!(r#"{{"pid":42,"sessionId":"{A}","procStart":"500"}}"#),
        )
        .unwrap();
        let pd = t.proc.join("42");
        fs::create_dir(&pd).unwrap();
        let stat = format!("42 (claude) S {} 500 1\n", vec!["0"; 18].join(" "));
        fs::write(pd.join("stat"), stat).unwrap();

        let mut c = t.catalog();
        let list = c.refresh(false).unwrap();
        assert!(get(&list, A).open);
        assert!(!get(&list, A).is_folder_missing());
        assert!(!get(&list, B).open);
        assert!(get(&list, B).is_folder_missing());

        fs::remove_dir_all(&pd).unwrap();
        assert!(!get(&c.refresh(false).unwrap(), A).open, "a killed process stops being open");
    }

    #[test]
    fn symlinks_are_not_followed_when_measuring() {
        let t = tree();
        let p = t.write("-p1", A, "one", "/x");
        let d = t.paths.projects().join(format!("-p1/{A}"));
        fs::create_dir_all(&d).unwrap();
        let big = t.dir.path().join("big");
        fs::write(&big, vec![0u8; 9000]).unwrap();
        std::os::unix::fs::symlink(&big, d.join("link")).unwrap();
        let list = t.catalog().refresh(true).unwrap();
        assert_eq!(get(&list, A).size_bytes, fs::metadata(&p).unwrap().len());
    }

    #[test]
    fn search_follows_changes_and_deletes() {
        let t = tree();
        let p = t.write("-p1", A, "alpha", "/x");
        t.write("-p2", B, "beta", "/y");
        let mut c = t.catalog();
        c.refresh(false).unwrap();
        let s = Searcher::open(&t.paths.index).unwrap();
        assert_eq!(s.search("alpha").unwrap(), Some(HashSet::from([A.to_string()])));

        let mut body = fs::read_to_string(&p).unwrap();
        body.push_str("{\"type\":\"custom-title\",\"customTitle\":\"gamma\"}\n");
        fs::write(&p, body).unwrap();
        c.refresh(false).unwrap();
        assert_eq!(s.search("alpha").unwrap(), Some(HashSet::new()), "the old title is gone");
        assert_eq!(s.search("gamma").unwrap(), Some(HashSet::from([A.to_string()])));

        fs::remove_file(t.paths.projects().join(format!("-p2/{B}.jsonl"))).unwrap();
        c.refresh(false).unwrap();
        assert_eq!(s.search("beta").unwrap(), Some(HashSet::new()), "a removed conversation leaves the index");
    }

    #[test]
    fn a_size_only_update_keeps_the_search_entry() {
        let t = tree();
        t.write("-p1", A, "alpha", "/x");
        let mut c = t.catalog();
        c.refresh(true).unwrap();
        let d = t.paths.projects().join(format!("-p1/{A}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("late"), "x").unwrap();
        c.refresh(true).unwrap();
        let s = Searcher::open(&t.paths.index).unwrap();
        assert_eq!(s.search("alpha").unwrap(), Some(HashSet::from([A.to_string()])));
    }

    #[test]
    fn no_projects_folder_is_an_empty_list() {
        let t = tree();
        fs::remove_dir_all(t.paths.projects()).unwrap();
        assert!(t.catalog().refresh(true).unwrap().is_empty());
    }
}
