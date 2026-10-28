//! Compacting a sleeping session before its prompt cache expires.
//!
//! Claude compacts a big idle conversation on its own about 54 minutes after
//! its last request ("idle compaction"), while the prompt cache is still warm,
//! so the next message doesn't reprocess everything. A hibernated session has
//! no claude process to do that, so the wrapper does it instead: a headless
//! `claude -p --resume <id> /compact` on the same schedule and conditions.
//!
//! Those conditions are Claude's, read the way Claude reads them: its
//! server-side `tengu_sunny_locket` flag (cached in `~/.claude.json`) turns
//! idle compaction on and sets the token minimum and timing; the
//! `idleCompaction` setting and `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS`
//! override it. When Claude wouldn't compact, neither do we.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

use crate::state::{claude_dir, runtime_dir, transcript_path};

/// The feature flag behind Claude's idle compaction for the one-hour cache.
const FLAG: &str = "tengu_sunny_locket";
/// Claude only compacts a conversation on the one-hour prompt cache.
const CACHE_TTL_SECS: f64 = 60.0 * 60.0;
/// Too close to expiry to start: `claude -p` takes a few seconds to send
/// its first request.
const START_MARGIN_SECS: f64 = 2.0 * 60.0;
/// Claude's defaults and limits for the flag's values.
const MIN_TOKENS: u64 = 200_000;
const MIN_TOKENS_FLOOR: u64 = 100_000;
const FIRE_AT: f64 = 0.9;
const FIRE_AT_RANGE: (f64, f64) = (0.5, 0.95);
/// How much of the transcript's end to read for the last request.
const TAIL_BYTES: u64 = 4 << 20;

#[derive(Debug, Clone)]
pub struct Plan {
    /// Start compacting at this time, and not after `deadline`.
    pub at: f64,
    pub deadline: f64,
    pub tokens: u64,
    /// Where the session last ran (a worktree it entered, say): its working
    /// directory is part of the system prompt, so of the cached prefix.
    pub cwd: PathBuf,
}

/// When to compact this sleeping session, if Claude would have compacted it
/// had it stayed open.
pub fn plan(session_id: &str, cwd: &Path) -> Option<Plan> {
    let rules = Rules::load(cwd)?;
    let last = last_request(&transcript_path(session_id)?)?;
    if !last.one_hour_cache || last.compacted_since {
        return None;
    }
    // For testing: smaller conversations, sooner.
    let min_tokens = env_u64("CLAUDE_HIBERNATE_COMPACT_MIN_TOKENS").unwrap_or(rules.min_tokens);
    if last.tokens < min_tokens {
        return None;
    }
    let at = match env_u64("CLAUDE_HIBERNATE_COMPACT_AFTER_SECS") {
        Some(secs) => last.at + secs as f64,
        None => last.at + CACHE_TTL_SECS * rules.fire_at,
    };
    let deadline = (last.at + CACHE_TTL_SECS - START_MARGIN_SECS).max(at + START_MARGIN_SECS);
    let cwd = last.cwd.map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(|| cwd.to_path_buf());
    Some(Plan { at, deadline, tokens: last.tokens, cwd })
}

/// Claude's idle compaction settings on this machine.
#[derive(Debug, PartialEq)]
struct Rules {
    min_tokens: u64,
    /// Compact at this fraction of the cache's lifetime.
    fire_at: f64,
}

impl Rules {
    /// `None` when Claude would not compact an idle conversation.
    fn load(cwd: &Path) -> Option<Rules> {
        if truthy("DISABLE_COMPACT") || truthy("DISABLE_AUTO_COMPACT") || settings_off(cwd) {
            return None;
        }
        let state = std::fs::read_to_string(claude_json()).ok()?;
        let flag = serde_json::from_str::<Value>(&state).ok()?["cachedGrowthBookFeatures"][FLAG].take();
        Rules::from_flag(&flag, env_u64("CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS"))
    }

    /// As Claude reads the flag: off unless its mode compacts; the env var
    /// beats the flag's minimum, with a floor; the fraction is clamped.
    fn from_flag(flag: &Value, env_min: Option<u64>) -> Option<Rules> {
        if !matches!(flag["mode"].as_str(), Some("compact" | "compact_on_return")) {
            return None;
        }
        let min_tokens = env_min.or(flag["minTokens"].as_u64()).map_or(MIN_TOKENS, |n| n.max(MIN_TOKENS_FLOOR));
        let fire_at = flag["fireAtFraction"].as_f64().map_or(FIRE_AT, |f| f.clamp(FIRE_AT_RANGE.0, FIRE_AT_RANGE.1));
        Some(Rules { min_tokens, fire_at })
    }
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Claude's state file, where it caches its feature flags.
fn claude_json() -> PathBuf {
    match config_dir() {
        Some(dir) => dir.join(".claude.json"),
        None => home().join(".claude.json"),
    }
}

/// Start `claude -p --resume <id> /compact` in the background. Holds a
/// box-wide lock so sleeping sessions compact one at a time; `None` when
/// another one is compacting.
pub fn start(real: &Path, flags: &[String], session_id: &str, cwd: &Path) -> Option<(Child, File)> {
    let lock = File::create(runtime_dir().join("compact.lock")).ok()?;
    lock.try_lock().ok()?;
    let mut cmd = Command::new(real);
    cmd.arg("-p")
        .args(flags)
        .args(["--resume", session_id, "/compact"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Not the pane's agent: keep herdr's hooks and our own away from it.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HERDR_") {
            cmd.env_remove(key);
        }
    }
    cmd.env_remove("CLAUDE_HIBERNATE_PID");
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid()?; // no controlling terminal
            Ok(())
        });
    }
    let child = cmd.spawn().ok()?;
    Some((child, lock))
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false"))
}

/// `idleCompaction` or `autoCompactEnabled` turned off in any settings file.
fn settings_off(cwd: &Path) -> bool {
    [
        claude_dir().join("settings.json"),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
        PathBuf::from("/etc/claude-code/managed-settings.json"),
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json"),
    ]
    .iter()
    .filter_map(|p| std::fs::read_to_string(p).ok())
    .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
    .any(|v| v["idleCompaction"] == Value::Bool(false) || v["autoCompactEnabled"] == Value::Bool(false))
}

#[derive(Debug, PartialEq)]
struct LastRequest {
    at: f64,
    /// Context size of the last request: what a compaction would read.
    tokens: u64,
    /// Claude wrote this conversation to the one-hour cache (not five minutes).
    one_hour_cache: bool,
    /// Already compacted after that request.
    compacted_since: bool,
    cwd: Option<String>,
}

fn last_request(path: &Path) -> Option<LastRequest> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).ok()?;
    let mut bytes = vec![];
    f.read_to_end(&mut bytes).ok()?;
    // The cut may land inside a line or character; that line is skipped.
    let buf = String::from_utf8_lossy(&bytes);
    parse_tail(&buf)
}

fn parse_tail(text: &str) -> Option<LastRequest> {
    let mut last: Option<LastRequest> = None;
    let mut one_hour = false;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let at = v["timestamp"].as_str().and_then(parse_time);
        if v["type"] == "system" && v["subtype"] == "compact_boundary" {
            if let Some(l) = &mut last {
                l.compacted_since = true;
            }
            continue;
        }
        if v["type"] != "assistant" || v["isSidechain"] == Value::Bool(true) {
            continue;
        }
        let u = &v["message"]["usage"];
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        let tokens = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens") + n("output_tokens");
        let (Some(at), true) = (at, tokens > 0) else { continue };
        let c = &u["cache_creation"];
        if c["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0) > 0 {
            one_hour = true;
        } else if c["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0) > 0 {
            one_hour = false;
        }
        let cwd = v["cwd"].as_str().map(str::to_string);
        last = Some(LastRequest { at, tokens, one_hour_cache: false, compacted_since: false, cwd });
    }
    last.map(|l| LastRequest { one_hour_cache: one_hour, ..l })
}

/// "2026-10-27T00:42:18.372Z" as Unix seconds.
fn parse_time(s: &str) -> Option<f64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i32>().ok();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = num(0..4)? - 1900;
    tm.tm_mon = num(5..7)? - 1;
    tm.tm_mday = num(8..10)?;
    tm.tm_hour = num(11..13)?;
    tm.tm_min = num(14..16)?;
    tm.tm_sec = num(17..19)?;
    let frac = s.get(19..).and_then(|r| r.strip_prefix('.')).map_or(0.0, |r| {
        let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
        format!("0.{digits}").parse().unwrap_or(0.0)
    });
    Some(unsafe { libc::timegm(&mut tm) } as f64 + frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(ts: &str, read: u64, h1: u64, m5: u64) -> String {
        serde_json::json!({"type": "assistant", "timestamp": ts, "message": {"usage": {
            "input_tokens": 2, "cache_read_input_tokens": read, "cache_creation_input_tokens": h1 + m5,
            "output_tokens": 100, "cache_creation": {"ephemeral_1h_input_tokens": h1, "ephemeral_5m_input_tokens": m5}}}})
        .to_string()
    }

    #[test]
    fn follows_claudes_flag() {
        use serde_json::json;
        assert_eq!(Rules::from_flag(&Value::Null, None), None); // not rolled out: Claude never compacts
        assert_eq!(Rules::from_flag(&json!({"mode": "log"}), None), None);
        assert_eq!(Rules::from_flag(&json!({"mode": "compact"}), None), Some(Rules { min_tokens: 200_000, fire_at: 0.9 }));
        let flag = json!({"mode": "compact", "minTokens": 300_000, "fireAtFraction": 0.99});
        assert_eq!(Rules::from_flag(&flag, None), Some(Rules { min_tokens: 300_000, fire_at: 0.95 }));
        assert_eq!(Rules::from_flag(&flag, Some(50_000)).unwrap().min_tokens, 100_000);
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_time("1970-01-01T00:01:00.500Z"), Some(60.5));
        assert_eq!(parse_time("2026-10-27T00:42:18Z"), Some(1793061738.0));
    }

    #[test]
    fn finds_the_last_request() {
        let t = [assistant("1970-01-01T00:00:10Z", 1000, 50, 0), assistant("1970-01-01T00:00:20Z", 250_000, 0, 0)].join("\n");
        assert_eq!(parse_tail(&t), Some(LastRequest { at: 20.0, tokens: 250_102, one_hour_cache: true, compacted_since: false, cwd: None }));
    }

    #[test]
    fn five_minute_cache_and_compactions() {
        let t = [assistant("1970-01-01T00:00:10Z", 1000, 0, 50)].join("\n");
        assert!(!parse_tail(&t).unwrap().one_hour_cache);
        let boundary = r#"{"type":"system","subtype":"compact_boundary","timestamp":"1970-01-01T00:00:30Z"}"#;
        let t = [assistant("1970-01-01T00:00:10Z", 1000, 50, 0), boundary.to_string()].join("\n");
        assert!(parse_tail(&t).unwrap().compacted_since);
    }
}
