//! App state: the ping host panels (always kept — the t key toggles tracing
//! without losing them), the active traceroute when tracing, the render
//! options gathered from argv (`ViewStyle`), and the key entry point shared
//! by the event loop and the render tests.

use std::collections::VecDeque;
use std::net::IpAddr;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Color;

use crate::stats::Totals;
use crate::trace::Hop;

/// Fixed memory: at most this many samples are kept per host for the graph.
pub const SAMPLE_CAP: usize = 512;

/// The default bar height ramp (`--bar-style` replaces it).
pub const BLOCK_RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The address families selected on argv and toggled with the 4/6 keys; the
/// default is both. Lives here because the App's family toggles are state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
    Both,
}

/// One ping outcome: `Some` RTT in ms for a reply, `None` for a timeout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub rtt_ms: Option<f64>,
}

/// A host label as structured spans: an optional prefix (the trace hop
/// index), the address, and the reverse/argv name when known — so `ip` and
/// `name` can be colored separately.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub prefix: String,
    pub ip: String,
    pub name: Option<String>,
}

impl Label {
    /// Bare address, no prefix, no name.
    pub fn plain(ip: impl Into<String>) -> Label {
        Label {
            prefix: String::new(),
            ip: ip.into(),
            name: None,
        }
    }

    /// The label as one plain string.
    pub fn plain_text(&self) -> String {
        match &self.name {
            Some(name) => format!("{}{} ({name})", self.prefix, self.ip),
            None => format!("{}{}", self.prefix, self.ip),
        }
    }
}

/// Per-element colors (`--color-style`); `None` renders the element's
/// default.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColorStyle {
    pub title: Option<Color>,
    pub bars: Option<Color>,
    pub details: Option<Color>,
    pub legend: Option<Color>,
    pub scale: Option<Color>,
    pub hop: Option<Color>,
    pub ip: Option<Color>,
    pub name: Option<Color>,
    /// Worker pause/fatal messages on the label row.
    pub errors: Option<Color>,
    /// The trace footer's failure/retry message.
    pub message: Option<Color>,
}

/// Parse a `--color-style` spec: comma-separated `key=value` pairs with keys
/// title/bars/details/legend/scale/hop/ip/name and named or `#rrggbb`
/// colors.
pub fn parse_color_style(spec: &str) -> Result<ColorStyle, String> {
    let mut style = ColorStyle::default();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!(
                "--color-style entry must be key=value: '{part}' (keys: title, bars, details, legend, scale, hop, ip, name, errors, message)"
            ));
        };
        let key = key.trim();
        let value = value.trim();
        let slot = match key {
            "title" => &mut style.title,
            "bars" => &mut style.bars,
            "details" => &mut style.details,
            "legend" => &mut style.legend,
            "scale" => &mut style.scale,
            "hop" => &mut style.hop,
            "ip" => &mut style.ip,
            "name" => &mut style.name,
            "errors" => &mut style.errors,
            "message" => &mut style.message,
            other => {
                return Err(format!(
                    "unknown --color-style key '{other}' (keys: title, bars, details, legend, scale, hop, ip, name, errors, message)"
                ));
            }
        };
        if slot.is_some() {
            return Err(format!("duplicate --color-style key '{key}'"));
        }
        *slot = Some(
            parse_color(value)
                .ok_or_else(|| format!("unknown color '{value}' for --color-style key '{key}'"))?,
        );
    }
    Ok(style)
}

/// Parse a `--bar-colors` spec: comma-separated colors, one per ramp level,
/// low to high (2-16 levels).
pub fn parse_color_list(spec: &str) -> Result<Vec<Color>, String> {
    let mut colors = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        colors.push(
            parse_color(part).ok_or_else(|| format!("unknown color '{part}' for --bar-colors"))?,
        );
    }
    if !(2..=16).contains(&colors.len()) {
        return Err(format!(
            "--bar-colors needs 2-16 colors, low to high (got {})",
            colors.len()
        ));
    }
    Ok(colors)
}

/// Named colors plus `#rrggbb`; `default` is the terminal's own foreground.
fn parse_color(value: &str) -> Option<Color> {
    match value.to_ascii_lowercase().as_str() {
        "default" => return Some(Color::Reset),
        "black" => return Some(Color::Black),
        "red" => return Some(Color::Red),
        "green" => return Some(Color::Green),
        "yellow" => return Some(Color::Yellow),
        "blue" => return Some(Color::Blue),
        "magenta" => return Some(Color::Magenta),
        "cyan" => return Some(Color::Cyan),
        "white" => return Some(Color::White),
        "gray" | "grey" => return Some(Color::Gray),
        "dark-gray" | "darkgray" => return Some(Color::DarkGray),
        "orange" => return Some(Color::Rgb(255, 136, 0)),
        _ => {}
    }
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        let (r, g, b) = (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        );
        return Some(Color::Rgb(r, g, b));
    }
    None
}

/// Render options gathered from argv, threaded through every panel.
pub struct ViewStyle {
    /// Detail line visibility: hidden by default, `--details` or `d` shows it.
    pub details: bool,
    /// Legend (key bindings) visibility: hidden by default, `--legend` or `h`
    /// shows it.
    pub legend: bool,
    /// Scale hint visibility: hidden by default, `--legend` or `s` shows it
    /// (independent of the legend).
    pub scale: bool,
    /// `--compact`: one line per host (label, bars, detail line).
    pub compact: bool,
    /// Bar height ramp, low -> high (`--bar-style`).
    pub bar_ramp: Vec<char>,
    /// Colors for the ramp levels (`--bar-colors`); empty = `--color-style`
    /// bars, else the latency bands.
    pub bar_colors: Vec<Color>,
    /// Per-element colors (`--color-style`).
    pub colors: ColorStyle,
}

impl Default for ViewStyle {
    fn default() -> Self {
        Self {
            details: false,
            legend: false,
            scale: false,
            compact: false,
            bar_ramp: BLOCK_RAMP.to_vec(),
            bar_colors: Vec::new(),
            colors: ColorStyle::default(),
        }
    }
}

/// Per-host ping state: argv order in ping mode, discovery order for hops.
/// `id` is globally unique (ping hosts and hop pingers share the counter), so
/// worker events route unambiguously and hosts survive trace toggles.
pub struct HostState {
    pub id: usize,
    pub label: Label,
    /// Graph ring: newest sample at the back, capped at [`SAMPLE_CAP`].
    pub samples: VecDeque<Sample>,
    /// Aggregates over all samples since start (unaffected by the ring cap).
    pub totals: Totals,
    /// Set when the worker stopped unrecoverably (paused/restart message).
    pub fatal: Option<String>,
}

pub struct HopPanel {
    /// Pinger id matching the worker event's `host`, when the hop has an
    /// address to ping. Drawn from the same global counter as ping hosts.
    pub ping_id: Option<usize>,
    pub host: HostState,
}

/// One trace group = one address family's traceroute (the target may resolve
/// to both v4 and v6, and then both are traced and shown).
pub struct TraceGroup {
    /// `ip (name)` of this family's target, updated from traceroute output.
    pub label: String,
    pub hops: Vec<HopPanel>,
    pub exit_code: Option<i32>,
    /// Why the last trace is being repeated; shown in the footer.
    pub error: Option<String>,
    /// Seconds to the re-trace, refreshed each tick by the event loop.
    pub retry_secs: Option<u64>,
    /// Last non-empty stderr line from the traceroute child.
    pub stderr_tail: Option<String>,
}

/// Active traceroute state: the target and one group per selected family.
pub struct TraceState {
    pub target: String,
    pub groups: Vec<TraceGroup>,
}

pub struct App {
    /// Ping host panels; kept even while tracing (workers keep pinging, and
    /// the t key returns to them with history intact).
    ping: Vec<HostState>,
    /// The active traceroute, when tracing (`--trace` or the t key).
    trace: Option<TraceState>,
    /// The first argv host — the t key's trace destination. None in
    /// `--trace` mode (nothing to toggle back to).
    trace_destination: Option<String>,
    /// Set by the t key; consumed (and cleared) by the event loop.
    pub trace_toggle_requested: bool,
    /// Set by the 1 key; consumed (and cleared) by the event loop, which
    /// re-resolves the hosts with the new [`App::single`] setting.
    pub resolve_requested: bool,
    /// One address per family (mirrors `-1`, toggled with the 1 key).
    pub single: bool,
    /// IPv4 family enabled (mirrors `-4`/`-6`; toggled with the 4 and 6
    /// keys, re-resolving like the 1 key).
    pub family_v4: bool,
    /// IPv6 family enabled.
    pub family_v6: bool,
    pub view: ViewStyle,
    pub quit: bool,
    next_ping_id: usize,
}

impl App {
    pub fn new_ping(labels: Vec<Label>, view: ViewStyle) -> App {
        let trace_destination = labels.first().map(|l| l.ip.clone());
        App {
            ping: labels
                .into_iter()
                .map(|label| HostState {
                    id: 0, // replaced below with real ids
                    label,
                    samples: VecDeque::new(),
                    totals: Totals::default(),
                    fatal: None,
                })
                .collect(),
            trace: None,
            trace_destination,
            trace_toggle_requested: false,
            resolve_requested: false,
            single: false,
            family_v4: true,
            family_v6: true,
            view,
            quit: false,
            next_ping_id: 0,
        }
        .with_ids()
    }

    /// One group per resolved family; `labels` are the pre-resolved
    /// `ip (name)` targets, refined by traceroute's own header as it runs.
    pub fn new_trace(target: String, labels: Vec<String>, view: ViewStyle) -> App {
        App {
            ping: Vec::new(),
            trace: Some(TraceState {
                target,
                groups: labels
                    .into_iter()
                    .map(|label| TraceGroup {
                        label,
                        hops: Vec::new(),
                        exit_code: None,
                        error: None,
                        retry_secs: None,
                        stderr_tail: None,
                    })
                    .collect(),
            }),
            trace_destination: None,
            trace_toggle_requested: false,
            resolve_requested: false,
            single: false,
            family_v4: true,
            family_v6: true,
            view,
            quit: false,
            next_ping_id: 0,
        }
    }

    /// Assign globally unique ids to the ping hosts (construction helper).
    fn with_ids(mut self) -> App {
        for (i, host) in self.ping.iter_mut().enumerate() {
            host.id = i;
        }
        if !self.ping.is_empty() {
            self.next_ping_id = self.ping.len();
        }
        self
    }

    /// Single key entry point. Press-only, so terminals emitting Press+Release
    /// don't double-toggle.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => self.quit = true,
            (KeyCode::Char('c'), mods) if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            // Plain c (the guarded Ctrl+C arm above is checked first).
            (KeyCode::Char('c'), _) => self.view.compact = !self.view.compact,
            (KeyCode::Char('d'), _) => self.view.details = !self.view.details,
            (KeyCode::Char('h'), _) => self.view.legend = !self.view.legend,
            (KeyCode::Char('s'), _) => self.view.scale = !self.view.scale,
            // t toggles tracing to the first host; 1 toggles one-per-family.
            (KeyCode::Char('t'), _) => {
                if self.trace_destination.is_some() {
                    self.trace_toggle_requested = true;
                }
            }
            (KeyCode::Char('1'), _) => {
                self.single = !self.single;
                self.resolve_requested = true;
            }
            // 4/6 toggle the IPv4/IPv6 families; disabling the last enabled
            // family is ignored so the screen never goes empty.
            (KeyCode::Char('4'), _) => {
                if !(self.family_v4 && !self.family_v6) {
                    self.family_v4 = !self.family_v4;
                    self.resolve_requested = true;
                }
            }
            (KeyCode::Char('6'), _) if !(self.family_v6 && !self.family_v4) => {
                self.family_v6 = !self.family_v6;
                self.resolve_requested = true;
            }
            _ => {}
        }
    }

    /// The address families currently enabled, from the 4/6 key state.
    pub fn selected_family(&self) -> Family {
        match (self.family_v4, self.family_v6) {
            (true, false) => Family::V4,
            (false, true) => Family::V6,
            _ => Family::Both,
        }
    }

    /// Seed the family toggles from the `-4`/`-6` selection.
    pub fn set_family(&mut self, family: Family) {
        match family {
            Family::V4 => {
                self.family_v4 = true;
                self.family_v6 = false;
            }
            Family::V6 => {
                self.family_v4 = false;
                self.family_v6 = true;
            }
            Family::Both => {
                self.family_v4 = true;
                self.family_v6 = true;
            }
        }
    }

    /// Ring + totals bookkeeping for one sample.
    fn record(state: &mut HostState, sample: Sample) {
        state.samples.push_back(sample);
        if state.samples.len() > SAMPLE_CAP {
            state.samples.pop_front();
        }
        state.totals.record(sample.rtt_ms);
    }

    /// Route a worker sample by id: ping host or trace hop panel — ids are
    /// globally unique. Unknown ids (stopped workers of a previous trace)
    /// are dropped.
    pub fn push_sample(&mut self, host: usize, sample: Sample) {
        if let Some(h) = self.ping.iter_mut().find(|h| h.id == host) {
            Self::record(h, sample);
            return;
        }
        if let Some(state) = &mut self.trace
            && let Some(panel) = Self::panel_by_id(state, host)
        {
            Self::record(&mut panel.host, sample);
        }
    }

    /// A worker paused or stopped unrecoverably; its label row shows the
    /// message.
    pub fn set_fatal(&mut self, host: usize, message: String) {
        if let Some(h) = self.ping.iter_mut().find(|h| h.id == host) {
            h.fatal = Some(message);
        } else if let Some(state) = &mut self.trace
            && let Some(panel) = Self::panel_by_id(state, host)
        {
            panel.host.fatal = Some(message);
        }
    }

    /// The worker rebuilt its socket and is pinging again; clear a paused
    /// state.
    pub fn clear_fatal(&mut self, host: usize) {
        if let Some(h) = self.ping.iter_mut().find(|h| h.id == host) {
            h.fatal = None;
        } else if let Some(state) = &mut self.trace
            && let Some(panel) = Self::panel_by_id(state, host)
        {
            panel.host.fatal = None;
        }
    }

    fn panel_by_id(state: &mut TraceState, host: usize) -> Option<&mut HopPanel> {
        state.groups.iter_mut().find_map(|g| {
            g.hops.iter_mut().find_map(|p| match p {
                p if p.ping_id == Some(host) => Some(p),
                _ => None,
            })
        })
    }

    /// Add a discovered hop to `group`. Unresponsive hops (no address) are
    /// not shown at all — they produce no row and no pinger. Returns
    /// `(ping_id, ip)` when the hop has an address and the caller must spawn
    /// its pinger (IPv4 or IPv6).
    pub fn push_hop(&mut self, group: usize, hop: Hop) -> Option<(usize, IpAddr)> {
        if hop.lost || hop.addr.is_empty() {
            return None;
        }
        let ip = hop.addr.parse::<IpAddr>().ok();
        // Assign the pinger id before borrowing the group: only pinging hops
        // consume an id.
        let ping_id = ip.is_some().then(|| {
            let id = self.next_ping_id;
            self.next_ping_id += 1;
            id
        });
        let g = self.mode_trace_group(group)?;
        let label = Label {
            prefix: format!("{:>2}  ", hop.idx),
            ip: hop.addr,
            name: hop.name,
        };
        g.hops.push(HopPanel {
            ping_id,
            host: HostState {
                id: 0,
                label,
                samples: VecDeque::new(),
                totals: Totals::default(),
                fatal: None,
            },
        });
        ip.map(|ip| (ping_id.expect("a ping panel always gets an id"), ip))
    }

    fn mode_trace_group(&mut self, group: usize) -> Option<&mut TraceGroup> {
        self.trace.as_mut()?.groups.get_mut(group)
    }

    /// traceroute resolved the target itself; refine the group label to
    /// `ip (name)`.
    pub fn set_group_label(&mut self, group: usize, label: String) {
        if let Some(g) = self.mode_trace_group(group) {
            g.label = label;
        }
    }

    /// A re-trace of `group` starts: drop its hops and status (the caller has
    /// already stopped its pingers).
    pub fn reset_trace(&mut self, group: usize) {
        if let Some(g) = self.mode_trace_group(group) {
            g.hops.clear();
            g.exit_code = None;
            g.error = None;
            g.stderr_tail = None;
        }
    }

    /// Remember the last non-empty stderr line (shown on failure).
    pub fn trace_stderr(&mut self, group: usize, line: String) {
        if let Some(g) = self.mode_trace_group(group)
            && !line.trim().is_empty()
        {
            g.stderr_tail = Some(line);
        }
    }

    /// The trace failed; the reason stays in the footer until the re-trace.
    pub fn trace_failed(&mut self, group: usize, reason: String) {
        if let Some(g) = self.mode_trace_group(group) {
            g.error = Some(reason);
        }
    }

    pub fn trace_finished(&mut self, group: usize, code: i32) {
        if let Some(g) = self.mode_trace_group(group) {
            g.exit_code = Some(code);
        }
    }

    /// Countdown to the scheduled re-trace, refreshed every tick.
    pub fn set_retry_secs(&mut self, group: usize, secs: Option<u64>) {
        if let Some(g) = self.mode_trace_group(group) {
            g.retry_secs = secs;
        }
    }

    /// The group's re-trace countdown, when tracing.
    pub fn retry_secs(&self, group: usize) -> Option<u64> {
        self.trace
            .as_ref()
            .and_then(|t| t.groups.get(group))
            .and_then(|g| g.retry_secs)
    }

    /// Tracing is active (`--trace` or the t key).
    pub fn tracing(&self) -> bool {
        self.trace.is_some()
    }

    /// A ping host's address, when its id exists.
    pub fn ping_host_ip(&self, id: usize) -> Option<IpAddr> {
        self.ping
            .iter()
            .find(|h| h.id == id)
            .and_then(|h| h.label.ip.parse::<IpAddr>().ok())
    }

    /// A trace hop's address, when its pinger id exists.
    pub fn hop_ip(&self, ping_id: usize) -> Option<IpAddr> {
        self.trace.as_ref().and_then(|t| {
            t.groups.iter().find_map(|g| {
                g.hops.iter().find_map(|p| match p.ping_id {
                    Some(id) if id == ping_id => p.host.label.ip.parse::<IpAddr>().ok(),
                    _ => None,
                })
            })
        })
    }

    /// Remove a trace hop panel by pinger id (a failed IPv6 hop is skipped).
    pub fn remove_hop(&mut self, ping_id: usize) -> bool {
        if let Some(state) = &mut self.trace {
            for g in &mut state.groups {
                let before = g.hops.len();
                g.hops.retain(|p| p.ping_id != Some(ping_id));
                if g.hops.len() != before {
                    return true;
                }
            }
        }
        false
    }

    /// Remove a whole trace group (a failed IPv6 trace is skipped); later
    /// groups shift down one index. False when the index does not exist.
    pub fn remove_trace_group(&mut self, group: usize) -> bool {
        match &mut self.trace {
            Some(state) if group < state.groups.len() => {
                state.groups.remove(group);
                true
            }
            _ => false,
        }
    }

    /// Start tracing `target` (the t key): one group per family label. The
    /// ping panels are kept untouched.
    pub fn start_trace(&mut self, target: String, labels: Vec<String>) {
        self.trace = Some(TraceState {
            target,
            groups: labels
                .into_iter()
                .map(|label| TraceGroup {
                    label,
                    hops: Vec::new(),
                    exit_code: None,
                    error: None,
                    retry_secs: None,
                    stderr_tail: None,
                })
                .collect(),
        });
    }

    /// Stop tracing (the t key); the ping panels were kept all along.
    pub fn stop_trace(&mut self) {
        self.trace = None;
    }

    /// The t key's trace destination: the first host's address, when hosts
    /// were given.
    pub fn trace_destination(&self) -> Option<&str> {
        self.trace_destination.as_deref()
    }

    /// The ping host panels (render order).
    pub fn ping(&self) -> &[HostState] {
        &self.ping
    }

    /// The active trace state, when tracing.
    pub fn trace(&self) -> Option<&TraceState> {
        self.trace.as_ref()
    }

    /// Current ping host addresses with their ids (for the 1-key re-resolve
    /// diff).
    pub fn ping_ids(&self) -> Vec<(usize, IpAddr)> {
        self.ping
            .iter()
            .filter_map(|h| h.label.ip.parse::<IpAddr>().ok().map(|ip| (h.id, ip)))
            .collect()
    }

    /// Add a ping host panel; returns its globally unique id.
    pub fn add_ping_host(&mut self, label: Label) -> usize {
        let id = self.next_ping_id;
        self.next_ping_id += 1;
        self.ping.push(HostState {
            id,
            label,
            samples: VecDeque::new(),
            totals: Totals::default(),
            fatal: None,
        });
        id
    }

    /// Remove a ping host panel by id (the 1-key re-resolve dropped it).
    pub fn remove_ping_host(&mut self, id: usize) -> bool {
        let before = self.ping.len();
        self.ping.retain(|h| h.id != id);
        self.ping.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Stats;

    #[test]
    fn parses_full_color_style() {
        let style = parse_color_style(
            "title=red, bars=cyan, details=#00ff88, legend=default, scale=blue, hop=orange, ip=orange, name=dark-gray",
        )
        .unwrap();
        assert_eq!(style.title, Some(Color::Red));
        assert_eq!(style.bars, Some(Color::Cyan));
        assert_eq!(style.details, Some(Color::Rgb(0, 255, 136)));
        assert_eq!(style.legend, Some(Color::Reset));
        assert_eq!(style.scale, Some(Color::Blue));
        assert_eq!(style.hop, Some(Color::Rgb(255, 136, 0)));
        assert_eq!(style.ip, Some(Color::Rgb(255, 136, 0)));
        assert_eq!(style.name, Some(Color::DarkGray));
    }

    #[test]
    fn color_style_errors() {
        assert!(parse_color_style("nope=red").is_err(), "unknown key");
        assert!(parse_color_style("title=purple").is_err(), "unknown color");
        assert!(
            parse_color_style("title=red,title=blue").is_err(),
            "duplicate"
        );
        assert!(parse_color_style("red").is_err(), "missing =");
    }

    #[test]
    fn parses_color_lists() {
        let colors = parse_color_list("green, yellow, #ff8800").unwrap();
        assert_eq!(
            colors,
            vec![Color::Green, Color::Yellow, Color::Rgb(255, 136, 0)]
        );
        assert!(
            parse_color_list("green").is_err(),
            "one color is not a ramp"
        );
        assert!(parse_color_list("green,purple").is_err(), "unknown color");
        assert!(parse_color_list("").is_err(), "empty");
    }

    #[test]
    fn labels_render_as_plain_text() {
        let with_name = Label {
            prefix: " 1  ".into(),
            ip: "127.0.0.1".into(),
            name: Some("localhost".into()),
        };
        assert_eq!(with_name.plain_text(), " 1  127.0.0.1 (localhost)");
        assert_eq!(Label::plain("9.9.9.9").plain_text(), "9.9.9.9");
    }

    #[test]
    fn ping_ids_are_stable_and_unique() {
        let mut app = App::new_ping(
            vec![Label::plain("127.0.0.1"), Label::plain("localhost")],
            ViewStyle::default(),
        );
        let ids: Vec<usize> = app.ping.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![0, 1]);
        let added = app.add_ping_host(Label::plain("9.9.9.9"));
        assert_eq!(added, 2);
        assert!(app.remove_ping_host(0), "removed by id");
        assert!(!app.remove_ping_host(0), "second removal fails");
        assert_eq!(app.ping.len(), 2);
        // Samples still route to the surviving ids.
        app.push_sample(added, Sample { rtt_ms: Some(5.0) });
        let added_host = app.ping.iter().find(|h| h.id == added).unwrap();
        assert_eq!(Stats::from_totals(&added_host.totals).received, 1);
    }

    #[test]
    fn family_keys_toggle_families_with_a_last_family_guard() {
        let mut app = App::new_ping(
            vec![Label::plain("127.0.0.1"), Label::plain("localhost")],
            ViewStyle::default(),
        );
        assert_eq!(app.selected_family(), Family::Both);
        // 4 disables IPv4 (IPv6 stays): re-resolve requested.
        app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(app.selected_family(), Family::V6);
        assert!(app.resolve_requested);
        app.resolve_requested = false; // the main loop consumes the request
        // 6 would disable the last family: ignored, no re-resolve.
        app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
        assert_eq!(app.selected_family(), Family::V6);
        assert!(!app.resolve_requested);
        // 4 re-enables IPv4: back to both.
        app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(app.selected_family(), Family::Both);
        // 6 alone disables IPv6.
        app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
        assert_eq!(app.selected_family(), Family::V4);
        // 6 re-enables it: re-resolve requested again.
        app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
        assert_eq!(app.selected_family(), Family::Both);
        assert!(app.resolve_requested);
    }

    #[test]
    fn trace_toggle_requests_route_to_the_first_host() {
        let mut app = App::new_ping(
            vec![Label::plain("127.0.0.1"), Label::plain("localhost")],
            ViewStyle::default(),
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(app.trace_toggle_requested);
        assert_eq!(app.trace_destination(), Some("127.0.0.1"));

        // Starting the trace keeps the ping panels; stopping restores them.
        app.start_trace("127.0.0.1".into(), vec!["127.0.0.1".into()]);
        assert!(app.tracing());
        assert_eq!(app.ping.len(), 2, "ping panels survive the trace");
        app.stop_trace();
        assert!(!app.tracing());
        assert_eq!(app.ping.len(), 2);
        // --trace mode has no destination: t must not request a toggle.
        let mut app = App::new_trace(
            "t.example".into(),
            vec!["t.example".into()],
            ViewStyle::default(),
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(
            !app.trace_toggle_requested,
            "--trace mode has no t destination"
        );
    }
}
