//! Pins and tags. They're `ccm`'s own, so they live in its state folder rather than in any tool's
//! files: one small JSON file keyed by conversation ID. Each change reads the file again under a
//! lock, so the terminal app and `ccm web` can both change it at once.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::delete::replace_file;
use crate::format::sanitize;
use crate::model::Session;
use crate::paths::{is_uuid, Paths};

/// Longer than this isn't a tag, and wouldn't fit beside a name.
pub const MAX_TAG: usize = 32;
/// More than this on one conversation stops being a way to find it.
pub const MAX_TAGS: usize = 20;
/// Said when a pinned conversation is asked to be deleted.
pub const PINNED: &str = "it's pinned, so unpin it first";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Labels {
    #[serde(default)]
    pub pinned: BTreeSet<String>,
    /// Each conversation's tags, in the order they were added.
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
}

/// One change, made to every conversation it's asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Pin(bool),
    /// Tags other than these are left as they are.
    Tags { add: Vec<String>, remove: Vec<String> },
    /// Drops everything about them, once they're deleted.
    Forget,
}

impl Labels {
    /// Copies each conversation's pin and tags onto it.
    pub fn apply(&self, sessions: &mut [Session]) {
        for s in sessions {
            s.pinned = self.pinned.contains(&s.id);
            s.tags = self.tags.get(&s.id).cloned().unwrap_or_default();
        }
    }

    /// Makes a change here only. The `change` function also saves it.
    pub fn change(&mut self, ids: &[String], change: &Change) -> Result<(), String> {
        for id in ids {
            match change {
                Change::Pin(true) => {
                    self.pinned.insert(id.clone());
                }
                Change::Pin(false) => {
                    self.pinned.remove(id);
                }
                Change::Tags { add, remove } => {
                    let tags = self.tags.entry(id.clone()).or_default();
                    tags.retain(|t| !remove.contains(t));
                    for tag in add {
                        if !tags.contains(tag) {
                            tags.push(tag.clone());
                        }
                    }
                    if tags.len() > MAX_TAGS {
                        return Err(format!("A conversation can have at most {MAX_TAGS} tags."));
                    }
                    if tags.is_empty() {
                        self.tags.remove(id);
                    }
                }
                Change::Forget => {
                    self.pinned.remove(id);
                    self.tags.remove(id);
                }
            }
        }
        Ok(())
    }
}

fn file(paths: &Paths) -> PathBuf {
    paths.state.join("labels.json")
}

/// What's saved. No file means nothing is pinned or tagged. A file that can't be read is an
/// error, so it's never shown or saved over as if it were empty.
pub fn load(paths: &Paths) -> Result<Labels, String> {
    let path = file(paths);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("Pins and tags can't be read from {}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Labels::default()),
        Err(e) => Err(format!("Pins and tags can't be read from {}: {e}", path.display())),
    }
}

/// Makes a change and saves it, returning everything as saved.
pub fn change(paths: &Paths, ids: &[String], change: &Change) -> Result<Labels, String> {
    if ids.iter().any(|id| !is_uuid(id)) {
        return Err("That isn't a conversation ID.".into());
    }
    let failed = |e: io::Error| format!("Couldn't save pins and tags: {e}");
    DirBuilder::new().recursive(true).mode(0o700).create(&paths.state).map_err(failed)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(paths.state.join("labels.lock")).map_err(failed)?;
    lock.lock().map_err(failed)?;
    let mut labels = load(paths)?;
    labels.change(ids, change)?;
    let mut text = serde_json::to_string_pretty(&labels).map_err(|e| failed(e.into()))?;
    text.push('\n');
    replace_file(&file(paths), text.as_bytes(), || true).map_err(failed)?;
    Ok(labels)
}

/// Tags as typed: split by spaces or commas, each with or without a leading `#`. Kept in
/// lowercase and in the order typed, without repeats.
pub fn parse(text: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()) {
        let tag = word.strip_prefix('#').unwrap_or(word).to_lowercase();
        if tag.is_empty() {
            continue;
        }
        if !tag.chars().all(|c| c.is_alphanumeric() || "-_./".contains(c)) {
            return Err(format!("{} can't be a tag. Use letters, digits and - _ . /", sanitize(word)));
        }
        if tag.chars().count() > MAX_TAG {
            return Err(format!("#{tag} is too long for a tag. The most is {MAX_TAG} characters."));
        }
        if !out.contains(&tag) {
            out.push(tag);
        }
    }
    if out.len() > MAX_TAGS {
        return Err(format!("A conversation can have at most {MAX_TAGS} tags."));
    }
    Ok(out)
}

/// Splits the `#tag` words out of a search. The rest is searched for as text.
pub fn split_query(query: &str) -> (Vec<String>, String) {
    if !query.contains('#') {
        return (Vec::new(), query.to_string());
    }
    let mut tags = Vec::new();
    let mut rest = Vec::new();
    for word in query.split_whitespace() {
        match word.strip_prefix('#') {
            Some(tag) if !tag.is_empty() => tags.push(tag.to_lowercase()),
            Some(_) => {}
            None => rest.push(word),
        }
    }
    (tags, rest.join(" "))
}

/// True when it has every one of these tags.
pub fn has_tags(s: &Session, tags: &[String]) -> bool {
    tags.iter().all(|t| s.tags.contains(t))
}

/// The tags all of these share, in the first one's order. Editing several at once starts here.
pub fn common(sessions: &[&Session]) -> Vec<String> {
    let Some((first, rest)) = sessions.split_first() else { return Vec::new() };
    first.tags.iter().filter(|t| rest.iter().all(|s| s.tags.contains(t))).cloned().collect()
}

/// What to add and remove so that tags `before`, shared by all, become `after`. Tags only some
/// of them have aren't touched.
pub fn edit(before: &[String], after: &[String]) -> Change {
    Change::Tags {
        add: after.iter().filter(|t| !before.contains(t)).cloned().collect(),
        remove: before.iter().filter(|t| !after.contains(t)).cloned().collect(),
    }
}

/// The tags as shown, each after a `#`.
pub fn shown(tags: &[String]) -> String {
    tags.iter().map(|t| format!("#{}", sanitize(t))).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    fn paths(root: &std::path::Path) -> Paths {
        Paths {
            claude: root.join("claude"),
            index: root.join("index.db"),
            config: root.join("config.toml"),
            claude_json: root.join("claude.json"),
            state: root.join("state/ccm"),
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn tags(list: &[&str]) -> Vec<String> {
        ids(list)
    }

    #[test]
    fn pins_and_tags_are_saved_in_an_owner_only_file_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        assert_eq!(load(&p).unwrap(), Labels::default(), "no file means nothing is pinned");
        change(&p, &ids(&[A, B]), &Change::Pin(true)).unwrap();
        change(&p, &ids(&[A]), &Change::Tags { add: tags(&["work", "bug"]), remove: vec![] }).unwrap();
        let saved = change(&p, &ids(&[B]), &Change::Pin(false)).unwrap();
        assert_eq!(saved, load(&p).unwrap());
        assert_eq!(saved.pinned, BTreeSet::from([A.to_string()]));
        assert_eq!(saved.tags[A], ["work", "bug"]);
        let mode = fs::metadata(file(&p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mode = fs::metadata(&p.state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn tags_are_added_and_removed_without_touching_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        change(&p, &ids(&[A]), &Change::Tags { add: tags(&["a", "only-a"]), remove: vec![] }).unwrap();
        change(&p, &ids(&[B]), &Change::Tags { add: tags(&["a", "only-b"]), remove: vec![] }).unwrap();
        let saved = change(&p, &ids(&[A, B]), &Change::Tags { add: tags(&["new", "only-b"]), remove: tags(&["a"]) }).unwrap();
        assert_eq!(saved.tags[A], ["only-a", "new", "only-b"]);
        assert_eq!(saved.tags[B], ["only-b", "new"], "a tag it had already isn't added twice");
        let saved = change(&p, &ids(&[A, B]), &Change::Tags { add: vec![], remove: tags(&["only-a", "only-b", "new"]) }).unwrap();
        assert!(saved.tags.is_empty(), "no tags left leaves no entry");
    }

    #[test]
    fn forgetting_drops_the_pin_and_the_tags() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        change(&p, &ids(&[A, B]), &Change::Pin(true)).unwrap();
        change(&p, &ids(&[A]), &Change::Tags { add: tags(&["x"]), remove: vec![] }).unwrap();
        let saved = change(&p, &ids(&[A]), &Change::Forget).unwrap();
        assert_eq!(saved.pinned, BTreeSet::from([B.to_string()]));
        assert!(saved.tags.is_empty());
    }

    #[test]
    fn a_file_that_cant_be_read_is_never_saved_over() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        fs::create_dir_all(&p.state).unwrap();
        fs::write(file(&p), "not json").unwrap();
        assert!(load(&p).is_err());
        assert!(change(&p, &ids(&[A]), &Change::Pin(true)).is_err());
        assert_eq!(fs::read_to_string(file(&p)).unwrap(), "not json");
    }

    #[test]
    fn only_conversation_ids_are_saved_and_too_many_tags_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        assert!(change(&p, &ids(&["../x"]), &Change::Pin(true)).is_err());
        assert!(!file(&p).exists());
        let many: Vec<String> = (0..=MAX_TAGS).map(|i| format!("t{i}")).collect();
        assert!(change(&p, &ids(&[A]), &Change::Tags { add: many, remove: vec![] }).is_err());
        assert!(!file(&p).exists(), "nothing is saved when a change is refused");
    }

    #[test]
    fn typed_tags_are_tidied_and_bad_ones_refused() {
        assert_eq!(parse(" #Work, bug-12  work api/v2 ").unwrap(), ["work", "bug-12", "api/v2"]);
        assert_eq!(parse("  ").unwrap(), Vec::<String>::new());
        assert_eq!(parse("# ,").unwrap(), Vec::<String>::new());
        assert!(parse("no!").is_err());
        assert!(parse("a\u{202e}b").is_err(), "control characters aren't tags");
        assert!(parse(&"x".repeat(MAX_TAG + 1)).is_err());
        assert!(parse(&"x".repeat(MAX_TAG)).is_ok());
    }

    #[test]
    fn a_search_gives_up_its_tags() {
        assert_eq!(split_query("login bug"), (vec![], "login bug".to_string()));
        assert_eq!(split_query("#Work login #bug"), (tags(&["work", "bug"]), "login".to_string()));
        assert_eq!(split_query("# login"), (vec![], "login".to_string()), "a lone # is ignored");
    }

    #[test]
    fn editing_several_starts_from_the_tags_they_share() {
        let session = |tags: &[&str]| {
            let mut s = crate::model::test_session("x");
            s.tags = ids(tags);
            s
        };
        let (x, y) = (session(&["a", "b", "c"]), session(&["c", "a"]));
        assert_eq!(common(&[&x, &y]), ["a", "c"]);
        assert_eq!(common(&[]), Vec::<String>::new());
        assert_eq!(edit(&tags(&["a", "c"]), &tags(&["c", "d"])), Change::Tags { add: tags(&["d"]), remove: tags(&["a"]) });
        assert!(has_tags(&x, &tags(&["a", "b"])) && !has_tags(&y, &tags(&["a", "b"])));
        assert_eq!(shown(&tags(&["a", "b"])), "#a #b");
    }
}
