//! Context window figures. Transcripts don't record the window size, and an unknown size is
//! shown as `?`, never guessed.

use std::collections::HashMap;

use crate::format::tokens;

/// The first available of: the size saved by the status line, then the model table.
pub fn window_size(saved: Option<u64>, model: Option<&str>, table: &HashMap<String, u64>) -> Option<u64> {
    saved.or_else(|| table.get(model?).copied())
}

/// `87k/1M`, `87k/?`, or `-` when no response used any context.
pub fn context_label(used: Option<u64>, window: Option<u64>) -> String {
    match (used, window) {
        (None, _) => "-".into(),
        (Some(u), Some(w)) => format!("{}/{}", tokens(u), tokens(w)),
        (Some(u), None) => format!("{}/?", tokens(u)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_size_beats_the_table() {
        let table = HashMap::from([("m".to_string(), 1_000_000)]);
        assert_eq!(window_size(Some(200_000), Some("m"), &table), Some(200_000));
        assert_eq!(window_size(None, Some("m"), &table), Some(1_000_000));
        assert_eq!(window_size(None, Some("other"), &table), None);
        assert_eq!(window_size(None, None, &table), None);
    }

    #[test]
    fn labels() {
        assert_eq!(context_label(Some(87_000), Some(1_000_000)), "87k/1M");
        assert_eq!(context_label(Some(87_000), None), "87k/?");
        assert_eq!(context_label(None, Some(1_000_000)), "-");
    }
}
