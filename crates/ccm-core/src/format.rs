//! Text for display. Transcript text is untrusted, so nothing reaches the terminal unsanitized.

/// Removes escape sequences and control characters, and collapses whitespace to single spaces.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut pending_space = false;
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' | '\u{9b}' | '\u{9d}' => skip_escape(c, &mut chars),
            c if c.is_whitespace() => pending_space = !out.is_empty(),
            c if c.is_control() || is_bidi_control(c) => {}
            c => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push(c);
            }
        }
    }
    out
}

/// Like `sanitize`, but keeps line breaks.
pub fn sanitize_lines(s: &str) -> String {
    s.lines().map(sanitize).collect::<Vec<_>>().join("\n")
}

/// True for characters that make a path look different on screen from what it really is.
pub(crate) fn hides_text(c: char) -> bool {
    c.is_control() || is_bidi_control(c) || matches!(c, '\u{200b}'..='\u{200f}' | '\u{061c}' | '\u{2028}' | '\u{2029}' | '\u{feff}')
}

/// These reorder text on screen, so they can make displayed text misleading.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn skip_escape(first: char, chars: &mut std::iter::Peekable<std::str::Chars>) {
    let kind = match first {
        '\u{9b}' => '[',
        '\u{9d}' => ']',
        _ => match chars.next() {
            Some(k) => k,
            None => return,
        },
    };
    match kind {
        '[' => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        ']' | 'P' | 'X' | '^' | '_' => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' || c == '\u{9c}' {
                    break;
                }
                if c == '\u{1b}' {
                    chars.next();
                    break;
                }
            }
        }
        _ => {}
    }
}

/// A path with the home folder written as `~`.
pub fn short_path(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// `87k`, `1.5M`, `1M`, `950`.
pub fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}k", n / 1_000),
        _ => {
            let tenths = n / 100_000;
            if tenths.is_multiple_of(10) {
                format!("{}M", tenths / 10)
            } else {
                format!("{}.{}M", tenths / 10, tenths % 10)
            }
        }
    }
}

/// `2.5G`, `340M`, `12K`, `900B`.
pub fn bytes(n: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * K;
    const G: u64 = M * K;
    match n {
        0..K => format!("{n}B"),
        K..M => format!("{}K", n / K),
        M..G => format!("{}M", n / M),
        _ => format!("{:.1}G", n as f64 / G as f64),
    }
}

/// `now`, `5m`, `3h`, `2d`, `6w`.
pub fn ago(now_ms: i64, then_ms: Option<i64>) -> String {
    let Some(then) = then_ms else { return "-".into() };
    let mins = (now_ms - then).max(0) / 60_000;
    match mins {
        0 => "now".into(),
        1..=59 => format!("{mins}m"),
        60..=1439 => format!("{}h", mins / 60),
        1440..=20159 => format!("{}d", mins / 1440),
        _ => format!("{}w", mins / 10080),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_every_escape_form() {
        assert_eq!(sanitize("a\x1b[1;31mb"), "ab");
        assert_eq!(sanitize("a\x1b]0;title\x07b"), "ab");
        assert_eq!(sanitize("a\x1b]52;c;ZXZpbA==\x1b\\b"), "ab");
        assert_eq!(sanitize("a\u{9b}31mb"), "ab");
        assert_eq!(sanitize("a\x08\x00b"), "ab");
        assert_eq!(sanitize("  one\n\ttwo  "), "one two");
        assert_eq!(sanitize("trailing\x1b"), "trailing");
        assert_eq!(sanitize("olá 日本"), "olá 日本");
        assert_eq!(sanitize("a\u{202e}b"), "ab");
        assert_eq!(sanitize_lines("one\x1b[31m\n\n  two  three"), "one\n\ntwo three");
    }

    #[test]
    fn token_format() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(87_010), "87k");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(1_500_000), "1.5M");
    }

    #[test]
    fn byte_format() {
        assert_eq!(bytes(900), "900B");
        assert_eq!(bytes(2048), "2K");
        assert_eq!(bytes(5 * 1024 * 1024), "5M");
        assert_eq!(bytes(2_684_354_560), "2.5G");
    }

    #[test]
    fn age_format() {
        let m = 60_000;
        assert_eq!(ago(100 * m, None), "-");
        assert_eq!(ago(100 * m, Some(100 * m)), "now");
        assert_eq!(ago(100 * m, Some(95 * m)), "5m");
        assert_eq!(ago(200 * m, Some(20 * m)), "3h");
        assert_eq!(ago(3000 * m, Some(0)), "2d");
        assert_eq!(ago(30240 * m, Some(0)), "3w");
    }
}
