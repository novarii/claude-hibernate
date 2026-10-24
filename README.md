# claude-hibernate

Idle Claude Code sessions keep their memory forever. A session sitting in a
terminal tab you haven't touched since Tuesday still holds 250–500 MB. Run a
few dozen across tmux windows or a box full of agents and the machine runs out
of RAM.

`claude-hibernate` wraps `claude`. When a session has been idle for a while,
it stops the Claude process and leaves a small sleep screen in its place:

```
  💤  hibernated — Fix flaky auth test

  session 36c15390-577f-4ced-97db-20dfc9db7735
  asleep since Oct 24 09:12 · freed 312 MB

  press any key to resume
```

Press a key, or just start typing, and the same conversation comes back
through `claude --resume`. Whatever you type while it wakes is held and passed
through once Claude is ready, so the keystroke that woke it isn't lost.

A sleeping session costs about **2 MB**.

On one dev box running ~60 sessions, the memory used by Claude went from
**17.9 GB to 4.1 GB**, with 50 sessions asleep.

## Install

```sh
cargo install claude-hibernate
claude-hibernate install --write-rc   # adds the shim dir to PATH in your shell rc
```

`install` puts a `claude` symlink in `~/.local/share/claude-hibernate/bin` and
records where the real `claude` is. Open a new shell and use `claude` as usual.
Running sessions are not affected; new ones are wrapped.

To remove it: `claude-hibernate uninstall`, then delete the PATH line.

## What keeps a session awake

It only sleeps when nothing could be lost. A session stays awake while any of
these is true:

- the screen or keyboard has been active within `idle_minutes` (default 30)
- there is text in the prompt box, or a dialog, menu, or permission prompt is open
- Claude is working, or a background shell, Monitor, or background agent is running
- a `/loop` wake-up or a cron job (`CronCreate`) is pending: these timers live
  only inside the Claude process, so stopping it would silently end the loop

Timers are tracked with Claude Code hooks that the wrapper passes in with
`--settings`; your own settings files are not modified.

## Commands

```
claude-hibernate status            sessions, state, memory, and what holds them awake
claude-hibernate sleep <pid|id|all>  hibernate now (the checks above still apply)
claude-hibernate wake <pid|id|all>   resume without typing
claude-hibernate config            effective configuration
claude-hibernate run [args...]     wrap one session without the shim
```

```
$ claude-hibernate status
     PID  STATE       FOR   MEMORY  HELD BY                      TITLE
 3727942  asleep      2h14m   2 MB                               Fix flaky auth test
 3729120  held        41m    318 MB  cron e3946f1b                Nightly report loop
 3731877  awake        3m    296 MB                               Refactor billing

3 sessions, 1 asleep · using 616 MB · freed 312 MB
```

## Configuration

`~/.config/claude-hibernate/config.toml`, re-read while sessions run:

```toml
idle_minutes = 30
enabled = true
exclude_dirs = ["~/work/always-on"]   # never hibernate sessions started here
# real_claude = "/path/to/claude"     # set by `install`; found on PATH otherwise
```

`CLAUDE_HIBERNATE_IDLE_SECS` and `CLAUDE_HIBERNATE_DISABLE=1` override it per
session.

## How it works

The shim runs Claude on a pseudo-terminal it owns and proxies your terminal
to it, keeping its own copy of the screen (via `vt100`). That copy is how it
tells an empty prompt from a draft and notices when the screen stops changing.
Non-interactive uses (`claude -p`, `claude mcp ...`, pipes) are passed straight
to the real binary.

Hibernating sends Claude `SIGTERM`. Claude writes its transcript as it goes,
so nothing in the conversation is lost. Waking runs
`claude --resume <session-id>` with your original flags.

Job control works as usual: Ctrl+Z suspends the session and `fg` brings it
back.

## herdr

Inside a [herdr](https://herdr.dev) pane, the shim still shows up as a
`claude` agent, keeps its agent name across sleep and wake, and gets a
`💤 hibernated` label in the sidebar. `herdr agent prompt` to a sleeping agent
wakes it and delivers the prompt.

## Limitations

- Linux and macOS only.
- Waking takes as long as `claude --resume`, usually 2–3 seconds.
- Sessions started with `--settings` are never hibernated, since timers can't
  be tracked alongside your own settings flag.
- The prompt-box and footer checks read Claude Code's UI. If a Claude Code
  update changes it, sessions err on the side of staying awake.
- Prompt caching is unaffected by sleeping, but a session that sat idle past
  the cache lifetime pays for a cold cache on its next message either way.

claude-hibernate is an independent project and is not affiliated with or
endorsed by Anthropic.

## License

MIT
