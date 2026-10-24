//! Optional integration with herdr (https://herdr.dev), active when the
//! wrapper runs inside a herdr pane. Herdr already recognizes the wrapper as a
//! `claude` agent because it is invoked through a `claude` symlink; this only
//! adds a sidebar label while the session sleeps.

use std::process::{Command, Stdio};

pub fn pane() -> Option<String> {
    (std::env::var("HERDR_ENV").as_deref() == Ok("1")).then(|| std::env::var("HERDR_PANE_ID").ok()).flatten()
}

fn herdr(args: &[&str]) {
    let _ = Command::new("herdr").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

pub fn mark_asleep() {
    if let Some(p) = pane() {
        herdr(&["pane", "report-metadata", &p, "--source", "claude-hibernate", "--agent", "claude", "--state-label", "idle=💤 hibernated"]);
    }
}

pub fn mark_awake() {
    if let Some(p) = pane() {
        herdr(&["pane", "report-metadata", &p, "--source", "claude-hibernate", "--agent", "claude", "--clear-state-labels"]);
    }
}
