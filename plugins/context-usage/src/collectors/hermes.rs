//! Hermes CLI collector (fallback for `prefer-herdr` mode).
//!
//! Hermes renders its own native context bar, so Herdr defers to it by default
//! (`[ui.context_usage.native] hermes = "prefer-native"`). When a user opts a
//! Hermes pane into `prefer-herdr`, this collector supplies the number from
//! Hermes's local SQLite state at `~/.hermes/state.db`. The `sessions` table has
//! a `cwd` (maps to the pane) and a `model`, but its token columns are session
//! *totals* (`input_tokens`, `cache_read_tokens` summed over every API call),
//! and per-message `token_count` is not populated, so neither gives the current
//! context size directly.
//!
//! We estimate current context occupancy as the average total input per API
//! call: `(input_tokens + cache_read_tokens) / api_call_count`. Each call
//! submits the active conversation as input (a cached prefix plus fresh tokens),
//! so this approximates the typical context window fill far better than the
//! cumulative total, which would clamp to 100% on any real session. It is an
//! estimate — reported with `estimated` confidence, never an official provider
//! percentage. No reset timer exists in Hermes state.

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

use crate::cache::{Confidence, UsageRecord, SCHEMA_VERSION};
use crate::context::PaneContext;

pub const SOURCE: &str = "herdr-context-usage:hermes-state";
const DEFAULT_STALE_AFTER_SECONDS: u64 = 900;

/// Usage extracted from a Hermes session.
#[derive(Debug, Default, PartialEq)]
pub struct HermesUsage {
    pub session_id: Option<String>,
    pub model: Option<String>,
    /// Estimated current context occupancy: average total input per API call.
    pub used_tokens: Option<u64>,
}

/// Resolve the Hermes state DB (`$HERMES_HOME/state.db`, else `~/.hermes`).
pub fn state_db() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("HERMES_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("state.db"));
        }
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".hermes").join("state.db"))
}

/// Read usage for the Hermes session rooted at `cwd`.
pub fn usage_for_cwd(cwd: &str) -> Option<HermesUsage> {
    let path = state_db()?;
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    // Hermes may be writing; wait briefly rather than failing on a busy DB.
    let _ = conn.busy_timeout(std::time::Duration::from_millis(200));
    query_usage(&conn, cwd)
}

/// Newest (active-first) session for `cwd` that has made at least one API call.
/// Estimates current context as the average total input per API call.
fn query_usage(conn: &Connection, cwd: &str) -> Option<HermesUsage> {
    let (session_id, model, input_tokens, cache_read_tokens, api_call_count): (
        String,
        Option<String>,
        i64,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT id, model, COALESCE(input_tokens, 0), COALESCE(cache_read_tokens, 0), \
                    COALESCE(api_call_count, 0) FROM sessions \
             WHERE cwd = ?1 AND COALESCE(api_call_count, 0) > 0 \
             ORDER BY (ended_at IS NULL) DESC, started_at DESC LIMIT 1",
            [cwd],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .ok()?;

    // Average total input (cached prefix + fresh) per API call.
    let calls = api_call_count.max(1);
    let total_input = input_tokens.saturating_add(cache_read_tokens);
    let per_call = u64::try_from(total_input / calls).ok()?;

    Some(HermesUsage {
        session_id: Some(session_id),
        model: model.filter(|m| !m.is_empty()),
        used_tokens: (per_call > 0).then_some(per_call),
    })
}

/// Build a [`UsageRecord`] for a pane from parsed Hermes usage.
pub fn record(usage: &HermesUsage, pane_id: &str, ctx: &PaneContext, now_unix: i64) -> UsageRecord {
    let window = usage
        .model
        .as_deref()
        .and_then(crate::models::context_window);
    let used_pct = match (usage.used_tokens, window) {
        (Some(used), Some(w)) => crate::report::pct_of(used, w),
        _ => None,
    };
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
        agent: Some("hermes".to_string()),
        source: SOURCE.to_string(),
        model: usage.model.clone(),
        model_family: usage
            .model
            .as_deref()
            .map(|m| m.rsplit('/').next().unwrap_or(m))
            .map(|m| m.split(['-', ':']).next().unwrap_or(m).to_string()),
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
        // Columns: input_tokens, cache_read_tokens, api_call_count are session
        // totals; current context is estimated as (input+cache_read)/api_calls.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, cwd TEXT, model TEXT, input_tokens INTEGER, \
                 cache_read_tokens INTEGER, api_call_count INTEGER, started_at INTEGER, \
                 ended_at INTEGER);
             INSERT INTO sessions VALUES ('old','/proj','codex/gpt-5.5',300000,100000,8,100,150);
             INSERT INTO sessions VALUES ('active','/proj','codex/gpt-5.5',600000,400000,10,200,NULL);
             INSERT INTO sessions VALUES ('nocalls','/proj','codex/gpt-5.5',0,0,0,300,NULL);
             INSERT INTO sessions VALUES ('elsewhere','/other','agent-fallback',50000,10000,6,400,NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn prefers_active_session_and_averages_input_per_call() {
        let conn = seed_db();
        let usage = query_usage(&conn, "/proj").expect("usage");
        assert_eq!(usage.session_id.as_deref(), Some("active"));
        // (600000 + 400000) / 10 api calls = 100000 tokens of current context.
        assert_eq!(usage.used_tokens, Some(100000));
        assert_eq!(usage.model.as_deref(), Some("codex/gpt-5.5"));
    }

    #[test]
    fn record_estimates_from_registry_and_strips_provider_prefix() {
        let conn = seed_db();
        let usage = query_usage(&conn, "/proj").unwrap();
        let rec = record(&usage, "w1:p2", &PaneContext::default(), 0);
        // gpt-5.5 -> 400000 window; 100000/400000 = 25%.
        assert_eq!(rec.used_pct, Some(25));
        assert_eq!(rec.confidence, Confidence::Estimated);
        assert_eq!(rec.agent.as_deref(), Some("hermes"));
        // model_family strips the "codex/" provider prefix and version.
        assert_eq!(rec.model_family.as_deref(), Some("gpt"));
        assert!(rec.reset_at_unix.is_none());
    }

    #[test]
    fn unknown_model_reports_tokens_only() {
        let conn = seed_db();
        let usage = query_usage(&conn, "/other").unwrap();
        // (50000 + 10000) / 6 = 10000 estimated current context.
        assert_eq!(usage.used_tokens, Some(10000));
        let rec = record(&usage, "w1:p2", &PaneContext::default(), 0);
        // "agent-fallback" is not in the registry, so no percentage.
        assert_eq!(rec.used_pct, None);
        assert_eq!(rec.used_tokens, Some(10000));
        assert!(!rec.notes.is_empty());
    }

    #[test]
    fn session_with_no_api_calls_is_skipped() {
        let conn = seed_db();
        // 'nocalls' has api_call_count=0; only 'active'/'old' qualify for /proj.
        let usage = query_usage(&conn, "/proj").unwrap();
        assert_ne!(usage.session_id.as_deref(), Some("nocalls"));
    }

    #[test]
    fn no_session_for_unknown_cwd() {
        let conn = seed_db();
        assert!(query_usage(&conn, "/nowhere").is_none());
    }
}
