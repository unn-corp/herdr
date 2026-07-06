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
    // Git status for the active space's repo, updating as you move between spaces.
    if let Some(ws) = app.active.and_then(|idx| app.workspaces.get(idx)) {
        push_git_spans(&mut spans, ws, p);
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(background), area);
}

fn push_git_spans(spans: &mut Vec<Span<'static>>, ws: &crate::workspace::Workspace, p: &Palette) {
    let branch = ws.branch();
    let dirty = ws.git_dirty();
    if branch.is_none() && dirty.is_none() {
        return;
    }
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
}
