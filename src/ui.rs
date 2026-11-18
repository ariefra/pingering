//! Pure render functions from `&App`: header row, stacked host panels (label
//! row, one-line graph, optional one-line detail block — or a single compact
//! line per host), trace groups with live-pinged hops. The `ViewStyle` from
//! argv drives everything: bar ramp, colors, compact mode, visibility.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, ColorStyle, HostState, Label, TraceState, ViewStyle};
use crate::stats::{Stats, nice_scale};

/// Default dim text (legend, group headers, hints) when no legend color is
/// set.
const DIM: Color = Color::DarkGray;
/// The orange used for pause/failure reasons.
const ORANGE: Color = Color::Rgb(255, 136, 0);
const DASH: &str = "—";
const HINT: &str = "4:ipv4 6:ipv6 t:trace 1:single h:legend s:scale c:compact d:details q:quit";
/// Graph window: the graph never exceeds this width, anchored left.
const GRAPH_MAX_WIDTH: u16 = 40;
/// Compact mode reserves this much for the one-line detail block.
const COMPACT_DETAIL_MIN: u16 = 34;
/// Fixed label width in compact mode (keeps bars aligned across hosts).
const COMPACT_LABEL_WIDTH: u16 = 24;
/// The app title carries the package version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Band color for one sample against the session average (half the average
/// and below green, up to the average yellow, up to 50% over orange, beyond
/// red). No average yet reads as the lowest band.
fn band_color(rtt_ms: f64, avg: Option<f64>) -> Color {
    let ratio = avg.filter(|a| *a > 0.0).map_or(0.5, |a| rtt_ms / a);
    if ratio <= 0.5 {
        Color::Green
    } else if ratio <= 1.0 {
        Color::Yellow
    } else if ratio <= 1.5 {
        ORANGE
    } else {
        Color::Red
    }
}

/// Rows one host panel occupies: one in compact mode, else label + graph,
/// plus the detail line when shown. Panels stack tightly; a hidden detail
/// row collapses.
fn panel_rows(view: &ViewStyle) -> u16 {
    if view.compact {
        1
    } else if view.details {
        3
    } else {
        2
    }
}

/// Full-screen redraw: header row, then the mode's content.
pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title = match app.trace() {
        Some(state) => format!("pingering v{VERSION} — traceroute {}", state.target),
        None => format!("pingering v{VERSION}"),
    };
    render_header(f, &title, &app.view);
    match app.trace() {
        Some(state) => render_trace(f, state, &app.view),
        None => render_ping(f, app.ping(), &app.view),
    }
}

/// Header row 0: mode title left (colored by `title`); with the legend, the
/// key hint right (colored by `legend`).
/// Header rows: the title row, plus the legend row when the legend is on
/// (the full key list no longer fits beside the title).
fn header_rows(view: &ViewStyle) -> u16 {
    if view.legend { 2 } else { 1 }
}

fn render_header(f: &mut Frame, title: &str, view: &ViewStyle) {
    let area = f.area();
    let title_style = view
        .colors
        .title
        .map(|c| Style::new().fg(c))
        .unwrap_or_default();
    f.render_widget(
        Paragraph::new(Line::styled(title, title_style)),
        Rect::new(0, 0, area.width, 1),
    );
    if view.legend {
        f.render_widget(
            Paragraph::new(Line::styled(HINT, legend_style(view))),
            Rect::new(0, 1, area.width, 1),
        );
    }
}

/// The legend color for dim informational text (hints, headers, overflow).
fn legend_style(view: &ViewStyle) -> Style {
    Style::new().fg(view.colors.legend.unwrap_or(DIM))
}

/// Ping mode: host panels stack top-down in argv order, each exactly as tall
/// as it needs (see [`panel_rows`]). Hosts that do not fit the viewport
/// surface as a dim `…+N more hosts` hint on the bottom row.
fn render_ping(f: &mut Frame, hosts: &[HostState], view: &ViewStyle) {
    let area = f.area();
    let needed = panel_rows(view);
    let mut y = header_rows(view);
    let mut overflow = 0;
    for host in hosts {
        if y + needed > area.height {
            overflow += 1;
            continue;
        }
        render_host(f, y, area.width, host, view);
        y += needed;
    }
    if overflow > 0 {
        render_overflow_hint(f, area.height - 1, area.width, overflow, "host", view);
    }
}

/// The host label as styled spans: prefix (raw), ip, and the parenthesized
/// name — each colored per `--color-style`.
fn label_line(label: &Label, colors: &ColorStyle) -> Line<'static> {
    let mut spans = Vec::new();
    if !label.prefix.is_empty() {
        // The trace hop index; colored by `hop`.
        spans.push(match colors.hop {
            Some(c) => Span::styled(label.prefix.clone(), Style::new().fg(c)),
            None => Span::raw(label.prefix.clone()),
        });
    }
    spans.push(match colors.ip {
        Some(c) => Span::styled(label.ip.clone(), Style::new().fg(c)),
        None => Span::raw(label.ip.clone()),
    });
    if let Some(name) = &label.name {
        spans.push(match colors.name {
            Some(c) => Span::styled(format!(" ({name})"), Style::new().fg(c)),
            None => Span::raw(format!(" ({name})")),
        });
    }
    Line::from(spans)
}

/// One host in full mode: label row (label left, scale hint or fatal reason
/// right — the scale hint only with the legend), the one-line graph under it,
/// and when enabled the one-line detail block right below the graph.
fn render_host(f: &mut Frame, y: u16, width: u16, host: &HostState, view: &ViewStyle) {
    if view.compact {
        render_host_compact(f, y, width, host, view);
        return;
    }
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
            if view.scale {
                format!("scale 0-{nice:.0} ms")
            } else {
                String::new()
            }
        },
        str::to_string,
    );
    let right_style = if host.fatal.is_some() {
        Style::new().fg(view.colors.errors.unwrap_or(ORANGE))
    } else {
        // The scale hint: colored by `scale`, falling back to the legend
        // color, then dim.
        Style::new().fg(view
            .colors
            .scale
            .unwrap_or(view.colors.legend.unwrap_or(DIM)))
    };
    let right_w = (right.chars().count() as u16).min(label_row.width);
    let [name_area, hint_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_w)]).areas(label_row);
    f.render_widget(
        Paragraph::new(label_line(&host.label, &view.colors)),
        name_area,
    );
    f.render_widget(Paragraph::new(Line::styled(right, right_style)), hint_area);

    render_graph(f, graph, host, nice, view);
    if view.details {
        render_stats(f, stats_area, &Stats::from_totals(&host.totals), view);
    }
}

/// One host in compact mode — a single line: `ip (name)` (fixed width,
/// truncated to the address alone when needed), the bar window, and when
/// enabled the detail block, with the pause/failure reason replacing the
/// details.
fn render_host_compact(f: &mut Frame, y: u16, width: u16, host: &HostState, view: &ViewStyle) {
    let label_w = COMPACT_LABEL_WIDTH.min(width);
    // Bars shrink so the detail block keeps its reserved space.
    let bars_w = if view.details {
        width
            .saturating_sub(label_w + 2 + COMPACT_DETAIL_MIN)
            .clamp(6, GRAPH_MAX_WIDTH)
    } else {
        width.saturating_sub(label_w + 2).clamp(6, GRAPH_MAX_WIDTH)
    };
    let label_area = Rect::new(0, y, label_w, 1);
    let graph = Rect::new(label_w + 1, y, bars_w, 1);
    let detail_area = Rect::new(
        label_w + bars_w + 2,
        y,
        width.saturating_sub(label_w + bars_w + 2),
        1,
    );

    // Scale from the VISIBLE samples (the columns about to be drawn).
    let visible_max = host
        .samples
        .iter()
        .rev()
        .take(bars_w as usize)
        .filter_map(|s| s.rtt_ms)
        .reduce(|a, b| a.max(b));
    let nice = nice_scale(visible_max);

    // Long labels drop the name first so the address stays readable.
    let mut line = label_line(&host.label, &view.colors);
    if host.label.plain_text().chars().count() as u16 > label_w && host.label.name.is_some() {
        line = label_line(
            &Label {
                prefix: host.label.prefix.clone(),
                ip: host.label.ip.clone(),
                name: None,
            },
            &view.colors,
        );
    }
    f.render_widget(Paragraph::new(line), label_area);

    render_graph(f, graph, host, nice, view);

    let detail_style = Style::new().fg(view.colors.details.unwrap_or(Color::Reset));
    let text = match &host.fatal {
        Some(reason) => Line::styled(
            reason.clone(),
            Style::new().fg(view.colors.errors.unwrap_or(ORANGE)),
        ),
        None if view.details => Line::styled(
            compact_stats(&Stats::from_totals(&host.totals)),
            detail_style,
        ),
        None => Line::default(),
    };
    f.render_widget(Paragraph::new(text), detail_area);
}

/// Graph: samples accumulate left-to-right from the panel's left edge; once
/// there are more samples than the window, the window scrolls (oldest
/// visible sample at the left edge, newest at its right). One sample = one
/// column, exactly one row tall. Height scales into the bar ramp (from
/// `--bar-style`) against the snapped scale; color is `--color-style bars`
/// when set, else the latency band (see [`band_color`]). Lost samples render
/// blank.
fn render_graph(f: &mut Frame, graph: Rect, host: &HostState, nice: f64, view: &ViewStyle) {
    if graph.width == 0 || graph.height == 0 {
        return;
    }
    let avg = Stats::from_totals(&host.totals).avg;
    let top = (view.bar_ramp.len().max(2) - 1) as f64;
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
        let step = ((rtt / nice * top).round() as usize).clamp(0, view.bar_ramp.len() - 1);
        // --bar-colors gives one color per ramp level, overriding both the
        // single --color-style bars color and the latency bands.
        let color = if view.bar_colors.is_empty() {
            view.colors.bars.unwrap_or_else(|| band_color(rtt, avg))
        } else {
            let cn = view.bar_colors.len();
            let cidx = ((rtt / nice * (cn - 1) as f64).round() as usize).clamp(0, cn - 1);
            view.bar_colors[cidx]
        };
        let cell = &mut buf[(x, graph.y)];
        cell.set_char(view.bar_ramp[step]);
        cell.set_style(Style::new().fg(color));
    }
}

/// Detail line, one row per host: Current/Min/Avg/Max in ms (2 decimals, the
/// unit stated once on Current), received/total probes, and loss percent.
fn render_stats(f: &mut Frame, area: Rect, stats: &Stats, view: &ViewStyle) {
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
    let style = Style::new().fg(view.colors.details.unwrap_or(Color::Reset));
    f.render_widget(Paragraph::new(Line::styled(line, style)), area);
}

/// Compact detail block: the three headline numbers.
fn compact_stats(stats: &Stats) -> String {
    let cur = stats
        .current
        .map_or_else(|| DASH.to_string(), |v| format!("{v:.2}ms"));
    let avg = stats
        .avg
        .map_or_else(|| DASH.to_string(), |v| format!("{v:.2}"));
    let loss = stats
        .loss_pct()
        .map_or_else(|| DASH.to_string(), |p| format!("{p:.1}%"));
    format!("Cur {cur}  Avg {avg}  Loss {loss}")
}

/// Trace mode: one group per address family — a dim `ip (name)` header row,
/// then its hop panels stacked. Rows that do not fit the viewport surface as
/// a dim `…+N more hops` hint on the footer's right. The footer shows the
/// first group's failure (with the re-trace countdown) or the overall state.
fn render_trace(f: &mut Frame, state: &TraceState, view: &ViewStyle) {
    let area = f.area();
    if area.height < 2 {
        return;
    }
    let footer_y = area.height - 1;
    let needed = panel_rows(view);
    let mut y = header_rows(view);
    let mut overflow = 0;
    for group in &state.groups {
        if y >= footer_y {
            overflow += 1;
            continue;
        }
        f.render_widget(
            Paragraph::new(Line::styled(group.label.as_str(), legend_style(view))),
            Rect::new(0, y, area.width, 1),
        );
        y += 1;
        for panel in &group.hops {
            if y + needed > footer_y {
                overflow += 1;
                continue;
            }
            render_host(f, y, area.width, &panel.host, view);
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
        (
            format!("{base}{suffix}"),
            Style::new().fg(view.colors.message.unwrap_or(ORANGE)),
        )
    } else if state.groups.iter().all(|g| g.exit_code.is_some()) {
        let bad = state
            .groups
            .iter()
            .find_map(|g| g.exit_code.filter(|code| *code != 0));
        (
            bad.map_or_else(|| "done".to_string(), |code| format!("done (exit {code})")),
            legend_style(view),
        )
    } else {
        ("tracing...".to_string(), legend_style(view))
    };
    if overflow > 0 {
        render_overflow(f, footer, &text, style, overflow, view);
    } else {
        f.render_widget(Paragraph::new(Line::styled(text, style)), footer);
    }
}

/// Dim `…+N more <what>s` hint on one row.
fn render_overflow_hint(
    f: &mut Frame,
    y: u16,
    width: u16,
    overflow: u16,
    what: &str,
    view: &ViewStyle,
) {
    let hint = format!(
        "…+{overflow} more {what}{}",
        if overflow == 1 { "" } else { "s" }
    );
    f.render_widget(
        Paragraph::new(Line::styled(hint, legend_style(view))),
        Rect::new(0, y, width, 1),
    );
}

/// Overflow hint right-aligned next to the footer text.
fn render_overflow(
    f: &mut Frame,
    footer: Rect,
    text: &str,
    style: Style,
    overflow: u16,
    view: &ViewStyle,
) {
    let hint = format!(
        "…+{overflow} more hop{}",
        if overflow == 1 { "" } else { "s" }
    );
    let hint_w = (hint.chars().count() as u16).min(footer.width);
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(hint_w)]).areas(footer);
    f.render_widget(Paragraph::new(Line::styled(text.to_string(), style)), left);
    f.render_widget(
        Paragraph::new(Line::styled(hint, legend_style(view))),
        right,
    );
}
