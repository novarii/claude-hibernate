//! The wrapper: runs claude on a PTY we own, proxies the terminal, and swaps
//! claude for a ~2 MB sleep screen when the session has been idle long enough.
//!
//!   awake ──idle + guards pass──▶ SIGTERM claude ──exit──▶ asleep
//!     ▲                                                     │
//!     └── ready (SessionStart + quiet) ◀── waking ◀── input or SIGUSR2
//!
//! Input that wakes the session is held and written to claude once it is
//! ready, so nothing typed during the resume is lost.

use std::hash::{Hash, Hasher};
use std::num::NonZeroU16;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::{self, SetArg, Termios};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::state::{Config, HookState, Status, now};
use crate::{args, detect, herdr, hook};

nix::ioctl_read_bad!(get_winsize, libc::TIOCGWINSZ, Winsize);
nix::ioctl_write_ptr_bad!(set_winsize, libc::TIOCSWINSZ, Winsize);

const TICK: Duration = Duration::from_millis(1000);
/// Output must be quiet this long after SessionStart before replaying input.
const READY_QUIET: Duration = Duration::from_millis(400);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const KILL_GRACE: Duration = Duration::from_secs(15);
/// Turn off the terminal modes claude may have left on: kitty keyboard,
/// bracketed paste, mouse reporting, alternate screen; show the cursor.
const RESET_MODES: &str = "\x1b[<u\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1049l\x1b[?25h\x1b[0m";

/// Keeps the window title claude sets (OSC 0/2); vt100 reports it by callback.
#[derive(Default)]
struct TitleCb {
    title: String,
}

impl vt100::Callbacks for TitleCb {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

fn new_parser(ws: &Winsize) -> vt100::Parser<TitleCb> {
    vt100::Parser::new_with_callbacks(nz(ws.ws_row), nz(ws.ws_col), 0, TitleCb::default())
}

struct Signals {
    winch: Arc<AtomicBool>,
    chld: Arc<AtomicBool>,
    term: Arc<AtomicBool>,
    sleep_now: Arc<AtomicBool>,
    wake_now: Arc<AtomicBool>,
    pipe: UnixStream,
}

impl Signals {
    fn install() -> Result<Self> {
        let (rx, tx) = UnixStream::pair()?;
        rx.set_nonblocking(true)?;
        tx.set_nonblocking(true)?;
        let flag = |sigs: &[i32]| -> Result<Arc<AtomicBool>> {
            let f = Arc::new(AtomicBool::new(false));
            for &s in sigs {
                signal_hook::flag::register(s, f.clone())?;
                signal_hook::low_level::pipe::register(s, tx.try_clone()?)?;
            }
            Ok(f)
        };
        Ok(Self {
            winch: flag(&[libc::SIGWINCH])?,
            chld: flag(&[libc::SIGCHLD])?,
            term: flag(&[libc::SIGTERM, libc::SIGHUP])?,
            sleep_now: flag(&[libc::SIGUSR1])?,
            wake_now: flag(&[libc::SIGUSR2])?,
            pipe: rx,
        })
    }

    fn drain(&self) {
        let mut buf = [0u8; 64];
        while nix::unistd::read(self.pipe.as_raw_fd(), &mut buf).is_ok_and(|n| n > 0) {}
    }
}

fn take(f: &AtomicBool) -> bool {
    f.swap(false, Ordering::SeqCst)
}

struct Child {
    /// The PTY leader (our direct child).
    pid: Pid,
    /// claude itself, the leader's child; looked up once it exists.
    claude: Option<Pid>,
    master: OwnedFd,
    /// Set once we asked it to exit for hibernation.
    stopping_since: Option<Instant>,
}

struct Waking {
    since: Instant,
    starts_before: u64,
    held_input: Vec<u8>,
}

pub struct Session {
    real: PathBuf,
    args: Vec<String>,
    exe: String,
    me: u32,
    cwd: PathBuf,
    track_timers: bool,
    orig_termios: Termios,
    parser: vt100::Parser<TitleCb>,
    child: Option<Child>,
    waking: Option<Waking>,
    asleep: bool,
    last_activity: Instant,
    last_output: Instant,
    screen_hash: u64,
    /// When the user last typed; compared with the last submitted prompt to
    /// tell whether text may be sitting unsent in the prompt box.
    last_input_at: f64,
    status: Status,
    cfg: Config,
}

fn stdin() -> BorrowedFd<'static> {
    unsafe { BorrowedFd::borrow_raw(0) }
}

fn write_all(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        match nix::unistd::write(unsafe { BorrowedFd::borrow_raw(fd) }, data) {
            Ok(n) => data = &data[n..],
            Err(Errno::EAGAIN | Errno::EINTR) => {
                let mut fds = [PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, PollFlags::POLLOUT)];
                let _ = poll(&mut fds, PollTimeout::from(100u16));
            }
            Err(_) => return,
        }
    }
}

fn out(data: &[u8]) {
    write_all(1, data);
}

fn winsize() -> Winsize {
    let mut ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe {
        let _ = get_winsize(0, &mut ws);
    }
    ws
}

fn rss_kb(pid: Pid) -> u64 {
    Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

fn child_of(parent: Pid) -> Option<Pid> {
    let o = Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output().ok()?;
    String::from_utf8_lossy(&o.stdout).lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (pid, ppid) = (it.next()?, it.next()?);
        (ppid == parent.to_string()).then(|| pid.parse().ok().map(Pid::from_raw)).flatten()
    })
}

/// Background Bash/Monitor commands run as `bash -c source
/// .../shell-snapshots/...` under claude; MCP servers do not.
fn has_tool_processes(pid: Pid) -> bool {
    let Ok(o) = Command::new("ps").args(["-A", "-o", "ppid=,args="]).output() else { return false };
    String::from_utf8_lossy(&o.stdout).lines().any(|l| {
        let l = l.trim_start();
        let (ppid, args) = l.split_once(' ').unwrap_or((l, ""));
        ppid == pid.to_string() && args.contains("shell-snapshots")
    })
}

impl Session {
    pub fn new(real: PathBuf, args: Vec<String>) -> Result<Self> {
        let ws = winsize();
        let exe = std::env::current_exe()?.canonicalize()?.to_string_lossy().into_owned();
        let me = std::process::id();
        let cwd = std::env::current_dir().unwrap_or_default();
        Ok(Self {
            track_timers: !args::has_own_settings(&args),
            real,
            args,
            exe,
            me,
            orig_termios: termios::tcgetattr(stdin()).context("stdin is not a terminal")?,
            parser: new_parser(&ws),
            child: None,
            waking: None,
            asleep: false,
            last_activity: Instant::now(),
            last_output: Instant::now(),
            screen_hash: 0,
            last_input_at: 0.0,
            status: Status {
                pid: me,
                cwd: cwd.to_string_lossy().into_owned(),
                since: now(),
                herdr_pane: herdr::pane(),
                ..Default::default()
            },
            cwd,
            cfg: Config::load(),
        })
    }

    fn hooks(&self) -> HookState {
        HookState::load(&HookState::path(self.me))
    }

    fn raw_mode(&self) {
        let mut raw = self.orig_termios.clone();
        termios::cfmakeraw(&mut raw);
        let _ = termios::tcsetattr(stdin(), SetArg::TCSANOW, &raw);
    }

    fn cooked_mode(&self) {
        let _ = termios::tcsetattr(stdin(), SetArg::TCSANOW, &self.orig_termios);
    }

    fn spawn(&mut self, claude_args: &[String]) -> Result<()> {
        let ws = winsize();
        let pty = openpty(Some(&ws), None)?;
        let mut argv: Vec<String> = vec![];
        if self.track_timers {
            argv.push("--settings".into());
            argv.push(hook::settings_json(&self.exe, &HookState::path(self.me)));
        }
        argv.extend_from_slice(claude_args);
        let slave = pty.slave;
        // A leader process owns the PTY session and runs claude beneath it;
        // see leader.rs for why claude cannot lead the session itself.
        let mut cmd = Command::new(&self.exe);
        cmd.arg("__pty-leader")
            .arg(&self.real)
            .args(&argv)
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave.try_clone()?)
            .env("CLAUDE_HIBERNATE_PID", self.me.to_string());
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()?;
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().with_context(|| format!("starting {}", self.real.display()))?;
        drop(slave);
        nix::fcntl::fcntl(pty.master.as_raw_fd(), nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK))?;
        self.parser = new_parser(&ws);
        self.child = Some(Child { pid: Pid::from_raw(child.id() as i32), claude: None, master: pty.master, stopping_since: None });
        self.status.child_pid = None;
        Ok(())
    }

    pub fn run(mut self) -> Result<i32> {
        let signals = Signals::install()?;
        detect::refresh_in_background();
        self.raw_mode();
        let first_args = self.args.clone();
        if let Err(e) = self.spawn(&first_args) {
            self.cooked_mode();
            return Err(e);
        }
        let code = self.event_loop(&signals);
        out(RESET_MODES.as_bytes());
        self.cooked_mode();
        crate::state::cleanup(self.me);
        Ok(code)
    }

    fn event_loop(&mut self, signals: &Signals) -> i32 {
        let mut buf = vec![0u8; 64 * 1024];
        let mut last_tick = Instant::now();
        loop {
            let master_fd = self.child.as_ref().map(|c| c.master.as_raw_fd());
            let mut fds = vec![
                PollFd::new(stdin(), PollFlags::POLLIN),
                PollFd::new(signals.pipe.as_fd(), PollFlags::POLLIN),
            ];
            if let Some(fd) = master_fd {
                fds.push(PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, PollFlags::POLLIN));
            }
            let _ = poll(&mut fds, PollTimeout::from(TICK.as_millis() as u16));
            let ready = |i: usize| fds.get(i).and_then(|f| f.revents()).is_some_and(|r| !r.is_empty());
            let (stdin_ready, sig_ready, master_ready) = (ready(0), ready(1), ready(2));
            drop(fds);

            if sig_ready {
                signals.drain();
            }
            if master_ready {
                self.pump_output(&mut buf);
            }
            if stdin_ready {
                match nix::unistd::read(0, &mut buf) {
                    Ok(0) | Err(Errno::EIO) => return self.shutdown(Signal::SIGHUP),
                    Ok(n) => self.on_input(&buf[..n]),
                    Err(_) => {}
                }
            }
            if take(&signals.winch) {
                self.resize();
            }
            if take(&signals.term) {
                return self.shutdown(Signal::SIGTERM);
            }
            if take(&signals.chld)
                && let Some(code) = self.reap() {
                    return code;
                }
            if take(&signals.wake_now) && self.asleep {
                self.wake(vec![]);
            }
            if take(&signals.sleep_now) {
                self.try_hibernate(true);
            }
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                self.tick();
            }
        }
    }

    fn pump_output(&mut self, buf: &mut [u8]) {
        let Some(child) = &self.child else { return };
        loop {
            match nix::unistd::read(child.master.as_raw_fd(), buf) {
                Ok(0) | Err(_) => return, // EAGAIN, or EIO once claude exits
                Ok(n) => {
                    self.last_output = Instant::now();
                    if child.stopping_since.is_some() {
                        continue; // claude's goodbye text; the sleep screen replaces it
                    }
                    self.parser.process(&buf[..n]);
                    out(&buf[..n]);
                }
            }
        }
    }

    fn on_input(&mut self, data: &[u8]) {
        if self.asleep {
            self.wake(data.to_vec());
        } else if let Some(w) = &mut self.waking {
            w.held_input.extend_from_slice(data);
        } else if let Some(child) = &self.child
            && child.stopping_since.is_none() {
                write_all(child.master.as_raw_fd(), data);
                self.last_activity = Instant::now();
                self.last_input_at = now();
            }
    }

    fn resize(&mut self) {
        let ws = winsize();
        self.parser.screen_mut().set_size(nz(ws.ws_row), nz(ws.ws_col));
        if let Some(c) = &self.child {
            unsafe {
                let _ = set_winsize(c.master.as_raw_fd(), &ws);
            }
        } else if self.asleep {
            self.draw_sleep_screen();
        }
    }

    /// Returns the exit code when the session is over.
    fn reap(&mut self) -> Option<i32> {
        let child = self.child.as_ref()?;
        let status = waitpid(child.pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED)).ok()?;
        match status {
            WaitStatus::Stopped(..) => {
                self.suspend();
                None
            }
            WaitStatus::Exited(_, code) => self.child_gone(code),
            WaitStatus::Signaled(_, sig, _) => self.child_gone(128 + sig as i32),
            _ => None,
        }
    }

    fn child_gone(&mut self, code: i32) -> Option<i32> {
        let mut buf = vec![0u8; 64 * 1024];
        self.pump_output(&mut buf); // flush claude's last words to the terminal
        let hibernating = self.child.as_ref().is_some_and(|c| c.stopping_since.is_some());
        self.child = None;
        self.status.child_pid = None;
        if !hibernating {
            return Some(code); // the user quit claude
        }
        self.asleep = true;
        self.status.asleep = true;
        self.status.since = now();
        self.draw_sleep_screen();
        herdr::mark_asleep();
        self.status.save();
        None
    }

    /// Claude handled Ctrl+Z and stopped, so the PTY leader stopped. Stop too
    /// so the shell gets the terminal back; on `fg`, resume claude.
    fn suspend(&mut self) {
        self.cooked_mode();
        let _ = kill(Pid::from_raw(0), Signal::SIGSTOP); // our process group
        // Continued by `fg`.
        self.raw_mode();
        if let Some(c) = &self.child {
            let _ = kill(c.pid, Signal::SIGCONT);
            // Nudge a full redraw: claude repaints on a size change.
            let real = winsize();
            let shrunk = Winsize { ws_row: real.ws_row.saturating_sub(1).max(1), ..real };
            unsafe {
                let _ = set_winsize(c.master.as_raw_fd(), &shrunk);
                let _ = set_winsize(c.master.as_raw_fd(), &real);
            }
        }
    }

    fn shutdown(&mut self, sig: Signal) -> i32 {
        let Some(pid) = self.child.as_ref().map(|c| c.pid) else { return 128 + sig as i32 };
        let _ = kill(pid, sig);
        let mut buf = vec![0u8; 64 * 1024];
        let deadline = Instant::now() + KILL_GRACE;
        while Instant::now() < deadline {
            // Pass claude's cleanup output through so it can restore the terminal.
            self.pump_output(&mut buf);
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    self.pump_output(&mut buf);
                    return 128 + sig as i32;
                }
            }
        }
        let _ = kill(pid, Signal::SIGKILL);
        128 + sig as i32
    }

    /// The visible screen as plain text, the input herdr's rules expect.
    fn screen_text(&self) -> String {
        let s = self.parser.screen();
        s.rows(0, s.size().1.get()).map(|r| r.trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn detect(&self) -> detect::Detection {
        detect::claude().detect(&self.screen_text(), &self.parser.callbacks().title)
    }

    fn title(&self) -> String {
        let t = self.parser.callbacks().title.trim();
        // Drop claude's status glyph (spinner, ✳) before the session title.
        let t = t.trim_start_matches(|c: char| !c.is_alphanumeric()).trim();
        if t.is_empty() { self.status.session_id.clone().unwrap_or_default() } else { t.to_string() }
    }

    fn tick(&mut self) {
        if let Some(w) = &self.waking {
            let started = self.hooks().session_starts > w.starts_before || !self.track_timers;
            let quiet = self.last_output.elapsed() >= READY_QUIET;
            // Ready once claude has drawn its prompt box and is not busy. An
            // empty screen also classifies as idle, so the box is required.
            let text = self.screen_text();
            let drawn = detect::prompt_box_body(&text).is_some() && self.detect().state == detect::State::Idle;
            if (started && quiet && drawn) || w.since.elapsed() >= READY_TIMEOUT {
                let w = self.waking.take().unwrap();
                if let Some(c) = &self.child {
                    replay(c.master.as_raw_fd(), &w.held_input);
                }
                if !w.held_input.is_empty() {
                    self.last_input_at = now();
                }
                self.last_activity = Instant::now();
                herdr::mark_awake();
            }
            return;
        }
        if let Some(c) = &mut self.child
            && let Some(since) = c.stopping_since {
                if since.elapsed() >= KILL_GRACE {
                    let _ = kill(c.pid, Signal::SIGKILL);
                }
                return;
            }
        if self.asleep || self.child.is_none() {
            return;
        }
        if let Some(c) = &mut self.child
            && c.claude.is_none()
        {
            c.claude = child_of(c.pid);
            self.status.child_pid = c.claude.map(|p| p.as_raw() as u32);
        }

        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.parser.screen().contents().hash(&mut h);
        let hash = h.finish();
        if hash != self.screen_hash {
            self.screen_hash = hash;
            self.last_activity = self.last_activity.max(self.last_output);
            self.status.held_by = None; // re-evaluated once idle again
        }
        if let Some(id) = self.hooks().session_id {
            self.status.session_id = Some(id);
        }
        self.status.title = self.title();
        self.cfg = Config::load();
        let idle = self.last_activity.elapsed().as_secs_f64();
        if idle >= self.cfg.idle_minutes * 60.0 {
            self.try_hibernate(false);
        }
        self.status.since = now() - idle;
        self.status.save();
    }

    /// Why the session cannot sleep right now, if anything.
    fn held_by(&self) -> Option<String> {
        if !self.cfg.enabled {
            return Some("disabled in config".into());
        }
        if self.cfg.excludes(&self.cwd) {
            return Some("excluded directory".into());
        }
        if !self.track_timers {
            return Some("--settings passed; cannot track timers".into());
        }
        let hooks = self.hooks();
        if hooks.session_id.is_none() {
            return Some("no session id yet".into());
        }
        let timers = hooks.pending_timers();
        if !timers.is_empty() {
            return Some(timers.join(", "));
        }
        let d = self.detect();
        if d.state != detect::State::Idle {
            let state = format!("{:?}", d.state).to_lowercase();
            return Some(match d.rule {
                Some(rule) => format!("claude {state} ({rule})"),
                None => format!("claude {state}"),
            });
        }
        // Typed since the last submitted prompt: unless the prompt box is
        // visibly empty, assume a draft is waiting.
        if self.last_input_at > hooks.last_submit {
            let text = self.screen_text();
            let empty = detect::prompt_box_body(&text)
                .is_some_and(|body| body.chars().all(|c| c.is_whitespace() || c == '❯'));
            if !empty {
                return Some("unsent text in the prompt".into());
            }
        }
        if self.child.as_ref().and_then(|c| c.claude).is_some_and(has_tool_processes) {
            return Some("background shell".into());
        }
        None
    }

    fn try_hibernate(&mut self, forced: bool) {
        if self.asleep || self.waking.is_some() {
            return;
        }
        let Some(child) = &self.child else { return };
        if child.stopping_since.is_some() {
            return;
        }
        self.status.held_by = self.held_by();
        if self.status.held_by.is_some() {
            if forced {
                self.status.refused = Some((now(), self.status.held_by.clone().unwrap_or_default()));
                self.status.save();
            }
            return;
        }
        let pid = child.pid;
        self.status.freed_kb = rss_kb(pid) + child.claude.map(rss_kb).unwrap_or(0);
        self.status.title = self.title();
        if let Some(c) = &mut self.child {
            c.stopping_since = Some(Instant::now());
        }
        let _ = kill(pid, Signal::SIGTERM);
    }

    fn draw_sleep_screen(&self) {
        let title = &self.status.title;
        let mut s = String::new();
        // Undo the modes claude left on: kitty keyboard, bracketed paste,
        // mouse reporting, alternate screen; then show the cursor and clear.
        s.push_str(RESET_MODES);
        s.push_str("\x1b[2J\x1b[H");
        s.push_str(&format!("\x1b]0;💤 {title}\x07"));
        let since = chrono_like(self.status.since);
        let freed = if self.status.freed_kb > 0 { format!(" · freed {} MB", self.status.freed_kb / 1024) } else { String::new() };
        let cols = winsize().ws_col as usize;
        for line in [
            String::new(),
            format!("  💤  hibernated — {title}"),
            String::new(),
            format!("  session {}", self.status.session_id.as_deref().unwrap_or("?")),
            format!("  asleep since {since}{freed}"),
            String::new(),
            "  press any key to resume".to_string(),
        ] {
            s.extend(line.chars().take(cols));
            s.push_str("\r\n");
        }
        out(s.as_bytes());
    }

    fn wake(&mut self, held_input: Vec<u8>) {
        let Some(id) = self.status.session_id.clone() else { return };
        self.asleep = false;
        self.status.asleep = false;
        self.status.freed_kb = 0;
        // Herdr reads Claude's state off the screen: a braille spinner title
        // and a `✻ …` status line count as working, so `herdr agent prompt
        // --wait` sees activity while the session resumes.
        out(format!("\x1b]0;⠂ {}\x07\x1b[2J\x1b[H\r\n✻ Resuming session…\r\n", self.status.title).as_bytes());
        let starts_before = self.hooks().session_starts;
        let mut argv = args::resume_args(&self.args);
        argv.push("--resume".into());
        argv.push(id);
        if let Err(e) = self.spawn(&argv) {
            out(format!("\r\nclaude-hibernate: could not resume: {e:#}\r\n").as_bytes());
            self.asleep = true;
            return;
        }
        self.waking = Some(Waking { since: Instant::now(), starts_before, held_input });
        self.status.since = now();
        self.status.save();
    }
}

/// Enter as a terminal sends it: CR, or kitty-protocol CSI 13 u.
const ENTERS: &[&[u8]] = &[b"\r", b"\x1b[13u", b"\x1b[13;1u"];

/// Write input held during a resume. Claude reads text and Enter arriving in
/// one chunk as a paste, where Enter is a newline rather than submit, so each
/// Enter goes in its own write after a pause, as if typed.
fn replay(fd: RawFd, data: &[u8]) {
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        if let Some(enter) = ENTERS.iter().find(|e| data[i..].starts_with(e)) {
            write_all(fd, &data[start..i]);
            std::thread::sleep(Duration::from_millis(250));
            write_all(fd, enter);
            i += enter.len();
            start = i;
        } else {
            i += 1;
        }
    }
    write_all(fd, &data[start..]);
}

/// Local "Oct 23 14:05" without pulling in a date crate.
fn chrono_like(ts: f64) -> String {
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    let mut buf = [0u8; 32];
    let fmt = c"%b %d %H:%M";
    let n = unsafe { libc::strftime(buf.as_mut_ptr() as *mut _, buf.len(), fmt.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}
