//! The optional one-row system monitor strip drawn at the top of a space.
//!
//! Pure rendering: it reads the latest [`SystemSample`] off [`AppState`] and
//! draws a compact `CPU / RAM / GPU` line. Sampling happens in the app loop
//! (`App::sync_system_monitor`); this module never touches the OS.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::state::Palette;
use crate::app::AppState;
use crate::platform::SystemSample;

/// Draw the one-row CPU / RAM / GPU usage strip into `area`.
pub(super) fn render_system_monitor(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let p = &app.palette;
    let background = Style::default().bg(p.surface0);

    let mut spans = match &app.system_monitor {
        Some(sample) => metric_spans(sample, p),
        None => vec![Span::styled(" sampling…", Style::default().fg(p.overlay0))],
    };
    // Per-tab segments, left to right: context usage (to the left of git so GPU
    // stays directly right of RAM), then git status per conversation.
    if let Some(tab) = app
        .active
        .and_then(|idx| app.workspaces.get(idx))
        .and_then(|ws| ws.tabs.get(ws.active_tab))
    {
        push_context_usage_spans(&mut spans, tab, app, p);
        push_tab_git_spans(&mut spans, tab, &app.pane_git, p);
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(background), area);
}

/// Draw the per-pane context-window usage for the active tab, e.g.
/// `ctx claude 63% ▰▰▰▰▰▰▱▱ 2h14m`. Panes without a known percentage are
/// omitted (usage is shown only when a provider actually reports it).
fn push_context_usage_spans(
    spans: &mut Vec<Span<'static>>,
    tab: &crate::workspace::Tab,
    app: &AppState,
    p: &Palette,
) {
    let cfg = &app.context_usage;
    if !cfg.enabled {
        return;
    }
    let now = std::time::Instant::now();
    let now_unix = now_unix_seconds();

    // Stable left-to-right order: pane ids sort deterministically.
    let mut pane_ids: Vec<crate::layout::PaneId> = tab.panes.keys().copied().collect();
    pane_ids.sort_by_key(|id| id.raw());

    let entries: Vec<&crate::api::schema::PaneUsageInfo> = pane_ids
        .into_iter()
        .filter_map(|pane_id| app.effective_pane_usage(pane_id, now))
        .filter(|info| info.used_pct.is_some())
        .collect();
    if entries.is_empty() {
        return;
    }

    push_separator(spans, p);
    spans.push(Span::styled("ctx", Style::default().fg(p.subtext0)));
    for info in entries {
        spans.push(Span::raw(" "));
        if let Some(agent) = &info.agent {
            spans.push(Span::styled(
                format!("{agent} "),
                Style::default().fg(p.subtext0),
            ));
        }
        if cfg.show_model {
            if let Some(model) = &info.model {
                spans.push(Span::styled(
                    format!("{model} "),
                    Style::default().fg(p.overlay0),
                ));
            }
        }
        if let Some(pct) = info.used_pct {
            spans.push(Span::styled(
                format!("{pct}%"),
                Style::default()
                    .fg(usage_color(pct, p))
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            push_usage_bar(spans, pct, cfg.bar_width, p);
            if cfg.show_reset {
                if let Some(reset) = info.reset_at_unix {
                    if let Some(rel) = humanize_reset(reset, now_unix) {
                        spans.push(Span::styled(
                            format!(" {rel}"),
                            Style::default().fg(p.subtext0),
                        ));
                    }
                }
            }
        }
    }
}

/// Draw a `bar_width`-cell bar filled proportionally to `pct`, using the same
/// threshold color as the numeric value.
fn push_usage_bar(spans: &mut Vec<Span<'static>>, pct: u8, bar_width: u16, p: &Palette) {
    let width = bar_width.clamp(1, 20) as usize;
    let filled = (((pct as usize * width) + 50) / 100).min(width);
    if filled > 0 {
        spans.push(Span::styled(
            "▰".repeat(filled),
            Style::default().fg(usage_color(pct, p)),
        ));
    }
    if filled < width {
        spans.push(Span::styled(
            "▱".repeat(width - filled),
            Style::default().fg(p.overlay0),
        ));
    }
}

/// Render seconds-until-reset as `2h14m` or `44m`. `None` once the reset time
/// has passed, so a stale countdown is never shown.
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

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn push_tab_git_spans(
    spans: &mut Vec<Span<'static>>,
    tab: &crate::workspace::Tab,
    pane_git: &std::collections::HashMap<crate::layout::PaneId, crate::workspace::PaneGitStatus>,
    p: &Palette,
) {
    // Collect the distinct (branch, dirty) among the tab's panes, then sort for a
    // stable left-to-right order (pane ids have no meaningful ordering here).
    let mut shown: Vec<(Option<String>, Option<usize>)> = Vec::new();
    for pane_id in tab.panes.keys() {
        let Some(status) = pane_git.get(pane_id) else {
            continue;
        };
        if status.branch.is_none() && status.dirty.unwrap_or(0) == 0 {
            continue;
        }
        let key = (status.branch.clone(), status.dirty);
        if !shown.contains(&key) {
            shown.push(key);
        }
    }
    shown.sort();
    for (branch, dirty) in shown {
        push_separator(spans, p);
        if let Some(branch) = branch {
            spans.push(Span::styled(
                branch,
                Style::default().fg(p.mauve).add_modifier(Modifier::BOLD),
            ));
        }
        match dirty {
            Some(count) if count > 0 => spans.push(Span::styled(
                format!("  +{count}"),
                Style::default().fg(p.yellow).add_modifier(Modifier::BOLD),
            )),
            Some(_) => spans.push(Span::styled("  ✓", Style::default().fg(p.green))),
            None => {}
        }
    }
}

fn metric_spans(sample: &SystemSample, p: &Palette) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(" ")];
    push_metric(&mut spans, "CPU", sample.cpu_pct, p);
    push_separator(&mut spans, p);
    push_metric(&mut spans, "RAM", sample.ram_pct, p);
    if let Some(gpu) = sample.gpu {
        push_separator(&mut spans, p);
        push_metric(&mut spans, "GPU", Some(gpu.util_pct), p);
        if let Some(vram) = gpu.vram_pct {
            spans.push(Span::styled(
                format!(" ({vram}%)"),
                Style::default().fg(p.subtext0),
            ));
        }
    }
    spans
}

fn push_separator(spans: &mut Vec<Span<'static>>, p: &Palette) {
    spans.push(Span::styled("   ", Style::default().fg(p.overlay0)));
}

fn push_metric(spans: &mut Vec<Span<'static>>, label: &str, value: Option<u8>, p: &Palette) {
    spans.push(Span::styled(
        format!("{label} "),
        Style::default().fg(p.subtext0),
    ));
    match value {
        Some(pct) => spans.push(Span::styled(
            format!("{pct:>3}%"),
            Style::default()
                .fg(usage_color(pct, p))
                .add_modifier(Modifier::BOLD),
        )),
        None => spans.push(Span::styled("  --", Style::default().fg(p.overlay0))),
    }
}

/// Green under 70%, amber up to 90%, red at or above 90%.
fn usage_color(pct: u8, p: &Palette) -> Color {
    if pct >= 90 {
        p.red
    } else if pct >= 70 {
        p.yellow
    } else {
        p.green
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::GpuSample;
    use ratatui::{backend::TestBackend, Terminal};

    fn render_to_string(app: &AppState, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("terminal");
        terminal
            .draw(|frame| render_system_monitor(app, frame, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..width)
            .map(|x| {
                buffer
                    .cell((x, 0))
                    .map_or(' ', |cell| cell.symbol().chars().next().unwrap_or(' '))
            })
            .collect()
    }

    #[test]
    fn renders_cpu_ram_gpu_segments() {
        let mut app = AppState::test_new();
        app.system_monitor_enabled = true;
        app.system_monitor = Some(SystemSample {
            cpu_pct: Some(12),
            ram_pct: Some(34),
            gpu: Some(GpuSample {
                util_pct: 7,
                vram_pct: Some(40),
            }),
        });
        let text = render_to_string(&app, 60);
        assert!(text.contains("CPU"), "missing CPU: {text:?}");
        assert!(text.contains("12%"), "missing cpu value: {text:?}");
        assert!(text.contains("RAM"), "missing RAM: {text:?}");
        assert!(text.contains("GPU"), "missing GPU: {text:?}");
        assert!(text.contains("7%"), "missing gpu util: {text:?}");
        assert!(text.contains("(40%)"), "missing vram in parens: {text:?}");
    }

    #[test]
    fn hides_gpu_segment_when_absent() {
        let mut app = AppState::test_new();
        app.system_monitor_enabled = true;
        app.system_monitor = Some(SystemSample {
            cpu_pct: Some(5),
            ram_pct: Some(50),
            gpu: None,
        });
        let text = render_to_string(&app, 60);
        assert!(text.contains("CPU"), "missing CPU: {text:?}");
        assert!(!text.contains("GPU"), "unexpected GPU: {text:?}");
    }

    #[test]
    fn shows_placeholder_before_first_sample() {
        let mut app = AppState::test_new();
        app.system_monitor_enabled = true;
        app.system_monitor = None;
        let text = render_to_string(&app, 40);
        assert!(text.contains("sampling"), "missing placeholder: {text:?}");
    }

    fn app_with_usage(info: crate::api::schema::PaneUsageInfo) -> AppState {
        use crate::app::state::StoredPaneUsage;
        let mut app = AppState::test_new();
        let ws = crate::workspace::Workspace::test_new("w");
        let pane_id = *ws.tabs[0].panes.keys().next().expect("a pane");
        app.workspaces.push(ws);
        app.active = Some(0);
        app.context_usage.enabled = true;
        app.pane_usage.insert(
            pane_id,
            StoredPaneUsage {
                info,
                expires_at: None,
                seq: None,
            },
        );
        app
    }

    fn usage(used_pct: Option<u8>) -> crate::api::schema::PaneUsageInfo {
        crate::api::schema::PaneUsageInfo {
            source: "herdr-context-usage:claude-statusline".into(),
            agent: Some("claude".into()),
            model: None,
            used_pct,
            used_tokens: None,
            context_window_tokens: None,
            remaining_tokens: None,
            reset_at_unix: None,
            window_kind: None,
            confidence: Some("official".into()),
        }
    }

    #[test]
    fn renders_context_usage_segment_for_active_tab() {
        let app = app_with_usage(usage(Some(63)));
        let text = render_to_string(&app, 100);
        assert!(text.contains("ctx"), "missing ctx label: {text:?}");
        assert!(text.contains("claude"), "missing agent: {text:?}");
        assert!(text.contains("63%"), "missing pct: {text:?}");
        assert!(text.contains('▰'), "missing bar fill: {text:?}");
    }

    #[test]
    fn hides_context_usage_when_disabled() {
        let mut app = app_with_usage(usage(Some(63)));
        app.context_usage.enabled = false;
        let text = render_to_string(&app, 100);
        assert!(
            !text.contains("ctx"),
            "unexpected ctx when disabled: {text:?}"
        );
    }

    #[test]
    fn omits_pane_without_a_percentage() {
        let app = app_with_usage(usage(None));
        let text = render_to_string(&app, 100);
        assert!(
            !text.contains("ctx"),
            "unexpected ctx with no pct: {text:?}"
        );
    }

    #[test]
    fn hides_expired_usage() {
        use crate::app::state::StoredPaneUsage;
        let mut app = app_with_usage(usage(Some(63)));
        // Force the stored entry to have already expired.
        let pane_id = *app.workspaces[0].tabs[0].panes.keys().next().unwrap();
        app.pane_usage.insert(
            pane_id,
            StoredPaneUsage {
                info: usage(Some(63)),
                expires_at: Some(std::time::Instant::now() - std::time::Duration::from_secs(1)),
                seq: None,
            },
        );
        let text = render_to_string(&app, 100);
        assert!(
            !text.contains("ctx"),
            "expired usage should not render: {text:?}"
        );
    }

    #[test]
    fn usage_bar_fills_proportionally() {
        let app = AppState::test_new();
        let p = &app.palette;
        let mut spans = Vec::new();
        push_usage_bar(&mut spans, 50, 8, p);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.chars().filter(|c| *c == '▰').count(), 4);
        assert_eq!(text.chars().filter(|c| *c == '▱').count(), 4);
    }

    #[test]
    fn humanize_reset_hours_minutes_and_past() {
        assert_eq!(humanize_reset(1000 + 8040, 1000).as_deref(), Some("2h14m"));
        assert_eq!(humanize_reset(1000 + 600, 1000).as_deref(), Some("10m"));
        assert_eq!(humanize_reset(1000, 1000), None);
    }
}
