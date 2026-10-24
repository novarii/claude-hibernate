//! `claude-hibernate __pty-leader <claude> [args...]`: the session leader on
//! the PTY the wrapper owns. It runs claude in its own process group so that
//! group is not orphaned: the kernel discards a self-sent SIGTSTP in an
//! orphaned group, which would break Claude's Ctrl+Z. When claude stops, the
//! leader stops too, which the wrapper sees and turns into a real suspend.

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

use nix::sys::signal::{SigHandler, Signal, kill, signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, getpid, setpgid, tcsetpgrp};

static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, sig) };
    }
}

pub fn run(real: &str, args: &[String]) -> i32 {
    let stdin = unsafe { std::os::fd::BorrowedFd::borrow_raw(0) };
    unsafe {
        // tcsetpgrp from a background group would otherwise stop us.
        let _ = signal(Signal::SIGTTOU, SigHandler::SigIgn);
    }
    let mut cmd = Command::new(real);
    cmd.arg0("claude").args(args);
    unsafe {
        cmd.pre_exec(|| {
            setpgid(Pid::from_raw(0), Pid::from_raw(0))?;
            // Undo our SIGTTOU ignore; ignored dispositions survive exec.
            let _ = signal(Signal::SIGTTOU, SigHandler::SigDfl);
            Ok(())
        });
    }
    let child = match cmd.spawn() {
        Ok(c) => Pid::from_raw(c.id() as i32),
        Err(e) => {
            eprintln!("claude-hibernate: starting {real}: {e}");
            return 127;
        }
    };
    CHILD.store(child.as_raw(), Ordering::SeqCst);
    let _ = setpgid(child, child);
    let _ = tcsetpgrp(stdin, child);
    for sig in [Signal::SIGTERM, Signal::SIGHUP] {
        unsafe {
            let _ = signal(sig, SigHandler::Handler(forward));
        }
    }
    loop {
        match waitpid(child, Some(WaitPidFlag::WUNTRACED)) {
            Ok(WaitStatus::Stopped(..)) => {
                let _ = kill(getpid(), Signal::SIGSTOP);
                // Continued by the wrapper after `fg`.
                let _ = tcsetpgrp(stdin, child);
                let _ = kill(child, Signal::SIGCONT);
            }
            Ok(WaitStatus::Exited(_, code)) => return code,
            Ok(WaitStatus::Signaled(_, sig, _)) => return 128 + sig as i32,
            Err(nix::errno::Errno::EINTR) | Ok(_) => continue,
            Err(_) => return 1,
        }
    }
}
