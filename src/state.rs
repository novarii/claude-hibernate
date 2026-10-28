//! Config file, per-session state files, and the hook-owned timer state.
//!
//! Each running wrapper owns two files in the runtime dir:
//!   <pid>.json        written by the wrapper: what `status` shows
//!   <pid>.hooks.json  written by `claude-hibernate hook`: session id and
//!                     pending /loop and cron timers

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("claude-hibernate-{}", unsafe { libc::getuid() })));
    let dir = if base.ends_with(format!("claude-hibernate-{}", unsafe { libc::getuid() })) {
        base
    } else {
        base.join("claude-hibernate")
    };
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("claude-hibernate/config.toml")
}

pub fn shim_dir() -> PathBuf {
    home().join(".local/share/claude-hibernate/bin")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Hibernate after this many minutes without screen or keyboard activity.
    pub idle_minutes: f64,
    /// Set to false to keep wrapping sessions but never hibernate them.
    pub enabled: bool,
    /// Sessions whose working directory starts with one of these never sleep.
    pub exclude_dirs: Vec<String>,
    /// The real claude binary; found on PATH when unset.
    pub real_claude: Option<String>,
    /// Compact a big sleeping conversation before its prompt cache expires,
    /// as Claude does to an idle one it still has open.
    pub compact_while_asleep: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { idle_minutes: 30.0, enabled: true, exclude_dirs: vec![], real_claude: None, compact_while_asleep: true }
    }
}

impl Config {
    /// Read on every check so edits apply to running sessions.
    pub fn load() -> Self {
        let mut cfg: Config = fs::read_to_string(config_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        if let Some(secs) = std::env::var("CLAUDE_HIBERNATE_IDLE_SECS").ok().and_then(|v| v.parse::<f64>().ok()) {
            cfg.idle_minutes = secs / 60.0;
        }
        if std::env::var("CLAUDE_HIBERNATE_DISABLE").as_deref() == Ok("1") {
            cfg.enabled = false;
        }
        cfg
    }

    pub fn excludes(&self, cwd: &Path) -> bool {
        self.exclude_dirs.iter().any(|d| {
            let d = d.strip_prefix("~/").map(|rest| home().join(rest)).unwrap_or_else(|| d.into());
            cwd.starts_with(d)
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HookState {
    pub session_id: Option<String>,
    /// Bumped on every SessionStart, so a resume can tell when claude is up.
    pub session_starts: u64,
    /// When the last prompt was submitted.
    pub last_submit: f64,
    /// A /loop wake-up is due before this time.
    pub wake_until: f64,
    /// Cron job id -> time after which it can no longer fire.
    pub crons: BTreeMap<String, f64>,
    /// Claude's last conversation compaction (PreCompact, PostCompact).
    pub compact_started: f64,
    pub compact_ended: f64,
    /// Directories added with /add-dir, which a resume doesn't restore.
    pub added_dirs: Vec<String>,
}

impl HookState {
    pub fn path(pid: u32) -> PathBuf {
        runtime_dir().join(format!("{pid}.hooks.json"))
    }

    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Claude is compacting the conversation, or just finished and is
    /// redrawing it. Seconds after PostCompact that still count.
    pub fn compacting(&self, now: f64) -> bool {
        const SETTLE_SECS: f64 = 10.0;
        self.compact_started > 0.0
            && (self.compact_ended < self.compact_started || now - self.compact_ended < SETTLE_SECS)
    }

    pub fn pending_timers(&self) -> Vec<String> {
        let now = now();
        let mut out = vec![];
        if self.wake_until > now {
            out.push("/loop wake-up".to_string());
        }
        for (id, until) in &self.crons {
            if *until > now {
                out.push(format!("cron {id}"));
            }
        }
        out
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Status {
    pub pid: u32,
    pub cwd: String,
    pub session_id: Option<String>,
    pub title: String,
    pub asleep: bool,
    /// When the session last went idle (awake) or went to sleep (asleep).
    pub since: f64,
    /// Why it is being kept awake, if it is idle but not eligible.
    pub held_by: Option<String>,
    /// The last `claude-hibernate sleep` request that was refused: (when, why).
    pub refused: Option<(f64, String)>,
    pub child_pid: Option<u32>,
    /// claude's RSS just before it was stopped, for `status` savings.
    pub freed_kb: u64,
    pub herdr_pane: Option<String>,
}

impl Status {
    pub fn path(pid: u32) -> PathBuf {
        runtime_dir().join(format!("{pid}.json"))
    }

    pub fn save(&self) {
        write_atomic(&Self::path(self.pid), &serde_json::to_vec_pretty(self).unwrap_or_default());
    }

    pub fn load_all() -> Vec<Status> {
        let Ok(entries) = fs::read_dir(runtime_dir()) else { return vec![] };
        let mut out: Vec<Status> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".json") && !name.ends_with(".hooks.json")
            })
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str::<Status>(&s).ok())
            .filter(|s| nix::sys::signal::kill(nix::unistd::Pid::from_raw(s.pid as i32), None).is_ok())
            .collect();
        out.sort_by_key(|s| s.pid);
        out
    }
}

/// Where Claude Code keeps a session's conversation, if it has saved one.
pub fn transcript_path(session_id: &str) -> Option<PathBuf> {
    fs::read_dir(claude_dir().join("projects")).ok()?.flatten()
        .map(|d| d.path().join(format!("{session_id}.jsonl")))
        .find(|p| p.exists())
}

/// Claude Code's config directory.
pub fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude"))
}

/// The permission mode a session was last in. Claude records every change
/// (shift+tab included) but a resume starts in the mode its flags or
/// settings give, so the wrapper carries it over itself.
pub fn last_permission_mode(transcript: &Path, session_id: &str) -> Option<String> {
    use std::io::BufRead;
    const NEEDLE: &[u8] = b"\"type\":\"permission-mode\"";
    let mut mode = None;
    let mut line = vec![];
    let mut r = std::io::BufReader::new(File::open(transcript).ok()?);
    while r.read_until(b'\n', &mut line).ok()? > 0 {
        if line.windows(NEEDLE.len()).any(|w| w == NEEDLE)
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line)
            && v["sessionId"] == session_id
            && let Some(m) = v["permissionMode"].as_str()
        {
            mode = Some(m.to_string());
        }
        line.clear();
    }
    mode
}

/// What a running claude process reports about itself in Claude's session
/// registry (`<claude dir>/sessions/<pid>.json`).
pub fn claude_registry(pid: u32) -> Option<serde_json::Value> {
    let s = fs::read_to_string(claude_dir().join(format!("sessions/{pid}.json"))).ok()?;
    serde_json::from_str(&s).ok()
}

/// A live claude process other than `except` has this session open.
pub fn session_open_elsewhere(session_id: &str, except: Option<u32>) -> bool {
    let Ok(entries) = fs::read_dir(claude_dir().join("sessions")) else { return false };
    entries.flatten().filter_map(|e| fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .any(|v| {
            let pid = v["pid"].as_u64().unwrap_or(0) as u32;
            v["sessionId"] == session_id && pid > 0 && Some(pid) != except
                && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
        })
}

pub fn write_atomic(path: &Path, data: &[u8]) {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if let Ok(mut f) = File::create(&tmp)
        && f.write_all(data).is_ok() {
            let _ = fs::rename(&tmp, path);
            return;
        }
    let _ = fs::remove_file(&tmp);
}

pub fn cleanup(pid: u32) {
    let _ = fs::remove_file(Status::path(pid));
    let _ = fs::remove_file(HookState::path(pid));
    let _ = fs::remove_file(HookState::path(pid).with_extension("lock"));
}
