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
use crate::{args, compact, detect, herdr, hook};

nix::ioctl_read_bad!(get_winsize, libc::TIOCGWINSZ, Winsize);
nix::ioctl_write_ptr_bad!(set_winsize, libc::TIOCSWINSZ, Winsize);

const TICK: Duration = Duration::from_millis(1000);
/// How often to check whether a resuming claude is ready.
const WAKE_POLL: Duration = Duration::from_millis(50);
/// Output must be quiet this long after SessionStart before replaying input.
const READY_QUIET: Duration = Duration::from_millis(300);
/// Show claude even though it isn't ready yet, in case it is waiting on a
/// dialog the detection rules don't know.
const REVEAL_AFTER: Duration = Duration::from_secs(8);
/// Give up on replaying held input if claude isn't ready by then.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Screen changes closer together than this are one burst of activity.
const BURST_GAP: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(15);
/// Turn off the terminal modes claude may have left on: kitty keyboard,
/// bracketed paste, mouse and focus reporting, alternate screen; show the cursor.
const RESET_MODES: &str = "\x1b[<u\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?1049l\x1b[?25h\x1b[0m";

/// Input the terminal generates on its own: focus in and out. It must not
/// count as a keystroke (or the session wakes when its pane is focused).
fn without_terminal_reports(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(b"\x1b[I") || data[i..].starts_with(b"\x1b[O") {
            i += 3;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

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
    /// claude's output while it starts up and the held input is replayed.
    /// Shown in one go once it is ready, instead of the startup flicker and
    /// the input being retyped.
    output: Vec<u8>,
    /// claude's screen is showing: it wanted something from the user first.
    revealed: bool,
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
    /// `last_activity` before the latest burst of screen changes, and when
    /// the screen last changed.
    prev_activity: Instant,
    last_change: Instant,
    /// The compaction (its PreCompact time) already accounted for.
    compact_seen: f64,
    last_output: Instant,
    screen_hash: u64,
    /// When the user last typed; compared with the last submitted prompt to
    /// tell whether text may be sitting unsent in the prompt box.
    last_input_at: f64,
    status: Status,
    cfg: Config,
    /// While asleep: when to compact the conversation, the compaction
    /// running now (with its lock), and when it finished.
    compact_plan: Option<compact::Plan>,
    compactor: Option<(std::process::Child, std::fs::File)>,
    compacted_at: Option<f64>,
    /// The permission mode to wake in, read when it went to sleep.
    resume_mode: Option<String>,
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
pub(crate) fn has_tool_processes(pid: Pid) -> bool {
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
            prev_activity: Instant::now(),
            last_change: Instant::now(),
            compact_seen: 0.0,
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
            compact_plan: None,
            compactor: None,
            compacted_at: None,
            resume_mode: None,
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
            let timeout = if self.waking.is_some() { WAKE_POLL } else { TICK };
            let _ = poll(&mut fds, PollTimeout::from(timeout.as_millis() as u16));
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
            if self.waking.is_some() {
                self.check_ready();
            }
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                self.tick();
            }
        }
    }

    fn pump_output(&mut self, buf: &mut [u8]) {
        let Some(child) = &self.child else { return };
        let (fd, stopping) = (child.master.as_raw_fd(), child.stopping_since.is_some());
        loop {
            match nix::unistd::read(fd, buf) {
                Ok(0) | Err(_) => return, // EAGAIN, or EIO once claude exits
                Ok(n) => {
                    self.last_output = Instant::now();
                    if stopping {
                        continue; // claude's goodbye text; the sleep screen replaces it
                    }
                    self.parser.process(&buf[..n]);
                    match &mut self.waking {
                        Some(w) if !w.revealed => w.output.extend_from_slice(&buf[..n]),
                        _ => out(&buf[..n]),
                    }
                }
            }
        }
    }

    /// Stop holding claude's output back: clear the resume screen and show
    /// everything claude drew since it started.
    fn reveal(&mut self) {
        if let Some(w) = &mut self.waking
            && !w.revealed {
                w.revealed = true;
                out(b"\x1b[2J\x1b[H");
                out(&std::mem::take(&mut w.output));
                herdr::mark_awake();
            }
    }

    fn on_input(&mut self, data: &[u8]) {
        if self.asleep {
            let keys = without_terminal_reports(data);
            if !keys.is_empty() {
                self.wake(keys);
            }
        } else if let Some(w) = &mut self.waking
            && !w.revealed {
            w.held_input.extend_from_slice(data);
            self.draw_waking_screen();
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
        self.reveal(); // a resume that failed: show why
        self.waking = None;
        let hibernating = self.child.as_ref().is_some_and(|c| c.stopping_since.is_some());
        self.child = None;
        self.status.child_pid = None;
        if !hibernating {
            return Some(code); // the user quit claude
        }
        self.asleep = true;
        self.status.asleep = true;
        self.status.since = now();
        self.compacted_at = None;
        let id = self.status.session_id.clone().unwrap_or_default();
        self.resume_mode = crate::state::transcript_path(&id).and_then(|t| crate::state::last_permission_mode(&t, &id));
        self.compact_plan = compact::plan(&id, &self.cwd);
        if let Some(p) = &self.compact_plan {
            log(&format!("will compact {} tokens in {:.0}s", p.tokens, p.at - now()));
        }
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
        self.stop_compacting();
        let Some(pid) = self.child.as_ref().map(|c| c.pid) else { return 128 + sig as i32 };
        self.reveal();
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

    /// While resuming: once claude is ready, replay the held input, then
    /// show the result.
    fn check_ready(&mut self) {
        let Some(w) = &self.waking else { return };
        let elapsed = w.since.elapsed();
        if self.last_output.elapsed() < READY_QUIET && elapsed < READY_TIMEOUT {
            return;
        }
        let started = !self.track_timers || self.hooks().session_starts > w.starts_before;
        // Ready once claude has drawn its prompt box and is not busy. An
        // empty screen also classifies as idle, so the box is required.
        let text = self.screen_text();
        let state = self.detect().state;
        if started && state == detect::State::Idle && detect::prompt_box_body(&text).is_some() {
            log(&format!("ready after {:.2}s", elapsed.as_secs_f64()));
            let held = std::mem::take(&mut self.waking.as_mut().unwrap().held_input);
            if !held.is_empty() {
                self.replay(&held);
                // Let the echo of the last keys land before showing the screen.
                std::thread::sleep(Duration::from_millis(100));
                self.pump_output(&mut vec![0u8; 64 * 1024]);
                self.last_input_at = now();
            }
            self.finish_waking();
            return;
        }
        // A login, trust or other dialog came up first: show it and pass keys
        // through. The held input waits for the prompt box.
        if state == detect::State::Blocked || elapsed >= REVEAL_AFTER {
            if !w.revealed {
                log(&format!("revealed after {:.2}s: claude is {state:?}", elapsed.as_secs_f64()));
            }
            self.reveal();
        }
        // Never type held keys into whatever is on screen now.
        if elapsed >= READY_TIMEOUT {
            log("not ready in time; held input dropped");
            self.finish_waking();
        }
    }

    fn finish_waking(&mut self) {
        self.reveal();
        self.waking = None;
        self.last_activity = Instant::now();
    }

    fn tick(&mut self) {
        if self.waking.is_some() {
            return;
        }
        if let Some(c) = &mut self.child
            && let Some(since) = c.stopping_since {
                if since.elapsed() >= KILL_GRACE {
                    let _ = kill(c.pid, Signal::SIGKILL);
                }
                return;
            }
        if self.asleep {
            self.tick_asleep();
            return;
        }
        if self.child.is_none() {
            return;
        }
        if let Some(c) = &mut self.child
            && c.claude.is_none()
        {
            c.claude = child_of(c.pid);
            self.status.child_pid = c.claude.map(|p| p.as_raw() as u32);
        }

        // Claude compacting the conversation on its own, as it does to an
        // idle session before the prompt cache expires, is housekeeping, not
        // use: its screen changes don't restart the idle clock. Undo a bump
        // from its first frames if they were drawn before the hook ran.
        let hooks = self.hooks();
        let compacting = hooks.compacting(now());
        if hooks.compact_started > self.compact_seen {
            self.compact_seen = hooks.compact_started;
            let started = Instant::now() - Duration::from_secs_f64((now() - hooks.compact_started).max(0.0));
            if self.last_input_at < hooks.compact_started && self.last_activity + BURST_GAP > started {
                self.last_activity = self.prev_activity.min(self.last_activity);
            }
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // The footer under the prompt box (status line, notices) redraws on
        // its own; only the conversation and the prompt count as activity.
        detect::through_prompt_box(&self.parser.screen().contents()).hash(&mut h);
        let hash = h.finish();
        if hash != self.screen_hash {
            self.screen_hash = hash;
            if !compacting {
                if self.last_change.elapsed() >= BURST_GAP {
                    self.prev_activity = self.last_activity;
                }
                self.last_activity = self.last_activity.max(self.last_output);
            }
            self.last_change = Instant::now();
            self.status.held_by = None; // re-evaluated once idle again
        }
        if let Some(id) = hooks.session_id {
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
        let Some(id) = hooks.session_id.as_deref() else {
            return Some("no session id yet".into());
        };
        // Resuming needs the saved conversation; a session can also switch to
        // another one (Claude's agents panel) without a new SessionStart.
        if crate::state::transcript_path(id).is_none() {
            return Some("no saved conversation for this session".into());
        }
        let timers = hooks.pending_timers();
        if !timers.is_empty() {
            return Some(timers.join(", "));
        }
        // Remote Control reaches the session through this process, from
        // claude.ai or the phone; asleep, it would show as offline.
        let claude = self.child.as_ref().and_then(|c| c.claude);
        if claude.and_then(|p| crate::state::claude_registry(p.as_raw() as u32))
            .is_some_and(|r| r["bridgeSessionId"].is_string())
        {
            return Some("Remote Control is on".into());
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
                .is_some_and(detect::prompt_is_empty);
            if !empty {
                return Some("unsent text in the prompt".into());
            }
        }
        if self.child.as_ref().and_then(|c| c.claude).is_some_and(has_tool_processes) {
            return Some("background shell".into());
        }
        None
    }

    /// While asleep, compact the conversation when Claude would have, had it
    /// stayed open: one headless `/compact` before the prompt cache expires.
    fn tick_asleep(&mut self) {
        if let Some((child, _)) = &mut self.compactor {
            let Ok(Some(exit)) = child.try_wait() else { return };
            log(&format!("compaction finished: {exit}"));
            self.compactor = None; // releases the lock
            self.status.held_by = None;
            if exit.success() {
                self.compacted_at = Some(now());
                self.draw_sleep_screen();
            }
            self.status.save();
            return;
        }
        let Some(plan) = &self.compact_plan else { return };
        let t = now();
        if t < plan.at {
            return;
        }
        self.cfg = Config::load();
        if t > plan.deadline || !self.cfg.compact_while_asleep {
            self.compact_plan = None;
            return;
        }
        let Some(id) = self.status.session_id.clone() else { return };
        // Opened in another terminal meanwhile: it is that claude's to
        // compact. Or it was used there and closed: plan again.
        if crate::state::session_open_elsewhere(&id, None) {
            log("not compacting: the session is open elsewhere");
            self.compact_plan = None;
            return;
        }
        let cwd = match compact::plan(&id, &self.cwd) {
            Some(fresh) if fresh.at <= t => fresh.cwd,
            fresh => {
                self.compact_plan = fresh;
                return;
            }
        };
        // Another session may hold the lock; try again next tick.
        if let Some(running) = compact::start(&self.real, &args::resume_args(&self.args), &id, &cwd) {
            log("compacting");
            self.compactor = Some(running);
            self.compact_plan = None;
            self.status.held_by = Some("compacting".into());
            self.status.save();
        }
    }

    /// Waking or exiting: a compaction still running is abandoned. Claude
    /// writes the compacted conversation only once it has the summary.
    fn stop_compacting(&mut self) {
        let Some((mut child, _lock)) = self.compactor.take() else { return };
        let pid = Pid::from_raw(child.id() as i32);
        let _ = kill(pid, Signal::SIGTERM);
        for _ in 0..40 {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        log("compaction stopped");
        self.status.held_by = None;
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

    /// Write input held during a resume. Claude reads text and Enter arriving
    /// in one chunk as a paste, where Enter is a newline rather than submit, so
    /// each Enter goes in its own write after a pause, as if typed.
    ///
    /// Before each Enter, look at the screen: if the text went into Claude's
    /// agents panel instead of the prompt (keyboard focus can sit on the panel
    /// with nothing on screen showing it), Enter would start a background agent
    /// task. Close the panel and drop the rest rather than submit there.
    fn replay(&mut self, data: &[u8]) {
        let Some(fd) = self.child.as_ref().map(|c| c.master.as_raw_fd()) else { return };
        let mut buf = vec![0u8; 64 * 1024];
        let (mut start, mut i) = (0, 0);
        while i < data.len() {
            let Some(enter) = ENTERS.iter().find(|e| data[i..].starts_with(e)) else {
                i += 1;
                continue;
            };
            write_all(fd, &data[start..i]);
            std::thread::sleep(Duration::from_millis(250));
            self.pump_output(&mut buf);
            if agents_panel_open(&self.screen_text()) {
                write_all(fd, b"\x1b");
                return;
            }
            write_all(fd, enter);
            i += enter.len();
            start = i;
        }
        write_all(fd, &data[start..]);
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
            self.compacted_at.map(|t| format!("  compacted at {}, before the prompt cache expired", chrono_like(t))).unwrap_or_default(),
            String::new(),
            "  press any key to resume".to_string(),
        ] {
            s.extend(line.chars().take(cols));
            s.push_str("\r\n");
        }
        out(s.as_bytes());
    }

    /// Shown while claude resumes, with what has been typed so far so the
    /// keys visibly land somewhere.
    ///
    /// Herdr reads Claude's state off the screen: a braille spinner title
    /// and a `✻ …` status line count as working, so `herdr agent prompt
    /// --wait` sees activity while the session resumes.
    fn draw_waking_screen(&self) {
        let cols = winsize().ws_col as usize;
        let held = self.waking.as_ref().map(|w| typed_preview(&w.held_input)).unwrap_or_default();
        let mut s = format!("\x1b]0;⠂ {}\x07\x1b[2J\x1b[H\r\n✻ Resuming session…\r\n", self.status.title);
        if !held.is_empty() {
            // Keep the end of long input in view.
            let room = cols.saturating_sub(4).max(1);
            let skip = held.chars().count().saturating_sub(room);
            s.push_str("\r\n\x1b[2m❯ ");
            s.extend(held.chars().skip(skip));
            s.push_str("\x1b[0m\r\n");
        }
        out(s.as_bytes());
    }

    fn wake(&mut self, held_input: Vec<u8>) {
        let Some(id) = self.status.session_id.clone() else { return };
        log(&format!("wake input {:?}", String::from_utf8_lossy(&held_input)));
        self.stop_compacting();
        self.compact_plan = None;
        self.asleep = false;
        self.status.asleep = false;
        self.status.freed_kb = 0;
        let hooks = self.hooks();
        let starts_before = hooks.session_starts;
        let mut argv = args::for_resume(&self.args, self.resume_mode.as_deref(), &hooks.added_dirs);
        argv.push("--resume".into());
        argv.push(id);
        log(&format!("resume with {argv:?}"));
        self.waking = Some(Waking { since: Instant::now(), starts_before, held_input, output: vec![], revealed: false });
        self.draw_waking_screen();
        if let Err(e) = self.spawn(&argv) {
            self.waking = None;
            out(format!("\r\nclaude-hibernate: could not resume: {e:#}\r\n").as_bytes());
            self.asleep = true;
            return;
        }
        self.status.since = now();
        self.status.save();
    }
}

/// Claude's agents panel, open for a new task: typing there and pressing
/// Enter starts a background agent rather than sending a prompt.
fn agents_panel_open(screen: &str) -> bool {
    let s = screen.to_lowercase();
    s.contains("describe a task for a new session") || (s.contains("enter to return") && s.contains("space to reply"))
}

/// Held keystrokes as text for the resume screen: printable characters,
/// Enter as ⏎, backspace applied, escape sequences (arrows and the like) dropped.
fn typed_preview(data: &[u8]) -> String {
    let mut out = String::new();
    let text = String::from_utf8_lossy(data);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // A CSI or SS3 sequence runs to its final letter or `~`.
                let mut seq = String::new();
                if let Some(intro) = chars.next() {
                    seq.push(intro);
                    if matches!(intro, '[' | 'O') {
                        for n in chars.by_ref() {
                            seq.push(n);
                            if n.is_ascii_alphabetic() || n == '~' {
                                break;
                            }
                        }
                    }
                }
                if seq == "[13u" || seq == "[13;1u" {
                    out.push_str(" ⏎ ");
                }
            }
            '\r' | '\n' => out.push_str(" ⏎ "),
            '\x7f' | '\x08' => {
                out.pop();
            }
            c if !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out.trim_end().to_string()
}

pub(crate) fn log(msg: &str) {
    if let Some(path) = std::env::var_os("CLAUDE_HIBERNATE_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{:.3} {msg}", now());
        }
    }
}

/// Enter as a terminal sends it: CR, or kitty-protocol CSI 13 u.
const ENTERS: &[&[u8]] = &[b"\r", b"\x1b[13u", b"\x1b[13;1u"];

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_reports_are_not_keystrokes() {
        assert!(without_terminal_reports(b"\x1b[I").is_empty());
        assert!(without_terminal_reports(b"\x1b[O\x1b[I").is_empty());
        assert_eq!(without_terminal_reports(b"\x1b[Ifix"), b"fix");
        assert_eq!(without_terminal_reports(b"\x1b[A"), b"\x1b[A"); // arrow keys still wake
    }

    #[test]
    fn previews_typed_input() {
        assert_eq!(typed_preview(b"fix the testx\x7f\r"), "fix the test ⏎");
        assert_eq!(typed_preview(b"hi\x1b[Dthere\x1b[13u"), "hithere ⏎");
        assert_eq!(typed_preview(b"\x1b[A"), "");
    }

    #[test]
    fn detects_the_agents_panel() {
        // As drawn by Claude 2.1.30x when text lands in the panel.
        let panel = "──────\n❯ describe a task for a new session\n──────\n  ⏵⏵ bypass permissions · enter to return · space to reply · ctrl+x to delete · ? for shortcuts";
        assert!(agents_panel_open(panel));
        let prompt = "──────\n❯ fix the redirect\n──────\n  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← 3 agents";
        assert!(!agents_panel_open(prompt));
    }
}
