//! Antigravity CLI collector (statusLine harvester).
//!
//! Antigravity (Google) exposes a statusLine JSON hook like Claude Code. The
//! exact payload shape has not been captured from a live session yet, so this
//! parser is deliberately defensive: it walks a [`Value`] looking for usage and
//! reset fields under several plausible locations and reports only what it finds
//! with an honest confidence, never fabricating a value.
//!
//! UNVERIFIED: once a real Antigravity statusLine payload is captured, tighten
//! the field paths and add an install path for its settings file. Until then a
//! run against an unexpected shape simply reports nothing.

use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::cache::{Confidence, UsageRecord, SCHEMA_VERSION};
use crate::context::PaneContext;

pub const SOURCE: &str = "herdr-context-usage:antigravity-statusline";
const DEFAULT_STALE_AFTER_SECONDS: u64 = 1800;

/// Parsed usage from an Antigravity statusLine payload.
#[derive(Debug, Default, PartialEq)]
pub struct AntigravityStatus {
    pub model: Option<String>,
    pub used_pct: Option<u8>,
    pub reset_at_unix: Option<i64>,
    /// True when any structured usage field was present (vs. an empty shape).
    pub had_usage: bool,
}

/// Defensively extract usage from an Antigravity statusLine JSON payload.
pub fn parse_payload(raw: &str) -> AntigravityStatus {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return AntigravityStatus::default();
    };

    let model = first_str(
        &value,
        &[
            "/model/id",
            "/model",
            "/model/name",
            "/activeModel",
            "/model_id",
        ],
    );

    // used percentage, tried under a few plausible containers.
    let used_pct = first_f64(
        &value,
        &[
            "/context/used_percentage",
            "/context/usedPercentage",
            "/usage/used_percentage",
            "/rate_limits/five_hour/used_percentage",
            "/tokens/used_percentage",
        ],
    )
    .map(clamp_pct);

    let reset_at_unix = first_reset(
        &value,
        &[
            "/context/resets_at",
            "/usage/resets_at",
            "/rate_limits/five_hour/resets_at",
            "/resets_at",
        ],
    );

    AntigravityStatus {
        model,
        had_usage: used_pct.is_some(),
        used_pct,
        reset_at_unix: used_pct.and(reset_at_unix),
    }
}

/// Build a record for a pane from a parsed status, or `None` without a pane id.
pub fn record(status: &AntigravityStatus, ctx: &PaneContext, now_unix: i64) -> Option<UsageRecord> {
    let pane_id = ctx.pane_id.clone()?;
    let confidence = if status.used_pct.is_some() {
        // From a structured statusLine field, so official; falls back otherwise.
        Confidence::Official
    } else {
        Confidence::Unavailable
    };
    Some(UsageRecord {
        schema: SCHEMA_VERSION,
        pane_id,
        workspace_id: ctx.workspace_id.clone(),
        tab_id: ctx.tab_id.clone(),
        agent: Some("antigravity".to_string()),
        source: SOURCE.to_string(),
        model: status.model.clone(),
        model_family: status
            .model
            .as_deref()
            .map(|m| m.split(['-', '/']).next().unwrap_or(m).to_string()),
        context_window_tokens: None,
        used_tokens: None,
        used_pct: status.used_pct,
        remaining_tokens: None,
        reset_at_unix: status.reset_at_unix,
        window_kind: status.used_pct.map(|_| "context".to_string()),
        updated_at_unix: now_unix,
        confidence,
        stale_after_seconds: DEFAULT_STALE_AFTER_SECONDS,
        notes: Vec::new(),
    })
}

/// statusLine entrypoint: read stdin, harvest, report, and print a compact line.
pub fn run_statusline(cache_root: &Path) -> String {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let ctx = PaneContext::from_env();
    let status = parse_payload(&raw);
    if ctx.in_pane() && status.had_usage {
        if let Some(record) = record(&status, &ctx, now_unix()) {
            crate::report::persist_and_report(cache_root, &record);
        }
    }
    let line = match status.used_pct {
        Some(pct) => format!("ctx {pct}%"),
        None => String::new(),
    };
    print!("{line}");
    line
}

fn first_str(value: &Value, paths: &[&str]) -> Option<String> {
    paths
        .iter()
        .find_map(|p| value.pointer(p).and_then(Value::as_str))
        .map(str::to_string)
}

fn first_f64(value: &Value, paths: &[&str]) -> Option<f64> {
    paths.iter().find_map(|p| {
        value.pointer(p).and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
    })
}

fn first_reset(value: &Value, paths: &[&str]) -> Option<i64> {
    paths.iter().find_map(|p| {
        value.pointer(p).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_f64().map(|n| n as i64))
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
    })
}

fn clamp_pct(value: f64) -> u8 {
    value.round().clamp(0.0, 100.0) as u8
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_context_used_percentage() {
        let raw = r#"{"model":{"id":"gemini-2.5-pro"},"context":{"used_percentage":72,"resets_at":1783468800}}"#;
        let s = parse_payload(raw);
        assert_eq!(s.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(s.used_pct, Some(72));
        assert_eq!(s.reset_at_unix, Some(1783468800));
        assert!(s.had_usage);
    }

    #[test]
    fn unknown_shape_reports_nothing() {
        let s = parse_payload(r#"{"something":"else"}"#);
        assert!(!s.had_usage);
        assert_eq!(s.used_pct, None);
        let rec = record(
            &s,
            &PaneContext {
                pane_id: Some("w1:p2".into()),
                ..Default::default()
            },
            0,
        )
        .unwrap();
        assert_eq!(rec.confidence, Confidence::Unavailable);
    }

    #[test]
    fn percentage_as_string_is_accepted() {
        let raw = r#"{"usage":{"used_percentage":"55.4"}}"#;
        assert_eq!(parse_payload(raw).used_pct, Some(55));
    }

    #[test]
    fn no_pane_no_record() {
        let s = parse_payload(r#"{"context":{"used_percentage":5}}"#);
        assert!(record(&s, &PaneContext::default(), 0).is_none());
    }
}
