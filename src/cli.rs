//! `claude-hibernate status | sleep | wake | install | uninstall | config`.

use std::os::unix::fs::symlink;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::args::find_real_claude;
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

pub fn status(json: bool) -> Result<()> {
    let sessions = Status::load_all();
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }
    if sessions.is_empty() {
        println!("no wrapped sessions running");
        return Ok(());
    }
    println!("{:>8}  {:<7} {:>7} {:>8}  {:<28} TITLE", "PID", "STATE", "FOR", "MEMORY", "HELD BY");
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
        println!(
            "{:>8}  {:<7} {:>7} {:>8}  {:<28} {}",
            s.pid,
            state,
            ago(s.since),
            mb(wrapper + child),
            s.held_by.as_deref().unwrap_or("").chars().take(28).collect::<String>(),
            title
        );
    }
    println!(
        "\n{} sessions, {asleep} asleep · using {} · freed {}",
        sessions.len(),
        mb(used),
        mb(freed)
    );
    Ok(())
}

fn targets(sel: &[String]) -> Result<Vec<Status>> {
    let all = Status::load_all();
    if sel.is_empty() {
        bail!("name a session: a PID, a session id prefix, or `all`");
    }
    if sel.iter().any(|s| s == "all") {
        return Ok(all);
    }
    let picked: Vec<Status> = all
        .into_iter()
        .filter(|s| {
            sel.iter().any(|q| q == &s.pid.to_string() || s.session_id.as_deref().is_some_and(|id| id.starts_with(q.as_str())))
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
    for s in targets(sel)? {
        if wake != s.asleep {
            continue;
        }
        kill(Pid::from_raw(s.pid as i32), if wake { Signal::SIGUSR2 } else { Signal::SIGUSR1 })?;
        println!("{} {} {}", if wake { "waking" } else { "hibernating" }, s.pid, s.title);
    }
    if !wake {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        for s in targets(sel)?.into_iter().filter(|s| !s.asleep) {
            if let Some(why) = s.held_by {
                println!("  {} stays awake: {why}", s.pid);
            }
        }
    }
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
        return Ok(());
    }
    let rc = rc_file();
    if write_rc {
        let rc = rc.context("unknown shell; add the PATH line yourself")?;
        let existing = std::fs::read_to_string(&rc).unwrap_or_default();
        if !existing.contains(&line) {
            std::fs::write(&rc, format!("{existing}\n# claude-hibernate\n{line}\n"))?;
        }
        println!("\nadded to {}. Open a new shell (or `source` it) and start `claude`.", rc.display());
    } else {
        println!("\nPut the shim ahead of the real claude on your PATH:\n\n    {line}\n");
        if let Some(rc) = rc {
            println!("Or run `claude-hibernate install --write-rc` to append it to {}.", rc.display());
        }
    }
    Ok(())
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
