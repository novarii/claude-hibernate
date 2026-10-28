//! Deciding whether to wrap an invocation, and finding the real binary.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SUBCOMMANDS: &[&str] = &[
    "agents", "attach", "auth", "auto-mode", "doctor", "gateway", "import", "install", "logs", "mcp",
    "plugin", "plugins", "purge", "respawn", "rm", "setup-token", "stop", "kill", "ultrareview",
    "update", "upgrade",
];
/// Not a local session to resume: print mode, help, and `--cloud`, which
/// runs the session in the cloud and only attaches to it here.
const NON_INTERACTIVE: &[&str] = &["-p", "--print", "-h", "--help", "-v", "--version", "--cloud"];
/// Options that take one value (`<x>` or `[x]` in `claude --help`). Like
/// commander, a following token that does not start with `-` is the value.
const VALUE_FLAGS: &[&str] = &[
    "--agent", "--agents", "--append-system-prompt", "--autocompact", "--cloud", "--debug", "-d",
    "--debug-file", "--effort", "--environment", "--fallback-model", "--from-pr", "--input-format",
    "--json-schema", "--max-budget-usd", "--model", "--name", "-n", "--output-format",
    "--permission-mode", "--permission-prompts", "--plugin-dir", "--plugin-url",
    "--prompt-suggestions", "--remote-control", "--remote-control-session-name-prefix", "--resume",
    "-r", "--session-id", "--setting-sources", "--settings", "--system-prompt",
    "--system-prompt-snapshot", "--teleport", "--worktree", "-w",
];
/// Variadic options (`<x...>`): every following non-flag token is a value.
const VARIADIC_FLAGS: &[&str] = &[
    "--add-dir", "--allowedTools", "--allowed-tools", "--betas", "--disallowedTools",
    "--disallowed-tools", "--file", "--mcp-config", "--tools",
];
/// Options that pick or create the session; waking passes `--resume <id>` instead.
const SESSION_FLAGS: &[&str] = &[
    "-c", "--continue", "--fork-session", "-r", "--resume", "--session-id", "--from-pr",
    "--teleport", "--worktree", "-w",
];

/// Quote for a POSIX shell, leaving plain words as they are.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-=:@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Only interactive TTY sessions are wrapped; everything else execs claude.
pub fn wants_wrapping(args: &[String]) -> bool {
    let tty = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    tty && !args.first().is_some_and(|a| SUBCOMMANDS.contains(&a.as_str()))
        && !args.iter().any(|a| NON_INTERACTIVE.contains(&a.split_once('=').map_or(a.as_str(), |(n, _)| n)))
}

/// A user-supplied --settings would compete with the one we inject.
pub fn has_own_settings(args: &[String]) -> bool {
    args.iter().any(|a| a == "--settings" || a.starts_with("--settings="))
}

/// The user's flags (model, permission mode, ...) without session pickers or
/// the initial prompt, which the first run already sent.
pub fn resume_args(args: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            break; // everything after is the prompt
        }
        if !a.starts_with('-') {
            i += 1; // the prompt
            continue;
        }
        let (name, inline_value) = match a.split_once('=') {
            Some((n, _)) => (n, true),
            None => (a, false),
        };
        let mut group = vec![a.to_string()];
        i += 1;
        if !inline_value && VALUE_FLAGS.contains(&name) {
            if let Some(v) = args.get(i).filter(|v| !v.starts_with('-')) {
                group.push(v.clone());
                i += 1;
            }
        } else if !inline_value && VARIADIC_FLAGS.contains(&name) {
            while let Some(v) = args.get(i).filter(|v| !v.starts_with('-')) {
                group.push(v.clone());
                i += 1;
            }
        }
        if !SESSION_FLAGS.contains(&name) {
            out.extend(group);
        }
    }
    out
}

/// `resume_args` plus what the session changed since it started that a
/// resume doesn't restore: its permission mode (shift+tab) and /add-dir
/// directories.
///
/// `--dangerously-skip-permissions` beats `--permission-mode`, so it becomes
/// `--allow-dangerously-skip-permissions`: bypass stays one shift+tab away
/// but a session left in plan mode wakes in plan mode.
pub fn for_resume(args: &[String], mode: Option<&str>, added_dirs: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut it = resume_args(args).into_iter().peekable();
    while let Some(a) = it.next() {
        if mode.is_some() && a == "--permission-mode" {
            it.next_if(|v| !v.starts_with('-'));
        } else if mode.is_some() && a.starts_with("--permission-mode=") {
            // replaced below
        } else if mode.is_some() && a == "--dangerously-skip-permissions" {
            out.push("--allow-dangerously-skip-permissions".to_string());
        } else {
            out.push(a);
        }
    }
    if let Some(m) = mode {
        out.extend(["--permission-mode".to_string(), m.to_string()]);
    }
    if !added_dirs.is_empty() {
        out.push("--add-dir".to_string());
        out.extend(added_dirs.iter().cloned());
    }
    out
}

fn is_executable(p: &Path) -> bool {
    p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The real claude: config, then PATH (skipping anything that resolves to
/// us), then well-known install locations.
pub fn find_real_claude(configured: Option<&str>) -> Option<PathBuf> {
    let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let not_me = |p: &Path| is_executable(p) && p.canonicalize().ok() != me;
    if let Some(c) = configured.map(PathBuf::from).filter(|p| not_me(p)) {
        return Some(c);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(found) = std::env::split_paths(&path).map(|d| d.join("claude")).find(|p| not_me(p)) {
        return Some(found);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    [".local/bin/claude", ".claude/local/claude", ".local/share/fnm/aliases/default/bin/claude"]
        .iter()
        .map(|rel| home.join(rel))
        .chain(["/usr/local/bin/claude", "/opt/homebrew/bin/claude"].map(PathBuf::from))
        .find(|p| not_me(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn resume_keeps_flags_drops_pickers_and_prompt() {
        assert_eq!(
            resume_args(&v(&["--model", "opus", "--resume", "abc", "-c", "--dangerously-skip-permissions", "hello"])),
            v(&["--model", "opus", "--dangerously-skip-permissions"])
        );
        assert_eq!(resume_args(&v(&["--model=opus", "-r"])), v(&["--model=opus"]));
        assert_eq!(
            resume_args(&v(&["--add-dir", "a", "b", "--permission-mode", "plan", "fix it"])),
            v(&["--add-dir", "a", "b", "--permission-mode", "plan"])
        );
        assert_eq!(resume_args(&v(&["--verbose", "--", "-starts with dash"])), v(&["--verbose"]));
    }

    #[test]
    fn resume_restores_mode_and_dirs() {
        assert_eq!(
            for_resume(&v(&["--dangerously-skip-permissions", "--permission-mode", "auto", "-r", "x"]), Some("plan"), &v(&["/a"])),
            v(&["--allow-dangerously-skip-permissions", "--permission-mode", "plan", "--add-dir", "/a"])
        );
        assert_eq!(for_resume(&v(&["--permission-mode=auto", "--model", "opus"]), Some("default"), &[]),
            v(&["--model", "opus", "--permission-mode", "default"]));
        assert_eq!(for_resume(&v(&["--dangerously-skip-permissions"]), None, &[]), v(&["--dangerously-skip-permissions"]));
    }
}
