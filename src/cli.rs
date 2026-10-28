//! `claude-hibernate status | sleep | wake | install | uninstall | config`.

use std::os::unix::fs::symlink;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::args::find_real_claude;
use crate::herdr;
use crate::state::{Config, Status, config_path, now, shim_dir};

fn rss_kb(pid: u32) -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

fn ago(ts: f64) -> String {
    let s = (now() - ts).max(0.0) as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{}h", s / 86400, s % 86400 / 3600),
    }
}

fn mb(kb: u64) -> String {
    if kb >= 1024 * 1024 { format!("{:.1} GB", kb as f64 / 1048576.0) } else { format!("{} MB", kb / 1024) }
}

pub fn status(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&Status::load_all())?);
        return Ok(());
    }
    if args.iter().any(|a| a == "--watch") {
        return watch();
    }
    print!("{}", status_table());
    Ok(())
}

fn status_table() -> String {
    use std::fmt::Write;
    let sessions = Status::load_all();
    if sessions.is_empty() {
        return "no wrapped sessions running\n".into();
    }
    let mut out = String::new();
    let _ = writeln!(out, "{:>8}  {:<7} {:>7} {:>8}  {:<28} TITLE", "PID", "STATE", "FOR", "MEMORY", "HELD BY");
    let (mut asleep, mut freed, mut used) = (0, 0u64, 0u64);
    for s in &sessions {
        let wrapper = rss_kb(s.pid);
        let child = s.child_pid.map(rss_kb).unwrap_or(0);
        used += wrapper + child;
        let state = if s.asleep { "asleep" } else if s.held_by.is_some() { "held" } else { "awake" };
        if s.asleep {
            asleep += 1;
            freed += s.freed_kb;
        }
        let title: String = s.title.chars().take(50).collect();
        let _ = writeln!(
            out,
            "{:>8}  {:<7} {:>7} {:>8}  {:<28} {}",
            s.pid,
            state,
            ago(s.since),
            mb(wrapper + child),
            s.held_by.as_deref().unwrap_or("").chars().take(28).collect::<String>(),
            title
        );
    }
    let _ = writeln!(
        out,
        "\n{} sessions, {asleep} asleep · using {} · freed {}",
        sessions.len(),
        mb(used),
        mb(freed)
    );
    out
}

fn term_cols() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0;
    if ok { ws.ws_col as usize } else { 120 }
}

/// Redraw the table every 2 s until `q`, Esc or Ctrl+C.
fn watch() -> Result<()> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use nix::sys::termios::{self, SetArg};
    use std::io::Write;
    use std::os::fd::BorrowedFd;
    let stdin = unsafe { BorrowedFd::borrow_raw(0) };
    let saved = termios::tcgetattr(stdin).ok();
    if let Some(t) = &saved {
        let mut raw = t.clone();
        termios::cfmakeraw(&mut raw);
        let _ = termios::tcsetattr(stdin, SetArg::TCSANOW, &raw);
    }
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b[?1049h\x1b[?25l");
    let result = (|| -> Result<()> {
        loop {
            // Trim to the terminal width so long titles don't wrap and break the table.
            let cols = term_cols();
            let table: String = status_table().lines().map(|l| l.chars().take(cols).collect::<String>() + "\r\n").collect();
            write!(stdout, "\x1b[H\x1b[2J{table}\r\nq to close · refreshes every 2 s\r\n")?;
            stdout.flush()?;
            let mut fds = [PollFd::new(stdin, PollFlags::POLLIN)];
            if poll(&mut fds, PollTimeout::from(2000u16)).unwrap_or(0) > 0 {
                let mut buf = [0u8; 16];
                let n = nix::unistd::read(0, &mut buf).unwrap_or(0);
                if n == 0 || buf[..n].iter().any(|&b| matches!(b, b'q' | b'Q' | 3 | 27)) {
                    return Ok(());
                }
            }
        }
    })();
    let _ = write!(stdout, "\x1b[?25h\x1b[?1049l");
    let _ = stdout.flush();
    if let Some(t) = &saved {
        let _ = termios::tcsetattr(stdin, SetArg::TCSANOW, t);
    }
    result
}

fn targets(sel: &[String]) -> Result<Vec<Status>> {
    let all = Status::load_all();
    if sel.is_empty() {
        bail!("name a session: a PID, a session id prefix, or `all`");
    }
    if sel.iter().any(|s| s == "all") {
        return Ok(all);
    }
    // `--current` is the herdr pane this runs in (or the focused one, for plugin actions).
    let current = std::env::var("HERDR_PANE_ID").ok();
    let sel: Vec<String> = sel
        .iter()
        .map(|q| if q == "--current" { current.clone().unwrap_or_default() } else { q.clone() })
        .collect();
    let picked: Vec<Status> = all
        .into_iter()
        .filter(|s| {
            sel.iter().any(|q| {
                !q.is_empty()
                    && (q == &s.pid.to_string()
                        || s.herdr_pane.as_deref() == Some(q.as_str())
                        || s.session_id.as_deref().is_some_and(|id| id.starts_with(q.as_str())))
            })
        })
        .collect();
    if picked.is_empty() {
        bail!("no wrapped session matches {}", sel.join(" "));
    }
    Ok(picked)
}

/// SIGUSR1 asks a wrapper to hibernate now (still subject to its guards);
/// SIGUSR2 wakes it.
pub fn signal(sel: &[String], wake: bool) -> Result<()> {
    let found = match targets(sel) {
        Ok(found) => found,
        Err(e) => {
            herdr::notify("claude-hibernate", &format!("{e:#}"));
            return Err(e);
        }
    };
    let mut asked = vec![];
    for s in found {
        if wake != s.asleep {
            continue;
        }
        kill(Pid::from_raw(s.pid as i32), if wake { Signal::SIGUSR2 } else { Signal::SIGUSR1 })?;
        println!("{} {} {}", if wake { "waking" } else { "asked to sleep:" }, s.pid, s.title);
        asked.push(s.pid);
    }
    if wake {
        let body = match asked.len() {
            0 => "nothing asleep there".to_string(),
            1 => "resuming the session".to_string(),
            n => format!("resuming {n} sessions"),
        };
        herdr::notify("claude-hibernate", &body);
        return Ok(());
    }
    let at = now();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let mut held = vec![];
    for s in targets(sel)?.into_iter().filter(|s| asked.contains(&s.pid) && !s.asleep) {
        if let Some((_, why)) = s.refused.filter(|(when, _)| *when >= at - 1.0) {
            println!("  {} stays awake: {why}", s.pid);
            held.push(why);
        }
    }
    let sleeping = asked.len() - held.len();
    let body = if asked.is_empty() {
        "nothing awake to hibernate".to_string()
    } else if held.is_empty() {
        format!("hibernating {sleeping} session{}", if sleeping == 1 { "" } else { "s" })
    } else if asked.len() == 1 {
        format!("stays awake: {}", held[0])
    } else {
        format!("hibernating {sleeping}; {} stay awake ({})", held.len(), held.join("; "))
    };
    herdr::notify("claude-hibernate", &body);
    Ok(())
}

pub fn install(write_rc: bool) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let mut cfg = Config::load();
    let real = find_real_claude(cfg.real_claude.as_deref()).context("could not find the real `claude` binary; install Claude Code first")?;
    let dir = shim_dir();
    std::fs::create_dir_all(&dir)?;
    let shim = dir.join("claude");
    let _ = std::fs::remove_file(&shim);
    symlink(&exe, &shim)?;
    println!("shim       {} -> {}", shim.display(), exe.display());
    println!("real       {}", real.display());

    if cfg.real_claude.is_none() {
        cfg.real_claude = Some(real.to_string_lossy().into_owned());
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        if !path.exists() {
            std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
            println!("config     {}", path.display());
        }
    }

    let line = format!("export PATH=\"{}:$PATH\"", dir.display()).replace(&std::env::var("HOME").unwrap_or_default(), "$HOME");
    let on_path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("claude"))
        .find(|p| p.exists())
        .is_some_and(|p| p == shim);
    if on_path {
        println!("\n`claude` already resolves to the shim. New sessions will hibernate when idle.");
    } else if write_rc {
        let rc = rc_file().context("unknown shell; add the PATH line yourself")?;
        let existing = std::fs::read_to_string(&rc).unwrap_or_default();
        if !existing.contains(&line) {
            std::fs::write(&rc, format!("{existing}\n# claude-hibernate\n{line}\n"))?;
        }
        println!("\nadded to {}. Open a new shell (or `source` it) and start `claude`.", rc.display());
    } else {
        println!("\nPut the shim ahead of the real claude on your PATH:\n\n    {line}\n");
        if let Some(rc) = rc_file() {
            println!("Or run `claude-hibernate install --write-rc` to append it to {}.", rc.display());
        }
    }
    report_running();
    Ok(())
}

/// Sessions started before the shim keep running without it: say so, and
/// inside herdr show what `migrate` would move.
fn report_running() {
    let n = unwrapped_claudes().len();
    if n == 0 {
        return;
    }
    println!("\n{n} Claude session{} already running without claude-hibernate.", if n == 1 { " is" } else { "s are" });
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        println!("They'll hibernate once restarted with `claude` (`claude --resume` picks up where each left off).");
        return;
    }
    println!("`claude-hibernate migrate` restarts them under it in place, keeping each conversation:\n");
    match crate::migrate::preview() {
        Ok(0) => println!("\nNone can move right now; run `claude-hibernate migrate` once they're idle."),
        Ok(_) => println!("\nTo move them: claude-hibernate migrate"),
        Err(e) => println!("(could not list them: {e:#})"),
    }
}

/// Interactive claude processes not under the wrapper. A wrapped claude
/// carries our hook in its `--settings`, or runs under our PTY leader; a
/// sleeping wrapper has no claude at all and is known by its status file.
pub fn unwrapped_claudes() -> Vec<u32> {
    let Ok(out) = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid=,tty=,args="]).output() else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let procs: Vec<(u32, u32, &str, &str)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (pid, ppid, tty) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?);
            let args = l.split_once(tty).map_or("", |(_, rest)| rest.trim());
            Some((pid, ppid, tty, args))
        })
        .collect();
    let wrappers: Vec<u32> = Status::load_all().iter().map(|s| s.pid).collect();
    let args_of = |pid: u32| procs.iter().find(|p| p.0 == pid).map_or("", |p| p.3);
    procs
        .iter()
        .filter(|&&(pid, ppid, tty, args)| {
            let argv0 = args.split_whitespace().next().unwrap_or("");
            argv0.rsplit('/').next() == Some("claude")
                && !matches!(tty, "?" | "??" | "-") // headless: claude -p from scripts
                && !args.split_whitespace().any(|a| a == "-p" || a == "--print")
                && !args.contains("claude-hibernate")
                && !args_of(ppid).contains("claude-hibernate")
                && !wrappers.contains(&pid)
                && !procs.iter().any(|c| c.1 == pid && c.3.contains("__pty-leader"))
        })
        .map(|p| p.0)
        .collect()
}

fn rc_file() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.ends_with("zsh") {
        Some(home.join(".zshrc"))
    } else if shell.ends_with("bash") {
        Some(home.join(".bashrc"))
    } else if shell.ends_with("fish") {
        None
    } else {
        Some(home.join(".profile"))
    }
}

pub fn uninstall() -> Result<()> {
    let shim = shim_dir().join("claude");
    if std::fs::remove_file(&shim).is_ok() {
        println!("removed {}", shim.display());
    }
    println!("Running sessions keep their wrapper until they exit. Remove the PATH line from your shell rc if you added one.");
    Ok(())
}

pub fn show_config() -> Result<()> {
    let path = config_path();
    println!("# {}{}", path.display(), if path.exists() { "" } else { " (not created; defaults)" });
    print!("{}", toml::to_string_pretty(&Config::load())?);
    Ok(())
}
