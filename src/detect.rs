//! Agent state (idle / working / blocked) from the screen, using herdr's
//! agent-detection manifests.
//!
//! The manifest format and the region definitions below are ported from
//! herdr (https://github.com/herdrdev/herdr, Apache-2.0), src/detect/manifest.rs.
//! herdr maintains the rules as Claude Code's UI changes and publishes them at
//! herdr.dev/agent-detection/, so we fetch updates from there and fall back to
//! the copy bundled at build time. See NOTICE.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use regex::Regex;
use serde::Deserialize;

/// Matches herdr's engine version for the regions implemented here.
const ENGINE_VERSION: u32 = 3;
const BUNDLED_CLAUDE: &str = include_str!("../manifests/claude.toml");
const CATALOG_BASE: &str = "https://herdr.dev/agent-detection";
const REFRESH_EVERY: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Working,
    Blocked,
    Unknown,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[allow(dead_code)]
    id: String,
    version: Option<String>,
    min_engine_version: Option<u32>,
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
struct Rule {
    id: String,
    state: Option<State>,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_region")]
    region: String,
    #[serde(flatten)]
    gate: Gate,
}

fn default_region() -> String {
    "whole_recent".into()
}

/// All `contains`, `regex`, `line_regex`, and `all` must match, at least one
/// `any` (if present), and no `not`. `contains` is case-insensitive.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Gate {
    all: Vec<Gate>,
    any: Vec<Gate>,
    #[serde(rename = "not")]
    not_gate: Vec<Gate>,
    contains: Vec<String>,
    regex: Vec<String>,
    line_regex: Vec<String>,
    // herdr-only presentation flags; accepted and ignored.
    visible_idle: Option<bool>,
    visible_blocker: Option<bool>,
    visible_working: Option<bool>,
    skip_state_update: Option<bool>,
}

struct CompiledGate {
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not_gate: Vec<CompiledGate>,
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

struct CompiledRule {
    id: String,
    state: State,
    priority: i32,
    region: String,
    gate: CompiledGate,
}

pub struct Detector {
    pub version: String,
    rules: Vec<CompiledRule>,
}

pub struct Detection {
    pub state: State,
    pub rule: Option<String>,
}

fn compile(g: &Gate) -> Result<CompiledGate, regex::Error> {
    Ok(CompiledGate {
        all: g.all.iter().map(compile).collect::<Result<_, _>>()?,
        any: g.any.iter().map(compile).collect::<Result<_, _>>()?,
        not_gate: g.not_gate.iter().map(compile).collect::<Result<_, _>>()?,
        contains: g.contains.iter().map(|s| s.to_lowercase()).collect(),
        regex: g.regex.iter().map(|r| Regex::new(r)).collect::<Result<_, _>>()?,
        line_regex: g.line_regex.iter().map(|r| Regex::new(r)).collect::<Result<_, _>>()?,
    })
}

fn matches(g: &CompiledGate, text: &str, lower: &str) -> bool {
    g.contains.iter().all(|n| lower.contains(n.as_str()))
        && g.regex.iter().all(|r| r.is_match(text))
        && g.line_regex.iter().all(|r| text.lines().any(|l| r.is_match(l)))
        && g.all.iter().all(|n| matches(n, text, lower))
        && (g.any.is_empty() || g.any.iter().any(|n| matches(n, text, lower)))
        && !g.not_gate.iter().any(|n| matches(n, text, lower))
}

impl Detector {
    fn parse(content: &str) -> Result<Self, String> {
        let m: Manifest = toml::from_str(content).map_err(|e| e.to_string())?;
        if m.min_engine_version.unwrap_or(1) > ENGINE_VERSION {
            return Err(format!("needs engine {}", m.min_engine_version.unwrap_or(0)));
        }
        let rules = m
            .rules
            .iter()
            .map(|r| {
                Ok(CompiledRule {
                    id: r.id.clone(),
                    state: r.state.unwrap_or(State::Unknown),
                    priority: r.priority,
                    region: r.region.clone(),
                    gate: compile(&r.gate).map_err(|e| format!("rule {}: {e}", r.id))?,
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Self { version: m.version.unwrap_or_default(), rules })
    }

    /// The highest-priority matching rule wins; with no match a known agent
    /// is idle, as in herdr.
    pub fn detect(&self, screen: &str, osc_title: &str) -> Detection {
        let mut best: Option<&CompiledRule> = None;
        for rule in &self.rules {
            let text = region(screen, osc_title, &rule.region);
            if matches(&rule.gate, text, &text.to_lowercase()) && best.is_none_or(|b| rule.priority > b.priority) {
                best = Some(rule);
            }
        }
        match best {
            Some(r) => Detection { state: r.state, rule: Some(r.id.clone()) },
            None => Detection { state: State::Idle, rule: None },
        }
    }
}

fn cache_path() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("claude-hibernate/agent-detection/claude.toml")
}

fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|s| s.parse().unwrap_or(0)).collect()
}

/// The newest valid manifest: the cached download if it is newer than the
/// bundled copy, otherwise the bundled one.
pub fn claude() -> &'static Detector {
    static D: OnceLock<Detector> = OnceLock::new();
    D.get_or_init(|| {
        let bundled = Detector::parse(BUNDLED_CLAUDE).expect("bundled manifest is valid");
        std::fs::read_to_string(cache_path())
            .ok()
            .and_then(|s| Detector::parse(&s).ok())
            .filter(|cached| version_key(&cached.version) > version_key(&bundled.version))
            .unwrap_or(bundled)
    })
}

/// Download the published manifest at most once a day, in the background.
/// Takes effect for sessions started afterwards.
pub fn refresh_in_background() {
    if std::env::var("CLAUDE_HIBERNATE_NO_MANIFEST_UPDATE").as_deref() == Ok("1") {
        return;
    }
    let path = cache_path();
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < REFRESH_EVERY);
    if fresh {
        return;
    }
    std::thread::spawn(move || {
        let Ok(out) = std::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "10", "--max-filesize", "262144", &format!("{CATALOG_BASE}/claude.toml")])
            .stderr(std::process::Stdio::null())
            .output()
        else {
            return;
        };
        let Ok(body) = String::from_utf8(out.stdout) else { return };
        if out.status.success() && Detector::parse(&body).is_ok() {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            crate::state::write_atomic(&path, body.as_bytes());
        } else if let Ok(f) = std::fs::File::options().append(true).open(&path) {
            // Keep the old copy but don't retry until tomorrow.
            let _ = f.set_modified(SystemTime::now());
        }
    });
}

// ---------------------------------------------------------------------------
// Regions (ported from herdr)

fn region<'a>(content: &'a str, osc_title: &'a str, spec: &str) -> &'a str {
    let spec = spec.trim();
    match spec {
        "osc_title" => return osc_title,
        "osc_progress" => return "",
        "whole_recent" => return content,
        "after_last_prompt_marker" => return after_last_prompt_marker(content),
        "before_current_prompt_marker" => return before_current_prompt_marker(content),
        "whole_recent_without_current_prompt_marker" => {
            let lines: Vec<&str> = content.lines().collect();
            return if current_codex_prompt_index(&lines).is_some() { "" } else { content };
        }
        "prompt_box_body" => return prompt_box_body(content).unwrap_or(""),
        "above_prompt_box" => return above_prompt_box(content),
        "last_non_empty_above_prompt_box" => return last_non_empty_line(above_prompt_box(content)),
        "after_last_horizontal_rule" => return after_last_horizontal_rule(content),
        _ => {}
    }
    let count = |name: &str| {
        spec.strip_prefix(name)?.strip_prefix('(')?.strip_suffix(')')?.parse::<usize>().ok()
    };
    if let Some(n) = count("bottom_lines") {
        let lines: Vec<&str> = content.lines().collect();
        return from_line(content, &lines, lines.len().saturating_sub(n));
    }
    if let Some(n) = count("bottom_non_empty_lines") {
        let lines: Vec<&str> = content.lines().collect();
        return match lines.iter().enumerate().rev().filter(|(_, l)| !l.trim().is_empty()).take(n).last() {
            Some((i, _)) => from_line(content, &lines, i),
            None => "",
        };
    }
    if let Some(n) = count("top_non_empty_lines").filter(|n| *n > 0) {
        let lines: Vec<&str> = content.lines().collect();
        return match lines.iter().enumerate().filter(|(_, l)| !l.trim().is_empty()).take(n).last() {
            Some((i, _)) => &content[..line_offset(content, &lines, i + 1)],
            None => "",
        };
    }
    ""
}

fn line_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())].iter().map(|l| l.len() + 1).sum::<usize>().min(content.len())
}

fn from_line<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    &content[line_offset(content, lines, index)..]
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker_line(line: &str) -> bool {
    line.starts_with('•') || line.starts_with('■') || line.starts_with('✗') || line.starts_with('✓')
}

fn current_codex_prompt_index(lines: &[&str]) -> Option<usize> {
    let i = lines.iter().rposition(|l| codex_prompt_line(l))?;
    (!lines[i + 1..].iter().any(|l| codex_block_marker_line(l))).then_some(i)
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    match lines.iter().rposition(|l| codex_prompt_line(l)) {
        Some(i) => from_line(content, &lines, i + 1),
        None => content,
    }
}

fn before_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    match current_codex_prompt_index(&lines) {
        Some(i) => &content[..line_offset(content, &lines, i)],
        None => content,
    }
}

fn is_horizontal_rule(line: &str) -> bool {
    let t = line.trim();
    let n = t.chars().take_while(|&c| c == '─').count();
    if n == 0 {
        return false;
    }
    let rest = t.char_indices().nth(n).map(|(i, _)| &t[i..]).unwrap_or("");
    rest.trim_start().is_empty() || n >= 3
}

fn prompt_box_top(lines: &[&str]) -> Option<usize> {
    let mut seen = 0;
    for i in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[i]) {
            seen += 1;
            if seen == 2 {
                return Some(i);
            }
        }
    }
    None
}

/// The text inside Claude's prompt box, between its two horizontal rules.
pub fn prompt_box_body(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let top = prompt_box_top(&lines)?;
    let end = lines[top + 1..].iter().position(|l| is_horizontal_rule(l)).map(|r| top + 1 + r).unwrap_or(lines.len());
    Some(&content[line_offset(content, &lines, top + 1)..line_offset(content, &lines, end)])
}

/// Whether a prompt box body (from `prompt_box_body`) holds nothing typed.
/// An empty prompt may show a dimmed suggestion, `Try "fix lint errors"`,
/// which reads as text once the styling is gone.
pub fn prompt_is_empty(body: &str) -> bool {
    let text = body.trim().trim_start_matches('❯').trim();
    text.is_empty() || (text.starts_with("Try \"") && text.ends_with('"') && !text.contains('\n'))
}

fn above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    match prompt_box_top(&lines) {
        Some(top) => &content[..line_offset(content, &lines, top)],
        None => content,
    }
}

fn last_non_empty_line(content: &str) -> &str {
    content.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let (mut last_end, mut offset) = (0, 0);
    for line in content.lines() {
        offset += line.len() + 1;
        if is_horizontal_rule(line) {
            last_end = offset.min(content.len());
        }
    }
    &content[last_end..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled() -> Detector {
        Detector::parse(BUNDLED_CLAUDE).unwrap()
    }

    #[test]
    fn bundled_manifest_parses() {
        assert!(!bundled().rules.is_empty());
    }

    #[test]
    fn idle_prompt() {
        let screen = "● pong\n✻ Brewed for 2s · done 6:26 AM\n────────\n❯\n────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(bundled().detect(screen, "✳ Pong").state, State::Idle);
    }

    #[test]
    fn spinner_title_is_working() {
        assert_eq!(bundled().detect("anything", "⠂ Fixing tests").state, State::Working);
    }

    #[test]
    fn interrupt_hint_is_working() {
        let screen = "✻ Sautéing… (12s · ↓ 300 tokens)\n────────\n❯\n────────\n  ⏸ plan mode on · esc to interrupt";
        assert_eq!(bundled().detect(screen, "").state, State::Working);
    }

    #[test]
    fn suggestion_is_an_empty_prompt() {
        assert!(prompt_is_empty("❯\n"));
        assert!(prompt_is_empty("❯ Try \"fix lint errors\"\n"));
        assert!(!prompt_is_empty("❯ fix the redirect\n"));
        assert!(!prompt_is_empty("❯ '\n"));
    }

    #[test]
    fn prompt_box_body_extracts_draft() {
        let screen = "x\n────────\n❯ half typed\n────────\nfooter";
        assert_eq!(prompt_box_body(screen).unwrap().trim(), "❯ half typed");
    }
}
