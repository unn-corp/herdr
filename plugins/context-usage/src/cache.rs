//! On-disk cache of per-pane context-window usage.
//!
//! One JSON file per pane lives under
//! `$cache_dir/panes/<safe-pane-id>.json`. Writes are atomic (write a temp
//! file next to the target, then rename) so a reader never observes a partial
//! record. The schema is deliberately small and stable — it is the contract
//! between the collectors that write it and any reader (the Herdr fork's
//! top-strip renderer, or `herdr-context-usage show`).
//!
//! Privacy: this record holds only counts, model names, pane/tab ids, and
//! timestamps. It never holds prompt or response text, file paths, or
//! transcript contents.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Current on-disk schema version. Bump only on a breaking field change; add
/// new optional fields without a bump.
pub const SCHEMA_VERSION: u32 = 1;

/// How confident the collector is in the numbers it recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// CLI/provider exposes usage/reset directly.
    Official,
    /// Calculated from transcript/tokenizer/model metadata.
    Estimated,
    /// Inferred from visible output or partial local state.
    Heuristic,
    /// Collector knows the pane/agent but no useful data exists.
    Unavailable,
}

/// A single pane's most recent context-usage snapshot.
///
/// Every field except the identity/bookkeeping ones is optional so a collector
/// can report exactly what it knows and nothing it is guessing. A reader must
/// treat a missing field as "unknown", not zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// On-disk schema version. See [`SCHEMA_VERSION`].
    pub schema: u32,
    /// Herdr pane id, e.g. `w1:p2`.
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tab_id: Option<String>,
    /// Detected agent label, e.g. `claude`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent: Option<String>,
    /// Which collector wrote this, e.g. `herdr-context-usage:claude-statusline`.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context_window_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub used_tokens: Option<u64>,
    /// Percent of the context window (or provider window) consumed, 0..=100.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub used_pct: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remaining_tokens: Option<u64>,
    /// Unix seconds at which the window resets, only when a machine-readable
    /// provider/CLI signal supplied it. Never synthesized.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reset_at_unix: Option<i64>,
    /// What kind of window `reset_at_unix` refers to, e.g. `five_hour`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub window_kind: Option<String>,
    /// Unix seconds when this record was written.
    pub updated_at_unix: i64,
    pub confidence: Confidence,
    /// Seconds after `updated_at_unix` beyond which a reader should treat this
    /// record as stale.
    pub stale_after_seconds: u64,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl UsageRecord {
    /// True when `now_unix` is past `updated_at_unix + stale_after_seconds`.
    pub fn is_stale(&self, now_unix: i64) -> bool {
        let deadline = self
            .updated_at_unix
            .saturating_add(self.stale_after_seconds as i64);
        now_unix > deadline
    }
}

/// Turn a pane id like `w1:p2` into a filesystem-safe stem `w1-p2`.
///
/// Any character outside `[A-Za-z0-9._-]` collapses to `-` so the id can never
/// escape the panes directory or collide with path separators.
pub fn safe_pane_stem(pane_id: &str) -> String {
    let mut out = String::with_capacity(pane_id.len());
    for ch in pane_id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("unknown");
    }
    out
}

/// The panes/ subdirectory holding one record per pane.
pub fn panes_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("panes")
}

/// Absolute path of the record file for `pane_id` under `cache_root`.
pub fn record_path(cache_root: &Path, pane_id: &str) -> PathBuf {
    panes_dir(cache_root).join(format!("{}.json", safe_pane_stem(pane_id)))
}

/// Write `record` for its pane atomically under `cache_root`.
///
/// Creates the panes directory if needed (best-effort `0700`), writes a temp
/// file, then renames it over the target so readers only ever see a complete
/// record.
pub fn write_record(cache_root: &Path, record: &UsageRecord) -> std::io::Result<()> {
    let dir = panes_dir(cache_root);
    fs::create_dir_all(&dir)?;
    restrict_dir_permissions(&dir);

    let target = record_path(cache_root, &record.pane_id);
    let tmp = dir.join(format!(".{}.tmp", safe_pane_stem(&record.pane_id)));

    let json = serde_json::to_vec_pretty(record)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&json)?;
        file.flush()?;
        restrict_file_permissions(&file);
    }
    fs::rename(&tmp, &target)
}

/// Read the record for `pane_id`, or `None` if it does not exist. A malformed
/// file surfaces as an error so `doctor` can report it.
pub fn read_record(cache_root: &Path, pane_id: &str) -> std::io::Result<Option<UsageRecord>> {
    let path = record_path(cache_root, pane_id);
    match fs::read(&path) {
        Ok(bytes) => {
            let record = serde_json::from_slice(&bytes)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
            Ok(Some(record))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Read every valid record under `cache_root/panes`. Skips files that fail to
/// parse rather than failing the whole scan.
pub fn read_all(cache_root: &Path) -> std::io::Result<Vec<UsageRecord>> {
    let dir = panes_dir(cache_root);
    let mut out = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(record) = serde_json::from_slice::<UsageRecord>(&bytes) {
                out.push(record);
            }
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn restrict_dir_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir_permissions(_dir: &Path) {}

#[cfg(unix)]
fn restrict_file_permissions(file: &fs::File) {
    use std::os::unix::fs::PermissionsExt;
    let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_file_permissions(_file: &fs::File) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pane: &str) -> UsageRecord {
        UsageRecord {
            schema: SCHEMA_VERSION,
            pane_id: pane.to_string(),
            workspace_id: Some("w1".to_string()),
            tab_id: Some("w1:t1".to_string()),
            agent: Some("claude".to_string()),
            source: "herdr-context-usage:claude-statusline".to_string(),
            model: Some("claude-sonnet-4".to_string()),
            model_family: Some("claude".to_string()),
            context_window_tokens: Some(200_000),
            used_tokens: Some(126_000),
            used_pct: Some(63),
            remaining_tokens: Some(74_000),
            reset_at_unix: Some(1_783_468_800),
            window_kind: Some("five_hour".to_string()),
            updated_at_unix: 1_783_459_000,
            confidence: Confidence::Official,
            stale_after_seconds: 1800,
            notes: vec![],
        }
    }

    #[test]
    fn safe_stem_neutralizes_separators() {
        assert_eq!(safe_pane_stem("w1:p2"), "w1-p2");
        // Slashes (the only real traversal risk in a flat filename) collapse to
        // '-'; dots are harmless in a single filename component and are kept.
        assert_eq!(safe_pane_stem("../../etc/passwd"), "..-..-etc-passwd");
        assert!(!safe_pane_stem("../../etc/passwd").contains('/'));
        assert_eq!(safe_pane_stem(""), "unknown");
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = sample("w1:p2");
        write_record(dir.path(), &record).expect("write");
        let read = read_record(dir.path(), "w1:p2")
            .expect("read")
            .expect("some");
        assert_eq!(read.used_pct, Some(63));
        assert_eq!(read.pane_id, "w1:p2");
        assert_eq!(read.confidence, Confidence::Official);
    }

    #[test]
    fn missing_record_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_record(dir.path(), "nope").expect("read").is_none());
    }

    #[test]
    fn staleness_uses_updated_plus_ttl() {
        let record = sample("w1:p2");
        assert!(!record.is_stale(1_783_459_000 + 1800));
        assert!(record.is_stale(1_783_459_000 + 1801));
    }

    #[test]
    fn read_all_collects_and_skips_garbage() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_record(dir.path(), &sample("w1:p1")).expect("write");
        write_record(dir.path(), &sample("w1:p2")).expect("write");
        // A malformed file must not fail the scan.
        std::fs::write(panes_dir(dir.path()).join("bad.json"), b"{ not json").expect("bad");
        let all = read_all(dir.path()).expect("read_all");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn optional_fields_omitted_when_none() {
        let mut record = sample("w1:p2");
        record.reset_at_unix = None;
        record.window_kind = None;
        let json = serde_json::to_string(&record).expect("json");
        assert!(!json.contains("reset_at_unix"));
        assert!(!json.contains("window_kind"));
    }
}
