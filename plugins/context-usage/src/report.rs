//! Shared reporting of a [`UsageRecord`] to Herdr's runtime.
//!
//! Every collector, after building a record, writes it to the pane cache and
//! reports it to Herdr via the `herdr pane report-usage` CLI so the top strip
//! can render from server state. The CLI transport (discrete flags mirroring
//! `report-metadata`) decouples collectors from Herdr's wire format and lets a
//! stock Herdr without the subcommand fall back to the cache file.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::cache::{self, Confidence, UsageRecord};

/// Persist `record` to the cache and report it to Herdr. Both are best-effort:
/// a cache or CLI failure is logged to stderr but never propagated, so a
/// collector can never break the agent it observes.
pub fn persist_and_report(cache_root: &Path, record: &UsageRecord) {
    if let Err(err) = cache::write_record(cache_root, record) {
        eprintln!("herdr-context-usage: cache write failed: {err}");
    }
    report_to_herdr(record);
}

/// Report the record to Herdr via `herdr pane report-usage`. A missing or older
/// Herdr (no such subcommand) fails silently; the cache file remains the source.
pub fn report_to_herdr(record: &UsageRecord) {
    let args = report_args(record);
    let bin = crate::context::herdr_bin();
    let result = Command::new(bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Err(err) = result {
        eprintln!("herdr-context-usage: report-usage failed: {err}");
    }
}

/// Build the `herdr pane report-usage` argument vector, including flags only for
/// fields that are actually known.
pub fn report_args(record: &UsageRecord) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "pane".into(),
        "report-usage".into(),
        // Pane id is positional, matching `herdr pane report-metadata`.
        record.pane_id.clone(),
        "--source".into(),
        record.source.clone(),
        "--confidence".into(),
        confidence_str(record.confidence).into(),
        "--ttl-ms".into(),
        record.stale_after_seconds.saturating_mul(1000).to_string(),
    ];
    let mut push = |flag: &str, value: String| {
        args.push(flag.to_string());
        args.push(value);
    };
    if let Some(agent) = &record.agent {
        push("--agent", agent.clone());
    }
    if let Some(model) = &record.model {
        push("--model", model.clone());
    }
    if let Some(pct) = record.used_pct {
        push("--used-pct", pct.to_string());
    }
    if let Some(t) = record.used_tokens {
        push("--used-tokens", t.to_string());
    }
    if let Some(t) = record.context_window_tokens {
        push("--context-window-tokens", t.to_string());
    }
    if let Some(t) = record.remaining_tokens {
        push("--remaining-tokens", t.to_string());
    }
    if let Some(reset) = record.reset_at_unix {
        push("--reset-at-unix", reset.to_string());
    }
    if let Some(kind) = &record.window_kind {
        push("--window-kind", kind.clone());
    }
    args
}

pub fn confidence_str(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Official => "official",
        Confidence::Estimated => "estimated",
        Confidence::Heuristic => "heuristic",
        Confidence::Unavailable => "unavailable",
    }
}

/// Percent of `window` consumed by `used`, rounded and clamped to 0..=100.
/// Returns `None` if the window is zero/unknown.
pub fn pct_of(used: u64, window: u64) -> Option<u8> {
    if window == 0 {
        return None;
    }
    let pct = ((used.min(window) as u128 * 100 + window as u128 / 2) / window as u128) as u8;
    Some(pct.min(100))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{UsageRecord, SCHEMA_VERSION};

    fn record() -> UsageRecord {
        UsageRecord {
            schema: SCHEMA_VERSION,
            pane_id: "w1:p2".into(),
            workspace_id: None,
            tab_id: None,
            agent: Some("codex".into()),
            source: "herdr-context-usage:codex-rollout".into(),
            model: Some("gpt-5.5".into()),
            model_family: Some("gpt".into()),
            context_window_tokens: Some(258400),
            used_tokens: Some(103757),
            used_pct: Some(40),
            remaining_tokens: None,
            reset_at_unix: None,
            window_kind: None,
            updated_at_unix: 1,
            confidence: Confidence::Official,
            stale_after_seconds: 60,
            notes: vec![],
        }
    }

    #[test]
    fn args_are_positional_pane_then_flags() {
        let args = report_args(&record());
        assert_eq!(args[0], "pane");
        assert_eq!(args[1], "report-usage");
        assert_eq!(args[2], "w1:p2");
        assert!(args.iter().any(|a| a == "--used-pct"));
        assert!(args.iter().any(|a| a == "--context-window-tokens"));
        // No reset flag when reset is unknown.
        assert!(!args.iter().any(|a| a == "--reset-at-unix"));
    }

    #[test]
    fn pct_of_rounds_and_clamps() {
        assert_eq!(pct_of(103757, 258400), Some(40));
        assert_eq!(pct_of(0, 100), Some(0));
        assert_eq!(pct_of(150, 100), Some(100));
        assert_eq!(pct_of(1, 0), None);
    }
}
