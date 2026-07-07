//! Install/uninstall the collectors into each CLI's own config.
//!
//! Phase 1 handles Claude Code: point its `statusLine.command` at our
//! collector, preserving any command the user already had by recording it in a
//! chain file the collector replays. Every mutation backs up the settings file
//! first and is idempotent — re-running install does not stack wrappers, and
//! uninstall restores the prior command (or removes the key we added).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::collectors::claude;

/// Unique subcommand our wrapper always invokes; recognizing it lets us detect
/// our own command regardless of how the binary path is spelled.
const CLAUDE_SUBCOMMAND: &str = "collect claude";

/// Outcome of an install/uninstall step, for user-facing reporting.
#[derive(Debug)]
pub struct StepReport {
    pub agent: &'static str,
    pub changed: bool,
    pub detail: String,
}

/// Resolve Claude Code's settings.json, honoring `CLAUDE_CONFIG_DIR`.
pub fn claude_settings_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("settings.json"));
        }
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".claude").join("settings.json"))
}

/// The command Claude should invoke: our current binary + `collect claude`.
fn our_command() -> std::io::Result<String> {
    let exe = std::env::current_exe()?;
    Ok(format!(
        "{} {CLAUDE_SUBCOMMAND}",
        shell_quote(&exe.to_string_lossy())
    ))
}

/// Install the Claude Code statusLine collector.
pub fn install_claude(
    cache_root: &Path,
    settings_override: Option<&Path>,
) -> std::io::Result<StepReport> {
    let path = match settings_override {
        Some(p) => p.to_path_buf(),
        None => claude_settings_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot resolve Claude config dir",
            )
        })?,
    };

    let mut root = read_json_object(&path)?;
    let command = our_command()?;

    let existing = root
        .get("statusLine")
        .and_then(Value::as_object)
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if let Some(existing) = &existing {
        if is_ours(existing) {
            return Ok(StepReport {
                agent: "claude",
                changed: false,
                detail: format!("already installed at {}", path.display()),
            });
        }
        // Preserve the user's real command so the collector can chain it.
        write_chain_file(cache_root, existing)?;
    }

    backup_once(&path)?;

    let status_line = root
        .entry("statusLine")
        .or_insert_with(|| Value::Object(Map::new()));
    let status_obj = status_line.as_object_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "statusLine is not an object",
        )
    })?;
    status_obj.insert("type".to_string(), Value::String("command".to_string()));
    status_obj.insert("command".to_string(), Value::String(command));

    write_json_object(&path, &root)?;

    let detail = match existing {
        Some(prev) if !is_ours(&prev) => {
            format!(
                "installed at {} (chaining previous statusLine)",
                path.display()
            )
        }
        _ => format!("installed at {}", path.display()),
    };
    Ok(StepReport {
        agent: "claude",
        changed: true,
        detail,
    })
}

/// Uninstall the Claude Code statusLine collector, restoring any chained
/// command or removing the statusLine entry we added.
pub fn uninstall_claude(
    cache_root: &Path,
    settings_override: Option<&Path>,
) -> std::io::Result<StepReport> {
    let path = match settings_override {
        Some(p) => p.to_path_buf(),
        None => claude_settings_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot resolve Claude config dir",
            )
        })?,
    };

    if !path.exists() {
        return Ok(StepReport {
            agent: "claude",
            changed: false,
            detail: "no Claude settings file".to_string(),
        });
    }

    let mut root = read_json_object(&path)?;
    let current = root
        .get("statusLine")
        .and_then(Value::as_object)
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let Some(current) = current else {
        return Ok(StepReport {
            agent: "claude",
            changed: false,
            detail: "no statusLine configured".to_string(),
        });
    };
    if !is_ours(&current) {
        return Ok(StepReport {
            agent: "claude",
            changed: false,
            detail: "statusLine is not ours; left untouched".to_string(),
        });
    }

    backup_once(&path)?;

    let chained = read_chain_file(cache_root);
    if let Some(chained) = &chained {
        if let Some(obj) = root.get_mut("statusLine").and_then(Value::as_object_mut) {
            obj.insert("command".to_string(), Value::String(chained.clone()));
        }
    } else {
        root.remove("statusLine");
    }
    remove_chain_file(cache_root);
    write_json_object(&path, &root)?;

    let detail = if chained.is_some() {
        "uninstalled (restored previous statusLine)".to_string()
    } else {
        "uninstalled (removed statusLine)".to_string()
    };
    Ok(StepReport {
        agent: "claude",
        changed: true,
        detail,
    })
}

/// True when a command string is our wrapper.
pub fn is_ours(command: &str) -> bool {
    command.contains(CLAUDE_SUBCOMMAND)
}

fn write_chain_file(cache_root: &Path, command: &str) -> std::io::Result<()> {
    fs::create_dir_all(cache_root)?;
    fs::write(claude::chain_file_path(cache_root), command)
}

fn read_chain_file(cache_root: &Path) -> Option<String> {
    let contents = fs::read_to_string(claude::chain_file_path(cache_root)).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn remove_chain_file(cache_root: &Path) {
    let _ = fs::remove_file(claude::chain_file_path(cache_root));
}

/// Read a JSON file as an object, defaulting to an empty object when absent.
fn read_json_object(path: &Path) -> std::io::Result<Map<String, Value>> {
    match fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok(Map::new()),
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
            match value {
                Value::Object(map) => Ok(map),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "settings root is not a JSON object",
                )),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(err) => Err(err),
    }
}

/// Write an object as pretty JSON, creating parent dirs.
fn write_json_object(path: &Path, root: &Map<String, Value>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(&Value::Object(root.clone()))
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(path, json)
}

/// Back up the settings file once (never overwrite an existing backup, so the
/// pristine pre-install state is preserved across repeated installs).
fn backup_once(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let backup = path.with_extension("json.herdr-context-usage.bak");
    if backup.exists() {
        return Ok(());
    }
    fs::copy(path, backup).map(|_| ())
}

/// Single-quote a string for `sh -c`, escaping embedded single quotes.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_our_command() {
        assert!(is_ours("/usr/bin/herdr-context-usage collect claude"));
        assert!(is_ours("'/opt/herdr-context-usage' collect claude"));
        assert!(!is_ours("starship prompt"));
        assert!(!is_ours("herdr-context-usage show"));
    }

    #[test]
    fn shell_quote_leaves_simple_paths() {
        assert_eq!(
            shell_quote("/usr/bin/herdr-context-usage"),
            "/usr/bin/herdr-context-usage"
        );
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn install_into_empty_settings_sets_statusline() {
        let cache = tempfile::tempdir().expect("cache");
        let settings_dir = tempfile::tempdir().expect("settings");
        let settings = settings_dir.path().join("settings.json");

        let report = install_claude(cache.path(), Some(&settings)).expect("install");
        assert!(report.changed);

        let root = read_json_object(&settings).expect("read");
        let command = root["statusLine"]["command"].as_str().expect("command");
        assert!(is_ours(command));
        // No chain file when there was nothing to preserve.
        assert!(read_chain_file(cache.path()).is_none());
    }

    #[test]
    fn install_preserves_existing_statusline_as_chain() {
        let cache = tempfile::tempdir().expect("cache");
        let settings_dir = tempfile::tempdir().expect("settings");
        let settings = settings_dir.path().join("settings.json");
        fs::write(
            &settings,
            r#"{"statusLine": {"type": "command", "command": "starship prompt"}, "theme": "dark"}"#,
        )
        .expect("seed");

        install_claude(cache.path(), Some(&settings)).expect("install");
        assert_eq!(
            read_chain_file(cache.path()).as_deref(),
            Some("starship prompt")
        );
        // Unrelated keys survive.
        let root = read_json_object(&settings).expect("read");
        assert_eq!(root["theme"], Value::String("dark".to_string()));
        // Backup captured the pristine file.
        assert!(settings
            .with_extension("json.herdr-context-usage.bak")
            .exists());
    }

    #[test]
    fn install_is_idempotent() {
        let cache = tempfile::tempdir().expect("cache");
        let settings_dir = tempfile::tempdir().expect("settings");
        let settings = settings_dir.path().join("settings.json");
        install_claude(cache.path(), Some(&settings)).expect("first");
        let second = install_claude(cache.path(), Some(&settings)).expect("second");
        assert!(!second.changed);
    }

    #[test]
    fn uninstall_restores_chained_command() {
        let cache = tempfile::tempdir().expect("cache");
        let settings_dir = tempfile::tempdir().expect("settings");
        let settings = settings_dir.path().join("settings.json");
        fs::write(
            &settings,
            r#"{"statusLine": {"type": "command", "command": "starship prompt"}}"#,
        )
        .expect("seed");

        install_claude(cache.path(), Some(&settings)).expect("install");
        let report = uninstall_claude(cache.path(), Some(&settings)).expect("uninstall");
        assert!(report.changed);

        let root = read_json_object(&settings).expect("read");
        assert_eq!(
            root["statusLine"]["command"],
            Value::String("starship prompt".to_string())
        );
        assert!(read_chain_file(cache.path()).is_none());
    }

    #[test]
    fn uninstall_without_chain_removes_statusline() {
        let cache = tempfile::tempdir().expect("cache");
        let settings_dir = tempfile::tempdir().expect("settings");
        let settings = settings_dir.path().join("settings.json");
        install_claude(cache.path(), Some(&settings)).expect("install");
        uninstall_claude(cache.path(), Some(&settings)).expect("uninstall");
        let root = read_json_object(&settings).expect("read");
        assert!(!root.contains_key("statusLine"));
    }

    #[test]
    fn uninstall_leaves_foreign_statusline() {
        let cache = tempfile::tempdir().expect("cache");
        let settings_dir = tempfile::tempdir().expect("settings");
        let settings = settings_dir.path().join("settings.json");
        fs::write(
            &settings,
            r#"{"statusLine": {"type": "command", "command": "starship prompt"}}"#,
        )
        .expect("seed");
        let report = uninstall_claude(cache.path(), Some(&settings)).expect("uninstall");
        assert!(!report.changed);
        let root = read_json_object(&settings).expect("read");
        assert_eq!(
            root["statusLine"]["command"],
            Value::String("starship prompt".to_string())
        );
    }
}
