//! Grok Build (xAI CLI) collector.
//!
//! Grok stores sessions under `$GROK_HOME/sessions/<url-encoded-cwd>/<session-id>/`
//! (default `~/.grok`). Official context occupancy lives in `signals.json`
//! (`contextWindowUsage`, `contextTokensUsed`, `contextWindowTokens`,
//! `primaryModelId`). Live sessions may lag before `signals.json` appears; we
//! then fall back to the latest `_meta.totalTokens` in `updates.jsonl` plus the
//! model window from `models_cache.json` / the registry (Estimated confidence).
//!
//! Pull collector: invoked by `poll` with the pane cwd. Single brand label
//! `grok` (no separate "Grok Build" identity). Privacy: only numeric usage
//! fields are retained; prompt/response text is never stored.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::cache::{Confidence, UsageRecord, SCHEMA_VERSION};
use crate::context::PaneContext;

pub const SOURCE_SIGNALS: &str = "herdr-context-usage:grok-signals";
pub const SOURCE_UPDATES: &str = "herdr-context-usage:grok-updates-totalTokens";
const DEFAULT_STALE_AFTER_SECONDS: u64 = 900;

/// Usage extracted from a Grok session directory.
#[derive(Debug, PartialEq)]
pub struct GrokUsage {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub context_window_tokens: Option<u64>,
    pub used_tokens: Option<u64>,
    /// Official percentage from `signals.json` when present.
    pub used_pct: Option<u8>,
    pub confidence: Confidence,
    pub source: &'static str,
    pub notes: Vec<String>,
}

/// Resolve Grok home (`$GROK_HOME`, else `~/.grok`).
pub fn grok_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GROK_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".grok"))
}

/// Encode a cwd the way Grok names session group directories (slash → `%2F`).
pub fn encode_cwd(cwd: &str) -> String {
    cwd.replace('/', "%2F")
}

/// Find and parse usage for the Grok session rooted at `cwd`.
pub fn usage_for_cwd(cwd: &str) -> Option<GrokUsage> {
    let home = grok_home()?;
    usage_for_cwd_in(&home, cwd)
}

/// Like [`usage_for_cwd`] but with an explicit Grok home (tests).
pub fn usage_for_cwd_in(home: &Path, cwd: &str) -> Option<GrokUsage> {
    let session_dir = find_session_dir(home, cwd)?;
    parse_session_dir(&session_dir, home)
}

/// Resolve the session directory for `cwd`: prefer live `active_sessions.json`,
/// else the newest directory under `sessions/<encoded-cwd>/`.
pub fn find_session_dir(home: &Path, cwd: &str) -> Option<PathBuf> {
    if let Some(dir) = session_from_active(home, cwd) {
        return Some(dir);
    }
    newest_session_under(home, cwd)
}

fn session_from_active(home: &Path, cwd: &str) -> Option<PathBuf> {
    let path = home.join("active_sessions.json");
    let bytes = fs::read(&path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let arr = value.as_array()?;
    let mut best: Option<(String, String)> = None; // (opened_at, session_id)
    for entry in arr {
        let entry_cwd = entry.get("cwd").and_then(Value::as_str)?;
        if entry_cwd != cwd {
            continue;
        }
        let session_id = entry
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        let opened = entry
            .get("opened_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if best.as_ref().is_none_or(|(t, _)| opened.as_str() >= t.as_str()) {
            best = Some((opened, session_id.to_string()));
        }
    }
    let (_, session_id) = best?;
    let dir = home
        .join("sessions")
        .join(encode_cwd(cwd))
        .join(&session_id);
    dir.is_dir().then_some(dir)
}

fn newest_session_under(home: &Path, cwd: &str) -> Option<PathBuf> {
    let group = home.join("sessions").join(encode_cwd(cwd));
    let entries = fs::read_dir(&group).ok()?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Prefer summary.json mtime; fall back to the directory mtime.
        let mtime = fs::metadata(path.join("summary.json"))
            .or_else(|_| fs::metadata(&path))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        // If summary carries a cwd, require a match (guards against encoding collisions).
        if let Some(summary_cwd) = summary_cwd(&path) {
            if summary_cwd != cwd {
                continue;
            }
        }
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

fn summary_cwd(session_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(session_dir.join("summary.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn summary_model(session_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(session_dir.join("summary.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("current_model_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse a session directory into [`GrokUsage`]. Prefers `signals.json`.
pub fn parse_session_dir(session_dir: &Path, home: &Path) -> Option<GrokUsage> {
    let session_id = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);

    if let Some(usage) = parse_signals_file(&session_dir.join("signals.json")) {
        let mut usage = usage;
        usage.session_id = session_id.or(usage.session_id);
        return Some(usage);
    }

    // Estimated fallback: latest totalTokens from updates.jsonl.
    let used = last_total_tokens(&session_dir.join("updates.jsonl"))?;
    let model = summary_model(session_dir);
    let window = model
        .as_deref()
        .and_then(|m| window_for_model(home, m))
        .or_else(|| model.as_deref().and_then(crate::models::context_window));
    let used_pct = match window {
        Some(w) => crate::report::pct_of(used, w),
        None => None,
    };
    let mut notes = vec![
        "signals.json absent; estimated from updates.jsonl totalTokens".to_string(),
    ];
    if used_pct.is_none() {
        notes.push("context window unknown for model; showing tokens only".to_string());
    }
    Some(GrokUsage {
        session_id,
        model,
        context_window_tokens: window,
        used_tokens: Some(used),
        used_pct,
        confidence: Confidence::Estimated,
        source: SOURCE_UPDATES,
        notes,
    })
}

/// Parse `signals.json` into official usage. Public for unit tests with fixtures.
pub fn parse_signals_str(contents: &str) -> Option<GrokUsage> {
    let value: Value = serde_json::from_str(contents).ok()?;
    let used_tokens = value.get("contextTokensUsed").and_then(as_u64);
    let context_window_tokens = value.get("contextWindowTokens").and_then(as_u64);
    let used_pct = value
        .get("contextWindowUsage")
        .and_then(as_u64)
        .map(|n| n.min(100) as u8)
        .or_else(|| match (used_tokens, context_window_tokens) {
            (Some(u), Some(w)) => crate::report::pct_of(u, w),
            _ => None,
        });
    // Need at least a percentage or a token count.
    if used_pct.is_none() && used_tokens.is_none() {
        return None;
    }
    let model = value
        .get("primaryModelId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(GrokUsage {
        session_id: None,
        model,
        context_window_tokens,
        used_tokens,
        used_pct,
        confidence: Confidence::Official,
        source: SOURCE_SIGNALS,
        notes: Vec::new(),
    })
}

fn parse_signals_file(path: &Path) -> Option<GrokUsage> {
    let contents = fs::read_to_string(path).ok()?;
    if contents.trim().is_empty() {
        return None;
    }
    parse_signals_str(&contents)
}

/// Scan `updates.jsonl` for the last numeric `_meta.totalTokens` (or any
/// nested `totalTokens`). Only the number is retained.
pub fn last_total_tokens_str(contents: &str) -> Option<u64> {
    for line in contents.lines().rev() {
        if !line.contains("totalTokens") {
            continue;
        }
        if let Some(n) = extract_total_tokens(line) {
            return Some(n);
        }
    }
    None
}

fn last_total_tokens(path: &Path) -> Option<u64> {
    let contents = fs::read_to_string(path).ok()?;
    last_total_tokens_str(&contents)
}

/// Pull only the numeric `totalTokens` field out of a JSON line. Walks the
/// tree without retaining prompt/response text.
fn extract_total_tokens(line: &str) -> Option<u64> {
    let value: Value = serde_json::from_str(line).ok()?;
    find_total_tokens(&value)
}

fn find_total_tokens(value: &Value) -> Option<u64> {
    match value {
        Value::Object(map) => {
            if let Some(t) = map.get("totalTokens").and_then(as_u64) {
                return Some(t);
            }
            for v in map.values() {
                if let Some(t) = find_total_tokens(v) {
                    return Some(t);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_total_tokens),
        _ => None,
    }
}

fn window_for_model(home: &Path, model_id: &str) -> Option<u64> {
    let path = home.join("models_cache.json");
    if let Ok(raw) = fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            if let Some(w) = value
                .pointer(&format!("/models/{model_id}/info/context_window"))
                .and_then(as_u64)
            {
                return Some(w);
            }
        }
    }
    crate::models::context_window(model_id)
}

fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().map(|n| n.max(0) as u64))
        .or_else(|| value.as_f64().map(|n| n.max(0.0) as u64))
}

/// Build a [`UsageRecord`] for a pane from parsed Grok usage.
pub fn record(usage: &GrokUsage, pane_id: &str, ctx: &PaneContext, now_unix: i64) -> UsageRecord {
    let model_family = usage.model.as_deref().map(|m| {
        m.split(['-', '.', '/'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(m)
            .to_string()
    });

    UsageRecord {
        schema: SCHEMA_VERSION,
        pane_id: pane_id.to_string(),
        workspace_id: ctx.workspace_id.clone(),
        tab_id: ctx.tab_id.clone(),
        agent: Some("grok".to_string()),
        source: usage.source.to_string(),
        model: usage.model.clone(),
        model_family,
        context_window_tokens: usage.context_window_tokens,
        used_tokens: usage.used_tokens,
        used_pct: usage.used_pct,
        remaining_tokens: match (usage.context_window_tokens, usage.used_tokens) {
            (Some(w), Some(u)) => Some(w.saturating_sub(u)),
            _ => None,
        },
        reset_at_unix: None,
        window_kind: Some("context".to_string()),
        updated_at_unix: now_unix,
        confidence: usage.confidence,
        stale_after_seconds: DEFAULT_STALE_AFTER_SECONDS,
        notes: usage.notes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SIGNALS: &str = r#"{
  "turnCount": 8,
  "contextWindowUsage": 58,
  "contextTokensUsed": 291176,
  "contextWindowTokens": 500000,
  "primaryModelId": "grok-4.5"
}"#;

    const UPDATES: &str = r#"{"timestamp":1,"method":"session/update","params":{"update":{"_meta":{"totalTokens":1000,"eventId":"a"}}}}
{"timestamp":2,"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"secret prompt body"},"_meta":{"totalTokens":105513,"eventId":"b"}}}}
"#;

    #[test]
    fn encode_cwd_percent_encodes_slashes() {
        assert_eq!(
            encode_cwd("/home/devotek/proj"),
            "%2Fhome%2Fdevotek%2Fproj"
        );
    }

    #[test]
    fn parses_signals_official_percentage() {
        let usage = parse_signals_str(SIGNALS).expect("signals");
        assert_eq!(usage.used_pct, Some(58));
        assert_eq!(usage.used_tokens, Some(291176));
        assert_eq!(usage.context_window_tokens, Some(500000));
        assert_eq!(usage.model.as_deref(), Some("grok-4.5"));
        assert_eq!(usage.confidence, Confidence::Official);
        assert_eq!(usage.source, SOURCE_SIGNALS);
    }

    #[test]
    fn last_total_tokens_takes_latest_and_ignores_body_text() {
        let n = last_total_tokens_str(UPDATES).expect("tokens");
        assert_eq!(n, 105513);
        // Sanity: the secret body is in the fixture but we only returned a number.
        assert!(!format!("{n}").contains("secret"));
    }

    #[test]
    fn record_from_signals_is_official_grok() {
        let usage = parse_signals_str(SIGNALS).unwrap();
        let rec = record(&usage, "w1:p2", &PaneContext::default(), 100);
        assert_eq!(rec.agent.as_deref(), Some("grok"));
        assert_eq!(rec.used_pct, Some(58));
        assert_eq!(rec.confidence, Confidence::Official);
        assert_eq!(rec.remaining_tokens, Some(500000 - 291176));
        assert!(rec.reset_at_unix.is_none());
        assert_eq!(rec.source, SOURCE_SIGNALS);
    }

    #[test]
    fn empty_or_malformed_signals_is_none() {
        assert!(parse_signals_str("").is_none());
        assert!(parse_signals_str("{not json").is_none());
        assert!(parse_signals_str(r#"{"turnCount":1}"#).is_none());
    }

    #[test]
    fn session_dir_prefers_active_then_signals() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let cwd = "/tmp/proj";
        let sid = "019f4d63-test-session";
        let session = home
            .join("sessions")
            .join(encode_cwd(cwd))
            .join(sid);
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("signals.json"), SIGNALS).unwrap();
        fs::write(
            home.join("active_sessions.json"),
            format!(
                r#"[{{"session_id":"{sid}","pid":1,"cwd":"{cwd}","opened_at":"2026-07-10T00:00:00Z"}}]"#
            ),
        )
        .unwrap();

        let usage = usage_for_cwd_in(home, cwd).expect("usage");
        assert_eq!(usage.used_pct, Some(58));
        assert_eq!(usage.session_id.as_deref(), Some(sid));
        assert_eq!(usage.confidence, Confidence::Official);
    }

    #[test]
    fn fallback_to_updates_when_signals_missing() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let cwd = "/tmp/proj";
        let sid = "sess-fallback";
        let session = home
            .join("sessions")
            .join(encode_cwd(cwd))
            .join(sid);
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("summary.json"),
            r#"{"info":{"id":"sess-fallback","cwd":"/tmp/proj"},"current_model_id":"grok-4.5"}"#,
        )
        .unwrap();
        fs::write(session.join("updates.jsonl"), UPDATES).unwrap();
        fs::write(
            home.join("models_cache.json"),
            r#"{"models":{"grok-4.5":{"info":{"context_window":500000}}}}"#,
        )
        .unwrap();

        let usage = usage_for_cwd_in(home, cwd).expect("usage");
        assert_eq!(usage.used_tokens, Some(105513));
        assert_eq!(usage.context_window_tokens, Some(500000));
        // 105513/500000 ≈ 21%
        assert_eq!(usage.used_pct, Some(21));
        assert_eq!(usage.confidence, Confidence::Estimated);
        assert_eq!(usage.source, SOURCE_UPDATES);
        assert!(!usage.notes.is_empty());

        let rec = record(&usage, "w1:p1", &PaneContext::default(), 0);
        assert_eq!(rec.agent.as_deref(), Some("grok"));
        assert_eq!(rec.confidence, Confidence::Estimated);
        // Privacy: serialized record must not include prompt text from updates.
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("secret prompt body"));
    }

    #[test]
    fn newest_session_directory_wins_without_active() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let cwd = "/tmp/proj";
        let group = home.join("sessions").join(encode_cwd(cwd));
        let old = group.join("old");
        let new = group.join("new");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(
            old.join("summary.json"),
            r#"{"info":{"cwd":"/tmp/proj"},"current_model_id":"grok-4.5"}"#,
        )
        .unwrap();
        fs::write(
            old.join("signals.json"),
            r#"{"contextWindowUsage":10,"contextTokensUsed":1,"contextWindowTokens":100,"primaryModelId":"grok-4.5"}"#,
        )
        .unwrap();
        // Ensure "new" is newer.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            new.join("summary.json"),
            r#"{"info":{"cwd":"/tmp/proj"},"current_model_id":"grok-4.5"}"#,
        )
        .unwrap();
        fs::write(new.join("signals.json"), SIGNALS).unwrap();
        // Touch new summary mtime explicitly.
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(new.join("summary.json"))
            .unwrap();
        f.write_all(b"\n").unwrap();

        let usage = usage_for_cwd_in(home, cwd).expect("usage");
        assert_eq!(usage.used_pct, Some(58));
        assert_eq!(usage.session_id.as_deref(), Some("new"));
    }

    #[test]
    fn no_session_for_unknown_cwd() {
        let root = tempfile::tempdir().unwrap();
        assert!(usage_for_cwd_in(root.path(), "/nowhere").is_none());
    }
}
