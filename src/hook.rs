//! `claude-hibernate hook <state-file>`: run by Claude Code hooks that the
//! wrapper injects with `--settings`. Records the session id, when a prompt
//! was last submitted (for draft detection), when Claude last compacted the
//! conversation (so housekeeping doesn't count as use), directories added
//! with /add-dir (to restore on resume), and the timers that only live inside
//! the claude process (/loop wake-ups, cron jobs), so the wrapper never kills
//! a session that still has work scheduled. Running background agents show
//! on screen, where herdr's detection rules see them.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;

use serde_json::Value;

use crate::args::shell_quote;
use crate::state::{now, write_atomic, HookState};

/// Tools whose PostToolUse results we track.
pub const TIMER_TOOLS: &str = "ScheduleWakeup|CronCreate|CronDelete";
/// Recurring cron jobs auto-expire after 7 days.
const CRON_MAX_SECS: f64 = 7.0 * 86400.0 + 1800.0;

/// The `--settings` JSON that routes the hooks we need to `exe hook <path>`.
pub fn settings_json(exe: &str, state_path: &Path) -> String {
    let cmd = format!("{} hook {}", shell_quote(exe), shell_quote(&state_path.to_string_lossy()));
    let hook = serde_json::json!([{ "hooks": [{ "type": "command", "command": cmd }] }]);
    let tools = serde_json::json!([{ "matcher": TIMER_TOOLS, "hooks": [{ "type": "command", "command": cmd }] }]);
    serde_json::json!({ "hooks": {
        "SessionStart": hook, "UserPromptSubmit": hook, "PostToolUse": tools,
        "PreCompact": hook, "PostCompact": hook,
        "DirectoryAdded": hook,
    } }).to_string()
}

pub fn run(state_path: &Path) -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let Ok(event) = serde_json::from_str::<Value>(&input) else { return Ok(()) };

    // Serialize concurrent hooks (parallel tool calls) on a lock file.
    let lock = OpenOptions::new().create(true).append(true).open(state_path.with_extension("lock"))?;
    lock.lock()?;

    let mut state = HookState::load(state_path);
    apply(&mut state, &event, now());
    write_atomic(state_path, &serde_json::to_vec(&state)?);
    Ok(())
}

pub(crate) fn apply(state: &mut HookState, event: &Value, now: f64) {
    match event["hook_event_name"].as_str().unwrap_or("") {
        "SessionStart" => {
            if event["agent_id"].is_string() {
                return; // a subagent, not the session we resume
            }
            if let Some(id) = event["session_id"].as_str() {
                state.session_id = Some(id.to_string());
            }
            state.session_starts += 1;
        }
        "DirectoryAdded" => {
            if let Some(dir) = event["directory"].as_str()
                && !state.added_dirs.iter().any(|d| d == dir)
            {
                state.added_dirs.push(dir.to_string());
            }
        }
        "UserPromptSubmit" => state.last_submit = now,
        "PreCompact" => state.compact_started = now,
        "PostCompact" => state.compact_ended = now,
        "PostToolUse" => {
            let input = &event["tool_input"];
            let response = &event["tool_response"];
            match event["tool_name"].as_str().unwrap_or("") {
                "ScheduleWakeup" => {
                    state.wake_until = if input["stop"].as_bool() == Some(true) {
                        0.0
                    } else {
                        now + input["delaySeconds"].as_f64().unwrap_or(3600.0) + 300.0
                    };
                }
                "CronCreate" => {
                    let id = response["id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("unknown-{}", now as u64));
                    // Recurring jobs hold the session until CronDelete or
                    // their expiry; one-shot jobs delete themselves on firing.
                    let until = if input["recurring"].as_bool().unwrap_or(true) {
                        now + CRON_MAX_SECS
                    } else {
                        response["nextFireAt"].as_f64().map(|ms| ms / 1000.0 + 600.0).unwrap_or(now + 86400.0)
                    };
                    state.crons.insert(id, until);
                }
                "CronDelete" => {
                    if let Some(id) = input["id"].as_str() {
                        state.crons.remove(id);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tracks_cron_lifecycle() {
        let mut s = HookState::default();
        apply(&mut s, &json!({"hook_event_name": "PostToolUse", "tool_name": "CronCreate",
            "tool_input": {"cron": "17 * * * *", "prompt": "x", "recurring": true},
            "tool_response": {"id": "0cbf77f2", "nextFireAt": 1.0}}), 1000.0);
        assert!(s.crons.contains_key("0cbf77f2"));
        apply(&mut s, &json!({"hook_event_name": "PostToolUse", "tool_name": "CronDelete",
            "tool_input": {"id": "0cbf77f2"}}), 1001.0);
        assert!(s.crons.is_empty());
    }

    #[test]
    fn one_shot_cron_expires_after_firing() {
        let mut s = HookState::default();
        apply(&mut s, &json!({"hook_event_name": "PostToolUse", "tool_name": "CronCreate",
            "tool_input": {"recurring": false}, "tool_response": {"id": "a", "nextFireAt": 2_000_000.0}}), 1000.0);
        assert_eq!(s.crons["a"], 2000.0 + 600.0);
    }

    #[test]
    fn loop_wakeup_and_stop() {
        let mut s = HookState::default();
        apply(&mut s, &json!({"hook_event_name": "PostToolUse", "tool_name": "ScheduleWakeup",
            "tool_input": {"delaySeconds": 1200}}), 1000.0);
        assert_eq!(s.wake_until, 2500.0);
        apply(&mut s, &json!({"hook_event_name": "PostToolUse", "tool_name": "ScheduleWakeup",
            "tool_input": {"stop": true}}), 1100.0);
        assert_eq!(s.wake_until, 0.0);
    }

    #[test]
    fn compaction_window() {
        let mut s = HookState::default();
        apply(&mut s, &json!({"hook_event_name": "PreCompact", "trigger": "idle"}), 100.0);
        assert!(s.compacting(110.0));
        apply(&mut s, &json!({"hook_event_name": "PostCompact", "trigger": "idle"}), 130.0);
        assert!(s.compacting(135.0)); // still drawing the compacted view
        assert!(!s.compacting(150.0));
    }

    #[test]
    fn session_start_ignores_subagents() {
        let mut s = HookState::default();
        apply(&mut s, &json!({"hook_event_name": "SessionStart", "session_id": "main"}), 0.0);
        apply(&mut s, &json!({"hook_event_name": "SessionStart", "session_id": "sub", "agent_id": "x"}), 0.0);
        assert_eq!(s.session_id.as_deref(), Some("main"));
        assert_eq!(s.session_starts, 1);
    }
}
