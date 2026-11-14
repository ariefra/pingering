//! App state: mode (ping hosts | trace target), detail flag, per-host sample
//! ring (capped, fixed memory) plus totals, trace groups with live-pinged
//! hops. `handle_key` is the single key entry point shared by the event loop
//! and the render tests.

use std::collections::VecDeque;
use std::net::IpAddr;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::stats::Totals;
use crate::trace::Hop;

/// Fixed memory: at most this many samples are kept per host for the graph.
pub const SAMPLE_CAP: usize = 512;

/// One ping outcome: `Some` RTT in ms for a reply, `None` for a timeout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub rtt_ms: Option<f64>,
}

/// Per-host ping state: argv order in ping mode, discovery order for hops.
pub struct HostState {
    pub name: String,
    /// Graph ring: newest sample at the back, capped at [`SAMPLE_CAP`].
    pub samples: VecDeque<Sample>,
    /// Aggregates over all samples since start (unaffected by the ring cap).
    pub totals: Totals,
    /// Set when the worker thread stopped with an unrecoverable error.
    pub fatal: Option<String>,
}

pub struct HopPanel {
    /// Pinger id matching the worker event's `host`, when the hop has an
    /// address to ping. Globally monotonic, so a late sample from a stopped
    /// worker of a previous trace can never land in a new panel.
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

/// Trace-mode state: the argv target and one group per selected family.
pub struct TraceState {
    pub target: String,
    pub groups: Vec<TraceGroup>,
}

pub enum Mode {
    Ping { hosts: Vec<HostState> },
    Trace(TraceState),
}

pub struct App {
    pub mode: Mode,
    /// Detail line visibility: hidden by default, `--details` or `d` shows it.
    pub details: bool,
    /// Legend visibility: hidden by default, `--legend` shows the key
    /// bindings and each graph's scale.
    pub legend: bool,
    pub quit: bool,
    next_ping_id: usize,
}

impl App {
    pub fn new_ping(hosts: Vec<String>, details: bool, legend: bool) -> App {
        App {
            mode: Mode::Ping {
                hosts: hosts
                    .into_iter()
                    .map(|name| HostState {
                        name,
                        samples: VecDeque::new(),
                        totals: Totals::default(),
                        fatal: None,
                    })
                    .collect(),
            },
            details,
            legend,
            quit: false,
            next_ping_id: 0,
        }
    }

    /// One group per resolved family; `labels` are the pre-resolved
    /// `ip (name)` targets, refined by traceroute's own header as it runs.
    pub fn new_trace(target: String, labels: Vec<String>, details: bool, legend: bool) -> App {
        App {
            mode: Mode::Trace(TraceState {
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
            details,
            legend,
            quit: false,
            next_ping_id: 0,
        }
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
            (KeyCode::Char('d'), _) => self.details = !self.details,
            _ => {}
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

    /// Route a worker sample by pinger id: argv index in ping mode, hop panel
    /// id in trace mode. Unknown ids (stopped workers of a previous trace)
    /// are dropped.
    pub fn push_sample(&mut self, host: usize, sample: Sample) {
        match &mut self.mode {
            Mode::Ping { hosts } => {
                if let Some(h) = hosts.get_mut(host) {
                    Self::record(h, sample);
                }
            }
            Mode::Trace(state) => {
                if let Some(panel) = Self::panel_by_id(state, host) {
                    Self::record(&mut panel.host, sample);
                }
            }
        }
    }

    /// A worker stopped unrecoverably; its label row shows the message.
    pub fn set_fatal(&mut self, host: usize, message: String) {
        match &mut self.mode {
            Mode::Ping { hosts } => {
                if let Some(h) = hosts.get_mut(host) {
                    h.fatal = Some(message);
                }
            }
            Mode::Trace(state) => {
                if let Some(panel) = Self::panel_by_id(state, host) {
                    panel.host.fatal = Some(message);
                }
            }
        }
    }

    /// The worker rebuilt its socket and is pinging again; clear a paused
    /// state.
    pub fn clear_fatal(&mut self, host: usize) {
        match &mut self.mode {
            Mode::Ping { hosts } => {
                if let Some(h) = hosts.get_mut(host) {
                    h.fatal = None;
                }
            }
            Mode::Trace(state) => {
                if let Some(panel) = Self::panel_by_id(state, host) {
                    panel.host.fatal = None;
                }
            }
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
        let label = match hop.name {
            Some(name) => format!("{:>2}  {} ({})", hop.idx, hop.addr, name),
            None => format!("{:>2}  {}", hop.idx, hop.addr),
        };
        g.hops.push(HopPanel {
            ping_id,
            host: HostState {
                name: label,
                samples: VecDeque::new(),
                totals: Totals::default(),
                fatal: None,
            },
        });
        ip.map(|ip| (ping_id.expect("a ping panel always gets an id"), ip))
    }

    fn mode_trace_group(&mut self, group: usize) -> Option<&mut TraceGroup> {
        match &mut self.mode {
            Mode::Trace(state) => state.groups.get_mut(group),
            Mode::Ping { .. } => None,
        }
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
}
