//! pi (https://pi.dev) support: the same wrapper, with pi's own extension
//! API in place of Claude's hooks and screen rules.
//!
//! The wrapper loads `pi/claude-hibernate.ts` into pi with `-e`. The extension
//! writes the session file, busy/idle (agent_start..agent_settled), queued
//! messages, open dialogs, compaction and unsent editor text to a state file;
//! the wrapper reads it to decide when pi may sleep and resumes it with
//! `pi --session <file>`, which restores the conversation, model and thinking
//! level from the session.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::state::{runtime_dir, write_atomic};

pub const EXTENSION: &str = include_str!("../pi/claude-hibernate.ts");
pub const STATE_ENV: &str = "CLAUDE_HIBERNATE_PI_STATE";

/// `pi <command>` subcommands, which never start a session.
const SUBCOMMANDS: &[&str] = &["install", "remove", "uninstall", "update", "list", "config", "auth", "mcp"];
/// Options that always take the next argument (`i + 1 < args.length` in pi's parser).
const VALUE_FLAGS: &[&str] = &[
    "--provider", "--model", "--api-key", "--system-prompt", "--append-system-prompt", "--name", "-n",
    "--session", "--session-id", "--fork", "--session-dir", "--models", "--tools", "-t",
    "--exclude-tools", "-xt", "--thinking", "--export", "--extension", "-e", "--skill",
    "--prompt-template", "--theme",
];
/// Options that take the next argument unless it looks like an option.
const OPTIONAL_VALUE_FLAGS: &[&str] = &["--mode", "--use-theme", "--tui-mode", "--list-models", "--print", "-p"];
/// Options that pick or create the session; waking passes `--session <file>`.
const SESSION_FLAGS: &[&str] = &["-c", "--continue", "-r", "--resume", "--session", "--session-id", "--fork", "--name", "-n"];
/// Restored from the session on resume; passing them again would override a
/// model or thinking level switched in the session (Ctrl+P, /model).
const SESSION_RESTORED: &[&str] = &["--model", "--provider", "--thinking"];

/// Only interactive TTY sessions that are saved can be resumed.
pub fn wants_wrapping(args: &[String]) -> bool {
    let tty = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    if !tty || args.first().is_some_and(|a| SUBCOMMANDS.contains(&a.as_str())) {
        return false;
    }
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => break,
            "-p" | "--print" | "-h" | "--help" | "-v" | "--version" | "--export" | "--list-models" | "--no-session" => {
                return false;
            }
            "--mode" if it.peek().is_some_and(|m| *m == "json" || *m == "rpc") => return false,
            _ => {}
        }
    }
    true
}

/// The user's flags without the session pickers, the initial prompt and
/// `@file` arguments (which the first run already sent), and without the
/// model and thinking level the session restores itself. Mirrors the value
/// rules of pi's argument parser, including extension flags (`--x value`).
pub fn resume_args(args: &[String]) -> Vec<String> {
    let has_api_key = args.iter().any(|a| a == "--api-key");
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        if a == "--" {
            break; // everything after is the prompt and files
        }
        if !a.starts_with('-') || a.starts_with('@') {
            continue; // a prompt or @file
        }
        let mut group = vec![a.to_string()];
        let next = args.get(i);
        let takes = if VALUE_FLAGS.contains(&a) {
            next.is_some()
        } else if OPTIONAL_VALUE_FLAGS.contains(&a) {
            next.is_some_and(|v| !v.starts_with('-') && !v.starts_with('@'))
        } else if a.starts_with("--") && !a.contains('=') {
            // An extension flag: pi gives it the next argument as a value.
            next.is_some_and(|v| !v.starts_with('-') && !v.starts_with('@'))
        } else {
            false
        };
        if takes {
            group.push(next.unwrap().clone());
            i += 1;
        }
        let restored = SESSION_RESTORED.contains(&a) && !has_api_key; // --api-key needs --model
        if !SESSION_FLAGS.contains(&a) && !restored {
            out.extend(group);
        }
    }
    out
}

/// What the extension reports; see pi/claude-hibernate.ts.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PiState {
    pub pid: u32,
    pub starts: u64,
    pub session_id: Option<String>,
    pub session_file: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub busy: bool,
    pub pending: bool,
    pub dialogs: u32,
    pub draft: bool,
    pub draft_text: String,
    pub polled_at: f64,
    pub draft_restored: f64,
    pub compacting: bool,
    pub last_submit: f64,
    pub last_event: f64,
    pub first_work: f64,
}

impl PiState {
    /// Not `.json`: `status` (in any version) reads every `<pid>.json` there.
    pub fn path(wrapper_pid: u32) -> PathBuf {
        runtime_dir().join(format!("{wrapper_pid}.pi-state"))
    }

    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Why pi must stay awake, judged from its own report. A draft holds it
    /// unless `draft_ok` (it sleeps and comes back with the draft).
    pub fn held_by(&self, draft_ok: bool) -> Option<String> {
        if self.starts == 0 {
            return Some("pi has not started its session yet".into());
        }
        let Some(file) = self.session_file.as_deref() else {
            return Some("session is not saved (--no-session)".into());
        };
        if !Path::new(file).exists() {
            return Some("no saved conversation for this session".into());
        }
        if self.busy {
            return Some("pi working".into());
        }
        if self.compacting {
            return Some("pi compacting".into());
        }
        if self.pending {
            return Some("queued messages".into());
        }
        if self.dialogs > 0 {
            return Some("dialog open".into());
        }
        if self.draft && !draft_ok {
            return Some("unsent text in the prompt".into());
        }
        None
    }
}

/// Write the extension where pi can load it; returns its path.
pub fn extension_path() -> PathBuf {
    let path = runtime_dir().join(format!("pi-extension-{}.ts", env!("CARGO_PKG_VERSION")));
    if fs::read_to_string(&path).ok().as_deref() != Some(EXTENSION) {
        write_atomic(&path, EXTENSION.as_bytes());
    }
    path
}

/// Children pi started for work: a `!` command, a bash tool call or a
/// background job. MCP servers are pi's children too, but pi starts them with
/// the session, so a child started within this long of pi, and before pi
/// first ran a tool or `!` command (`first_work`), is taken to be one.
const MCP_STARTUP_SECS: u64 = 60;

pub fn work_in_children(pid: u32, first_work: f64) -> Option<String> {
    let o = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid=,etime=,comm="]).output().ok()?;
    let text = String::from_utf8_lossy(&o.stdout);
    let rows: Vec<(u32, u32, u64, String)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (p, pp, et) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, parse_etime(f.next()?)?);
            let comm = f.collect::<Vec<_>>().join(" ");
            Some((p, pp, et, comm.rsplit('/').next().unwrap_or("").to_string()))
        })
        .collect();
    let age = rows.iter().find(|r| r.0 == pid)?.2;
    let now = crate::state::now();
    rows.iter()
        .find(|r| {
            let started = now - r.2 as f64;
            r.1 == pid
                && (age.saturating_sub(r.2) > MCP_STARTUP_SECS || (first_work > 0.0 && started >= first_work - 1.0))
        })
        .map(|r| r.3.clone())
}

/// `ps` elapsed time: `[[dd-]hh:]mm:ss`.
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let secs = parts.iter().fold(0, |acc, p| acc * 60 + p);
    Some(days * 86400 + secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn resume_drops_pickers_prompt_and_restored_model() {
        assert_eq!(
            resume_args(&v(&["--model", "sonnet", "--thinking", "high", "-c", "--session-dir", "/s", "fix it", "@a.md"])),
            v(&["--session-dir", "/s"])
        );
        assert_eq!(
            resume_args(&v(&["--session", "abc", "-e", "./x.ts", "--tools", "read,bash", "-n", "My work"])),
            v(&["-e", "./x.ts", "--tools", "read,bash"])
        );
        assert_eq!(resume_args(&v(&["--verbose", "--", "-starts with dash"])), v(&["--verbose"]));
    }

    #[test]
    fn resume_keeps_model_needed_by_api_key() {
        assert_eq!(
            resume_args(&v(&["--api-key", "k", "--model", "openai/gpt-4o", "hi"])),
            v(&["--api-key", "k", "--model", "openai/gpt-4o"])
        );
    }

    #[test]
    fn resume_keeps_extension_flags_with_values() {
        // pi gives an unknown --flag the next argument unless it is an option or @file.
        assert_eq!(resume_args(&v(&["--plan", "--mode", "text", "--color", "red", "go"])), v(&["--plan", "--mode", "text", "--color", "red"]));
        assert_eq!(resume_args(&v(&["--plan", "@f.md"])), v(&["--plan"]));
        assert_eq!(resume_args(&v(&["--x=1", "prompt"])), v(&["--x=1"]));
    }

    #[test]
    fn parses_ps_etime() {
        assert_eq!(parse_etime("05"), Some(5));
        assert_eq!(parse_etime("01:05"), Some(65));
        assert_eq!(parse_etime("02:01:05"), Some(7265));
        assert_eq!(parse_etime("3-02:01:05"), Some(3 * 86400 + 7265));
        assert_eq!(parse_etime("x"), None);
    }

    #[test]
    fn holds_while_pi_has_anything_to_lose() {
        let dir = std::env::temp_dir().join(format!("ch-pi-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        fs::write(&file, "{}\n").unwrap();
        let idle = PiState { starts: 1, session_file: Some(file.to_string_lossy().into()), ..Default::default() };
        assert_eq!(idle.held_by(false), None);
        assert!(PiState { busy: true, ..idle.clone() }.held_by(false).is_some());
        assert!(PiState { draft: true, ..idle.clone() }.held_by(false).is_some());
        assert_eq!(PiState { draft: true, ..idle.clone() }.held_by(true), None); // sleeps with it
        assert!(PiState { pending: true, ..idle.clone() }.held_by(false).is_some());
        assert!(PiState { dialogs: 1, ..idle.clone() }.held_by(false).is_some());
        assert!(PiState { session_file: None, ..idle.clone() }.held_by(false).is_some());
        assert!(PiState { session_file: Some(dir.join("missing").to_string_lossy().into()), ..idle }.held_by(false).is_some());
        let _ = fs::remove_dir_all(dir);
    }
}
