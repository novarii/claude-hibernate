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

/// A toast in herdr, for results of plugin actions (which have no terminal).
pub fn notify(title: &str, body: &str) {
    if std::env::var_os("HERDR_PLUGIN_ID").is_none() {
        return;
    }
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let _ = Command::new(bin).args(["notification", "show", title, "--body", body])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// herdr-core (`hc`, used by the Herdr app): a terminal label the sidebar reads.
fn hc_label(value: &str) {
    let Ok(t) = std::env::var("HC_TERMINAL_ID") else { return };
    let _ = Command::new("hc").args(["label", &t, &format!("hibernated={value}")])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

pub fn mark_asleep() {
    hc_label("1");
    if let Some(p) = pane() {
        herdr(&["pane", "report-metadata", &p, "--source", "claude-hibernate", "--agent", "claude", "--state-label", "idle=💤 hibernated"]);
    }
}

pub fn mark_awake() {
    hc_label("");
    if let Some(p) = pane() {
        herdr(&["pane", "report-metadata", &p, "--source", "claude-hibernate", "--agent", "claude", "--clear-state-labels"]);
    }
}
