//! claude-hibernate: free the memory of idle Claude Code sessions.
//!
//! Invoked through a `claude` symlink it is the shim: interactive sessions run
//! under the hibernating wrapper, everything else execs the real claude.
//! Invoked as `claude-hibernate` it is the management CLI.

mod args;
mod cli;
mod herdr;
mod hook;
mod leader;
mod screen;
mod session;
mod state;

use std::os::unix::process::CommandExt;
use std::path::Path;

const USAGE: &str = "\
claude-hibernate: free the memory of idle Claude Code sessions

usage:
  claude [args...]                   via the shim: run Claude, hibernate when idle
  claude-hibernate run [args...]     same, without the shim
  claude-hibernate status [--json]   wrapped sessions, their state and memory
  claude-hibernate sleep <pid|id|all>  hibernate now (if nothing holds it awake)
  claude-hibernate wake <pid|id|all>   resume a hibernated session
  claude-hibernate install [--write-rc]  put the `claude` shim on PATH
  claude-hibernate uninstall
  claude-hibernate config            show the effective config
";

fn shim(args: Vec<String>) -> anyhow::Result<i32> {
    let cfg = state::Config::load();
    let real = args::find_real_claude(cfg.real_claude.as_deref())
        .ok_or_else(|| anyhow::anyhow!("claude-hibernate: real `claude` not found (set real_claude in {})", state::config_path().display()))?;
    if !args::wants_wrapping(&args) {
        let err = std::process::Command::new(&real).arg0("claude").args(&args).exec();
        anyhow::bail!("exec {}: {err}", real.display());
    }
    session::Session::new(real, args)?.run()
}

fn main() {
    let mut argv = std::env::args();
    let argv0 = argv.next().unwrap_or_default();
    let rest: Vec<String> = argv.collect();
    let invoked_as = Path::new(&argv0).file_name().and_then(|n| n.to_str()).unwrap_or("");

    let result = if invoked_as == "claude" {
        shim(rest)
    } else {
        let (cmd, tail) = rest.split_first().map(|(c, t)| (c.as_str(), t.to_vec())).unwrap_or(("", vec![]));
        match cmd {
            "run" => shim(tail),
            "__pty-leader" if !tail.is_empty() => Ok(leader::run(&tail[0], &tail[1..])),
            "hook" if tail.len() == 1 => hook::run(Path::new(&tail[0])).map(|_| 0),
            "status" => cli::status(tail.iter().any(|a| a == "--json")).map(|_| 0),
            "sleep" => cli::signal(&tail, false).map(|_| 0),
            "wake" => cli::signal(&tail, true).map(|_| 0),
            "install" => cli::install(tail.iter().any(|a| a == "--write-rc")).map(|_| 0),
            "uninstall" => cli::uninstall().map(|_| 0),
            "config" => cli::show_config().map(|_| 0),
            "-V" | "--version" | "version" => {
                println!("claude-hibernate {}", env!("CARGO_PKG_VERSION"));
                Ok(0)
            }
            _ => {
                print!("{USAGE}");
                Ok(if cmd.is_empty() || cmd == "help" || cmd == "-h" || cmd == "--help" { 0 } else { 2 })
            }
        }
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("claude-hibernate: {e:#}");
            std::process::exit(1);
        }
    }
}
