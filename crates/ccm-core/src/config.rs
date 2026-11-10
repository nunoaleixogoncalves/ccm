use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::model::Source;
use crate::query::Limits;

/// `~/.config/ccm/config.toml`. Every field is optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub large_threshold_mb: u64,
    /// A conversation not active for this many days counts as old.
    pub old_after_days: u64,
    /// Context window by model ID. Entries here add to or replace the defaults.
    pub windows: HashMap<String, u64>,
    /// Command that opens a terminal and runs what follows it, such as `["foot", "-e"]`.
    /// `xdg-terminal-exec` is used when this isn't set.
    pub terminal: Option<Vec<String>>,
    /// Which tools to read, such as `["claude", "codex"]`. When this isn't set, every tool whose
    /// folder exists is read.
    pub tools: Option<Vec<ToolName>>,
    /// Whether Resume and Fork go to the new conversation, where `ccm` can control that.
    pub focus_new_conversation: bool,
}

/// A tool's name in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolName {
    Claude,
    Codex,
}

impl ToolName {
    pub fn source(self) -> Source {
        match self {
            ToolName::Claude => Source::ClaudeCode,
            ToolName::Codex => Source::Codex,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { large_threshold_mb: 100, old_after_days: 30, windows: default_windows(), terminal: None, tools: None, focus_new_conversation: true }
    }
}

/// Current models as documented. Older models are left out on purpose, so they show `?`.
fn default_windows() -> HashMap<String, u64> {
    ["claude-opus-5-5", "claude-fable-5-1", "claude-sonnet-5-5", "claude-haiku-5-5"]
        .into_iter()
        .map(|m| (m.to_string(), 1_000_000))
        .collect()
}

impl Config {
    /// Returns the defaults and a message if the file is present but invalid.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return (Self::default(), None);
        };
        match Self::parse(&text) {
            Ok(c) => (c, None),
            Err(e) => (Self::default(), Some(format!("Ignoring {}: {e}", path.display()))),
        }
    }

    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let mut config: Self = toml::from_str(text)?;
        let mut windows = default_windows();
        windows.extend(std::mem::take(&mut config.windows));
        config.windows = windows;
        Ok(config)
    }

    /// Saves "Focus new conversation", keeping the rest of the file, comments included. A file
    /// with a mistake in it is left alone, as the person may be halfway through editing it.
    pub fn save_focus(path: &Path, on: bool) -> Result<(), String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.to_string()),
        };
        Self::parse(&text).map_err(|_| "config.toml has a mistake in it, so it wasn't changed".to_string())?;
        let new = with_focus(&text, on);
        if Self::parse(&new).map(|c| c.focus_new_conversation) != Ok(on) {
            return Err("config.toml couldn't be changed safely".into());
        }
        write_replacing(path, &new).map_err(|e| e.to_string())
    }

    pub fn large_threshold_bytes(&self) -> u64 {
        self.large_threshold_mb.saturating_mul(1024 * 1024)
    }

    /// What counts as large and old, as of `now_ms`.
    pub fn limits(&self, now_ms: i64) -> Limits {
        let days = i64::try_from(self.old_after_days).unwrap_or(i64::MAX);
        Limits { large_bytes: self.large_threshold_bytes(), old_before_ms: now_ms.saturating_sub(days.saturating_mul(24 * 60 * 60 * 1000)) }
    }
}

const FOCUS_KEY: &str = "focus_new_conversation";

/// `text` with the setting changed where it is, or added at the top, the one place that's always
/// outside any `[table]`.
fn with_focus(text: &str, on: bool) -> String {
    let line = format!("{FOCUS_KEY} = {on}");
    let is_setting = |l: &str| l.trim_start().strip_prefix(FOCUS_KEY).is_some_and(|rest| rest.trim_start().starts_with('='));
    if text.lines().any(is_setting) {
        let mut lines: Vec<&str> = text.lines().map(|l| if is_setting(l) { line.as_str() } else { l }).collect();
        if text.ends_with('\n') {
            lines.push("");
        }
        lines.join("\n")
    } else {
        format!("{line}\n{text}")
    }
}

/// Replaces the file in one step, so nothing ever reads half of it. A link stays a link, as the
/// file often lives with someone's dotfiles.
fn write_replacing(path: &Path, text: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = target.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&temporary, text)?;
    let kept = std::fs::metadata(&target).and_then(|m| std::fs::set_permissions(&temporary, m.permissions()));
    if let Err(e) = kept.or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }).and_then(|()| std::fs::rename(&temporary, &target)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_values_extend_the_defaults() {
        let c = Config::parse("large_threshold_mb = 5\nold_after_days = 2\n[windows]\nclaude-opus-4-8 = 200000\n").unwrap();
        assert_eq!(c.large_threshold_bytes(), 5 * 1024 * 1024);
        assert_eq!(c.windows["claude-opus-4-8"], 200_000);
        assert_eq!(c.limits(10 * 86_400_000).old_before_ms, 8 * 86_400_000);
        assert_eq!(Config::default().old_after_days, 30);
        assert_eq!(c.windows["claude-opus-5-5"], 1_000_000);
    }

    #[test]
    fn tools_are_named_in_lowercase_and_an_unknown_one_is_an_error() {
        let c = Config::parse("tools = [\"codex\"]\n").unwrap();
        assert_eq!(c.tools, Some(vec![ToolName::Codex]));
        assert_eq!(ToolName::Claude.source(), Source::ClaudeCode);
        assert!(Config::default().tools.is_none());
        assert!(Config::parse("tools = [\"cursor\"]\n").is_err());
    }

    #[test]
    fn focus_is_on_by_default_and_saving_keeps_everything_else() {
        assert!(Config::default().focus_new_conversation);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# my settings\nlarge_threshold_mb = 5\n\n[windows]\nclaude-opus-4-8 = 200000\n";
        std::fs::write(&path, original).unwrap();
        Config::save_focus(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with(original), "{text}");
        let (c, msg) = Config::load(&path);
        assert!(msg.is_none());
        assert!(!c.focus_new_conversation);
        assert_eq!(c.large_threshold_mb, 5);
        Config::save_focus(&path, true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(FOCUS_KEY).count(), 1, "changed where it is: {text}");
        assert!(Config::load(&path).0.focus_new_conversation);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file left");
    }

    #[test]
    fn saving_focus_makes_the_file_and_keeps_a_link_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/config.toml");
        Config::save_focus(&path, false).unwrap();
        assert!(!Config::load(&path).0.focus_new_conversation);
        let real = dir.path().join("dotfiles.toml");
        std::fs::write(&real, "old_after_days = 3\n").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        Config::save_focus(&link, false).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(std::fs::read_to_string(&real).unwrap().contains("focus_new_conversation = false"));
    }

    #[test]
    fn a_file_with_a_mistake_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "large_threshold_mb = \"lots\"").unwrap();
        assert!(Config::save_focus(&path, false).unwrap_err().contains("mistake"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "large_threshold_mb = \"lots\"");
    }

    #[test]
    fn invalid_file_falls_back_with_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "large_threshold_mb = \"lots\"").unwrap();
        let (c, msg) = Config::load(&path);
        assert_eq!(c.large_threshold_mb, 100);
        assert!(msg.is_some());
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let (c, msg) = Config::load(Path::new("/nonexistent/config.toml"));
        assert_eq!(c.large_threshold_mb, 100);
        assert!(msg.is_none());
    }
}
