//! `herdr-context-usage` — collects AI agent context-window and reset usage for
//! Herdr panes and reports it so Herdr's top strip can render per-pane usage.
//!
//! Phase 1 supports Claude Code via its statusLine hook. See the crate README
//! and the plan at `_arcwright-output/specs/herdr-context-window-usage-plan.md`.

mod cache;
mod collectors;
mod context;
mod install;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use crate::context::{resolve_cache_root, PaneContext};

#[derive(Parser, Debug)]
#[command(
    name = "herdr-context-usage",
    about = "Collect AI agent context-window and reset usage for Herdr panes.",
    version
)]
struct Cli {
    /// Cache directory root (overrides HERDR_CONTEXT_USAGE_CACHE_DIR and the
    /// platform default).
    #[arg(long, global = true)]
    cache_dir: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Harvest usage for an agent. Wired into the agent's own hook (for Claude,
    /// its statusLine command); reads the hook payload on stdin.
    Collect {
        /// Agent to collect for (currently only `claude`).
        agent: String,
    },
    /// Install collectors into each supported agent's config.
    Install {
        /// Path to the agent settings file (advanced/testing; defaults to the
        /// agent's standard location).
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// Remove collectors, restoring prior config.
    Uninstall {
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// Print cached usage for a pane or all panes.
    Show {
        /// Pane id; defaults to $HERDR_PANE_ID.
        #[arg(long)]
        pane: Option<String>,
        /// Show every cached pane.
        #[arg(long)]
        all: bool,
        /// Emit JSON instead of a human line.
        #[arg(long)]
        json: bool,
    },
    /// Continuously print cached usage (for a Herdr sidebar pane).
    Watch {
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 2000)]
        interval_ms: u64,
    },
    /// Diagnose the collector setup.
    Doctor,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let cache_root = resolve_cache_root(cli.cache_dir.as_deref());

    let result = match cli.command {
        Command::Collect { agent } => run_collect(&agent, &cache_root),
        Command::Install { settings } => run_install(&cache_root, settings.as_deref()),
        Command::Uninstall { settings } => run_uninstall(&cache_root, settings.as_deref()),
        Command::Show { pane, all, json } => run_show(&cache_root, pane, all, json),
        Command::Watch { interval_ms } => run_watch(&cache_root, interval_ms),
        Command::Doctor => run_doctor(&cache_root),
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("herdr-context-usage: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_collect(
    agent: &str,
    cache_root: &std::path::Path,
) -> std::io::Result<std::process::ExitCode> {
    match agent {
        "claude" => {
            // Prints the status line to stdout as a side effect.
            let _ = collectors::claude::run_statusline(cache_root);
            Ok(std::process::ExitCode::SUCCESS)
        }
        other => {
            eprintln!("herdr-context-usage: unknown collect agent '{other}' (supported: claude)");
            Ok(std::process::ExitCode::FAILURE)
        }
    }
}

fn run_install(
    cache_root: &std::path::Path,
    settings: Option<&std::path::Path>,
) -> std::io::Result<std::process::ExitCode> {
    let report = install::install_claude(cache_root, settings)?;
    print_step(&report);
    Ok(std::process::ExitCode::SUCCESS)
}

fn run_uninstall(
    cache_root: &std::path::Path,
    settings: Option<&std::path::Path>,
) -> std::io::Result<std::process::ExitCode> {
    let report = install::uninstall_claude(cache_root, settings)?;
    print_step(&report);
    Ok(std::process::ExitCode::SUCCESS)
}

fn print_step(report: &install::StepReport) {
    let marker = if report.changed { "✔" } else { "•" };
    println!("{marker} [{}] {}", report.agent, report.detail);
}

fn run_show(
    cache_root: &std::path::Path,
    pane: Option<String>,
    all: bool,
    json: bool,
) -> std::io::Result<std::process::ExitCode> {
    let now = now_unix();
    if all {
        let records = cache::read_all(cache_root)?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&records).unwrap_or_default()
            );
        } else if records.is_empty() {
            println!("no cached usage");
        } else {
            for record in &records {
                println!("{}", human_line(record, now));
            }
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }

    let pane_id = pane
        .or_else(|| PaneContext::from_env().pane_id)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no pane given and $HERDR_PANE_ID is unset",
            )
        })?;

    match cache::read_record(cache_root, &pane_id)? {
        Some(record) if json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&record).unwrap_or_default()
            )
        }
        Some(record) => println!("{}", human_line(&record, now)),
        None if json => println!("null"),
        None => println!("no cached usage for {pane_id}"),
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn run_watch(
    cache_root: &std::path::Path,
    interval_ms: u64,
) -> std::io::Result<std::process::ExitCode> {
    let interval = std::time::Duration::from_millis(interval_ms.max(200));
    loop {
        let now = now_unix();
        let records = cache::read_all(cache_root)?;
        // Clear screen + home, then list panes with usage.
        print!("\x1b[2J\x1b[H");
        if records.is_empty() {
            println!("context usage: (no data)");
        } else {
            println!("context usage");
            for record in &records {
                println!("  {}", human_line(record, now));
            }
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::thread::sleep(interval);
    }
}

fn run_doctor(cache_root: &std::path::Path) -> std::io::Result<std::process::ExitCode> {
    let mut ok = true;
    println!("herdr-context-usage doctor");

    let ctx = PaneContext::from_env();
    if ctx.in_pane() {
        println!(
            "  [ok]   running inside a Herdr pane ({})",
            ctx.pane_id.as_deref().unwrap_or("?")
        );
    } else {
        println!(
            "  [warn] not inside a Herdr pane (HERDR_PANE_ID unset); collectors need pane env"
        );
    }

    println!("  [info] cache root: {}", cache_root.display());
    match writable_probe(cache_root) {
        Ok(()) => println!("  [ok]   cache root is writable"),
        Err(err) => {
            ok = false;
            println!("  [fail] cache root not writable: {err}");
        }
    }

    match install::claude_settings_path() {
        Some(path) if path.exists() => {
            let installed = claude_status_installed(&path);
            match installed {
                Some(true) => println!(
                    "  [ok]   Claude statusLine collector installed ({})",
                    path.display()
                ),
                Some(false) => {
                    println!("  [warn] Claude settings present but collector not installed; run `install`")
                }
                None => println!(
                    "  [warn] could not read Claude settings at {}",
                    path.display()
                ),
            }
        }
        Some(path) => println!(
            "  [warn] no Claude settings at {} yet; run `install` after first launch",
            path.display()
        ),
        None => println!("  [warn] could not resolve Claude config dir"),
    }

    let now = now_unix();
    match cache::read_all(cache_root) {
        Ok(records) if records.is_empty() => println!("  [info] no cached pane usage yet"),
        Ok(records) => {
            let stale = records.iter().filter(|r| r.is_stale(now)).count();
            println!("  [info] {} cached pane(s), {} stale", records.len(), stale);
        }
        Err(err) => {
            ok = false;
            println!("  [fail] reading cache: {err}");
        }
    }

    Ok(if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

fn claude_status_installed(path: &std::path::Path) -> Option<bool> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let command = value.pointer("/statusLine/command")?.as_str()?;
    Some(install::is_ours(command))
}

fn writable_probe(cache_root: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_root)?;
    let probe = cache_root.join(".write-probe");
    std::fs::write(&probe, b"ok")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// A compact one-line human summary of a record, e.g.
/// `w1:p2 claude 63% resets in 2h14m (official)`.
fn human_line(record: &cache::UsageRecord, now_unix: i64) -> String {
    let agent = record.agent.as_deref().unwrap_or("?");
    let pct = record
        .used_pct
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "--".to_string());
    let mut out = format!("{} {agent} {pct}", record.pane_id);
    if let Some(reset) = record.reset_at_unix {
        if let Some(rel) = humanize_reset(reset, now_unix) {
            out.push_str(&format!(" resets in {rel}"));
        }
    }
    out.push_str(&format!(" ({:?})", record.confidence).to_lowercase());
    if record.is_stale(now_unix) {
        out.push_str(" [stale]");
    }
    out
}

/// Render seconds-until-reset as `2h14m`, `44m`, or `<1m`. `None` if already
/// past (a stale reset we should not display as a countdown).
fn humanize_reset(reset_at_unix: i64, now_unix: i64) -> Option<String> {
    let remaining = reset_at_unix - now_unix;
    if remaining <= 0 {
        return None;
    }
    let minutes = remaining / 60;
    let hours = minutes / 60;
    let mins = minutes % 60;
    Some(if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "<1m".to_string()
    })
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
    use crate::cache::{Confidence, UsageRecord, SCHEMA_VERSION};

    fn record() -> UsageRecord {
        UsageRecord {
            schema: SCHEMA_VERSION,
            pane_id: "w1:p2".to_string(),
            workspace_id: None,
            tab_id: None,
            agent: Some("claude".to_string()),
            source: "s".to_string(),
            model: None,
            model_family: None,
            context_window_tokens: None,
            used_tokens: None,
            used_pct: Some(63),
            remaining_tokens: None,
            reset_at_unix: Some(1000 + 2 * 3600 + 14 * 60),
            window_kind: None,
            updated_at_unix: 1000,
            confidence: Confidence::Official,
            stale_after_seconds: 1800,
            notes: vec![],
        }
    }

    #[test]
    fn human_line_includes_pct_and_reset() {
        let line = human_line(&record(), 1000);
        assert!(line.contains("w1:p2"));
        assert!(line.contains("63%"));
        assert!(line.contains("resets in 2h14m"));
        assert!(line.contains("(official)"));
    }

    #[test]
    fn humanize_reset_formats() {
        assert_eq!(humanize_reset(1000 + 8000, 1000).as_deref(), Some("2h13m"));
        assert_eq!(humanize_reset(1000 + 600, 1000).as_deref(), Some("10m"));
        assert_eq!(humanize_reset(1000 + 30, 1000).as_deref(), Some("<1m"));
        assert_eq!(humanize_reset(1000, 1000), None);
    }

    #[test]
    fn stale_record_is_flagged() {
        let line = human_line(&record(), 1000 + 5000);
        assert!(line.contains("[stale]"));
    }
}
