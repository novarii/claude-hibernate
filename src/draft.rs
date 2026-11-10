//! Keeping an unsent draft across a sleep.
//!
//! Claude: a Claude Code mod (`claude-mod/`, loaded with `--plugin-dir` for
//! the wrapped process only) reads the prompt box with `$.prompt.read()` and
//! reports it to `<pid>.draft` when it changes (checked every second); the
//! wrapper takes the draft from it just before stopping Claude. On a resume
//! the mod puts the draft back with `$.prompt.fill()`.
//! pi: its extension does the same with the editor (pi.rs).
//!
//! The wrapper passes the draft to the resumed process in `<pid>.draft-in`,
//! named by CLAUDE_HIBERNATE_DRAFT_IN, and removes the file once it is back.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::state::{runtime_dir, write_atomic};

pub const OUT_ENV: &str = "CLAUDE_HIBERNATE_DRAFT_OUT";
pub const IN_ENV: &str = "CLAUDE_HIBERNATE_DRAFT_IN";

const MOD_FILES: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", include_str!("../claude-mod/.claude-plugin/plugin.json")),
    ("hooks/hooks.json", include_str!("../claude-mod/hooks/hooks.json")),
    ("hooks/register.ts", include_str!("../claude-mod/hooks/register.ts")),
];

/// Write the Claude mod where `--plugin-dir` can load it; returns its folder.
pub fn claude_mod_dir() -> PathBuf {
    let dir = runtime_dir().join(format!("claude-mod-{}", env!("CARGO_PKG_VERSION")));
    for (rel, text) in MOD_FILES {
        let path = dir.join(rel);
        if fs::read_to_string(&path).ok().as_deref() != Some(*text) {
            let _ = fs::create_dir_all(path.parent().unwrap());
            write_atomic(&path, text.as_bytes());
        }
    }
    dir
}

/// Not `.json`: `status` reads every `<pid>.json` in the runtime dir.
pub fn out_path(wrapper_pid: u32) -> PathBuf {
    runtime_dir().join(format!("{wrapper_pid}.draft"))
}

pub fn in_path(wrapper_pid: u32) -> PathBuf {
    runtime_dir().join(format!("{wrapper_pid}.draft-in"))
}

pub fn write_in(wrapper_pid: u32, text: &str) {
    write_atomic(&in_path(wrapper_pid), serde_json::json!({ "text": text }).to_string().as_bytes());
}

/// What the Claude mod reports (claude-mod/hooks/register.ts).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ClaudeReport {
    pub text: String,
    /// Milliseconds since the epoch.
    pub at: f64,
    /// When the draft was put back after a resume (ms); -1 when it failed.
    pub restored: f64,
}

impl ClaudeReport {
    pub fn load(path: &Path) -> Option<Self> {
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }
}

/// Parts of a draft that its text alone cannot bring back: an attached
/// image, or a long paste Claude shows collapsed.
pub fn unrestorable(text: &str) -> Option<&'static str> {
    if text.contains("[Image #") {
        Some("an image in the prompt")
    } else if text.contains("[Pasted text #") || text.contains("[...Truncated text #") {
        Some("a collapsed paste in the prompt")
    } else {
        None
    }
}

/// The draft for the sleep screen: up to `max` lines, each cut to `cols`.
pub fn preview(text: &str, cols: usize, max: usize) -> Vec<String> {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let room = cols.saturating_sub(6).max(1);
    let mut out: Vec<String> = lines
        .iter()
        .take(max)
        .map(|l| {
            let clean: String = l.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            let mut cut: String = clean.chars().take(room).collect();
            if clean.chars().count() > room {
                cut.pop();
                cut.push('…');
            }
            format!("  │ {cut}")
        })
        .collect();
    if lines.len() > max {
        out.push(format!("  │ … {} more lines", lines.len() - max));
    }
    out
}

/// Text for a terminal paste: bracketed when the program asked for it, so
/// newlines are never taken as Enter.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\n").replace('\n', "\r");
    if bracketed { format!("\x1b[200~{body}\x1b[201~").into_bytes() } else { body.into_bytes() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_drafts() {
        assert_eq!(preview("fix the\nflaky test", 80, 6), vec!["  │ fix the", "  │ flaky test"]);
        assert_eq!(preview("a\nb\nc", 80, 2), vec!["  │ a", "  │ b", "  │ … 1 more lines"]);
        assert_eq!(preview("abcdefghij", 10, 6), vec!["  │ abc…"]);
    }

    #[test]
    fn flags_what_text_cannot_restore() {
        assert!(unrestorable("see [Image #1] here").is_some());
        assert!(unrestorable("[Pasted text #2 +40 lines]").is_some());
        assert!(unrestorable("plain\nmulti-line").is_none());
    }

    #[test]
    fn pastes_with_brackets() {
        assert_eq!(paste("a\nb", true), b"\x1b[200~a\rb\x1b[201~");
        assert_eq!(paste("a\nb", false), b"a\rb");
    }
}
