#!/bin/sh
# Launcher for the herdr plugin's actions and panes.
#
# Finds the claude-hibernate binary that `herdr plugin install` built with
# `cargo install`, falling back to PATH and to a local build (for `plugin link`).
# With --pause first, waits for Enter before exiting so a popup stays readable.
set -u

pause=0
if [ "${1:-}" = "--pause" ]; then
  pause=1
  shift
fi

bin=""
for candidate in \
  "${CLAUDE_HIBERNATE_BIN:-}" \
  "${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/claude-hibernate" \
  "$(command -v claude-hibernate 2>/dev/null || true)" \
  "${HERDR_PLUGIN_ROOT:-.}/target/release/claude-hibernate"; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then
    bin=$candidate
    break
  fi
done

if [ -z "$bin" ]; then
  msg="claude-hibernate binary not found; reinstall the plugin (needs cargo)"
  echo "$msg" >&2
  "${HERDR_BIN_PATH:-herdr}" notification show claude-hibernate --body "$msg" >/dev/null 2>&1
  status=127
else
  "$bin" "$@"
  status=$?
fi

if [ "$pause" = 1 ]; then
  printf '\npress Enter to close'
  read -r _
fi
exit "$status"
