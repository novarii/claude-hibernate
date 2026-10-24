//! Reading Claude Code's TUI from our own copy of the screen.
//!
//! These are the only checks tied to Claude Code's UI; keep them small and
//! conservative: when unsure, report "not idle" so the session stays awake.

/// Claude's input box is a `❯` line between two horizontal rules. Empty means
/// nothing after the glyph and the closing rule right below it (a long draft
/// would wrap onto that line instead).
pub fn prompt_is_empty(lines: &[String]) -> bool {
    for (i, line) in lines.iter().enumerate().rev() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix('❯') {
            if !rest.trim().is_empty() {
                return false;
            }
            return lines.get(i + 1).is_some_and(|next| next.trim_start().starts_with('─'));
        }
    }
    false // no input box: a menu, dialog, or something we don't recognize
}

/// Footer and status text that means work is still running.
pub fn footer_busy(lines: &[String]) -> bool {
    let start = lines.len().saturating_sub(6);
    lines[start..].iter().any(|l| {
        let l = l.to_lowercase();
        l.contains("esc to interrupt")
            || l.contains("still running")
            || l.contains("waiting for")
            || counted(&l, &["shell", "shells", "monitor", "monitors", "background", "task", "tasks"])
    })
}

/// "1 shell", "2 background tasks", ...: a number followed by one of `words`.
fn counted(line: &str, words: &[&str]) -> bool {
    let tokens: Vec<&str> = line.split(|c: char| c.is_whitespace() || c == '·').filter(|t| !t.is_empty()).collect();
    tokens.windows(2).any(|w| w[0].chars().all(|c| c.is_ascii_digit()) && words.contains(&w[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn empty_prompt() {
        let l = lines("● pong\n───────\n❯\n───────\n  ⏵⏵ bypass permissions on · ← 3 agents");
        assert!(prompt_is_empty(&l));
        assert!(!footer_busy(&l));
    }

    #[test]
    fn draft_prompt() {
        assert!(!prompt_is_empty(&lines("───────\n❯ fix the bug\n───────")));
        assert!(!prompt_is_empty(&lines("───────\n❯\n  continued draft line\n───────")));
    }

    #[test]
    fn no_prompt_box() {
        assert!(!prompt_is_empty(&lines(" Do you want to proceed?\n 1. Yes\n 2. No")));
    }

    #[test]
    fn background_shell_footer() {
        let l = lines("───────\n❯\n───────\n  ⏵⏵ bypass permissions on · 1 shell · ← 3 agents · ↓ to manage");
        assert!(footer_busy(&l));
    }
}
