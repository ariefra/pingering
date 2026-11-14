//! ratatui TestBackend render tests (80x24) driving the real fns: state is
//! seeded through App::push_sample / push_hop / trace setters, keys go
//! through App::handle_key (the real path), render through
//! terminal.draw(ui::render). The modules are compiled from the same sources
//! the binary uses.

// The binary's modules, compiled into the test target via #[path]. Some of
// their pub items are driven by main() and not by these tests, so the
// dead-code lint is off only inside this target.
#[allow(dead_code)]
#[path = "../src/app.rs"]
mod app;
#[allow(dead_code)]
#[path = "../src/stats.rs"]
mod stats;
#[allow(dead_code)]
#[path = "../src/trace.rs"]
mod trace;
#[allow(dead_code)]
#[path = "../src/ui.rs"]
mod ui;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::Color;

use app::{App, Sample};
use trace::Hop;

// The render spec's colors and height ramp, restated here so the tests assert
// exact values rather than echoing the ui module's constants.
const GREEN: Color = Color::Green;
const YELLOW: Color = Color::Yellow;
const ORANGE: Color = Color::Rgb(255, 136, 0);
const RED: Color = Color::Red;
const RAMP: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

// The one-line detail block's labels.
const LABELS: [&str; 6] = ["Current", "Min", "Avg", "Max", "Recv", "Loss"];

fn render(app: &App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn press(app: &mut App, key: char) {
    app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
}

fn all_text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell(Position::new(x, y)).unwrap().symbol())
        .collect()
}

/// Positions of cells with the given symbol and foreground color.
fn positions_of(buf: &Buffer, symbol: &str, fg: Color) -> Vec<(u16, u16)> {
    buf.content
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.symbol() == symbol && cell.fg == fg)
        .map(|(i, _)| ((i as u16) % buf.area.width, (i as u16) / buf.area.width))
        .collect()
}

/// Seed `n` successful samples of `ms` so the running average is pinned near
/// `ms` before the case under test pushes its own sample.
fn seed_average(app: &mut App, n: usize, ms: f64) {
    for _ in 0..n {
        app.push_sample(0, Sample { rtt_ms: Some(ms) });
    }
}

fn hop(idx: usize, addr: &str, name: Option<&str>, lost: bool) -> Hop {
    Hop {
        idx,
        addr: if lost {
            String::new()
        } else {
            addr.to_string()
        },
        name: name.map(str::to_string),
        latency_ms: None,
        lost,
    }
}

// (a) The detail line is hidden by default: no stats labels anywhere.
#[test]
fn default_view_hides_stats() {
    let mut app = App::new_ping(vec!["127.0.0.1".into()], false, false);
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    app.push_sample(0, Sample { rtt_ms: Some(30.0) });
    let text = all_text(&render(&app));
    for label in LABELS {
        assert!(!text.contains(label), "{label} must be hidden by default");
    }
}

// (b) Pressing d shows the one-line detail block under the graph.
#[test]
fn d_key_shows_stats() {
    let mut app = App::new_ping(vec!["127.0.0.1".into()], false, false);
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    press(&mut app, 'd');
    // One line: every label on the same row (row 3, under label+graph).
    let stats_row = row_text(&render(&app), 3);
    for label in LABELS {
        assert!(
            stats_row.contains(label),
            "{label} missing from the detail line"
        );
    }
}

// (c) --details shows the line from the start, before any key.
#[test]
fn details_flag_shows_from_start() {
    let mut app = App::new_ping(vec!["127.0.0.1".into()], true, false);
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    let stats_row = row_text(&render(&app), 3);
    for label in LABELS {
        assert!(stats_row.contains(label), "{label} missing with --details");
    }
}

// (b2) Toggle round-trip: d then d hides the line again.
#[test]
fn toggle_round_trip() {
    let mut app = App::new_ping(vec!["127.0.0.1".into()], false, false);
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    press(&mut app, 'd');
    assert!(all_text(&render(&app)).contains("Current"));
    press(&mut app, 'd');
    assert!(
        !all_text(&render(&app)).contains("Current"),
        "second d must hide the stats again"
    );
}

// (d) Two hosts collapse to two rows each when details are hidden (label
// row, graph row) — no blank filler between panels.
#[test]
fn two_hosts_render_two_collapsed_graphs() {
    let mut app = App::new_ping(vec!["127.0.0.1".into(), "localhost".into()], false, false);
    app.push_sample(0, Sample { rtt_ms: Some(10.0) });
    app.push_sample(0, Sample { rtt_ms: Some(20.0) }); // avg 15: 20 -> 1.33x orange, top of the ramp
    app.push_sample(1, Sample { rtt_ms: Some(5.0) }); // avg 5: 5 -> 1x yellow, top of the ramp
    let buf = render(&app);
    assert!(row_text(&buf, 1).contains("127.0.0.1"));
    assert!(row_text(&buf, 3).contains("localhost"));
    let orange = positions_of(&buf, RAMP[7], ORANGE);
    assert!(
        orange.iter().any(|&(x, y)| y == 2 && x < 40),
        "no orange top-ramp bar in host 0 graph row (grows from the left)"
    );
    let yellow = positions_of(&buf, RAMP[7], YELLOW);
    assert!(
        yellow.iter().any(|&(x, y)| y == 4 && x < 40),
        "no yellow top-ramp bar in host 1 graph row (grows from the left)"
    );
}

// (d3) The graph window is capped at 40 columns, anchored left: with more
// samples than fit, the newest sample sits at column 39 and column 40 stays
// empty even on an 80-column terminal.
#[test]
fn graph_caps_at_40_columns() {
    let mut app = App::new_ping(vec!["127.0.0.1".into()], false, false);
    for _ in 0..45 {
        app.push_sample(0, Sample { rtt_ms: Some(40.0) });
    }
    let buf = render(&app);
    let newest = buf.cell(Position::new(39, 2)).unwrap();
    assert_eq!(
        newest.symbol(),
        RAMP[6],
        "newest sample at the window's right edge (40 ms on the 50 ms scale)"
    );
    let past_window = buf.cell(Position::new(40, 2)).unwrap();
    assert_eq!(
        past_window.symbol(),
        " ",
        "nothing beyond the 40-column window"
    );
    let oldest_visible = buf.cell(Position::new(0, 2)).unwrap();
    assert_eq!(
        oldest_visible.symbol(),
        RAMP[6],
        "window's oldest sample at the left edge"
    );
}

// (d2) With details shown each host takes three rows: label, graph, detail.
#[test]
fn two_hosts_expand_to_three_rows_with_details() {
    let mut app = App::new_ping(vec!["127.0.0.1".into(), "localhost".into()], true, false);
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    app.push_sample(1, Sample { rtt_ms: Some(5.0) });
    let buf = render(&app);
    assert!(row_text(&buf, 1).contains("127.0.0.1"));
    assert!(row_text(&buf, 3).contains("Current"), "host 0 detail line");
    assert!(row_text(&buf, 4).contains("localhost"));
    assert!(row_text(&buf, 6).contains("Current"), "host 1 detail line");
}

// Band rule: color against the running average — at/below 0.5x green, up to
// 1x yellow, up to 1.5x orange, beyond red. Each case in a fresh app so the
// pushed sample cannot drift an earlier case's average; 100 seeded 40 ms
// samples pin the average near 40. Height scales into the ramp against the
// snapped scale (nice_scale of the visible max).
#[test]
fn band_colors_follow_the_average() {
    for (rtt, step, expected) in [
        (12.0, 2, GREEN),  // 12/50 of the scale, ~0.30x avg
        (38.0, 5, YELLOW), // 38/50 of the scale, ~0.95x avg
        (45.0, 6, ORANGE), // 45/50 of the scale, ~1.13x avg
        (70.0, 5, RED),    // 70/100 of the scale, ~1.74x avg
    ] {
        let mut app = App::new_ping(vec!["127.0.0.1".into()], false, false);
        seed_average(&mut app, 100, 40.0);
        app.push_sample(0, Sample { rtt_ms: Some(rtt) });
        // Single host: label row 1, graph row 2; 101 samples scroll the
        // 40-column window, so the newest sample is at x=39.
        let buf = render(&app);
        let cell = buf.cell(Position::new(39, 2)).unwrap();
        assert_eq!(cell.symbol(), RAMP[step]);
        assert_eq!(cell.fg, expected, "rtt {rtt} against a ~40 ms average");
    }
}

// A lost sample renders blank: no character at all in its column (the spec
// replaced the old dashed stub). The adjacent success sits exactly at the
// average -> the 1x band (yellow), at the top of the ramp (50 ms scale).
#[test]
fn lost_sample_renders_blank() {
    let mut app = App::new_ping(vec!["127.0.0.1".into()], false, false);
    app.push_sample(0, Sample { rtt_ms: Some(50.0) });
    app.push_sample(0, Sample { rtt_ms: None }); // newest sample lost
    let buf = render(&app);
    // Two samples grow from the left: oldest at x=0, newest at x=1.
    let newest = buf.cell(Position::new(1, 2)).unwrap();
    assert_eq!(newest.symbol(), " ");
    assert_eq!(newest.fg, Color::Reset);
    let adjacent = buf.cell(Position::new(0, 2)).unwrap();
    assert_eq!(adjacent.symbol(), RAMP[7]);
    assert_eq!(adjacent.fg, YELLOW);
}

// (f) Legend: the keys hint in the header and the scale hint on the label
// row are hidden by default and shown with --legend. The app title always
// carries the version.
#[test]
fn legend_hidden_by_default_shown_with_flag() {
    let mut app = App::new_ping(vec!["127.0.0.1".into()], false, false);
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    let buf = render(&app);
    let text = all_text(&buf);
    assert!(
        !text.contains("d: details"),
        "keys hint hidden without --legend"
    );
    assert!(
        !text.contains("scale"),
        "scale hint hidden without --legend"
    );
    assert!(
        row_text(&buf, 0).contains("pingering v"),
        "version in the title"
    );

    let mut app = App::new_ping(vec!["127.0.0.1".into()], false, true);
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    let buf = render(&app);
    assert!(
        row_text(&buf, 0).contains("d: details"),
        "keys hint with --legend"
    );
    assert!(row_text(&buf, 0).contains("q: quit"));
    assert!(
        row_text(&buf, 1).contains("scale"),
        "scale hint with --legend"
    );
}

// (e0) A paused worker (network error / sustained loss) shows its message on
// the label row until the restart's Started clears it.
#[test]
fn paused_worker_shows_message_on_label_row() {
    let mut app = App::new_ping(vec!["127.0.0.1".into()], false, false);
    app.set_fatal(0, "7 pings lost in a row; restarting in 60 s".into());
    let label = row_text(&render(&app), 1);
    assert!(
        label.contains("restarting in 60 s"),
        "paused reason on the label row"
    );
    app.clear_fatal(0);
    assert!(
        !all_text(&render(&app)).contains("restarting"),
        "a restarted worker clears the message"
    );
}

// (e) Trace mode: a dim group header (`ip (name)`) on row 1, then the hop's
// panel (label above, one-line graph below). An unresponsive hop produces no
// row at all. Detail labels stay hidden by default; the footer carries the
// status.
#[test]
fn trace_mode_renders_hop_panels() {
    let mut app = App::new_trace("127.0.0.1".into(), vec!["127.0.0.1".into()], false, false);
    let pinger = app.push_hop(0, hop(1, "127.0.0.1", Some("localhost"), false));
    assert!(pinger.is_some(), "a hop with an address must get a pinger");
    assert!(
        app.push_hop(0, hop(2, "", None, true)).is_none(),
        "an unresponsive hop must not get a pinger"
    );
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    let buf = render(&app);
    // Group header row 1, hop label row 2, graph row 3, footer row 23.
    assert!(row_text(&buf, 1).contains("127.0.0.1"), "group header");
    let label = row_text(&buf, 2);
    assert!(label.contains("127.0.0.1"), "hop ip on the label row");
    assert!(
        label.contains("localhost"),
        "resolved name on the label row"
    );
    let bar = buf.cell(Position::new(0, 3)).unwrap();
    assert_eq!(bar.symbol(), RAMP[4]); // 12/20 of the scale
    assert_eq!(
        bar.fg, YELLOW,
        "12 ms against a 12 ms average is the 1x band"
    );
    assert!(
        !all_text(&buf).contains('*'),
        "an unresponsive hop must not render"
    );
    assert!(row_text(&buf, 23).contains("tracing..."));
    assert!(
        row_text(&buf, 0).contains("pingering v"),
        "version in the trace title"
    );
    // Hidden by default in trace mode too: no detail labels before 'd'.
    let text = all_text(&buf);
    for label in LABELS {
        assert!(!text.contains(label), "{label} must be hidden by default");
    }
}

// (e2) With details shown, the one-line block renders under the hop's graph.
#[test]
fn trace_details_render_under_the_hop_graph() {
    let mut app = App::new_trace("127.0.0.1".into(), vec!["127.0.0.1".into()], true, false);
    assert!(app.push_hop(0, hop(1, "127.0.0.1", None, false)).is_some());
    app.push_sample(0, Sample { rtt_ms: Some(12.0) });
    let stats_row = row_text(&render(&app), 4);
    for label in LABELS {
        assert!(stats_row.contains(label), "{label} missing with --details");
    }
}

// (e3) A dual-stack target renders one group header per family, each with
// its own hops.
#[test]
fn dual_stack_trace_renders_both_families() {
    let mut app = App::new_trace(
        "localhost".into(),
        vec!["127.0.0.1 (localhost)".into(), "::1 (localhost)".into()],
        false,
        false,
    );
    assert!(app.push_hop(0, hop(1, "127.0.0.1", None, false)).is_some());
    assert!(app.push_hop(1, hop(1, "::1", None, false)).is_some());
    let buf = render(&app);
    // Group 0: header row 1, hop rows 2-3. Group 1: header row 4, hop rows 5-6.
    assert!(row_text(&buf, 1).contains("127.0.0.1"));
    assert!(row_text(&buf, 2).contains("127.0.0.1"));
    assert!(row_text(&buf, 4).contains("::1"));
    assert!(row_text(&buf, 5).contains("::1"));
}

// (e4) A failed trace shows the reason and the re-trace countdown in the
// footer; a long reason is truncated so the countdown stays visible; the
// Reset that precedes a re-trace clears the group's hops again.
#[test]
fn trace_retry_error_shows_in_footer_and_reset_clears() {
    let mut app = App::new_trace("127.0.0.1".into(), vec!["127.0.0.1".into()], false, false);
    assert!(app.push_hop(0, hop(1, "127.0.0.1", None, false)).is_some());
    app.trace_failed(0, "no responsive hops found".into());
    app.set_retry_secs(0, Some(42));
    let footer = row_text(&render(&app), 23);
    assert!(
        footer.contains("no responsive hops found"),
        "the retry reason must show while waiting"
    );
    assert!(footer.contains("42s"), "the re-trace countdown must show");
    // A long stderr tail must not push the countdown off the screen.
    app.trace_stderr(0, "x".repeat(120));
    let footer = row_text(&render(&app), 23);
    assert!(
        footer.contains("retrying in 42s"),
        "countdown must stay visible with a long reason: {footer}"
    );
    // A re-trace resets the group's hop rows; the header stays.
    app.reset_trace(0);
    let buf = render(&app);
    assert!(
        row_text(&buf, 1).contains("127.0.0.1"),
        "group header stays"
    );
    assert!(
        row_text(&buf, 2).trim().is_empty(),
        "cleared hops must not render"
    );
    assert!(row_text(&buf, 23).contains("tracing..."));
}
