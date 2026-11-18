//! pingering: gping-style terminal ping over unprivileged ICMP datagram
//! sockets (IPv4 and IPv6 — a host resolving to both shows both), plus a
//! traceroute view that live-pings every discovered hop, one trace per
//! selected family. Entry point: argv parse, pre-flight, terminal init
//! (panic hook installed first), 50 ms event loop draining crossterm +
//! worker + traceroute events and redrawing every tick, clean restore on
//! quit/panic/error, exit codes 0 (help, clean quit) and 1 (every error).

mod app;
mod cli;
mod ping;
mod stats;
mod trace;
mod ui;

use std::io::{self, Write as _};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use signal_hook::consts::signal::{SIGHUP, SIGQUIT, SIGTERM};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use app::{App, Label, ViewStyle};
use cli::{CliMode, Family, IntervalMode, Parsed, USAGE};
use ping::WorkerEvent;
use trace::{TargetInfo, TraceEvent, TraceRunner};

/// Event loop tick: drain inputs, redraw.
const TICK: Duration = Duration::from_millis(50);

/// Worker spawn context threaded through the event loop: cadence, interface
/// binding, family/mode selection, the argv hosts with their resolved
/// addresses (for the 1-key re-resolve and the IPv6->IPv4 fallback), and the
/// RNG for random-mode scheduling.
struct PingCtx {
    interval_ms: u64,
    ifbind: Option<ping::IfBind>,
    mode: IntervalMode,
    interface: Option<String>,
    no_resolve: bool,
    hosts: Vec<String>,
    resolved: Vec<(String, Vec<IpAddr>)>,
    rng: AtomicU64,
}

impl PingCtx {
    fn next_random(&self) -> u64 {
        self.rng
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |mut s| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                Some(s)
            })
            .unwrap_or(1)
    }
}

/// Which worker a stop flag belongs to: a ping host or a trace hop, each by
/// its globally unique pinger id (hops also carry their group for resets).
#[derive(Debug, Clone, Copy, PartialEq)]
enum WorkerFlag {
    Ping(usize),
    Hop { group: usize, ping_id: usize },
}

impl WorkerFlag {
    /// The pinger id this flag stops.
    fn id(&self) -> usize {
        match self {
            WorkerFlag::Ping(id) | WorkerFlag::Hop { ping_id: id, .. } => *id,
        }
    }
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let mut args = Vec::new();
    for arg in std::env::args_os().skip(1) {
        match arg.into_string() {
            Ok(s) => args.push(s),
            Err(_) => return usage_error("arguments must be valid UTF-8"),
        }
    }
    let cli = match cli::parse(&args) {
        Parsed::Help => {
            let _ = io::stdout().write_all(USAGE.as_bytes());
            let _ = io::stdout().flush();
            return 0;
        }
        Parsed::Error(msg) => return usage_error(&msg),
        Parsed::Run(cli) => cli,
    };
    let view = match build_view(&cli) {
        Ok(view) => view,
        Err(e) => return usage_error(&e),
    };
    // --interface: resolve the name to the addresses sockets will bind to,
    // before any terminal setup.
    let ifbind = match &cli.interface {
        Some(name) => match ping::lookup_interface(name) {
            Ok(bind) => Some(bind),
            Err(e) => return usage_error(&e),
        },
        None => None,
    };
    let mut ctx = PingCtx {
        interval_ms: cli.interval_ms,
        ifbind,
        resolved: Vec::new(),
        mode: cli.interval_mode,
        interface: cli.interface.clone(),
        no_resolve: cli.no_resolve,
        hosts: Vec::new(),
        rng: AtomicU64::new(seed()),
    };

    // Pre-flight, before any terminal setup: resolve every host to all its
    // selected-family addresses (a host with both v4 and v6 yields both).
    let mut ping_hosts: Vec<(Label, IpAddr)> = Vec::new();
    // argv target + (group label, is_v6) per family with an address.
    let mut trace_groups: Vec<(String, bool)> = Vec::new();
    let mut trace_target: Option<String> = None;
    match cli.mode {
        CliMode::Ping { hosts } => {
            ctx.hosts = hosts.clone();
            for host in hosts {
                let addrs = resolve_all(&host, cli.family, cli.single);
                if addrs.is_empty() {
                    error_ln(&format!("error: cannot resolve '{host}'"));
                    return 1;
                }
                ctx.resolved.push((host.clone(), addrs.clone()));
                for ip in addrs {
                    ping_hosts.push((host_label(&host, &ip), ip));
                }
            }
        }
        CliMode::Trace { target } => {
            if !traceroute_in_path() {
                error_ln("error: traceroute not found in PATH");
                return 1;
            }
            let addrs = resolve_all(&target, cli.family, cli.single);
            let mut v4 = None;
            let mut v6 = None;
            for ip in addrs {
                if v4.is_none() && ip.is_ipv4() {
                    v4 = Some(ip);
                }
                if v6.is_none() && ip.is_ipv6() {
                    v6 = Some(ip);
                }
            }
            if let Some(ip) = v4 {
                trace_groups.push((host_label(&target, &ip).plain_text(), false));
            }
            if let Some(ip) = v6 {
                trace_groups.push((host_label(&target, &ip).plain_text(), true));
            }
            if trace_groups.is_empty() {
                // Unresolvable (or the family selection excluded every
                // address): let traceroute itself report it — its stderr
                // feeds the footer and the 60 s retry loop handles DNS
                // recovery.
                trace_groups.push((target.clone(), false));
            }
            trace_target = Some(target);
        }
    }

    // The app owns the host ids; workers spawn with them below.
    let mut app = match &trace_target {
        Some(target) => App::new_trace(
            target.clone(),
            trace_groups.iter().map(|(l, _)| l.clone()).collect(),
            view,
        ),
        None => App::new_ping(ping_hosts.iter().map(|(l, _)| l.clone()).collect(), view),
    };
    app.set_family(cli.family);

    // Panic hook BEFORE terminal init: restore raw mode / alternate screen,
    // then chain the default hook.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    // Fatal signals (SIGTERM/SIGHUP/SIGQUIT, e.g. from a timeout/supervisor
    // wrapper) kill without unwinding, so neither the panic hook nor Drop
    // guards run: catch them, restore the terminal, then re-raise with the
    // default disposition so the process still dies by the signal.
    let mut signals = match signal_hook::iterator::Signals::new([SIGHUP, SIGQUIT, SIGTERM]) {
        Ok(signals) => signals,
        Err(e) => {
            error_ln(&format!("error: failed to install signal handlers: {e}"));
            return 1;
        }
    };
    if thread::Builder::new()
        .name("signal-restore".into())
        .spawn(move || {
            for sig in signals.forever() {
                restore_terminal();
                let _ = signal_hook::low_level::emulate_default_handler(sig);
            }
        })
        .is_err()
    {
        error_ln("error: failed to spawn signal handler thread");
        return 1;
    }

    // Terminal init: raw mode + alternate screen. Fails when stdin/stdout is
    // not a TTY.
    if let Err(e) = enable_raw_mode() {
        error_ln(&format!(
            "error: failed to enable raw mode (is stdin/stdout a TTY?): {e}"
        ));
        return 1;
    }
    if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        error_ln(&format!("error: failed to enter the alternate screen: {e}"));
        return 1;
    }
    let mut terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
        Ok(terminal) => terminal,
        Err(e) => {
            restore_terminal();
            error_ln(&format!(
                "error: failed to create the terminal backend: {e}"
            ));
            return 1;
        }
    };

    // From here on, every exit path restores the terminal (quit, error, panic
    // unwind through the hook and this guard).
    struct TerminalGuard;
    impl Drop for TerminalGuard {
        fn drop(&mut self) {
            restore_terminal();
        }
    }
    let _guard = TerminalGuard;

    // Start one traceroute runner per family and/or the ping workers. ICMP
    // socket creation fails e.g. when ping_group_range is restricted: clean
    // exit 1.
    let (tx, rx) = mpsc::channel();
    let mut runners: Vec<(TraceRunner, bool)> = Vec::new();
    if let Some(target) = &trace_target {
        runners = match spawn_runners(target, &trace_groups, &ctx, &tx) {
            Ok(runners) => runners,
            Err(e) => {
                restore_terminal();
                error_ln(&format!("error: failed to spawn traceroute: {e}"));
                return 1;
            }
        };
    }
    // Live pingers with their stop flags: ping hosts by id, trace hops by
    // group (a group's pingers stop when its re-trace resets the hop list;
    // the 1-key re-resolve stops dropped ping hosts).
    let mut worker_flags: Vec<(WorkerFlag, Arc<AtomicBool>)> = Vec::new();
    if !ping_hosts.is_empty() {
        let slots = shuffled_slots(ping_hosts.len(), &ctx, cli.interval_mode);
        let ids: Vec<usize> = app.ping().iter().map(|h| h.id).collect();
        for (i, (_, ip)) in ping_hosts.iter().enumerate() {
            let socket = match ping::create_socket(*ip, ifbind.as_ref()) {
                Ok(socket) => socket,
                Err(_) if ip.is_ipv6() => {
                    // IPv6 failed (no route, EPERM, interface without v6):
                    // skip it and keep the IPv4 panels — the disappearing
                    // panel is the signal; stderr would pollute the screen.
                    app.remove_ping_host(ids[i]);
                    continue;
                }
                Err(e) => {
                    restore_terminal();
                    error_ln(&format!("error: ICMP sockets unavailable: {e}"));
                    return 1;
                }
            };
            // Ping-mode workers run until the process exits; their stop flag
            // is only set when the 1-key re-resolve drops the host (they
            // self-heal on errors instead).
            let stop = Arc::new(AtomicBool::new(false));
            let (cadence, delay) = worker_schedule(
                cli.interval_mode,
                cli.interval_ms,
                slots[i],
                ping_hosts.len(),
            );
            let spec = ping::WorkerSpec {
                peer: *ip,
                is_v6: ip.is_ipv6(),
                interval: cadence,
                start_delay: delay,
                restart_delay: ping::RESTART_DELAY,
                ifbind,
            };
            if let Err(e) = ping::spawn_worker(ids[i], socket, spec, stop.clone(), tx.clone()) {
                restore_terminal();
                error_ln(&format!("error: failed to spawn ping workers: {e}"));
                return 1;
            }
            worker_flags.push((WorkerFlag::Ping(ids[i]), stop));
        }
    }

    // Frames render only when something changed; the first frame always
    // draws.
    let mut dirty = true;
    loop {
        match pump_once(
            &mut app,
            &mut runners,
            &rx,
            &tx,
            &mut ctx,
            &mut worker_flags,
            &mut terminal,
            &mut dirty,
        ) {
            Ok(true) => return 0,
            Ok(false) => {}
            Err(e) => return io_error_exit(e),
        }
    }
}

/// Assemble the render options from argv; `--color-style` and `--bar-colors`
/// parsing errors are usage errors.
fn build_view(cli: &cli::Cli) -> Result<ViewStyle, String> {
    let colors = match &cli.color_style {
        Some(spec) => app::parse_color_style(spec)?,
        None => app::ColorStyle::default(),
    };
    let bar_colors = match &cli.bar_colors {
        Some(spec) => app::parse_color_list(spec)?,
        None => Vec::new(),
    };
    Ok(ViewStyle {
        details: cli.details,
        legend: cli.legend,
        scale: cli.legend,
        compact: cli.compact,
        bar_ramp: cli
            .bar_style
            .as_ref()
            .map(|s| s.chars().collect())
            .unwrap_or_else(|| app::BLOCK_RAMP.to_vec()),
        bar_colors,
        colors,
    })
}

/// One event-loop tick: drain crossterm events within the tick (crossterm
/// 0.29 has no event::ready; poll(Duration::ZERO) drains), honor the t/1
/// toggle requests, drain worker events, poll every traceroute runner
/// (target header, hops, exit classification, re-trace timer), then redraw —
/// only when something changed (`dirty`), which skips most tick work: the
/// label/detail strings are rebuilt per draw, not per idle tick.
/// Returns whether to quit.
#[allow(clippy::too_many_arguments)] // distinct event-loop concerns
fn pump_once(
    app: &mut App,
    runners: &mut Vec<(TraceRunner, bool)>,
    rx: &mpsc::Receiver<WorkerEvent>,
    tx: &mpsc::Sender<WorkerEvent>,
    ctx: &mut PingCtx,
    worker_flags: &mut Vec<(WorkerFlag, Arc<AtomicBool>)>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    dirty: &mut bool,
) -> io::Result<bool> {
    if event::poll(TICK)? {
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => {
                    app.handle_key(key);
                    *dirty = true;
                }
                Event::Resize(..) => *dirty = true, // the draw picks up the size
                _ => {}
            }
        }
    }
    if app.trace_toggle_requested {
        app.trace_toggle_requested = false;
        *dirty = true;
        if app.tracing() {
            // Back to ping: the runners die (Drop kills the children) and the
            // hop pingers stop; the ping panels were kept all along.
            runners.clear();
            let (hops, rest): (Vec<_>, Vec<_>) = worker_flags
                .drain(..)
                .partition(|(kind, _)| matches!(kind, WorkerFlag::Hop { .. }));
            for (_, flag) in hops {
                flag.store(true, Ordering::Relaxed);
            }
            *worker_flags = rest;
            app.stop_trace();
        } else if let Some(dest) = app.trace_destination().map(str::to_string) {
            // Trace the first host: one group per family it resolves to.
            let addrs = resolve_all(&dest, app.selected_family(), app.single);
            let mut labels: Vec<String> = Vec::new();
            let mut groups: Vec<(String, bool)> = Vec::new();
            let (mut v4, mut v6) = (None, None);
            for ip in &addrs {
                if v4.is_none() && ip.is_ipv4() {
                    v4 = Some(*ip);
                }
                if v6.is_none() && ip.is_ipv6() {
                    v6 = Some(*ip);
                }
            }
            if let Some(ip) = v4 {
                labels.push(host_label(&dest, &ip).plain_text());
                groups.push((labels.last().unwrap().clone(), false));
            }
            if let Some(ip) = v6 {
                labels.push(host_label(&dest, &ip).plain_text());
                groups.push((labels.last().unwrap().clone(), true));
            }
            if groups.is_empty() {
                if let Some(id) = app.ping().first().map(|h| h.id) {
                    app.set_fatal(id, format!("cannot resolve '{dest}'"));
                }
            } else {
                match spawn_runners(&dest, &groups, ctx, tx) {
                    Ok(started) => {
                        app.start_trace(dest, labels);
                        *runners = started;
                    }
                    Err(e) => {
                        if let Some(id) = app.ping().first().map(|h| h.id) {
                            app.set_fatal(id, format!("failed to spawn traceroute: {e}"));
                        }
                    }
                }
            }
        }
    }
    if app.resolve_requested {
        app.resolve_requested = false;
        re_resolve(app, ctx, tx, worker_flags);
        *dirty = true;
    }
    while let Ok(ev) = rx.try_recv() {
        *dirty = true;
        match ev {
            WorkerEvent::Started { host } => app.clear_fatal(host),
            WorkerEvent::Sample { host, sample } => app.push_sample(host, sample),
            WorkerEvent::Paused { host, message } => {
                // A failed IPv6 pinger is skipped and the host falls back to
                // IPv4 (when it has one); IPv4 pingers keep their pause and
                // self-heal.
                let failed_ip = app.ping_host_ip(host).or_else(|| app.hop_ip(host));
                if failed_ip.is_some_and(|ip| ip.is_ipv6()) {
                    skip_failed_worker(failed_ip.unwrap(), host, ctx, app, worker_flags, tx);
                } else {
                    app.set_fatal(host, message);
                }
            }
        }
    }
    for (group, (runner, _is_v6)) in runners.iter_mut().enumerate() {
        while let Some(ev) = runner.next_event() {
            *dirty = true;
            match ev {
                TraceEvent::Target(info) => app.set_group_label(group, target_label(&info)),
                TraceEvent::Hop(hop) => {
                    if let Some((ping_id, ip)) = app.push_hop(group, hop) {
                        spawn_hop_pinger(app, ping_id, ip, group, ctx, tx, worker_flags);
                    }
                }
                TraceEvent::StderrLine(line) => app.trace_stderr(group, line),
                TraceEvent::Failed(reason) => app.trace_failed(group, reason),
                TraceEvent::Reset => {
                    // Stop only this group's pingers; the others keep running.
                    let (ours, rest): (Vec<_>, Vec<_>) = worker_flags.drain(..).partition(
                        |(kind, _)| matches!(kind, WorkerFlag::Hop { group: g, .. } if *g == group),
                    );
                    for (_, flag) in ours {
                        flag.store(true, Ordering::Relaxed);
                    }
                    *worker_flags = rest;
                    app.reset_trace(group);
                }
                TraceEvent::Done(code) => app.trace_finished(group, code),
            }
        }
        // Only a changed countdown dirties the frame (the value moves at
        // 1 Hz, not per tick).
        let secs = runner.retry_in_secs();
        if app.retry_secs(group) != secs {
            app.set_retry_secs(group, secs);
            *dirty = true;
        }
    }
    // A failed IPv6 trace group is skipped when another group (the IPv4 one)
    // remains — the last group stays visible with its error instead.
    let mut skipped: Vec<usize> = Vec::new();
    for (group, (_, is_v6)) in runners.iter().enumerate() {
        if *is_v6
            && runners.len() - skipped.len() > 1
            && app
                .trace()
                .is_some_and(|t| t.groups.get(group).is_some_and(|g| g.error.is_some()))
        {
            skipped.push(group);
        }
    }
    for gi in skipped.into_iter().rev() {
        runners.remove(gi);
        let (ours, mut rest): (Vec<_>, Vec<_>) = worker_flags
            .drain(..)
            .partition(|(kind, _)| matches!(kind, WorkerFlag::Hop { group: g, .. } if *g == gi));
        for (_, flag) in ours {
            flag.store(true, Ordering::Relaxed);
        }
        // Later groups shift down one index after the removal.
        for (kind, _) in rest.iter_mut() {
            if let WorkerFlag::Hop { group: g, .. } = kind
                && *g > gi
            {
                *g -= 1;
            }
        }
        *worker_flags = rest;
        app.remove_trace_group(gi);
        *dirty = true;
    }
    if *dirty {
        terminal.draw(|f| ui::render(f, app))?;
        *dirty = false;
    }
    Ok(app.quit)
}

/// Spawn the traceroute runners for one target's family groups.
fn spawn_runners(
    target: &str,
    groups: &[(String, bool)],
    ctx: &PingCtx,
    tx: &mpsc::Sender<WorkerEvent>,
) -> io::Result<Vec<(TraceRunner, bool)>> {
    let _ = tx;
    let mut runners = Vec::with_capacity(groups.len());
    for (_, is_v6) in groups {
        runners.push((
            TraceRunner::spawn(target, *is_v6, ctx.no_resolve, ctx.interface.as_deref())?,
            *is_v6,
        ));
    }
    Ok(runners)
}

/// Spawn the live pinger for a discovered hop; socket or thread failures mark
/// the hop fatal instead of killing the run. Hops ping every interval
/// regardless of --interval-mode: they appear over time, so no fixed
/// rotation can be assigned up front — random mode only jitters their first
/// send.
fn spawn_hop_pinger(
    app: &mut App,
    ping_id: usize,
    ip: IpAddr,
    group: usize,
    ctx: &PingCtx,
    tx: &mpsc::Sender<WorkerEvent>,
    worker_flags: &mut Vec<(WorkerFlag, Arc<AtomicBool>)>,
) {
    match ping::create_socket(ip, ctx.ifbind.as_ref()) {
        Ok(socket) => {
            let stop = Arc::new(AtomicBool::new(false));
            let interval = Duration::from_millis(ctx.interval_ms);
            let start_delay = if ctx.mode == IntervalMode::Random {
                interval + Duration::from_millis(ctx.next_random() % ctx.interval_ms.max(1))
            } else {
                interval
            };
            let spec = ping::WorkerSpec {
                peer: ip,
                is_v6: ip.is_ipv6(),
                interval,
                start_delay,
                restart_delay: ping::RESTART_DELAY,
                ifbind: ctx.ifbind,
            };
            match ping::spawn_worker(ping_id, socket, spec, stop.clone(), tx.clone()) {
                Ok(()) => worker_flags.push((WorkerFlag::Hop { group, ping_id }, stop)),
                Err(e) => app.set_fatal(ping_id, format!("worker: {e}")),
            }
        }
        Err(e) => app.set_fatal(ping_id, format!("ICMP sockets unavailable: {e}")),
    }
}

/// The failed address's host's first IPv4 address, when the failure was
/// IPv6 — the "skip it and use ipv4" fallback. None for IPv4 failures or
/// hosts without an IPv4 address.
fn fallback_v4(resolved: &[(String, Vec<IpAddr>)], failed: &IpAddr) -> Option<IpAddr> {
    if !failed.is_ipv6() {
        return None;
    }
    resolved
        .iter()
        .find(|(_, ips)| ips.contains(failed))
        .and_then(|(_, ips)| ips.iter().find(|ip| ip.is_ipv4()))
        .copied()
}

/// A failed IPv6 pinger is skipped: its worker stops, its panel is removed,
/// and the host falls back to its IPv4 address (spawning a pinger when none
/// exists, starting immediately). IPv4 pingers keep their pause and
/// self-heal instead.
fn skip_failed_worker(
    failed_ip: IpAddr,
    host: usize,
    ctx: &PingCtx,
    app: &mut App,
    worker_flags: &mut Vec<(WorkerFlag, Arc<AtomicBool>)>,
    tx: &mpsc::Sender<WorkerEvent>,
) {
    if let Some((_, flag)) = worker_flags.iter().find(|(kind, _)| kind.id() == host) {
        flag.store(true, Ordering::Relaxed);
    }
    worker_flags.retain(|(kind, _)| kind.id() != host);
    app.remove_ping_host(host);
    app.remove_hop(host);
    if let Some(v4) = fallback_v4(&ctx.resolved, &failed_ip) {
        if app.ping_ids().iter().any(|(_, cur)| *cur == v4) {
            return; // the host already pings over IPv4
        }
        let arg = ctx
            .resolved
            .iter()
            .find(|(_, ips)| ips.contains(&failed_ip))
            .map(|(a, _)| a.clone())
            .unwrap_or_default();
        let id = app.add_ping_host(host_label(&arg, &v4));
        match ping::create_socket(v4, ctx.ifbind.as_ref()) {
            Ok(socket) => {
                let stop = Arc::new(AtomicBool::new(false));
                // The fallback pinger starts immediately at the base cadence.
                let spec = ping::WorkerSpec {
                    peer: v4,
                    is_v6: false,
                    interval: Duration::from_millis(ctx.interval_ms),
                    start_delay: Duration::ZERO,
                    restart_delay: ping::RESTART_DELAY,
                    ifbind: ctx.ifbind,
                };
                match ping::spawn_worker(id, socket, spec, stop.clone(), tx.clone()) {
                    Ok(()) => worker_flags.push((WorkerFlag::Ping(id), stop)),
                    Err(e) => app.set_fatal(id, format!("worker: {e}")),
                }
            }
            Err(e) => app.set_fatal(id, format!("ICMP sockets unavailable: {e}")),
        }
    }
}

/// The 1-key re-resolve: rebuild the ping host set with the new
/// one-per-family setting. Hosts whose address sets are unchanged keep their
/// panels and history; dropped addresses stop their workers; new addresses
/// spawn workers. Diff is keyed by address.
fn re_resolve(
    app: &mut App,
    ctx: &mut PingCtx,
    tx: &mpsc::Sender<WorkerEvent>,
    worker_flags: &mut Vec<(WorkerFlag, Arc<AtomicBool>)>,
) {
    let mut want: Vec<(Label, IpAddr)> = Vec::new();
    ctx.resolved.clear();
    for arg in &ctx.hosts {
        let addrs = resolve_all(arg, app.selected_family(), app.single);
        ctx.resolved.push((arg.clone(), addrs.clone()));
        for ip in addrs {
            want.push((host_label(arg, &ip), ip));
        }
    }
    let want_ips: Vec<IpAddr> = want.iter().map(|(_, ip)| *ip).collect();
    // Drop hosts whose address is no longer wanted.
    for (id, ip) in app.ping_ids() {
        if !want_ips.contains(&ip) {
            if let Some((_, flag)) = worker_flags
                .iter()
                .find(|(kind, _)| *kind == WorkerFlag::Ping(id))
            {
                flag.store(true, Ordering::Relaxed);
            }
            worker_flags.retain(|(kind, _)| *kind != WorkerFlag::Ping(id));
            app.remove_ping_host(id);
        }
    }
    // Add hosts with brand-new addresses (existing ips keep their panels).
    for (label, ip) in &want {
        if app.ping_ids().iter().any(|(_, cur)| cur == ip) {
            continue;
        }
        let id = app.add_ping_host(label.clone());
        match ping::create_socket(*ip, ctx.ifbind.as_ref()) {
            Ok(socket) => {
                let stop = Arc::new(AtomicBool::new(false));
                let slot = app.ping().len().saturating_sub(1);
                let (cadence, delay) = worker_schedule(ctx.mode, ctx.interval_ms, slot, want.len());
                let spec = ping::WorkerSpec {
                    peer: *ip,
                    is_v6: ip.is_ipv6(),
                    interval: cadence,
                    start_delay: delay,
                    restart_delay: ping::RESTART_DELAY,
                    ifbind: ctx.ifbind,
                };
                match ping::spawn_worker(id, socket, spec, stop.clone(), tx.clone()) {
                    Ok(()) => worker_flags.push((WorkerFlag::Ping(id), stop)),
                    Err(e) => app.set_fatal(id, format!("worker: {e}")),
                }
            }
            Err(_) if ip.is_ipv6() => {
                // IPv6 failed: skip it and keep the IPv4 panels.
                app.remove_ping_host(id);
            }
            Err(e) => app.set_fatal(id, format!("ICMP sockets unavailable: {e}")),
        }
    }
}

/// Runtime I/O failure: restore the terminal, print the error, exit 1.
fn io_error_exit(e: io::Error) -> i32 {
    restore_terminal();
    error_ln(&format!("error: {e}"));
    1
}

/// Restore raw mode / alternate screen / cursor. Safe to call more than once
/// and before initialization (crossterm no-ops when raw mode was never set).
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

/// Usage errors: message + usage block to stderr, exit 1, before any
/// terminal setup.
fn usage_error(msg: &str) -> i32 {
    let _ = io::stderr().write_all(format!("error: {msg}\n\n{USAGE}").as_bytes());
    let _ = io::stderr().flush();
    1
}

fn error_ln(msg: &str) {
    let _ = io::stderr().write_all(format!("{msg}\n").as_bytes());
}

/// All addresses of `host` (deduped, resolver order), restricted to the
/// selected families; with `single`, at most one address per family (the
/// resolver's first). Empty when nothing resolves.
fn resolve_all(host: &str, family: Family, single: bool) -> Vec<IpAddr> {
    let mut out = Vec::new();
    let Ok(list) = (host, 0).to_socket_addrs() else {
        return out;
    };
    let (mut have_v4, mut have_v6) = (false, false);
    for sa in list {
        let ip = match sa {
            SocketAddr::V4(v4) => IpAddr::V4(*v4.ip()),
            SocketAddr::V6(v6) => IpAddr::V6(*v6.ip()),
        };
        let selected = !matches!(
            (family, &ip),
            (Family::V4, IpAddr::V6(_)) | (Family::V6, IpAddr::V4(_))
        );
        let family_free = !single
            || match &ip {
                IpAddr::V4(_) => !have_v4,
                IpAddr::V6(_) => !have_v6,
            };
        if selected && family_free && !out.contains(&ip) {
            if ip.is_ipv4() {
                have_v4 = true;
            } else {
                have_v6 = true;
            }
            out.push(ip);
        }
    }
    out
}

/// Label as `ip (name)`: the argv string itself when it is a hostname,
/// otherwise a best-effort reverse lookup for a literal address.
fn host_label(argv: &str, ip: &IpAddr) -> Label {
    let name = if argv.parse::<IpAddr>().is_ok() {
        reverse_name(&ip.to_string())
    } else {
        Some(argv.to_string())
    }
    .filter(|n| !n.is_empty() && n != &ip.to_string());
    let mut label = Label::plain(ip.to_string());
    label.name = name;
    label
}

/// Reverse-DNS name via getent hosts (best effort; None on any failure).
fn reverse_name(ip: &str) -> Option<String> {
    let out = Command::new("getent").args(["hosts", ip]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let mut toks = line.split_whitespace();
    toks.next()?;
    let name = toks.next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// `ip (name)` from traceroute's resolved target; a name equal to the address
/// (failed reverse lookup) is redundant and dropped.
fn target_label(info: &TargetInfo) -> String {
    match &info.name {
        Some(n) if !n.is_empty() && *n != info.addr => format!("{} ({})", info.addr, n),
        _ => info.addr.clone(),
    }
}

/// Per-worker (send cadence, start delay) for the selected interval mode.
/// burst: everyone sends together every interval, after one interval's delay.
/// sequence: hosts rotate one send per interval, so each host's own cadence
/// is host_count * interval, and the host in `slot` first sends after
/// (slot+1) intervals. random uses the same rotation with shuffled slots
/// (the shuffling happens in the caller).
fn worker_schedule(
    mode: IntervalMode,
    interval_ms: u64,
    slot: usize,
    count: usize,
) -> (Duration, Duration) {
    match mode {
        IntervalMode::Burst => (
            Duration::from_millis(interval_ms),
            Duration::from_millis(interval_ms),
        ),
        IntervalMode::Sequence | IntervalMode::Random => (
            Duration::from_millis(interval_ms.saturating_mul(count as u64)),
            Duration::from_millis(interval_ms.saturating_mul(slot as u64 + 1)),
        ),
    }
}

/// Random send slots for random mode: a shuffled permutation of
/// 0..count (Fisher-Yates over the ctx RNG). Other modes pass the identity.
fn shuffled_slots(count: usize, ctx: &PingCtx, mode: IntervalMode) -> Vec<usize> {
    let mut slots: Vec<usize> = (0..count).collect();
    if mode == IntervalMode::Random {
        for i in (1..count).rev() {
            let j = (ctx.next_random() as usize) % (i + 1);
            slots.swap(i, j);
        }
    }
    slots
}

/// Clock-seeded xorshift state (nonzero).
fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64) << 32)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1
}

/// True when an executable `traceroute` file exists in PATH (pre-flight).
fn traceroute_in_path() -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join("traceroute");
        std::fs::metadata(&candidate).is_ok_and(|m| m.is_file() && is_executable(&m))
    })
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn resolves_literals_with_family_filter() {
        let v4 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(resolve_all("127.0.0.1", Family::Both, false), vec![v4]);
        assert_eq!(resolve_all("::1", Family::V6, false), vec![v6]);
        assert_eq!(resolve_all("::1", Family::V4, false), Vec::<IpAddr>::new());
        assert_eq!(resolve_all("::1", Family::V6, true), vec![v6]);
        assert_eq!(
            resolve_all("nope.invalid", Family::Both, false),
            Vec::<IpAddr>::new()
        );
    }

    #[test]
    fn single_caps_one_address_per_family() {
        let both = resolve_all("localhost", Family::Both, false);
        assert!(!both.is_empty(), "localhost resolves on this machine");
        let single = resolve_all("localhost", Family::Both, true);
        assert!(single.iter().filter(|ip| ip.is_ipv4()).count() <= 1);
        assert!(single.iter().filter(|ip| ip.is_ipv6()).count() <= 1);
        // -1 keeps a subset of what the unbounded resolve found.
        for ip in &single {
            assert!(both.contains(ip));
        }
    }

    #[test]
    fn ipv6_failure_falls_back_to_the_host_ipv4() {
        let v4 = IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9));
        let v6 = IpAddr::V6("2620:fe::fe".parse().unwrap());
        let resolved = vec![("dual.example".to_string(), vec![v4, v6])];
        // A failed v6 address falls back to the host's IPv4.
        assert_eq!(fallback_v4(&resolved, &v6), Some(v4));
        // An IPv4 failure has no fallback.
        assert_eq!(fallback_v4(&resolved, &v4), None);
        // A v6-only host has nothing to fall back to.
        let v6_only = vec![("v6only.example".to_string(), vec![v6])];
        assert_eq!(fallback_v4(&v6_only, &v6), None);
        // An unknown address (not in the resolved map) has no fallback.
        let other = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(fallback_v4(&resolved, &other), None);
    }

    #[test]
    fn labels_are_ip_with_name() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        // A hostname argv is the name; an address argv would reverse-lookup.
        assert_eq!(
            host_label("localhost", &v4).plain_text(),
            "127.0.0.1 (localhost)"
        );
        assert_eq!(
            target_label(&TargetInfo {
                addr: "1.2.3.4".into(),
                name: Some("host.example".into()),
            }),
            "1.2.3.4 (host.example)"
        );
        assert_eq!(
            target_label(&TargetInfo {
                addr: "1.2.3.4".into(),
                name: Some("1.2.3.4".into()),
            }),
            "1.2.3.4",
            "a name equal to the address is redundant"
        );
        assert_eq!(
            target_label(&TargetInfo {
                addr: "1.2.3.4".into(),
                name: None,
            }),
            "1.2.3.4"
        );
    }

    #[test]
    fn burst_schedule_sends_together() {
        for host in 0..3 {
            assert_eq!(
                worker_schedule(IntervalMode::Burst, 500, host, 3),
                (Duration::from_millis(500), Duration::from_millis(500))
            );
        }
    }

    #[test]
    fn sequence_and_random_schedule_rotate() {
        // random uses the same rotation as sequence, with shuffled slots.
        for mode in [IntervalMode::Sequence, IntervalMode::Random] {
            assert_eq!(
                worker_schedule(mode, 500, 0, 3),
                (Duration::from_millis(1500), Duration::from_millis(500))
            );
            assert_eq!(
                worker_schedule(mode, 500, 2, 3),
                (Duration::from_millis(1500), Duration::from_millis(1500))
            );
        }
        // A single host degenerates to burst-like timing.
        assert_eq!(
            worker_schedule(IntervalMode::Random, 500, 0, 1),
            (Duration::from_millis(500), Duration::from_millis(500))
        );
    }

    #[test]
    fn shuffled_slots_are_a_permutation() {
        let ctx = PingCtx {
            interval_ms: 1000,
            ifbind: None,
            mode: IntervalMode::Random,
            interface: None,
            no_resolve: false,
            hosts: Vec::new(),
            resolved: Vec::new(),
            rng: AtomicU64::new(seed()),
        };
        let slots = shuffled_slots(5, &ctx, IntervalMode::Random);
        let mut sorted = slots.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2, 3, 4], "a permutation of 0..5");
        // The identity for non-random modes.
        assert_eq!(
            shuffled_slots(5, &ctx, IntervalMode::Burst),
            vec![0, 1, 2, 3, 4]
        );
    }
}
