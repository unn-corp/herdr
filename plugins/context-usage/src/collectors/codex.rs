//! Codex CLI collector.
//!
//! Codex has no per-render statusLine, but it writes a rollout JSONL per session
//! under `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`. The first line is a
//! `session_meta` (session id, cwd, model provider); a `token_count` event is
//! appended after each turn carrying `model_context_window` and the last
//! request's token usage. We map a Herdr pane to its session by matching cwd,
//! then read the most recent `token_count` to compute context-window usage.
//!
//! This is a pull collector: it is invoked by `poll` (per pane, with that pane's
//! cwd) rather than pushed by the agent. No reset timer exists in Codex local
//! state, so none is reported.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cache::{Confidence, UsageRecord, SCHEMA_VERSION};
use crate::context::PaneContext;

pub const SOURCE: &str = "herdr-context-usage:codex-rollout";
/// Codex is polled, so a shorter horizon than Claude: 15 min of no new turn
/// reads as stale.
const DEFAULT_STALE_AFTER_SECONDS: u64 = 900;

/// Usage extracted from a Codex rollout.
#[derive(Debug, Default, PartialEq)]
pub struct CodexUsage {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub context_window_tokens: Option<u64>,
    /// Input tokens of the most recent request: the current context occupancy.
    pub used_tokens: Option<u64>,
}

/// Resolve `$CODEX_HOME`, falling back to `~/.codex`.
pub fn codex_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".codex"))
}

/// Find the rollout file for the Codex session whose `session_meta.cwd` matches
/// `cwd`, preferring the most recently modified. `None` if no session matches.
pub fn find_session_for_cwd(home: &Path, cwd: &str) -> Option<PathBuf> {
    let sessions = home.join("sessions");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for path in jsonl_files(&sessions) {
        let Some(meta_cwd) = rollout_cwd(&path) else {
            continue;
        };
        if meta_cwd != cwd {
            continue;
        }
        let mtime = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, path)| path)
}

/// Read the cwd from a rollout's first-line `session_meta`, cheaply.
fn rollout_cwd(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let first = contents.lines().next()?;
    let value: Value = serde_json::from_str(first).ok()?;
    value
        .pointer("/payload/cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Recursively collect `.jsonl` files under `dir` (Codex nests by Y/M/D).
fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out
}

/// Parse a rollout file into [`CodexUsage`]. Reads the first line for session
/// metadata and the last `token_count` event for the current usage.
pub fn parse_rollout(path: &Path) -> Option<CodexUsage> {
    let contents = fs::read_to_string(path).ok()?;
    parse_rollout_str(&contents)
}

/// Pure parse over rollout contents, for testing.
pub fn parse_rollout_str(contents: &str) -> Option<CodexUsage> {
    let mut usage = CodexUsage::default();

    let mut lines = contents.lines();
    if let Some(first) = lines.next() {
        if let Ok(meta) = serde_json::from_str::<Value>(first) {
            let p = meta.pointer("/payload");
            usage.session_id = p
                .and_then(|p| p.get("session_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            usage.cwd = p
                .and_then(|p| p.get("cwd"))
                .and_then(Value::as_str)
                .map(str::to_string);
            usage.model_provider = p
                .and_then(|p| p.get("model_provider"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // The model name is not always in session_meta; take it if present.
            usage.model = p
                .and_then(|p| p.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }

    // Scan for the LAST token_count event. Filter by substring first to avoid
    // JSON-parsing every response item in a large rollout.
    for line in contents.lines().rev() {
        if !line.contains("\"token_count\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let info = value.pointer("/payload/info");
        let Some(info) = info else { continue };
        usage.context_window_tokens = info.get("model_context_window").and_then(Value::as_u64);
        usage.used_tokens = info
            .pointer("/last_token_usage/input_tokens")
            .and_then(Value::as_u64);
        if usage.model.is_none() {
            usage.model = info
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        break;
    }

    // Only meaningful if we found at least a token figure.
    usage.used_tokens?;
    Some(usage)
}

/// Convenience: find and parse the session for a cwd.
pub fn usage_for_cwd(cwd: &str) -> Option<CodexUsage> {
    let home = codex_home()?;
    let path = find_session_for_cwd(&home, cwd)?;
    parse_rollout(&path)
}

/// Build a [`UsageRecord`] for a pane from parsed Codex usage.
pub fn record(usage: &CodexUsage, pane_id: &str, ctx: &PaneContext, now_unix: i64) -> UsageRecord {
    let used_pct = match (usage.used_tokens, usage.context_window_tokens) {
        (Some(used), Some(window)) => crate::report::pct_of(used, window),
        _ => None,
    };
    let confidence = if used_pct.is_some() {
        Confidence::Official
    } else if usage.used_tokens.is_some() {
        // Token count known but no window mapped: cannot express a percentage.
        Confidence::Estimated
    } else {
        Confidence::Unavailable
    };
    let model = usage.model.clone();
    let model_family = model
        .as_deref()
        .map(family_of)
        .or_else(|| usage.model_provider.clone());

    UsageRecord {
        schema: SCHEMA_VERSION,
        pane_id: pane_id.to_string(),
        workspace_id: ctx.workspace_id.clone(),
        tab_id: ctx.tab_id.clone(),
        agent: Some("codex".to_string()),
        source: SOURCE.to_string(),
        model,
        model_family,
        context_window_tokens: usage.context_window_tokens,
        used_tokens: usage.used_tokens,
        used_pct,
        remaining_tokens: match (usage.context_window_tokens, usage.used_tokens) {
            (Some(w), Some(u)) => Some(w.saturating_sub(u)),
            _ => None,
        },
        reset_at_unix: None,
        window_kind: Some("context".to_string()),
        updated_at_unix: now_unix,
        confidence,
        stale_after_seconds: DEFAULT_STALE_AFTER_SECONDS,
        notes: Vec::new(),
    }
}

fn family_of(model: &str) -> String {
    // e.g. "gpt-5.5" -> "gpt", "o4-mini" -> "o4".
    model
        .split(['-', '.'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(model)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLLOUT: &str = r#"{"timestamp":"2026-07-07T18:29:34.639Z","type":"session_meta","payload":{"session_id":"019f3dd5","cwd":"/home/devotek/proj","model_provider":"openai"}}
{"type":"response_item","payload":{"type":"message"}}
{"timestamp":"2026-07-07T18:40:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":50000,"total_tokens":51000},"model_context_window":200000}}}
{"type":"response_item","payload":{"type":"message"}}
{"timestamp":"2026-07-07T18:49:29Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":103757,"total_tokens":104110},"model_context_window":258400}}}
"#;

    #[test]
    fn parses_meta_and_last_token_count() {
        let usage = parse_rollout_str(ROLLOUT).expect("usage");
        assert_eq!(usage.cwd.as_deref(), Some("/home/devotek/proj"));
        assert_eq!(usage.model_provider.as_deref(), Some("openai"));
        // Uses the LAST token_count, not the first.
        assert_eq!(usage.used_tokens, Some(103757));
        assert_eq!(usage.context_window_tokens, Some(258400));
    }

    #[test]
    fn record_computes_official_percentage() {
        let usage = parse_rollout_str(ROLLOUT).unwrap();
        let ctx = PaneContext {
            pane_id: Some("w1:p2".into()),
            ..Default::default()
        };
        let rec = record(&usage, "w1:p2", &ctx, 100);
        // 103757 / 258400 ~= 40%
        assert_eq!(rec.used_pct, Some(40));
        assert_eq!(rec.confidence, Confidence::Official);
        assert_eq!(rec.agent.as_deref(), Some("codex"));
        assert_eq!(rec.remaining_tokens, Some(258400 - 103757));
        assert!(rec.reset_at_unix.is_none());
    }

    #[test]
    fn no_token_count_yields_none() {
        let meta_only = r#"{"type":"session_meta","payload":{"cwd":"/x"}}"#;
        assert!(parse_rollout_str(meta_only).is_none());
    }

    #[test]
    fn token_count_without_window_is_estimated() {
        let s = r#"{"type":"session_meta","payload":{"cwd":"/x","model_provider":"openai"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1234}}}}"#;
        let usage = parse_rollout_str(s).unwrap();
        let ctx = PaneContext::default();
        let rec = record(&usage, "w1:p2", &ctx, 0);
        assert_eq!(rec.used_pct, None);
        assert_eq!(rec.used_tokens, Some(1234));
        assert_eq!(rec.confidence, Confidence::Estimated);
        assert_eq!(rec.model_family.as_deref(), Some("openai"));
    }

    #[test]
    fn family_extraction() {
        assert_eq!(family_of("gpt-5.5"), "gpt");
        assert_eq!(family_of("o4-mini"), "o4");
    }
}
