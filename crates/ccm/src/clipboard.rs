//! Copies text to the clipboard. A desktop tool is preferred because it can report failure. The
//! terminal escape code is the fallback for sessions without one, such as over SSH.

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// Returns what to tell the user.
pub fn copy(text: &str) -> Result<String, String> {
    let has = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    let mut tools: Vec<&[&str]> = Vec::new();
    if cfg!(target_os = "macos") {
        tools.push(&["pbcopy"]);
    }
    if has("WAYLAND_DISPLAY") {
        tools.push(&["wl-copy"]);
    }
    if has("DISPLAY") {
        tools.push(&["xclip", "-selection", "clipboard"]);
    }
    if tools.iter().any(|tool| run_tool(tool, text).is_ok()) {
        return Ok("Copied".into());
    }
    let mut out = io::stdout();
    out.write_all(osc52(text).as_bytes()).and_then(|()| out.flush()).map_err(|e| format!("Couldn't copy it: {e}"))?;
    Ok("Sent to your terminal's clipboard, if it allows that".into())
}

/// Output goes nowhere on purpose: these tools stay running to serve the clipboard, and would
/// otherwise keep a pipe open and scribble on the screen.
fn run_tool(tool: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(tool[0])
        .args(&tool[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child.stdin.take().expect("stdin is piped").write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() { Ok(()) } else { Err(io::Error::other(status.to_string())) }
}

fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16 | u32::from(*chunk.get(1).unwrap_or(&0)) << 8 | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn base64_matches_known_values() {
        for (input, expected) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("cd 'é' && x", "Y2QgJ8OpJyAmJiB4")] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn the_terminal_code_has_no_raw_text_in_it() {
        assert_eq!(osc52("a\x07b"), "\x1b]52;c;YQdi\x07");
    }

    #[test]
    fn a_tool_receives_the_text_and_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let tool = dir.path().join("tool");
        fs::write(&tool, format!("#!/bin/sh\ncat > '{}'\n", out.display())).unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        run_tool(&[tool.to_str().unwrap()], "hello 'x'").unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "hello 'x'");
        assert!(run_tool(&["/bin/false"], "x").is_err());
        assert!(run_tool(&["/no/such/tool"], "x").is_err());
    }
}
