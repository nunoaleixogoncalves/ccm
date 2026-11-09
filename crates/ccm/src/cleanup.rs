//! The clean-up suggestions, worked out and described the same in the terminal and on the page.

use ccm_core::config::Config;
use ccm_core::format::{bytes, sanitize};
use ccm_core::jobs::Leftover;
use ccm_core::model::Session;
use ccm_core::query::Suggestion;
use ccm_core::removal;

use crate::worktrees::Worktrees;

pub const KEPT_JOBS: &str = "These job folders may have worktrees, so ccm leaves them. Press Enter on the line, then c, to copy the claude rm commands.";
pub const SAFETY: &str = "Nothing is deleted without asking. Each one is checked again just before, and any that opened meanwhile are kept.";
pub const JOB_KEPT_NOTE: &str = "may have a worktree, kept";

/// One line of the clean-up screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cleanup {
    Conversations(Suggestion),
    Worktrees,
    Jobs,
}

impl Cleanup {
    pub const ALL: [Cleanup; 5] = [
        Cleanup::Conversations(Suggestion::NoReply),
        Cleanup::Conversations(Suggestion::FolderGone),
        Cleanup::Conversations(Suggestion::LargeOld),
        Cleanup::Worktrees,
        Cleanup::Jobs,
    ];
}

/// How many items a clean-up line has and what removing them would free. `bytes` is `None`
/// when some of them haven't been measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub count: usize,
    pub bytes: Option<u64>,
}

/// The conversations on a clean-up line, as indexes. Other lines have none.
pub fn conversations(line: Cleanup, sessions: &[Session], config: &Config, now_ms: i64) -> Vec<usize> {
    match line {
        // Only what can be deleted from here is suggested.
        Cleanup::Conversations(suggestion) => suggestion.select(sessions, config.limits(now_ms)).into_iter().filter(|i| sessions[*i].source.features().delete).collect(),
        Cleanup::Worktrees | Cleanup::Jobs => Vec::new(),
    }
}

/// What a clean-up line holds. `None` while it's still being checked.
pub fn tally(line: Cleanup, sessions: &[Session], config: &Config, now_ms: i64, worktrees: &Worktrees, leftovers: Option<&[Leftover]>) -> Option<Tally> {
    match line {
        Cleanup::Conversations(_) => {
            let rows = conversations(line, sessions, config, now_ms);
            Some(Tally { count: rows.len(), bytes: Some(rows.iter().map(|&i| sessions[i].size_bytes).sum()) })
        }
        Cleanup::Worktrees => {
            worktrees.scan.as_ref()?;
            let idle = worktrees.idle();
            let bytes = idle.iter().map(|w| if w.prunable { Some(0) } else { worktrees.sizes.get(&w.path).copied() }).sum();
            Some(Tally { count: idle.len(), bytes })
        }
        Cleanup::Jobs => {
            let found = leftovers?;
            // Ones that may have a worktree stay, so they free nothing here.
            Some(Tally { count: found.len(), bytes: Some(found.iter().filter(|l| !l.worktree).map(|l| l.bytes).sum()) })
        }
    }
}

pub fn label(config: &Config, line: Cleanup) -> String {
    match line {
        Cleanup::Conversations(Suggestion::NoReply) => "Conversations Claude never replied to".into(),
        Cleanup::Conversations(Suggestion::FolderGone) => "Conversations whose folder is gone".into(),
        Cleanup::Conversations(Suggestion::LargeOld) => format!("Conversations over {} not used for {} days", bytes(config.large_threshold_bytes()), config.old_after_days),
        Cleanup::Worktrees => "Worktrees with no conversation and nothing to lose".into(),
        Cleanup::Jobs => "Background job folders with no conversation".into(),
    }
}

/// What a line is about and what can be done with it.
pub fn explanation(config: &Config, line: Cleanup, tally: Option<Tally>) -> Vec<String> {
    let empty = "Nothing matches this right now, so there's nothing to delete here.";
    let is_empty = tally.is_some_and(|t| t.count == 0);
    let first = match line {
        Cleanup::Conversations(Suggestion::NoReply) => "Conversations where Claude never replied, often started by mistake or cut off at once.".to_string(),
        Cleanup::Conversations(Suggestion::FolderGone) => "The folder these started in no longer exists. Resuming one would recreate it empty.".to_string(),
        Cleanup::Conversations(Suggestion::LargeOld) => format!(
            "Larger than {} and not active for {} days. Change these with large_threshold_mb and old_after_days in config.toml.",
            bytes(config.large_threshold_bytes()),
            config.old_after_days
        ),
        Cleanup::Worktrees => {
            let what = if tally.is_none() {
                "Still checking the worktrees."
            } else if is_empty {
                empty
            } else {
                "d removes all of them, and their branches are kept. Space marks lines to delete several at once. Enter shows them in the worktree view, to look through them or remove only some."
            };
            return vec![
                "Worktrees no conversation worked in, with no changes, no unpushed commits and a branch, or whose folder is gone.".into(),
                "Locked ones, and ones a running session is in, aren't suggested. Only repositories your conversations used are checked.".into(),
                what.into(),
            ];
        }
        Cleanup::Jobs => {
            let what = if tally.is_none() {
                "Job folders can't be checked."
            } else if is_empty {
                empty
            } else {
                "d deletes the ones ccm can, and Space marks lines to delete several at once. Enter lists them, where c copies the claude rm commands."
            };
            return vec![
                "Folders in ~/.claude/jobs whose conversation is gone, untouched for a day and not running.".into(),
                "Ones that may have a worktree are only listed, with the claude rm command that removes them safely.".into(),
                what.into(),
            ];
        }
    };
    let what = if is_empty { empty } else { "d deletes all of them, and Space marks lines to delete several at once. Enter shows them in the list, to look through them or delete only some." };
    vec![first, "Open ones aren't suggested.".into(), what.into()]
}

/// What's known of a job folder.
pub fn job_what(l: &Leftover) -> String {
    match (&l.name, l.no_state) {
        (_, true) => "no state file".to_string(),
        (Some(name), _) => sanitize(name),
        (None, _) => "unnamed".to_string(),
    }
}

/// One `claude rm` per job that may have a worktree, ready to paste. A name that isn't plainly
/// safe in a shell is left out.
pub fn rm_commands(jobs: &[Leftover]) -> Option<String> {
    let safe = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let commands: Vec<String> = jobs.iter().filter(|l| l.worktree).map(Leftover::short).filter(|n| safe(n)).map(|n| format!("claude rm {n}")).collect();
    (!commands.is_empty()).then(|| commands.join("; "))
}

/// Only worktrees with nothing to lose are suggested. One that gained commits since goes through
/// the worktree view, where removing it has its own countdown.
pub fn without_unpushed(mut plan: removal::Plan) -> removal::Plan {
    let (ahead, fine): (Vec<_>, Vec<_>) = plan.items.into_iter().partition(|i| i.unpushed > 0);
    plan.items = fine;
    plan.refused.extend(ahead.into_iter().map(|i| removal::Refused { path: i.target.path, reason: "it has unpushed commits now. Remove it from the worktree view".into() }));
    plan
}
