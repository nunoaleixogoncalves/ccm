use std::path::{Path, PathBuf};

/// Where `ccm` reads from and keeps its own files.
#[derive(Debug, Clone)]
pub struct Paths {
    pub claude: PathBuf,
    pub index: PathBuf,
    pub config: PathBuf,
    /// Claude Code's main settings file, which records trusted folders.
    pub claude_json: PathBuf,
    /// `ccm`'s own log, saved window sizes and config backups.
    pub state: PathBuf,
}

impl Paths {
    pub fn from_env() -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let var = |name: &str, fallback: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
        };
        let claude_json = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir).join(".claude.json"),
            None => home.join(".claude.json"),
        };
        Some(Self {
            claude_json,
            claude: var("CLAUDE_CONFIG_DIR", ".claude"),
            index: var("XDG_CACHE_HOME", ".cache").join("ccm/index.db"),
            config: var("XDG_CONFIG_HOME", ".config").join("ccm/config.toml"),
            state: var("XDG_STATE_HOME", ".local/state").join("ccm"),
        })
    }

    pub fn projects(&self) -> PathBuf {
        self.claude.join("projects")
    }

    pub fn sessions(&self) -> PathBuf {
        self.claude.join("sessions")
    }
}

/// Accepts only the canonical lowercase UUID form Claude Code uses for session IDs.
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

/// The session ID of a transcript path, if its name is `<uuid>.jsonl`.
pub fn session_id_of(path: &Path) -> Option<&str> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    is_uuid(stem).then_some(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_uuid_transcripts_have_an_id() {
        let id = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
        assert_eq!(session_id_of(Path::new(&format!("/p/{id}.jsonl"))), Some(id));
        assert_eq!(session_id_of(Path::new("/p/agent-abc.jsonl")), None);
        assert_eq!(session_id_of(Path::new(&format!("/p/{id}.txt"))), None);
        assert!(!is_uuid("../../../../etc/passwd-aaaaaaaaaaaaaaaaaa"));
        assert!(!is_uuid("0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D"));
    }
}
