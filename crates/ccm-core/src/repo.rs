//! Which Git repository each folder belongs to, so conversations can be grouped by repository.
//! A repository's main folder and all its worktrees count as one.
//!
//! Git runs the same careful way as for the worktree view. Answers are worth keeping, since a
//! worktree's folder may be removed later and Git can't say anything about it then.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::worktree::{git, path_of, Failure};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Repo {
        /// The main folder, or the repository folder itself when it's bare.
        repo: String,
        /// The worktree the folder is in, when it isn't the main one.
        worktree: Option<String>,
    },
    NotGit,
    /// The folder no longer exists.
    Gone,
}

/// Asks Git about one folder. `None` when Git couldn't run at all, which says nothing about the
/// folder.
pub fn look_up(folder: &str) -> Option<Place> {
    let dir = Path::new(folder);
    if !dir.is_dir() {
        return Some(Place::Gone);
    }
    let (common, top) = match git(dir, &["rev-parse", "--git-common-dir", "--show-toplevel"]) {
        Ok(out) => {
            let mut lines = out.split(|b| *b == b'\n');
            (lines.next().map(<[u8]>::to_vec).unwrap_or_default(), lines.next().map(path_of))
        }
        // A bare repository has no top level, which fails the whole command.
        Err(Failure::Failed) => match git(dir, &["rev-parse", "--git-common-dir"]) {
            Ok(out) => (out.strip_suffix(b"\n").unwrap_or(&out).to_vec(), None),
            Err(Failure::Failed) => return Some(Place::NotGit),
            Err(Failure::Missing) => return None,
        },
        Err(Failure::Missing) => return None,
    };
    let common = path_of(&common);
    let common = fs::canonicalize(if common.is_absolute() { common } else { dir.join(common) }).ok()?;
    let repo = if common.file_name().is_some_and(|n| n == ".git") { common.parent()?.to_path_buf() } else { common };
    let worktree = top.filter(|t| !t.as_os_str().is_empty() && *t != repo);
    Some(Place::Repo { repo: shown(&repo), worktree: worktree.map(|w| shown(&w)) })
}

fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Looks up many folders at once. Folders Git couldn't be asked about are left out.
pub fn look_up_all(folders: &[String]) -> HashMap<String, Place> {
    folders.par_iter().filter_map(|f| look_up(f).map(|p| (f.clone(), p))).collect()
}

/// The repository a folder Git can't answer for most likely belonged to: the deepest known
/// repository whose main folder contains it, as with worktrees under `.claude/worktrees`.
pub fn infer<'a>(folder: &str, repos: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let folder = PathBuf::from(folder);
    repos.filter(|r| folder.starts_with(r)).max_by_key(|r| Path::new(r).components().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "init.defaultBranch=main", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn real(p: &Path) -> String {
        fs::canonicalize(p).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn a_main_folder_and_its_worktrees_share_one_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("shop");
        fs::create_dir_all(main.join("src")).unwrap();
        sh(&main, &["init", "-q"]);
        fs::write(main.join("src/a"), "a").unwrap();
        sh(&main, &["add", "."]);
        sh(&main, &["commit", "-qm", "a"]);
        let beside = tmp.path().join("shop-login");
        sh(&main, &["worktree", "add", "-q", beside.to_str().unwrap(), "-b", "login"]);
        sh(&main, &["worktree", "add", "-q", ".claude/worktrees/tidy", "-b", "tidy"]);
        let repo = real(&main);

        let at = |p: &Path| look_up(p.to_str().unwrap()).unwrap();
        assert_eq!(at(&main), Place::Repo { repo: repo.clone(), worktree: None });
        assert_eq!(at(&main.join("src")), Place::Repo { repo: repo.clone(), worktree: None }, "a subfolder is the main worktree");
        assert_eq!(at(&beside.join("src")), Place::Repo { repo: repo.clone(), worktree: Some(real(&beside)) });
        assert_eq!(
            at(&main.join(".claude/worktrees/tidy")),
            Place::Repo { repo: repo.clone(), worktree: Some(real(&main.join(".claude/worktrees/tidy"))) }
        );
    }

    #[test]
    fn a_bare_repository_is_named_by_its_own_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("site.git");
        sh(tmp.path(), &["init", "-q", "--bare", bare.to_str().unwrap()]);
        assert_eq!(look_up(bare.to_str().unwrap()), Some(Place::Repo { repo: real(&bare), worktree: None }));
    }

    #[test]
    fn plain_and_missing_folders_say_so() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("notes");
        fs::create_dir(&plain).unwrap();
        // Git looks up through parent folders, so this only holds when the temporary folder isn't
        // inside a repository itself.
        if look_up(tmp.path().to_str().unwrap()) == Some(Place::NotGit) {
            assert_eq!(look_up(plain.to_str().unwrap()), Some(Place::NotGit));
        }
        assert_eq!(look_up(tmp.path().join("gone").to_str().unwrap()), Some(Place::Gone));
        let all = look_up_all(&[tmp.path().join("gone").to_string_lossy().into_owned()]);
        assert_eq!(all.values().collect::<Vec<_>>(), [&Place::Gone]);
    }

    #[test]
    fn an_unknown_folder_goes_to_the_deepest_repository_containing_it() {
        let repos = ["/w/shop", "/w/shop/vendor/lib", "/w/shopping"];
        let inferred = |f| infer(f, repos.iter().copied());
        assert_eq!(inferred("/w/shop/.claude/worktrees/x"), Some("/w/shop"));
        assert_eq!(inferred("/w/shop/vendor/lib/src"), Some("/w/shop/vendor/lib"));
        assert_eq!(inferred("/w/shop-login"), None, "a sibling folder is not inside");
        assert_eq!(inferred("/elsewhere"), None);
    }
}
