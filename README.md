<div align="center">

# 💤 claude-hibernate

### Idle Claude Code sessions: **~300 MB → 2 MB**

Put idle sessions to sleep. Wake them with one keystroke.

[![ci](https://github.com/novarii/claude-hibernate/actions/workflows/ci.yml/badge.svg)](https://github.com/novarii/claude-hibernate/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![platforms](https://img.shields.io/badge/platform-linux%20%7C%20macos-lightgrey)

</div>

---

Every open Claude Code session holds **250–500 MB of RAM**, even one you
haven't touched in days. Ten tabs is 3 GB. Fifty agents is a box that falls over.

`claude-hibernate` puts idle sessions to sleep and brings them back the moment
you need them.

|                 | Awake    | Hibernated |
|-----------------|----------|------------|
| 1 session       | ~300 MB  | **2 MB**   |
| 10 sessions     | ~3 GB    | **20 MB**  |
| 50 sessions     | ~15 GB   | **100 MB** |

```
  💤  hibernated — Fix flaky auth test

  session 36c15390-577f-4ced-97db-20dfc9db7735
  asleep since Oct 24 09:12 · freed 312 MB

  press any key to resume
```

Start typing and the same conversation is back in about 2 seconds, with
everything you typed already in the prompt.

## Install

```sh
cargo install --git https://github.com/novarii/claude-hibernate
claude-hibernate install --write-rc
```

Open a new shell and use `claude` like you always do. That's it.

## Safe by default

A session only sleeps when nothing can be lost. It stays awake while:

- you've used it in the last 30 minutes
- there's a draft in the prompt, or a dialog or permission prompt is open
- Claude is working, or a background shell, Monitor, or agent is running
- a `/loop` or scheduled cron job is pending

The conversation is saved as it goes, and waking resumes it with your original
flags.

## Commands

```
$ claude-hibernate status
     PID  STATE    FOR    MEMORY  HELD BY          TITLE
 3727942  asleep  2h14m     2 MB                   Fix flaky auth test
 3729120  held      41m   318 MB  cron e3946f1b    Nightly report loop
 3731877  awake      3m   296 MB                   Refactor billing

3 sessions, 1 asleep · using 616 MB · freed 312 MB
```

```
claude-hibernate sleep <pid|id|all>   hibernate now
claude-hibernate wake  <pid|id|all>   resume without switching tabs
claude-hibernate status --watch       live view
claude-hibernate migrate --dry-run    herdr: which running agents can move under the wrapper
claude-hibernate config               show settings
claude-hibernate uninstall
```

## Configure

`~/.config/claude-hibernate/config.toml` (changes apply to running sessions):

```toml
idle_minutes = 30
exclude_dirs = ["~/work/always-on"]
```

## How it works

The `claude` shim runs Claude Code on a terminal it owns and passes everything
through untouched. When the session goes idle it stops Claude and shows the
sleep screen. On your first keystroke it runs `claude --resume <id>` and
replays what you typed. Ctrl+Z, `fg`, resizing, and `claude -p` all work as usual.

Works in any terminal, tmux, or SSH session. Inside [herdr](https://herdr.dev),
sleeping agents keep their name, get a 💤 label, and wake on `herdr agent prompt`.

## herdr plugin

In [herdr](https://herdr.dev), install it as a plugin instead (needs `cargo`):

```sh
herdr plugin install novarii/claude-hibernate
```

Then run **Set up the claude-hibernate shim** once. The plugin adds:

- **Hibernate this Claude agent** / **Wake this Claude agent** on the focused pane
- **Hibernate all idle Claude agents**, with a note for any that stay awake and why
- **Claude hibernation status**: a live popup of sessions, memory, and what holds them
- **Move running Claude agents under claude-hibernate**: lists the agents it can
  move, asks, then restarts each in place with its session and name. It skips any
  with unsent text, background work, or a pending `/loop` or cron job.

`claude-hibernate migrate --dry-run` shows the same list from a terminal.

## Credits

Telling whether Claude is working, idle, or waiting on a dialog uses the
agent-detection rules from [herdr](https://github.com/herdrdev/herdr)
(Apache-2.0), which are kept up to date as Claude Code's interface changes.
claude-hibernate bundles them and refreshes them daily. See [NOTICE](NOTICE).

## Limitations

- Linux and macOS.
- Waking takes as long as `claude --resume`, about 2–3 seconds.
- Sessions started with your own `--settings` flag never hibernate.
- A new Claude Code dialog that the detection rules don't know yet could look
  idle until the rules are updated. Prompts you've typed are still protected,
  since drafts are tracked from your keystrokes.

---

<sub>An independent project, not affiliated with or endorsed by Anthropic. MIT licensed.</sub>
