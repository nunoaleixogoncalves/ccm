//! Keeps the index in step with each tool's files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::index::{Index, SearchDoc};
use crate::model::Session;
use crate::paths::Paths;
use crate::tools::claude::ClaudeCode;
use crate::tools::{Found, Live, Tool};

pub struct Catalog {
    tools: Vec<Box<dyn Tool>>,
    index: Index,
    known: HashMap<String, Session>,
    /// Files a tool read and left out, such as Codex's non-interactive runs, with the fingerprint
    /// they had, so they aren't parsed again on every scan.
    skipped: HashMap<PathBuf, (u64, i64)>,
}

impl Catalog {
    pub fn open(paths: Paths) -> rusqlite::Result<Self> {
        Self::with_proc_root(paths, PathBuf::from("/proc"))
    }

    pub fn with_proc_root(paths: Paths, proc_root: PathBuf) -> rusqlite::Result<Self> {
        let index = paths.index.clone();
        Self::with_tools(&index, vec![Box::new(ClaudeCode::new(paths, proc_root))])
    }

    pub fn with_tools(index: &Path, tools: Vec<Box<dyn Tool>>) -> rusqlite::Result<Self> {
        let index = Index::open(index)?;
        let known = index.load()?.into_iter().map(|s| (s.id.clone(), s)).collect();
        Ok(Self { tools, index, known, skipped: HashMap::new() })
    }

    /// What the index held at the last scan, for an instant first screen.
    pub fn cached(&self) -> Vec<Session> {
        self.annotated()
    }

    /// Stats every conversation file, parses those that changed and returns the full list.
    /// `resize_all` also re-measures unchanged conversations, whose subagent folders can grow
    /// after the transcript's last write.
    pub fn refresh(&mut self, resize_all: bool) -> rusqlite::Result<Vec<Session>> {
        // Conversations of a tool that is no longer scanned leave the list too.
        let found: Vec<(&dyn Tool, Found)> =
            self.tools.iter().flat_map(|t| t.find().into_iter().map(move |f| (t.as_ref(), f))).collect();
        let present: HashSet<&str> = found.iter().map(|(_, f)| f.id.as_str()).collect();
        let remove: Vec<String> =
            self.known.keys().filter(|id| !present.contains(id.as_str())).cloned().collect();

        let changed: Vec<&(&dyn Tool, Found)> = found
            .iter()
            .filter(|(_, f)| self.skipped.get(&f.path) != Some(&(f.size, f.mtime_ns)))
            .filter(|(t, f)| {
                self.known.get(&f.id).is_none_or(|s| {
                    s.source != t.source()
                        || s.transcript_size != f.size
                        || s.transcript_mtime_ns != f.mtime_ns
                        || s.transcript_path != f.path.to_string_lossy()
                })
            })
            .collect();
        let read: Vec<(&Found, Option<(Session, SearchDoc)>)> = changed.par_iter().map(|(t, f)| (f, t.read(f))).collect();
        let mut upsert: Vec<(Session, Option<SearchDoc>)> = Vec::new();
        let mut left_out: Vec<String> = Vec::new();
        for (f, r) in read {
            match r {
                Some((s, doc)) => upsert.push((s, Some(doc))),
                None => {
                    self.skipped.insert(f.path.clone(), (f.size, f.mtime_ns));
                    left_out.push(f.id.clone());
                }
            }
        }
        // A conversation that changed into one its tool leaves out goes from the list too.
        let remove: Vec<String> = remove.into_iter().chain(left_out.into_iter().filter(|id| self.known.contains_key(id))).collect();
        let found_paths: HashSet<&Path> = found.iter().map(|(_, f)| f.path.as_path()).collect();
        self.skipped.retain(|p, _| found_paths.contains(p.as_path()));

        if resize_all {
            let changed_ids: HashSet<&str> = changed.iter().map(|(_, f)| f.id.as_str()).collect();
            let resized: Vec<(Session, Option<SearchDoc>)> = found
                .par_iter()
                .filter(|(_, f)| !changed_ids.contains(f.id.as_str()))
                .filter_map(|(t, f)| {
                    let s = self.known.get(&f.id)?;
                    let size = t.measure(f);
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
        let live: Vec<Live> = self.tools.iter().map(|t| t.live()).collect();
        let mut folders: HashMap<&str, bool> = HashMap::new();
        self.known
            .values()
            .map(|s| {
                let mut s = s.clone();
                s.open = live.iter().any(|l| l.open.contains(&s.id));
                s.crashed = live.iter().any(|l| l.crashed.contains(&s.id));
                s.window_size = live.iter().find_map(|l| l.windows.get(&s.id).copied());
                s.folder_exists = match self.known[&s.id].folder.as_deref() {
                    Some(f) => *folders.entry(f).or_insert_with(|| Path::new(f).is_dir()),
                    None => true,
                };
                s
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Searcher;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
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

    /// A second tool that reads Claude Code transcripts from another folder and says which of
    /// them are running.
    struct Other {
        claude: ClaudeCode,
        open: HashSet<String>,
    }

    impl Tool for Other {
        fn source(&self) -> crate::model::Source {
            self.claude.source()
        }
        fn find(&self) -> Vec<Found> {
            self.claude.find()
        }
        fn read(&self, found: &Found) -> Option<(Session, SearchDoc)> {
            self.claude.read(found)
        }
        fn measure(&self, found: &Found) -> u64 {
            self.claude.measure(found)
        }
        fn live(&self) -> Live {
            Live { open: self.open.clone(), ..Live::default() }
        }
    }

    #[test]
    fn every_tool_is_listed_and_one_no_longer_scanned_leaves_the_list() {
        let t = tree();
        t.write("-p1", A, "one", "/x");
        let other_paths = Paths { claude: t.dir.path().join("other"), ..t.paths.clone() };
        let d = other_paths.projects().join("-p2");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(format!("{B}.jsonl")), "{\"type\":\"user\",\"cwd\":\"/y\",\"message\":{\"content\":\"hi\"}}\n").unwrap();
        let claude = || Box::new(ClaudeCode::new(t.paths.clone(), t.proc.clone())) as Box<dyn Tool>;
        let other = Other { claude: ClaudeCode::new(other_paths, t.proc.clone()), open: HashSet::from([B.to_string()]) };

        let list = Catalog::with_tools(&t.paths.index, vec![claude(), Box::new(other)]).unwrap().refresh(false).unwrap();
        assert_eq!(list.len(), 2);
        assert!(!get(&list, A).open);
        assert!(get(&list, B).open);

        let mut alone = Catalog::with_tools(&t.paths.index, vec![claude()]).unwrap();
        assert_eq!(alone.cached().len(), 2, "the cache still holds both until the next scan");
        let ids: Vec<String> = alone.refresh(false).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, [A]);
        assert!(Searcher::open(&t.paths.index).unwrap().search("hi").unwrap().unwrap().len() == 1);
    }

    struct Counting {
        inner: crate::tools::codex::Codex,
        reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Tool for Counting {
        fn source(&self) -> crate::model::Source {
            self.inner.source()
        }
        fn find(&self) -> Vec<Found> {
            self.inner.find()
        }
        fn read(&self, found: &Found) -> Option<(Session, SearchDoc)> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.read(found)
        }
        fn measure(&self, found: &Found) -> u64 {
            self.inner.measure(found)
        }
        fn live(&self) -> Live {
            self.inner.live()
        }
    }

    #[test]
    fn a_file_its_tool_leaves_out_is_not_read_again_until_it_changes() {
        let t = tree();
        let home = t.dir.path().join("codex");
        let day = home.join("sessions/2026/11/05");
        fs::create_dir_all(&day).unwrap();
        let meta = |id: &str, source: &str| {
            format!("{{\"timestamp\":\"2026-11-05T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/w\",\"source\":\"{source}\"}}}}\n")
        };
        let exec = day.join(format!("rollout-2026-11-05T10-00-00-{B}.jsonl"));
        fs::write(day.join(format!("rollout-2026-11-05T10-00-00-{A}.jsonl")), meta(A, "cli")).unwrap();
        fs::write(&exec, meta(B, "exec")).unwrap();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = Counting { inner: crate::tools::codex::Codex::new(home), reads: reads.clone() };
        let mut c = Catalog::with_tools(&t.paths.index, vec![Box::new(tool)]).unwrap();

        let ids: Vec<String> = c.refresh(false).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, [A]);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
        c.refresh(true).unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2, "nothing changed, so nothing is read");

        fs::write(&exec, meta(B, "exec") + "{}\n").unwrap();
        c.refresh(false).unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
