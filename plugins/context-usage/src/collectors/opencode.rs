//! OpenCode collector.
//!
//! OpenCode keeps its sessions in a local SQLite store at
//! `$XDG_DATA_HOME/opencode/opencode.db`. The `session` table has a `directory`
//! column (the project dir, which maps to a Herdr pane's cwd) plus the model;
//! each assistant `message.data` JSON blob carries per-turn `tokens.input` (the
//! current context occupancy) and `modelID`. We read the most recent assistant
//! message of the session whose directory matches the pane, and size it against
//! a model context-window registry.
//!
//! Read-only, WAL-safe: we open the DB `OPEN_READ_ONLY` so a live OpenCode
//! writing to it is never blocked. OpenCode exposes no provider reset timer, so
//! none is reported.

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::cache::{Confidence, UsageRecord, SCHEMA_VERSION};
use crate::context::PaneContext;

pub const SOURCE: &str = "herdr-context-usage:opencode-db";
const DEFAULT_STALE_AFTER_SECONDS: u64 = 900;

/// Usage extracted from an OpenCode session.
#[derive(Debug, Default, PartialEq)]
pub struct OpenCodeUsage {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Input tokens of the most recent assistant turn: current context size.
    pub used_tokens: Option<u64>,
}

/// Resolve the OpenCode SQLite DB path (honors `XDG_DATA_HOME`).
pub fn db_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("opencode").join("opencode.db"));
        }
    }
    directories::BaseDirs::new().map(|d| d.data_dir().join("opencode").join("opencode.db"))
}

/// Read usage for the OpenCode session whose `directory` matches `cwd`.
pub fn usage_for_cwd(cwd: &str) -> Option<OpenCodeUsage> {
    let path = db_path()?;
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    query_usage(&conn, cwd)
}

/// Query the newest session for `cwd` and its most recent assistant tokens.
/// Separated from [`usage_for_cwd`] so it can be tested against an in-memory DB.
fn query_usage(conn: &Connection, cwd: &str) -> Option<OpenCodeUsage> {
    // Newest session rooted at this directory.
    let (session_id, model): (String, Option<String>) = conn
        .query_row(
            "SELECT id, model FROM session WHERE directory = ?1 \
             ORDER BY time_updated DESC LIMIT 1",
            [cwd],
            |row| Ok((row.get(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .ok()?;

    let mut usage = OpenCodeUsage {
        session_id: Some(session_id.clone()),
        // session.model is stored as a JSON object ({"id":...}); normalize it as
        // a fallback. The per-turn message modelID (below) is preferred.
        model: model.and_then(|m| clean_model(&m)),
        ..Default::default()
    };

    // Walk assistant messages newest-first; take the first with token input.
    let mut stmt = conn
        .prepare(
            "SELECT data FROM message WHERE session_id = ?1 \
             ORDER BY time_created DESC LIMIT 50",
        )
        .ok()?;
    let rows = stmt
        .query_map([&session_id], |row| row.get::<_, String>(0))
        .ok()?;
    for data in rows.flatten() {
        if let Some((tokens, model, provider)) = assistant_tokens(&data) {
            usage.used_tokens = Some(tokens);
            // The turn's own modelID is the authoritative, clean model name.
            if model.is_some() {
                usage.model = model;
            }
            usage.provider = provider;
            break;
        }
    }

    usage.used_tokens?;
    Some(usage)
}

/// Normalize a `session.model` value, which OpenCode stores as a JSON object
/// like `{"id":"nemotron-3-ultra-free","providerID":"opencode"}` (extract the
/// `id`), or occasionally a bare string.
fn clean_model(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(id) = value.get("id").and_then(Value::as_str) {
                return Some(id.to_string());
            }
        }
        return None;
    }
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Extract `(input_tokens, model, provider)` from an assistant message's `data`
/// JSON blob, or `None` if it is not an assistant turn with token info.
fn assistant_tokens(data: &str) -> Option<(u64, Option<String>, Option<String>)> {
    let value: Value = serde_json::from_str(data).ok()?;
    if value.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let input = value.pointer("/tokens/input").and_then(Value::as_u64)?;
    let model = value
        .get("modelID")
        .and_then(Value::as_str)
        .map(str::to_string);
    let provider = value
        .get("providerID")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((input, model, provider))
}

/// Build a [`UsageRecord`] for a pane from parsed OpenCode usage.
pub fn record(
    usage: &OpenCodeUsage,
    pane_id: &str,
    ctx: &PaneContext,
    now_unix: i64,
) -> UsageRecord {
    let window = usage
        .model
        .as_deref()
        .and_then(crate::models::context_window);
    let used_pct = match (usage.used_tokens, window) {
        (Some(used), Some(w)) => crate::report::pct_of(used, w),
        _ => None,
    };
    // OpenCode reports token counts directly, but the window comes from our
    // registry, so any percentage is an estimate, not an official provider
    // figure. Tokens present (with or without a window) is still estimated.
    let confidence = if usage.used_tokens.is_some() {
        Confidence::Estimated
    } else {
        Confidence::Unavailable
    };
    let mut notes = Vec::new();
    if used_pct.is_none() && usage.used_tokens.is_some() {
        notes.push("context window unknown for model; showing tokens only".to_string());
    }

    UsageRecord {
        schema: SCHEMA_VERSION,
        pane_id: pane_id.to_string(),
        workspace_id: ctx.workspace_id.clone(),
        tab_id: ctx.tab_id.clone(),
        agent: Some("opencode".to_string()),
        source: SOURCE.to_string(),
        model: usage.model.clone(),
        model_family: usage.provider.clone(),
        context_window_tokens: window,
        used_tokens: usage.used_tokens,
        used_pct,
        remaining_tokens: match (window, usage.used_tokens) {
            (Some(w), Some(u)) => Some(w.saturating_sub(u)),
            _ => None,
        },
        reset_at_unix: None,
        window_kind: Some("context".to_string()),
        updated_at_unix: now_unix,
        confidence,
        stale_after_seconds: DEFAULT_STALE_AFTER_SECONDS,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT, directory TEXT, model TEXT, time_updated INTEGER);
             CREATE TABLE message (session_id TEXT, time_created INTEGER, data TEXT);
             INSERT INTO session VALUES ('ses_old','/proj','claude-sonnet-4',100);
             INSERT INTO session VALUES ('ses_new','/proj','claude-sonnet-4',200);
             INSERT INTO session VALUES ('ses_other','/elsewhere','gpt-5',300);
             INSERT INTO message VALUES ('ses_new',10,'{\"role\":\"user\"}');
             INSERT INTO message VALUES ('ses_new',20,'{\"role\":\"assistant\",\"modelID\":\"claude-sonnet-4\",\"providerID\":\"anthropic\",\"tokens\":{\"input\":35091,\"output\":166}}');
             INSERT INTO message VALUES ('ses_new',30,'{\"role\":\"user\"}');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn assistant_tokens_parses_input() {
        let data = r#"{"role":"assistant","modelID":"m","providerID":"p","tokens":{"input":42}}"#;
        assert_eq!(
            assistant_tokens(data),
            Some((42, Some("m".into()), Some("p".into())))
        );
        assert_eq!(assistant_tokens(r#"{"role":"user"}"#), None);
    }

    #[test]
    fn picks_newest_session_and_last_assistant_tokens() {
        let conn = seed_db();
        let usage = query_usage(&conn, "/proj").expect("usage");
        assert_eq!(usage.session_id.as_deref(), Some("ses_new"));
        assert_eq!(usage.used_tokens, Some(35091));
        assert_eq!(usage.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn clean_model_extracts_id_from_json() {
        assert_eq!(
            clean_model(r#"{"id":"nemotron-3-ultra-free","providerID":"opencode"}"#).as_deref(),
            Some("nemotron-3-ultra-free")
        );
        assert_eq!(
            clean_model("claude-sonnet-4").as_deref(),
            Some("claude-sonnet-4")
        );
        assert_eq!(clean_model("{}"), None);
    }

    #[test]
    fn no_session_for_unknown_dir() {
        let conn = seed_db();
        assert!(query_usage(&conn, "/nowhere").is_none());
    }

    #[test]
    fn record_estimates_percentage_from_registry() {
        let usage = OpenCodeUsage {
            session_id: Some("s".into()),
            model: Some("claude-sonnet-4".into()),
            provider: Some("anthropic".into()),
            used_tokens: Some(100_000),
        };
        let rec = record(&usage, "w1:p2", &PaneContext::default(), 0);
        // 100000 / 200000 = 50%, estimated (window from registry).
        assert_eq!(rec.used_pct, Some(50));
        assert_eq!(rec.confidence, Confidence::Estimated);
        assert_eq!(rec.agent.as_deref(), Some("opencode"));
        assert!(rec.reset_at_unix.is_none());
    }

    #[test]
    fn record_tokens_only_when_window_unknown() {
        let usage = OpenCodeUsage {
            session_id: Some("s".into()),
            model: Some("bespoke-xyz".into()),
            provider: None,
            used_tokens: Some(1234),
        };
        let rec = record(&usage, "w1:p2", &PaneContext::default(), 0);
        assert_eq!(rec.used_pct, None);
        assert_eq!(rec.used_tokens, Some(1234));
        assert!(!rec.notes.is_empty());
    }
}
