//! Claude Code statusLine collector.
//!
//! Claude Code invokes a configured `statusLine.command` on every render,
//! piping a JSON status payload on stdin and displaying whatever the command
//! prints on stdout. We install ourselves as that command (chaining any command
//! the user already had), harvest the usage fields out of the payload, write a
//! pane-aware [`UsageRecord`], and reprint the status line so the user sees no
//! change.
//!
//! Only fields Claude reports as machine-readable are recorded. `rate_limits`
//! is present on Pro/Max-style sessions; it may be absent on API-key, Bedrock,
//! Vertex, or Enterprise sessions, in which case we record an `Unavailable`
//! percentage rather than fabricating one, and never clobber a prior good file
//! with invented numbers.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::cache::{self, Confidence, UsageRecord, SCHEMA_VERSION};
use crate::context::PaneContext;

/// Source tag stamped on records this collector writes.
pub const SOURCE: &str = "herdr-context-usage:claude-statusline";
/// Env var the installer sets to the user's pre-existing statusLine command, if
/// any, so we can chain it and preserve their status line.
pub const CHAIN_ENV: &str = "HERDR_CONTEXT_USAGE_CLAUDE_CHAIN";
/// Default staleness horizon: a Claude statusLine renders often, so 30 min of
/// silence means the pane is idle/closed and the record should read as stale.
const DEFAULT_STALE_AFTER_SECONDS: u64 = 1800;

/// Parsed, provider-agnostic view of the fields we care about in a payload.
#[derive(Debug, Default, PartialEq)]
pub struct ClaudeStatus {
    pub model_id: Option<String>,
    pub used_pct: Option<u8>,
    pub reset_at_unix: Option<i64>,
    pub window_kind: Option<String>,
    /// True when the payload carried a `rate_limits` block at all.
    pub had_rate_limits: bool,
}

/// Extract the usage-relevant fields from a Claude statusLine JSON payload.
///
/// Defensive by construction: it walks a [`Value`] rather than a fixed struct,
/// so an unexpected shape yields `None`s instead of an error.
pub fn parse_payload(raw: &str) -> ClaudeStatus {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return ClaudeStatus::default();
    };

    let model_id = value
        .pointer("/model/id")
        .and_then(Value::as_str)
        .map(str::to_string);

    let rate_limits = value.get("rate_limits");
    let had_rate_limits = rate_limits.is_some();

    // Prefer the five-hour window (the one users watch); fall back to seven-day.
    let (window, window_kind) = rate_limits
        .and_then(|rl| {
            rl.get("five_hour")
                .map(|w| (w, "five_hour"))
                .or_else(|| rl.get("seven_day").map(|w| (w, "seven_day")))
        })
        .map(|(w, k)| (Some(w), Some(k.to_string())))
        .unwrap_or((None, None));

    let used_pct = window
        .and_then(|w| w.get("used_percentage"))
        .and_then(as_f64_flexible)
        .map(clamp_pct);

    let reset_at_unix = window
        .and_then(|w| w.get("resets_at"))
        .and_then(as_unix_seconds);

    ClaudeStatus {
        model_id,
        used_pct,
        // Only carry a window kind when we actually pulled a percentage from it.
        window_kind: used_pct.and(window_kind),
        reset_at_unix: used_pct.and(reset_at_unix),
        had_rate_limits,
    }
}

/// Build the [`UsageRecord`] for a parsed status in a given pane at `now_unix`.
pub fn record_from_status(
    status: &ClaudeStatus,
    ctx: &PaneContext,
    now_unix: i64,
) -> Option<UsageRecord> {
    let pane_id = ctx.pane_id.clone()?;

    let mut notes = Vec::new();
    let (used_pct, confidence) = if let Some(pct) = status.used_pct {
        (Some(pct), Confidence::Official)
    } else {
        if !status.had_rate_limits {
            notes.push("payload had no rate_limits block".to_string());
        }
        (None, Confidence::Unavailable)
    };

    let model_family = status.model_id.as_deref().map(model_family_of);

    Some(UsageRecord {
        schema: SCHEMA_VERSION,
        pane_id,
        workspace_id: ctx.workspace_id.clone(),
        tab_id: ctx.tab_id.clone(),
        agent: Some("claude".to_string()),
        source: SOURCE.to_string(),
        model: status.model_id.clone(),
        model_family,
        context_window_tokens: None,
        used_tokens: None,
        used_pct,
        remaining_tokens: None,
        reset_at_unix: status.reset_at_unix,
        window_kind: status.window_kind.clone(),
        updated_at_unix: now_unix,
        confidence,
        stale_after_seconds: DEFAULT_STALE_AFTER_SECONDS,
        notes,
    })
}

/// Full statusLine run: read stdin, harvest, persist, report to Herdr, then
/// reprint the status line so Claude shows what it always did.
///
/// Returns the string that was printed (for tests). Errors from caching or
/// reporting are swallowed to a note on stderr — a broken harvester must never
/// break the user's status line.
pub fn run_statusline(cache_root: &Path) -> String {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);

    let ctx = PaneContext::from_env();
    let status = parse_payload(&raw);
    let now_unix = now_unix();

    if ctx.in_pane() {
        if let Some(record) = record_from_status(&status, &ctx, now_unix) {
            // Never clobber a prior good record with an "unavailable" one.
            let should_write = record.confidence != Confidence::Unavailable
                || !prior_was_useful(cache_root, &record.pane_id, now_unix);
            if should_write {
                if let Err(err) = cache::write_record(cache_root, &record) {
                    eprintln!("herdr-context-usage: cache write failed: {err}");
                }
                report_to_herdr(&record);
            }
        }
    }

    let line = status_line_output(cache_root, &raw, &status);
    print!("{line}");
    line
}

/// Whether a currently-cached record still holds usable, non-stale data.
fn prior_was_useful(cache_root: &Path, pane_id: &str, now_unix: i64) -> bool {
    match cache::read_record(cache_root, pane_id) {
        Ok(Some(prior)) => prior.confidence != Confidence::Unavailable && !prior.is_stale(now_unix),
        _ => false,
    }
}

/// Report the record to Herdr's runtime via the `herdr pane report-usage` CLI,
/// so the top strip can render from server state. Best-effort: a missing or
/// older Herdr (no `report-usage` subcommand) simply fails silently and the
/// cache-file path is used instead. Discrete flags mirror `report-metadata`
/// rather than coupling to the plugin's cache JSON shape.
fn report_to_herdr(record: &UsageRecord) {
    let args = herdr_report_args(record);
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

/// Build the `herdr pane report-usage` argument vector from a record. Only
/// includes flags for fields that are actually known.
fn herdr_report_args(record: &UsageRecord) -> Vec<String> {
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

fn confidence_str(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Official => "official",
        Confidence::Estimated => "estimated",
        Confidence::Heuristic => "heuristic",
        Confidence::Unavailable => "unavailable",
    }
}

/// Decide what to print as the status line: chain the user's original command
/// if one was recorded (via env or the installer's chain file), otherwise print
/// a compact default.
fn status_line_output(cache_root: &Path, raw: &str, status: &ClaudeStatus) -> String {
    if let Some(chain) = chained_command(cache_root) {
        if let Some(out) = run_chained(&chain, raw) {
            return out;
        }
    }
    default_status_line(status)
}

/// Resolve the user's original statusLine command, if any. Env override first
/// (for tests/manual use), then the installer's chain file.
fn chained_command(cache_root: &Path) -> Option<String> {
    if let Ok(chain) = std::env::var(CHAIN_ENV) {
        if !chain.trim().is_empty() {
            return Some(chain);
        }
    }
    let path = chain_file_path(cache_root);
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Path where the installer records the pre-existing Claude statusLine command.
pub fn chain_file_path(cache_root: &Path) -> std::path::PathBuf {
    cache_root.join("claude-chain")
}

/// Run the chained statusLine command, feeding it the same payload and
/// returning its stdout. `None` on any failure so we fall back to a default.
fn run_chained(command: &str, raw: &str) -> Option<String> {
    use std::io::Write;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(raw.as_bytes());
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// A minimal, informative default when the user had no statusLine of their own.
fn default_status_line(status: &ClaudeStatus) -> String {
    match status.used_pct {
        Some(pct) => format!("ctx {pct}%"),
        None => String::new(),
    }
}

/// Coarse model family from a model id, e.g. `claude-sonnet-4-...` -> `claude`.
fn model_family_of(model_id: &str) -> String {
    model_id
        .split('-')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(model_id)
        .to_string()
}

fn clamp_pct(value: f64) -> u8 {
    value.round().clamp(0.0, 100.0) as u8
}

/// Accept a JSON number or a numeric string as an `f64`.
fn as_f64_flexible(value: &Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        return Some(n);
    }
    value.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}

/// Interpret a `resets_at` value as Unix seconds. Accepts an integer, a numeric
/// string, or an RFC3339 timestamp. Returns `None` for anything we cannot read
/// as a concrete instant — we never synthesize a reset time.
fn as_unix_seconds(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(n) = value.as_f64() {
        return Some(n as i64);
    }
    let s = value.as_str()?.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    parse_rfc3339_to_unix(s)
}

/// Parse an RFC3339 / ISO8601 UTC timestamp (`2026-01-02T15:04:05Z` and common
/// offset forms) into Unix seconds without pulling in a date crate.
///
/// Supports a trailing `Z` or a `±HH:MM` offset and ignores fractional seconds.
/// Returns `None` for anything it cannot fully parse.
fn parse_rfc3339_to_unix(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    // Separator is 'T' or ' '.
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let minute: i64 = s.get(14..16)?.parse().ok()?;
    let second: i64 = s.get(17..19)?.parse().ok()?;

    // Offset handling: default UTC; support Z or ±HH:MM at the end.
    let mut offset_seconds: i64 = 0;
    let rest = &s[19..];
    // Skip optional fractional seconds ".123".
    let rest = rest.strip_prefix('.').map_or(rest, |frac| {
        let non_digit = frac
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(frac.len());
        &frac[non_digit..]
    });
    if !(rest.is_empty() || rest.eq_ignore_ascii_case("z")) {
        let sign = match rest.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return None,
        };
        let oh: i64 = rest.get(1..3)?.parse().ok()?;
        let om: i64 = rest.get(4..6)?.parse().ok()?;
        offset_seconds = sign * (oh * 3600 + om * 60);
    }

    let days = days_from_civil(year, month, day)?;
    let utc = days * 86_400 + hour * 3600 + minute * 60 + second;
    Some(utc - offset_seconds)
}

/// Days since the Unix epoch (1970-01-01) for a civil date. Howard Hinnant's
/// `days_from_civil` algorithm. Returns `None` for out-of-range months/days.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
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
    fn parses_five_hour_percentage_and_reset() {
        let raw = r#"{
            "model": {"id": "claude-sonnet-4-20250514", "display_name": "Sonnet 4"},
            "rate_limits": {
                "five_hour": {"used_percentage": 63, "resets_at": 1783468800},
                "seven_day": {"used_percentage": 12}
            }
        }"#;
        let status = parse_payload(raw);
        assert_eq!(status.model_id.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(status.used_pct, Some(63));
        assert_eq!(status.reset_at_unix, Some(1783468800));
        assert_eq!(status.window_kind.as_deref(), Some("five_hour"));
        assert!(status.had_rate_limits);
    }

    #[test]
    fn missing_rate_limits_yields_unavailable() {
        let raw = r#"{"model": {"id": "claude-opus-4-20250101"}}"#;
        let status = parse_payload(raw);
        assert_eq!(status.used_pct, None);
        assert!(!status.had_rate_limits);

        let ctx = PaneContext {
            pane_id: Some("w1:p2".to_string()),
            ..Default::default()
        };
        let record = record_from_status(&status, &ctx, 1_783_459_000).expect("record");
        assert_eq!(record.confidence, Confidence::Unavailable);
        assert_eq!(record.used_pct, None);
        assert_eq!(record.model_family.as_deref(), Some("claude"));
    }

    #[test]
    fn falls_back_to_seven_day_when_five_hour_absent() {
        let raw = r#"{"rate_limits": {"seven_day": {"used_percentage": 80, "resets_at": 111}}}"#;
        let status = parse_payload(raw);
        assert_eq!(status.used_pct, Some(80));
        assert_eq!(status.window_kind.as_deref(), Some("seven_day"));
        assert_eq!(status.reset_at_unix, Some(111));
    }

    #[test]
    fn accepts_percentage_as_string_and_rounds() {
        let raw = r#"{"rate_limits": {"five_hour": {"used_percentage": "62.6"}}}"#;
        assert_eq!(parse_payload(raw).used_pct, Some(63));
    }

    #[test]
    fn clamps_out_of_range_percentage() {
        let raw = r#"{"rate_limits": {"five_hour": {"used_percentage": 250}}}"#;
        assert_eq!(parse_payload(raw).used_pct, Some(100));
    }

    #[test]
    fn garbage_payload_is_empty_status() {
        assert_eq!(parse_payload("not json"), ClaudeStatus::default());
    }

    #[test]
    fn no_pane_id_means_no_record() {
        let status = parse_payload(r#"{"rate_limits": {"five_hour": {"used_percentage": 5}}}"#);
        let ctx = PaneContext::default();
        assert!(record_from_status(&status, &ctx, 0).is_none());
    }

    #[test]
    fn rfc3339_reset_z() {
        let raw = r#"{"rate_limits": {"five_hour": {"used_percentage": 5, "resets_at": "2026-01-02T15:04:05Z"}}}"#;
        // 2026-01-02T15:04:05Z == 1767366245
        assert_eq!(parse_payload(raw).reset_at_unix, Some(1767366245));
    }

    #[test]
    fn rfc3339_reset_with_offset_and_fraction() {
        // 2026-01-02T15:04:05.250+02:00 == 13:04:05Z == 1767358... let's compute:
        // 15:04:05 +02:00 -> 13:04:05Z. Epoch for 2026-01-02T13:04:05Z = 1767359045.
        let raw = r#"{"rate_limits": {"five_hour": {"used_percentage": 5, "resets_at": "2026-01-02T15:04:05.250+02:00"}}}"#;
        assert_eq!(parse_payload(raw).reset_at_unix, Some(1767359045));
    }

    #[test]
    fn default_status_line_shows_pct_or_empty() {
        let with = ClaudeStatus {
            used_pct: Some(42),
            ..Default::default()
        };
        assert_eq!(default_status_line(&with), "ctx 42%");
        assert_eq!(default_status_line(&ClaudeStatus::default()), "");
    }
}
