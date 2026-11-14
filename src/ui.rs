//! Pure render functions from `&App`: header row, stacked host panels (label
//! row, one-line graph, optional one-line detail block), trace groups with
//! live-pinged hops. Color constants and the latency-band rule live here; no
//! logic.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::{App, HostState, Mode, TraceState};
use crate::stats::{Stats, nice_scale};

// Latency-band colors against the session average: at or below half the
// average green, up to the average yellow, up to 50% over orange, beyond red.
const GREEN: Color = Color::Green;
const YELLOW: Color = Color::Yellow;
const ORANGE: Color = Color::Rgb(255, 136, 0);
const RED: Color = Color::Red;
/// Dim text (hints, footer, trace group headers).
const DIM: Color = Color::DarkGray;
/// Height ramp for the one-line graph: taller latency = taller block char.
const RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Graph window: the graph never exceeds this width, anchored left.
const GRAPH_MAX_WIDTH: u16 = 40;
const DASH: &str = "—";
const HINT: &str = "d: details  q: quit";
/// The app title carries the package version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Band color for one sample against the session average (half the average
/// and below green, up to the average yellow, up to 50% over orange, beyond
/// red). No average yet reads as the lowest band.
fn band_color(rtt_ms: f64, avg: Option<f64>) -> Color {
    let ratio = avg.filter(|a| *a > 0.0).map_or(0.5, |a| rtt_ms / a);
    if ratio <= 0.5 {
        GREEN
    } else if ratio <= 1.0 {
        YELLOW
    } else if ratio <= 1.5 {
        ORANGE
    } else {
        RED
    }
}

/// Rows one host panel occupies: label + graph, plus the detail line when
/// shown. Panels stack tightly; a hidden detail row collapses.
fn panel_rows(details: bool) -> u16 {
    if details { 3 } else { 2 }
}

/// Full-screen redraw: header row, then the mode's content.
pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title = match &app.mode {
        Mode::Ping { .. } => format!("pingering v{VERSION}"),
        Mode::Trace(state) => {
            format!("traceroute {} — pingering v{VERSION}", state.target)
        }
    };
    render_header(f, &title, app.legend);
    match &app.mode {
        Mode::Ping { hosts } => render_ping(f, hosts, app.details, app.legend),
        Mode::Trace(state) => render_trace(f, state, app.details, app.legend),
    }
}

/// Header row 0: mode title left; with the legend, the dim key hint right.
fn render_header(f: &mut Frame, title: &str, legend: bool) {
    let area = f.area();
    let row = Rect::new(0, 0, area.width, 1);
    if legend {
        let hint_w = (HINT.len() as u16).min(area.width);
        let [left, right] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(hint_w)]).areas(row);
        f.render_widget(Paragraph::new(Line::from(title)), left);
        f.render_widget(
            Paragraph::new(Line::styled(HINT, Style::new().fg(DIM))),
            right,
        );
    } else {
        f.render_widget(Paragraph::new(Line::from(title)), row);
    }
}

/// Ping mode: host panels stack top-down in argv order, each exactly as tall
/// as it needs (see [`panel_rows`]). Hosts that do not fit the viewport
/// surface as a dim `…+N more hosts` hint on the bottom row.
fn render_ping(f: &mut Frame, hosts: &[HostState], details: bool, legend: bool) {
    let area = f.area();
    let needed = panel_rows(details);
    let mut y = 1u16;
    let mut overflow = 0;
    for host in hosts {
        if y + needed > area.height {
            overflow += 1;
            continue;
        }
        render_host(f, y, area.width, &host.name, host, details, legend);
        y += needed;
    }
    if overflow > 0 {
        render_overflow_hint(f, area.height - 1, area.width, overflow, "host");
    }
}

/// One host: label row (label left, scale hint or fatal reason right — the
/// scale hint only with the legend), the one-line graph under it, and when
/// enabled the one-line detail block right below the graph.
fn render_host(
    f: &mut Frame,
    y: u16,
    width: u16,
    label: &str,
    host: &HostState,
    details: bool,
    legend: bool,
) {
    let label_row = Rect::new(0, y, width, 1);
    let graph = Rect::new(0, y + 1, width.min(GRAPH_MAX_WIDTH), 1);
    let stats_area = Rect::new(0, y + 2, width, 1);

    // Scale from the VISIBLE samples (the columns about to be drawn).
    let visible_max = host
        .samples
        .iter()
        .rev()
        .take(graph.width as usize)
        .filter_map(|s| s.rtt_ms)
        .reduce(|a, b| a.max(b));
    let nice = nice_scale(visible_max);

    let right = host.fatal.as_deref().map_or_else(
        || {
            if legend {
                format!("scale 0-{nice:.0} ms")
            } else {
                String::new()
            }
        },
        str::to_string,
    );
    let right_w = (right.chars().count() as u16).min(label_row.width);
    let [name_area, hint_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_w)]).areas(label_row);
    f.render_widget(Paragraph::new(Line::from(label)), name_area);
    f.render_widget(
        Paragraph::new(Line::styled(right, Style::new().fg(DIM))),
        hint_area,
    );

    render_graph(f, graph, host, nice);
    if details {
        render_stats(f, stats_area, &Stats::from_totals(&host.totals));
    }
}

/// Graph: samples accumulate left-to-right from the panel's left edge; once
/// there are more samples than the 40-column window, the window scrolls
/// (oldest visible sample at the left edge, newest at its right). One sample
/// = one column, exactly one row tall. Height scales into the block ramp
/// (▁..█) against the snapped scale; color is the latency band (see
/// [`band_color`]). Lost samples render blank.
fn render_graph(f: &mut Frame, graph: Rect, host: &HostState, nice: f64) {
    if graph.width == 0 || graph.height == 0 {
        return;
    }
    let avg = Stats::from_totals(&host.totals).avg;
    let top = (RAMP.len() - 1) as f64;
    let visible = host.samples.len().min(graph.width as usize);
    let buf = f.buffer_mut();
    for (offset, sample) in host
        .samples
        .iter()
        .rev()
        .take(graph.width as usize)
        .enumerate()
    {
        let x = graph.x + (visible - 1 - offset) as u16;
        let Some(rtt) = sample.rtt_ms else {
            continue; // loss: blank column
        };
        let step = ((rtt / nice * top).round() as usize).clamp(0, RAMP.len() - 1);
        let cell = &mut buf[(x, graph.y)];
        cell.set_char(RAMP[step]);
        cell.set_style(Style::new().fg(band_color(rtt, avg)));
    }
}

/// Detail line, one row per host: Current/Min/Avg/Max in ms (2 decimals, the
/// unit stated once on Current), received/total probes, and loss percent.
fn render_stats(f: &mut Frame, area: Rect, stats: &Stats) {
    let ms = |v: Option<f64>| v.map_or_else(|| DASH.to_string(), |v| format!("{v:.2}"));
    let loss = stats
        .loss_pct()
        .map_or_else(|| DASH.to_string(), |p| format!("{p:.1}%"));
    let line = format!(
        "Current {} ms  Min {}  Avg {}  Max {}  Recv {}/{}  Loss {}",
        ms(stats.current),
        ms(stats.min),
        ms(stats.avg),
        ms(stats.max),
        stats.received,
        stats.received + stats.lost,
        loss,
    );
    f.render_widget(Paragraph::new(line), area);
}

/// Trace mode: one group per address family — a dim `ip (name)` header row,
/// then its hop panels stacked. Rows that do not fit the viewport surface as
/// a dim `…+N more hops` hint on the footer's right. The footer shows the
/// first group's failure (with the re-trace countdown) or the overall state.
fn render_trace(f: &mut Frame, state: &TraceState, details: bool, legend: bool) {
    let area = f.area();
    if area.height < 2 {
        return;
    }
    let footer_y = area.height - 1;
    let needed = panel_rows(details);
    let mut y = 1u16;
    let mut overflow = 0;
    for group in &state.groups {
        if y >= footer_y {
            overflow += 1;
            continue;
        }
        f.render_widget(
            Paragraph::new(Line::styled(group.label.as_str(), Style::new().fg(DIM))),
            Rect::new(0, y, area.width, 1),
        );
        y += 1;
        for panel in &group.hops {
            if y + needed > footer_y {
                overflow += 1;
                continue;
            }
            render_host(
                f,
                y,
                area.width,
                &panel.host.name,
                &panel.host,
                details,
                legend,
            );
            y += needed;
        }
    }

    let footer = Rect::new(0, footer_y, area.width, 1);
    let err_group = state.groups.iter().find(|g| g.error.is_some());
    let (text, style) = if let Some(g) = err_group {
        let base = g
            .stderr_tail
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(g.error.as_deref().unwrap_or(""));
        let countdown = g
            .retry_secs
            .map_or_else(|| "retrying".to_string(), |s| format!("retrying in {s}s"));
        let suffix = format!(" — {countdown}");
        let avail = (footer.width as usize).saturating_sub(suffix.chars().count());
        let base = if base.chars().count() > avail {
            format!(
                "{}…",
                base.chars()
                    .take(avail.saturating_sub(1))
                    .collect::<String>()
            )
        } else {
            base.to_string()
        };
        (format!("{base}{suffix}"), Style::new().fg(ORANGE))
    } else if state.groups.iter().all(|g| g.exit_code.is_some()) {
        let bad = state
            .groups
            .iter()
            .find_map(|g| g.exit_code.filter(|code| *code != 0));
        (
            bad.map_or_else(|| "done".to_string(), |code| format!("done (exit {code})")),
            Style::new().fg(DIM),
        )
    } else {
        ("tracing...".to_string(), Style::new().fg(DIM))
    };
    if overflow > 0 {
        render_overflow(f, footer, &text, style, overflow);
    } else {
        f.render_widget(Paragraph::new(Line::styled(text, style)), footer);
    }
}

/// Dim `…+N more <what>s` hint on one row.
fn render_overflow_hint(f: &mut Frame, y: u16, width: u16, overflow: u16, what: &str) {
    let hint = format!(
        "…+{overflow} more {what}{}",
        if overflow == 1 { "" } else { "s" }
    );
    f.render_widget(
        Paragraph::new(Line::styled(hint, Style::new().fg(DIM))),
        Rect::new(0, y, width, 1),
    );
}

/// Overflow hint right-aligned next to the footer text.
fn render_overflow(f: &mut Frame, footer: Rect, text: &str, style: Style, overflow: u16) {
    let hint = format!(
        "…+{overflow} more hop{}",
        if overflow == 1 { "" } else { "s" }
    );
    let hint_w = (hint.chars().count() as u16).min(footer.width);
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(hint_w)]).areas(footer);
    f.render_widget(Paragraph::new(Line::styled(text.to_string(), style)), left);
    f.render_widget(
        Paragraph::new(Line::styled(hint, Style::new().fg(DIM))),
        right,
    );
}
