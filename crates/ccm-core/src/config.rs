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
        Self { large_threshold_mb: 100, old_after_days: 30, windows: default_windows(), terminal: None, tools: None }
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

    pub fn large_threshold_bytes(&self) -> u64 {
        self.large_threshold_mb.saturating_mul(1024 * 1024)
    }

    /// What counts as large and old, as of `now_ms`.
    pub fn limits(&self, now_ms: i64) -> Limits {
        let days = i64::try_from(self.old_after_days).unwrap_or(i64::MAX);
        Limits { large_bytes: self.large_threshold_bytes(), old_before_ms: now_ms.saturating_sub(days.saturating_mul(24 * 60 * 60 * 1000)) }
    }
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
