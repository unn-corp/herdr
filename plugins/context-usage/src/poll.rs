//! Poll pull-based agents across all Herdr panes.
//!
//! Push collectors (Claude, Antigravity) run inside the pane via a statusLine
//! hook. Pull collectors (Codex, OpenCode) have no per-render hook, so instead
//! this command asks Herdr for the full pane list, maps each pane to its
//! session by working directory, parses that session's local telemetry, and
//! reports usage. Run it once, or with `--watch` as a background daemon (for
//! example from the plugin's sidebar `watch` pane or a Herdr event hook).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::collectors::{codex, grok, hermes, opencode};
use crate::context::{herdr_bin, PaneContext};
use crate::report;

/// One pane as reported by `herdr pane list`.
#[derive(Debug, Deserialize)]
struct Pane {
    pane_id: String,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    tab_id: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground_cwd: Option<String>,
}

impl Pane {
    /// Best cwd to resolve a session: the foreground process's cwd if known,
    /// else the pane's cwd.
    fn effective_cwd(&self) -> Option<&str> {
        self.foreground_cwd.as_deref().or(self.cwd.as_deref())
    }

    fn context(&self) -> PaneContext {
        PaneContext {
            pane_id: Some(self.pane_id.clone()),
            tab_id: self.tab_id.clone(),
            workspace_id: self.workspace_id.clone(),
        }
    }
}

pub fn run(
    cache_root: &Path,
    watch: bool,
    interval_ms: u64,
) -> std::io::Result<std::process::ExitCode> {
    let interval = Duration::from_millis(interval_ms.max(500));
    loop {
        let n = poll_once(cache_root);
        if !watch {
            println!("polled {n} pane(s)");
            return Ok(std::process::ExitCode::SUCCESS);
        }
        std::thread::sleep(interval);
    }
}

/// Report usage for every pull-based pane once. Returns how many panes reported.
fn poll_once(cache_root: &Path) -> usize {
    let panes = match list_panes() {
        Ok(panes) => panes,
        Err(err) => {
            eprintln!("herdr-context-usage: pane list failed: {err}");
            return 0;
        }
    };
    let now = now_unix();
    let mut reported = 0;
    for pane in &panes {
        if collect_pane(cache_root, pane, now) {
            reported += 1;
        }
    }
    reported
}

/// Collect and report usage for one pane based on its agent. Returns true if a
/// record was reported.
fn collect_pane(cache_root: &Path, pane: &Pane, now_unix: i64) -> bool {
    let agent = pane.agent.as_deref().unwrap_or("");
    let Some(cwd) = pane.effective_cwd() else {
        return false;
    };
    match agent {
        "codex" => {
            if let Some(usage) = codex::usage_for_cwd(cwd) {
                let record = codex::record(&usage, &pane.pane_id, &pane.context(), now_unix);
                report::persist_and_report(cache_root, &record);
                return true;
            }
            false
        }
        "opencode" => {
            if let Some(usage) = opencode::usage_for_cwd(cwd) {
                let record = opencode::record(&usage, &pane.pane_id, &pane.context(), now_unix);
                report::persist_and_report(cache_root, &record);
                return true;
            }
            false
        }
        "hermes" => {
            // Collect even though Hermes defaults to prefer-native display: the
            // record still powers detail panels, and shows in the strip if the
            // user set hermes = "prefer-herdr".
            if let Some(usage) = hermes::usage_for_cwd(cwd) {
                let record = hermes::record(&usage, &pane.pane_id, &pane.context(), now_unix);
                report::persist_and_report(cache_root, &record);
                return true;
            }
            false
        }
        // Single Grok brand: process alias grok-build normalizes to agent "grok",
        // but tolerate the alias if it ever appears on a pane label.
        "grok" | "grok-build" => {
            if let Some(usage) = grok::usage_for_cwd(cwd) {
                let record = grok::record(&usage, &pane.pane_id, &pane.context(), now_unix);
                report::persist_and_report(cache_root, &record);
                return true;
            }
            false
        }
        // Push-based agents (claude, antigravity) report via their own hooks.
        _ => false,
    }
}

/// Run `herdr pane list` and parse the pane array out of the JSON envelope.
fn list_panes() -> std::io::Result<Vec<Pane>> {
    let output = Command::new(herdr_bin()).args(["pane", "list"]).output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "herdr pane list exited with {}",
            output.status
        )));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let panes = value
        .get("result")
        .and_then(|r| r.get("panes"))
        .cloned()
        .unwrap_or(value);
    let panes: Vec<Pane> = serde_json::from_value(panes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(panes)
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
    fn parses_pane_list_envelope() {
        let json = r#"{"id":"x","result":{"type":"pane_list","panes":[
            {"pane_id":"w1:p1","agent":"codex","cwd":"/a","foreground_cwd":"/a/b","workspace_id":"w1","tab_id":"w1:t1"},
            {"pane_id":"w1:p2","agent":"claude","cwd":"/c"}
        ]}}"#;
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let panes: Vec<Pane> = serde_json::from_value(value["result"]["panes"].clone()).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].effective_cwd(), Some("/a/b"));
        assert_eq!(panes[1].effective_cwd(), Some("/c"));
        assert_eq!(panes[0].context().workspace_id.as_deref(), Some("w1"));
    }

    #[test]
    fn non_pull_agents_are_skipped() {
        let pane = Pane {
            pane_id: "w1:p2".into(),
            workspace_id: None,
            tab_id: None,
            agent: Some("claude".into()),
            cwd: Some("/c".into()),
            foreground_cwd: None,
        };
        let dir = tempfile::tempdir().unwrap();
        assert!(!collect_pane(dir.path(), &pane, 0));
    }
}
