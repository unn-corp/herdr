//! Runtime context the collector reads from its environment.
//!
//! A collector runs *inside* a Herdr pane (as a Claude Code statusLine command,
//! for example), so Herdr's injected `HERDR_*` env vars tell it which pane it
//! belongs to. The cache root is resolved once here so every subcommand agrees
//! on where records live.

use std::path::PathBuf;

/// Env var Herdr injects with the pane id, e.g. `w1:p2`.
pub const PANE_ID_ENV: &str = "HERDR_PANE_ID";
/// Env var Herdr injects with the tab id.
pub const TAB_ID_ENV: &str = "HERDR_TAB_ID";
/// Env var Herdr injects with the workspace id.
pub const WORKSPACE_ID_ENV: &str = "HERDR_WORKSPACE_ID";
/// Env var Herdr injects with the absolute path to the `herdr` binary.
pub const BIN_PATH_ENV: &str = "HERDR_BIN_PATH";
/// Optional override for where records are cached.
pub const CACHE_DIR_ENV: &str = "HERDR_CONTEXT_USAGE_CACHE_DIR";

/// The pane a collector is reporting for, discovered from the environment.
#[derive(Debug, Clone, Default)]
pub struct PaneContext {
    pub pane_id: Option<String>,
    pub tab_id: Option<String>,
    pub workspace_id: Option<String>,
}

impl PaneContext {
    /// Read pane identity from the current process environment.
    pub fn from_env() -> Self {
        PaneContext {
            pane_id: non_empty_env(PANE_ID_ENV),
            tab_id: non_empty_env(TAB_ID_ENV),
            workspace_id: non_empty_env(WORKSPACE_ID_ENV),
        }
    }

    /// True when we are actually running inside a Herdr pane.
    pub fn in_pane(&self) -> bool {
        self.pane_id.is_some()
    }
}

/// Resolve the cache root, in priority order:
/// 1. explicit `--cache-dir` (passed in as `override_dir`),
/// 2. `HERDR_CONTEXT_USAGE_CACHE_DIR`,
/// 3. the platform cache dir + `herdr-context-usage`,
/// 4. `~/.cache/herdr-context-usage` as a last resort.
pub fn resolve_cache_root(override_dir: Option<&str>) -> PathBuf {
    if let Some(dir) = override_dir.filter(|d| !d.is_empty()) {
        return expand_tilde(dir);
    }
    if let Some(dir) = non_empty_env(CACHE_DIR_ENV) {
        return expand_tilde(&dir);
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        return dirs.cache_dir().join("herdr-context-usage");
    }
    expand_tilde("~/.cache/herdr-context-usage")
}

/// Resolve the `herdr` binary to invoke for API reporting: prefer the
/// `HERDR_BIN_PATH` Herdr injects, else fall back to `herdr` on `PATH`.
pub fn herdr_bin() -> String {
    non_empty_env(BIN_PATH_ENV).unwrap_or_else(|| "herdr".to_string())
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(dirs) = directories::BaseDirs::new() {
            return dirs.home_dir().join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_dir_wins() {
        let root = resolve_cache_root(Some("/tmp/custom-cache"));
        assert_eq!(root, PathBuf::from("/tmp/custom-cache"));
    }

    #[test]
    fn empty_override_is_ignored() {
        // Falls through to a non-empty resolution.
        let root = resolve_cache_root(Some(""));
        assert!(root.to_string_lossy().contains("herdr-context-usage"));
    }
}
