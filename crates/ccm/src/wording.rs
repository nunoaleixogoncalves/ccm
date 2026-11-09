//! What's said about a delete or a worktree removal, the same in the terminal and on the page.

use ccm_core::delete;
use ccm_core::format::{bytes, sanitize, short_path};
use ccm_core::model::Session;
use ccm_core::removal::{self, Planned, NAMED_IGNORED};
use ccm_core::worktree::Link;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Bold,
    Warn,
    Danger,
}

pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// How much of a conversation happened here, which tells working here from passing through.
pub fn share(l: &Link) -> String {
    let percent = (l.records * 100).checked_div(l.total).unwrap_or(0);
    if percent == 0 { "under 1%".into() } else { format!("{percent}%") }
}

/// What removing one worktree would do, ending with an empty line to separate it from the next.
pub fn planned_lines(item: &Planned, sessions: &[Session], home: &str) -> Vec<(String, Tone)> {
    let path = sanitize(&short_path(&item.target.path.to_string_lossy(), home));
    let branch = item.branch.as_deref().map_or("(detached)".to_string(), sanitize);
    let mut lines = vec![(format!("{path}  {branch}"), Tone::Bold)];
    match (item.unpushed, item.has_remote) {
        (0, _) => {}
        (n, true) => lines.push((format!("  {} on no remote branch, as of the last fetch.", plural(n, "commit", "commits")), Tone::Danger)),
        (n, false) => lines.push((format!("  No remote, so {} only on this machine.", plural(n, "commit is", "commits are")), Tone::Danger)),
    }
    if item.unpushed > 0 && item.upstream_gone {
        lines.push(("  Its remote branch was deleted, which usually means it was merged. ccm can't check.".into(), Tone::Plain));
    }
    if item.gone {
        lines.push(("  Its folder is already gone. This only clears Git's record of it.".into(), Tone::Plain));
    }
    if !item.ignored.is_empty() {
        let at_least = if item.ignored_more { "at least " } else { "" };
        let mut names: Vec<String> = item.ignored.iter().take(NAMED_IGNORED).map(|n| sanitize(n)).collect();
        if item.ignored.len() > NAMED_IGNORED {
            names.push(format!("and {} more", item.ignored.len() - NAMED_IGNORED));
        }
        lines.push((format!("  Deletes ignored files, {at_least}{}: {}", bytes(item.ignored_bytes), names.join(", ")), Tone::Warn));
    }
    match item.links.len() {
        0 => {}
        1 => lines.push(("  1 conversation worked here:".into(), Tone::Plain)),
        n => lines.push((format!("  {n} conversations worked here:"), Tone::Warn)),
    }
    let mut links: Vec<&Link> = item.links.iter().collect();
    links.sort_by_key(|l| std::cmp::Reverse(l.last_ms));
    for l in links {
        let name = sessions.iter().find(|s| s.id == l.id).map_or(l.id.clone(), |s| sanitize(&s.display_name()));
        lines.push((format!("    {name}  ({} of its activity)", share(l)), Tone::Plain));
        if l.started_here {
            lines.push(("      It started here, so resuming it later recreates this folder empty.".into(), Tone::Plain));
        }
    }
    lines.push((String::new(), Tone::Plain));
    lines
}

/// The one-line result of a delete, and anything else that needs saying.
pub fn delete_report(plan: &delete::Plan, include_notes: bool, outcome: &delete::Outcome, name_of: impl Fn(&str) -> String) -> (String, Vec<String>) {
    let freed: u64 = plan.items.iter().filter(|i| outcome.deleted.contains(&i.id)).map(|i| i.bytes).sum();
    let freed = freed + if include_notes { plan.note_bytes() } else { 0 };
    let count = outcome.deleted.len();
    let summary = format!("Deleted {count} conversation{}, freed {}.", if count == 1 { "" } else { "s" }, bytes(freed));
    let mut lines = Vec::new();
    for f in &outcome.failures {
        lines.push(format!("Not removed: {} ({})", sanitize(&f.path.display().to_string()), sanitize(&f.error)));
    }
    for s in plan.skipped.iter().chain(&outcome.skipped) {
        lines.push(format!("Skipped {}: {}", name_of(&s.id), s.reason));
    }
    lines.extend(plan.warnings.iter().map(|w| sanitize(w)));
    (summary, lines)
}

/// The same for removing worktrees. A plan with nothing to remove only has its refusals to report.
pub fn removal_report(plan: &removal::Plan, outcome: &removal::Outcome) -> (String, Vec<String>) {
    let path = |p: &std::path::Path| sanitize(&p.display().to_string());
    let refused = plan.refused.iter().map(|s| format!("Skipped {}: {}", path(&s.path), sanitize(&s.reason)));
    if plan.items.is_empty() {
        return (String::new(), refused.collect());
    }
    let summary = match outcome.removed.len() {
        1 => "Removed 1 worktree. Its branch is kept.".to_string(),
        n => format!("Removed {n} worktrees. Their branches are kept."),
    };
    let mut lines: Vec<String> = outcome.failures.iter().map(|f| format!("Not removed {}: {}", path(&f.path), sanitize(&f.reason))).collect();
    lines.extend(refused);
    lines.extend(outcome.skipped.iter().map(|s| format!("Skipped {}: {}", path(&s.path), sanitize(&s.reason))));
    (summary, lines)
}
