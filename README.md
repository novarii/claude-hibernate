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

Sessions already running keep going without it until they restart; `install`
counts them. Inside [herdr](https://herdr.dev), `claude-hibernate migrate` moves
them under it in place, each resuming the same conversation. It skips any agent
that's working, waiting on you, or holding unsent text, so nothing is
interrupted. Run from a Claude session (say, by asking Claude to set this up),
that session moves too, once its turn ends.

## Safe by default

A session only sleeps when nothing can be lost. It stays awake while:

- you've used it in the last 30 minutes
- a dialog or permission prompt is open, or you're mid-keystroke
- Claude is working, or a background shell, Monitor, or subagent is running
- a `/loop` or scheduled cron job is pending
- Remote Control is on, so the session stays reachable from claude.ai and your phone

Claude's own housekeeping, like compacting an idle conversation, is left to
finish but doesn't count as use, and neither is a status line redrawing on a
timer. The conversation is saved as it goes, and waking resumes it with your
original flags, plus what a plain `--resume` would lose: the permission mode
you switched to with shift+tab (a session left in plan mode wakes in plan
mode) and directories added with `/add-dir`.

## Commands

```
$ claude-hibernate status
     PID  AGENT  STATE    FOR    MEMORY  HELD BY          TITLE
 3727942  claude asleep  2h14m     2 MB                   Fix flaky auth test
 3729120  claude held      41m   318 MB  cron e3946f1b    Nightly report loop
 3731877  claude awake      3m   296 MB                   Refactor billing
 3733410  pi     asleep    52m     2 MB                   Port the parser to Rust

4 sessions, 2 asleep · using 618 MB · freed 495 MB
```

```
claude-hibernate sleep <pid|id|all>   hibernate now
claude-hibernate wake  <pid|id|all>   resume without switching tabs
claude-hibernate status --watch       live view
claude-hibernate migrate [--dry-run]  herdr: move running agents under the wrapper
claude-hibernate config               show settings
claude-hibernate uninstall
```

## Configure

`~/.config/claude-hibernate/config.toml` (changes apply to running sessions):

```toml
idle_minutes = 30
exclude_dirs = ["~/work/always-on"]
compact_while_asleep = true   # see below
sleep_with_draft = true       # false: unsent text keeps a session awake
```

### Unsent drafts

A session with text typed in the prompt still goes to sleep. The draft is
shown on the sleep screen and is back in the prompt when it wakes, unsent,
line breaks and all; whatever you type to wake it lands after it.

For Claude, the wrapper loads a small Claude Code mod for that process only
(`--plugin-dir`, nothing installed). The mod reads the prompt box with
`$.prompt.read()` and puts it back after `--resume` with `$.prompt.fill()`,
which is exact where replaying keystrokes or reading the screen is not.
A prompt holding an image or a long paste Claude shows collapsed
(`[Pasted text #1 +29 lines]`) still keeps the session awake, since the
mod can read only the placeholder. Where the mod doesn't load (an older
Claude Code, mods turned off), a draft keeps the session awake as before.
pi's extension does the same with pi's editor.

### Compacting while asleep

Where Claude has idle compaction turned on, it compacts a big conversation
that has sat idle for a while, just before its one-hour prompt cache expires,
so your next message doesn't reprocess all of it. A sleeping session has no
Claude running to do that, so claude-hibernate does it for you, at the same
moment and under the same conditions: it runs `claude -p --resume <id> /compact`
in the background, one session at a time, while the cache is still warm.

The conditions are Claude's, read from your machine: whether idle compaction
is on and its token minimum and timing (200K tokens and about 54 minutes by
default) come from Claude's cached feature flags, and the `idleCompaction`
setting, `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS`, and `DISABLE_AUTO_COMPACT`
override them. When Claude wouldn't compact, neither does claude-hibernate.
It also skips a session you've meanwhile opened somewhere else, and waking the
session cancels a compaction in progress.

## How it works

The `claude` shim runs Claude Code on a terminal it owns and passes everything
through untouched. When the session goes idle it stops Claude and shows the
sleep screen. On your first keystroke it runs `claude --resume <id>` and
replays what you typed. Ctrl+Z, `fg`, resizing, and `claude -p` all work as usual.

Works in any terminal, tmux, or SSH session. Inside [herdr](https://herdr.dev),
sleeping agents keep their name, get a 💤 label, and wake on `herdr agent prompt`.

## pi

The same wrapper hibernates [pi](https://pi.dev) sessions (~180 MB each).
Where pi is installed, `install` also puts a `pi` shim next to the `claude`
one, so `pi` starts under it like `claude` does; `claude-hibernate pi [args]`
does the same without the shim. `status`, `sleep` and `wake` list and control
both, with an AGENT column telling them apart.

The wrapper loads a small pi extension with `-e` (nothing is added to your pi
settings). It tells the wrapper which session file to resume and keeps pi
awake while:

- a prompt is running (`agent_start` until `agent_settled`), or compacting
- messages are queued, or an extension dialog is open
- work pi started is still running: a tool call (bash, a subagent), a `!`
  command, or a job of the bash-background extension (it reports jobs on
  pi's extension event bus). Other child processes, such as MCP servers,
  never hold it

Waking runs `pi --session <file>` with your original flags, minus the session
pickers and initial prompt. `--model`, `--provider` and `--thinking` are left
off too, because pi restores the model and thinking level from the session,
so one you switched with Ctrl+P or `/model` survives a sleep. pi's own
prompt-cache refreshes don't count as use. `--no-session`, `-p`, `--mode
json|rpc` and the `pi install`/`config`/... subcommands run pi directly.

pi has no hook for its built-in pickers (`/model`, `/resume`, `/tree`), so one
left open doesn't hold the session awake; closing it loses nothing.

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
  move, asks, then restarts each in place with its session, name, and permission
  mode. It skips any that's working or waiting on you, or has unsent text,
  background work, or a pending `/loop` or cron job.

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
- While asleep, a session isn't there to be reached: it drops out of other
  Claude sessions' agent list, and messages pushed to it (artifact comments,
  channels) don't reach it.
- Waking restarts MCP servers, so state they keep in memory (an open browser,
  say) starts fresh.
- Hooks you've set on `SessionEnd` and `SessionStart` run on every sleep and wake.
- A new Claude Code dialog that the detection rules don't know yet could look
  idle until the rules are updated. Prompts you've typed are still protected,
  since drafts are tracked from your keystrokes.

---

<sub>An independent project, not affiliated with or endorsed by Anthropic. MIT licensed.</sub>
