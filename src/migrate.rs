//! `claude-hibernate migrate`: move Claude Code agents that are already
//! running in herdr panes under the wrapper, without losing their sessions.
//!
//! For each claude agent that is safe to restart (idle, empty prompt, no
//! background shells, no pending /loop or cron job, running directly under the
//! pane's shell), it stops claude with SIGTERM (the conversation is already on
//! disk), runs `cd <cwd> && claude <flags> --resume <id>` at the shell prompt,
//! and puts the agent's name back. Anything unexpected leaves the pane alone
//! and is reported.
//!
//! It never types into a running claude: with the agents panel holding focus
//! (which the screen does not show), typed text such as `/exit` becomes a new
//! background agent task, and `/exit` can also stop at a feedback-draft dialog.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::args::{resume_args, shell_quote};
use crate::detect;
use crate::session::has_tool_processes;
use crate::state::{HookState, now, transcript_path};

struct Opts {
    dry_run: bool,
    confirm: bool,
    only: Vec<String>,
    exclude: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts> {
    let mut o = Opts { dry_run: false, confirm: false, only: vec![], exclude: vec![] };
    let mut list: Option<&mut Vec<String>> = None;
    for a in args {
        match a.as_str() {
            "--dry-run" => o.dry_run = true,
            "--confirm" => o.confirm = true,
            "--only" => list = Some(&mut o.only),
            "--exclude" => list = Some(&mut o.exclude),
            s if s.starts_with('-') => bail!("unknown option {s}"),
            pane => match list.as_deref_mut() {
                Some(l) => l.push(pane.to_string()),
                None => bail!("pane ids follow --only or --exclude"),
            },
        }
    }
    Ok(o)
}

// ---------------------------------------------------------------- herdr calls

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into())
}

fn herdr(args: &[&str]) -> Result<String> {
    let out = Command::new(herdr_bin()).args(args).stdin(Stdio::null()).output().context("running herdr")?;
    if !out.status.success() {
        bail!("herdr {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn herdr_json(args: &[&str]) -> Result<Value> {
    let v: Value = serde_json::from_str(&herdr(args)?).context("herdr returned non-JSON")?;
    Ok(v["result"].clone())
}

fn pane_text(pane: &str) -> String {
    herdr(&["pane", "read", pane, "--source", "visible"]).unwrap_or_default()
}

// ---------------------------------------------------------------- processes

fn ppid(pid: u64) -> Option<u64> {
    let out = Command::new("ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn alive(pid: u64) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// A foreground process that is already a hibernating wrapper: this binary
/// (whatever its argv0), or the Python prototype it replaced.
fn is_wrapper(p: &Value) -> bool {
    let argv: Vec<&str> = p["argv"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let argv0 = argv.first().copied().unwrap_or("");
    let base = argv0.rsplit('/').next().unwrap_or("");
    if base.starts_with("python") || argv.iter().any(|a| a.contains("claude-hibernate")) {
        return true;
    }
    // After an upgrade the old binary shows as "claude-hibernate (deleted)".
    let exe = p["pid"].as_u64().and_then(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).ok());
    exe.is_some_and(|e| e.to_string_lossy().trim_end_matches(" (deleted)").ends_with("/claude-hibernate"))
        || std::fs::canonicalize(argv0).ok().is_some_and(|e| e.file_name().is_some_and(|n| n == "claude-hibernate"))
}

fn is_claude(p: &Value) -> bool {
    let argv0 = p["argv"][0].as_str().unwrap_or("");
    argv0.rsplit('/').next() == Some("claude") && !is_wrapper(p)
}

// ---------------------------------------------------------------- transcript timers

/// Seconds since the epoch for an ISO-8601 UTC timestamp ("2026-10-23T06:34:50.396Z").
fn iso_epoch(ts: &str) -> Option<f64> {
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.trim_end_matches('Z');
    let mut t = time.split(':');
    let (hh, mm) = (t.next()?.parse::<f64>().ok()?, t.next()?.parse::<f64>().ok()?);
    let ss: f64 = t.next()?.parse().ok()?;
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days as f64 * 86400.0 + hh * 3600.0 + mm * 60.0 + ss)
}

/// Replay the transcript's ScheduleWakeup / CronCreate / CronDelete calls
/// through the same rules the live hook uses.
fn pending_timers(path: &PathBuf) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec!["transcript unreadable".into()] };
    let mut state = HookState::default();
    let mut uses: std::collections::HashMap<String, (String, Value, f64)> = Default::default();
    for line in text.lines() {
        if !line.contains("ScheduleWakeup") && !line.contains("Cron") {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        // Only the conversation itself; request snapshots repeat old history.
        if !matches!(ev["type"].as_str(), Some("user" | "assistant")) {
            continue;
        }
        let at = ev["timestamp"].as_str().and_then(iso_epoch).unwrap_or_else(now);
        let Some(content) = ev["message"]["content"].as_array() else { continue };
        for c in content {
            match c["type"].as_str() {
                Some("tool_use") => {
                    let name = c["name"].as_str().unwrap_or("");
                    if matches!(name, "ScheduleWakeup" | "CronCreate" | "CronDelete") {
                        let id = c["id"].as_str().unwrap_or("").to_string();
                        uses.insert(id, (name.to_string(), c["input"].clone(), at));
                    }
                }
                Some("tool_result") => {
                    let Some((name, input, used_at)) = c["tool_use_id"].as_str().and_then(|id| uses.get(id)).cloned() else {
                        continue;
                    };
                    // The transcript keeps the result as text; the job id is its first 8-hex word.
                    let text = c["content"].to_string();
                    let id = text
                        .split(|ch: char| !ch.is_ascii_hexdigit())
                        .find(|w| w.len() == 8)
                        .map(str::to_string);
                    let response = match id {
                        Some(id) => serde_json::json!({ "id": id }),
                        None => Value::Null,
                    };
                    let event = serde_json::json!({
                        "hook_event_name": "PostToolUse", "tool_name": name,
                        "tool_input": input, "tool_response": response,
                    });
                    crate::hook::apply(&mut state, &event, used_at);
                }
                _ => {}
            }
        }
    }
    state.pending_timers()
}

// ---------------------------------------------------------------- the check

struct Plan {
    pane: String,
    session_id: String,
    pid: u64,
    argv: Vec<String>,
    cwd: String,
    name: Option<String>,
    title: String,
}

enum Verdict {
    Move(Plan),
    Skip(String),
    Quiet, // not a claude agent; not worth a line
}

fn check(agent: &Value) -> Verdict {
    let pane = agent["pane_id"].as_str().unwrap_or("").to_string();
    if agent["agent"].as_str() != Some("claude") {
        return Verdict::Quiet;
    }
    let status = agent["agent_status"].as_str().unwrap_or("");
    if !matches!(status, "idle" | "done") {
        return Verdict::Skip(status.to_string());
    }
    let Some(session_id) = agent["agent_session"]["value"].as_str().map(str::to_string) else {
        return Verdict::Skip("no session id yet".into());
    };
    let info = match herdr_json(&["pane", "process-info", "--pane", &pane]) {
        Ok(v) => v["process_info"].clone(),
        Err(e) => return Verdict::Skip(format!("{e:#}")),
    };
    let procs = info["foreground_processes"].as_array().cloned().unwrap_or_default();
    if procs.iter().any(is_wrapper) {
        return Verdict::Skip("already hibernating".into());
    }
    let claude: Vec<&Value> = procs.iter().filter(|p| is_claude(p)).collect();
    let [proc_] = claude.as_slice() else {
        return Verdict::Skip("unexpected foreground processes".into());
    };
    let pid = proc_["pid"].as_u64().unwrap_or(0);
    if info["shell_pid"].as_u64().is_none() || ppid(pid) != info["shell_pid"].as_u64() {
        return Verdict::Skip("claude is not running directly under the pane's shell".into());
    }
    let screen = pane_text(&pane);
    let title = agent["terminal_title"].as_str().unwrap_or("");
    if screen.contains("enter to return") {
        return Verdict::Skip("the agents panel has keyboard focus".into());
    }
    let state = detect::claude().detect(&screen, title).state;
    if state != detect::State::Idle {
        return Verdict::Skip(format!("claude looks {state:?}").to_lowercase());
    }
    match detect::prompt_box_body(&screen) {
        None => return Verdict::Skip("no prompt box on screen".into()),
        Some(body) if !detect::prompt_is_empty(body) => {
            return Verdict::Skip("unsent text in the prompt".into());
        }
        _ => {}
    }
    if has_tool_processes(nix::unistd::Pid::from_raw(pid as i32)) {
        return Verdict::Skip("background shell running".into());
    }
    let Some(path) = transcript_path(&session_id) else {
        return Verdict::Skip("no saved conversation yet".into());
    };
    let timers = pending_timers(&path);
    if !timers.is_empty() {
        return Verdict::Skip(timers.join(", "));
    }
    let argv = proc_["argv"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let cwd = proc_["cwd"].as_str().map(str::to_string)
        .or_else(|| std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned()));
    let Some(cwd) = cwd else { return Verdict::Skip("working directory unknown".into()) };
    Verdict::Move(Plan {
        pane, session_id, pid, argv, cwd,
        name: agent["name"].as_str().map(str::to_string),
        title: agent["terminal_title_stripped"].as_str().unwrap_or("").to_string(),
    })
}

// ---------------------------------------------------------------- the move

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    done()
}

/// The `claude` shim to relaunch through: `claude-hibernate install`'s, or
/// CLAUDE_HIBERNATE_SHIM. It must be a `claude` that resolves to this binary,
/// so herdr still sees a `claude` agent and the session is actually wrapped.
/// A bare `claude` would run whatever each pane's PATH finds first.
fn shim() -> Result<PathBuf> {
    let path = std::env::var_os("CLAUDE_HIBERNATE_SHIM").map(PathBuf::from).unwrap_or_else(|| crate::state::shim_dir().join("claude"));
    let me = std::env::current_exe()?.canonicalize()?;
    let ok = path.file_name().is_some_and(|n| n == "claude") && path.canonicalize().ok().as_ref() == Some(&me);
    if !ok {
        bail!("the claude-hibernate shim is not installed at {}; run `claude-hibernate install` first", path.display());
    }
    Ok(path)
}

fn relaunch_command(p: &Plan, shim: &std::path::Path) -> String {
    let flags: Vec<String> = resume_args(p.argv.get(1..).unwrap_or(&[])).iter().map(|f| shell_quote(f)).collect();
    let mut cmd = format!("cd {} && {}", shell_quote(&p.cwd), shell_quote(&shim.to_string_lossy()));
    for f in flags {
        cmd.push(' ');
        cmd.push_str(&f);
    }
    cmd.push_str(" --resume ");
    cmd.push_str(&p.session_id);
    cmd
}

fn migrate_one(p: &Plan, shim: &std::path::Path) -> Result<()> {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(p.pid as i32), nix::sys::signal::Signal::SIGTERM)
        .context("stopping claude")?;
    if !wait_until(Duration::from_secs(20), || !alive(p.pid)) {
        bail!("claude did not exit on SIGTERM; left it running");
    }
    // Only type the relaunch once the shell itself is back in the foreground.
    let shell_back = wait_until(Duration::from_secs(10), || {
        herdr_json(&["pane", "process-info", "--pane", &p.pane])
            .map(|v| {
                let info = &v["process_info"];
                let fg = info["foreground_processes"].as_array().cloned().unwrap_or_default();
                fg.len() == 1 && fg[0]["pid"].as_u64() == info["shell_pid"].as_u64()
            })
            .unwrap_or(false)
    });
    if !shell_back {
        bail!("the shell did not come back after claude exited; run `claude --resume {}` there", p.session_id);
    }
    std::thread::sleep(Duration::from_millis(500));
    let cmd = relaunch_command(p, shim);
    herdr(&["pane", "run", &p.pane, &cmd])?;
    let _ = herdr(&["pane", "wait-output", &p.pane, "--match", "❯", "--source", "visible", "--timeout", "60000"]);
    std::thread::sleep(Duration::from_secs(3));
    let agent = herdr_json(&["agent", "get", &p.pane]).context("agent not detected after the restart")?;
    if agent["agent"]["agent"].as_str() != Some("claude") {
        bail!("no claude agent after `{cmd}`");
    }
    if let Some(name) = &p.name
        && agent["agent"]["name"].as_str() != Some(name)
    {
        herdr(&["agent", "rename", &p.pane, name]).context("restoring the agent name")?;
    }
    Ok(())
}

// ---------------------------------------------------------------- entry

pub fn run(args: &[String]) -> Result<i32> {
    let o = parse(args)?;
    let shim = match shim() {
        Ok(shim) => Some(shim),
        Err(e) if o.dry_run => {
            println!("note: {e:#}\n");
            None
        }
        Err(e) => return Err(e),
    };
    let me = std::env::var("HERDR_PANE_ID").ok();
    let agents = herdr_json(&["agent", "list"])?["agents"].as_array().cloned().unwrap_or_default();
    let mut plans = vec![];
    let mut skipped = 0;
    for a in &agents {
        let pane = a["pane_id"].as_str().unwrap_or("");
        if me.as_deref() == Some(pane) || o.exclude.iter().any(|x| x == pane) || (!o.only.is_empty() && !o.only.iter().any(|x| x == pane)) {
            continue;
        }
        let title: String = a["terminal_title_stripped"].as_str().unwrap_or("").chars().take(44).collect();
        match check(a) {
            Verdict::Move(p) => {
                println!("{:7} {pane:9} {title:44}", if o.dry_run || o.confirm { "would" } else { "move" });
                plans.push(p);
            }
            Verdict::Skip(why) => {
                skipped += 1;
                println!("skip    {pane:9} {title:44} {why}");
            }
            Verdict::Quiet => {}
        }
    }
    println!("\n{} to move, {skipped} skipped{}", plans.len(), if me.is_some() { " (this pane is never moved)" } else { "" });
    if o.dry_run || plans.is_empty() {
        return Ok(0);
    }
    if o.confirm {
        print!("Move {} agents under claude-hibernate? Each one exits and resumes in place. [y/N] ", plans.len());
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("nothing moved");
            return Ok(0);
        }
    }
    let (mut moved, mut failed) = (0, 0);
    for p in &plans {
        let title: String = p.title.chars().take(44).collect();
        let Some(shim) = shim.as_deref() else { break };
        match migrate_one(p, shim) {
            Ok(()) => {
                moved += 1;
                println!("ok      {:9} {title}", p.pane);
            }
            Err(e) => {
                failed += 1;
                println!("FAIL    {:9} {title:44} {e:#}", p.pane);
            }
        }
    }
    println!("\nmoved {moved}, failed {failed}, skipped {skipped}");
    Ok(if failed > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamps() {
        assert_eq!(iso_epoch("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(iso_epoch("2026-10-23T06:34:50.5Z"), Some(1792737290.5));
    }

    #[test]
    fn relaunch_keeps_flags_and_quotes_cwd() {
        let p = Plan {
            pane: "w1:p1".into(), session_id: "abc".into(), pid: 1, name: None, title: String::new(),
            argv: vec!["claude".into(), "--model".into(), "opus".into(), "--resume".into(), "old".into()],
            cwd: "/tmp/my dir".into(),
        };
        let shim = std::path::Path::new("/home/me/.local/share/claude-hibernate/bin/claude");
        assert_eq!(relaunch_command(&p, shim),
            "cd '/tmp/my dir' && /home/me/.local/share/claude-hibernate/bin/claude --model opus --resume abc");
    }

    #[test]
    fn options() {
        let o = parse(&["--dry-run".into(), "--only".into(), "w1:p1".into(), "w1:p2".into()]).unwrap();
        assert!(o.dry_run && o.only == ["w1:p1", "w1:p2"]);
        assert!(parse(&["w1:p1".into()]).is_err());
    }
}
