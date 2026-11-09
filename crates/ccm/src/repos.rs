//! Which repository each folder belongs to, for grouping. Git runs elsewhere, so this keeps what
//! has been asked and answered.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use ccm_core::index::Index;
use ccm_core::model::Session;
use ccm_core::repo::{self, Place};

/// Folders to ask Git about. A full request also asks again about folders that weren't in Git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRequest {
    pub folders: Vec<String>,
    /// Every folder a conversation started in, so answers for others can be dropped.
    pub used: Vec<String>,
    pub full: bool,
}

#[derive(Default)]
pub struct Repos {
    /// What Git said about each folder conversations started in.
    pub places: HashMap<String, Place>,
    /// Set when repository grouping is picked, so every folder is looked at again.
    full_due: bool,
    looking: bool,
    /// Folders asked about since the last full look, so one Git can't answer isn't asked forever.
    asked: HashSet<String>,
}

impl Repos {
    /// Makes the next request ask about every folder.
    pub fn ask_again(&mut self) {
        self.full_due = true;
    }

    /// The folders to ask about now, if any and if no look is running.
    pub fn request(&mut self, sessions: &[Session]) -> Option<RepoRequest> {
        if self.looking {
            return None;
        }
        let mut used: Vec<String> = sessions.iter().filter_map(|s| s.folder.clone()).collect();
        used.sort_unstable();
        used.dedup();
        let full = std::mem::take(&mut self.full_due);
        if full {
            self.asked.clear();
        }
        let folders: Vec<String> = used.iter().filter(|f| full || !(self.places.contains_key(*f) || self.asked.contains(*f))).cloned().collect();
        if folders.is_empty() {
            return None;
        }
        self.asked.extend(folders.iter().cloned());
        self.looking = true;
        Some(RepoRequest { folders, used, full })
    }

    pub fn finish(&mut self, answers: HashMap<String, Place>) {
        self.looking = false;
        self.places.extend(answers);
    }
}

/// Answers what each folder's repository is, reusing what the index saved before. Folders that
/// weren't in Git are asked about again on a full request, in case one has become a repository.
pub fn look_up(index: &Path, request: &RepoRequest) -> HashMap<String, Place> {
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
    let mut out: HashMap<String, Place> = request.folders.iter().filter_map(|f| saved.get(f).filter(|_| reusable(f)).map(|p| (f.clone(), p.clone()))).collect();
    out.extend(answers);
    out
}
