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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use signal_hook::consts::signal::{SIGHUP, SIGQUIT, SIGTERM};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use app::App;
use cli::{CliMode, Family, IntervalMode, Parsed, USAGE};
use ping::WorkerEvent;
use trace::{TargetInfo, TraceEvent, TraceRunner};

/// Event loop tick: drain inputs, redraw.
const TICK: Duration = Duration::from_millis(50);

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

    // Pre-flight, before any terminal setup: resolve every host to all its
    // selected-family addresses (a host with both v4 and v6 yields both).
    let mut ping_hosts: Vec<(String, IpAddr)> = Vec::new();
    // argv target + (group label, is_v6) per family with an address.
    let mut trace_groups: Vec<(String, bool)> = Vec::new();
    let mut trace_target: Option<String> = None;
    match cli.mode {
        CliMode::Ping { hosts } => {
            for host in hosts {
                let addrs = resolve_all(&host, cli.family, cli.single);
                if addrs.is_empty() {
                    error_ln(&format!("error: cannot resolve '{host}'"));
                    return 1;
                }
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
            if addrs.is_empty() {
                error_ln(&format!("error: cannot resolve '{target}'"));
                return 1;
            }
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
                trace_groups.push((host_label(&target, &ip), false));
            }
            if let Some(ip) = v6 {
                trace_groups.push((host_label(&target, &ip), true));
            }
            trace_target = Some(target);
        }
    }

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
    let mut runners: Vec<TraceRunner> = Vec::new();
    let mut group_labels: Vec<String> = Vec::new();
    if let Some(target) = &trace_target {
        for (label, is_v6) in &trace_groups {
            group_labels.push(label.clone());
            runners.push(match TraceRunner::spawn(target, *is_v6, cli.no_resolve) {
                Ok(runner) => runner,
                Err(e) => {
                    // Restore first: the guard would erase a message printed
                    // on the alternate screen.
                    restore_terminal();
                    error_ln(&format!("error: failed to spawn traceroute: {e}"));
                    return 1;
                }
            });
        }
    }
    if !ping_hosts.is_empty() {
        for (i, (_, ip)) in ping_hosts.iter().enumerate() {
            let socket = match ping::create_socket(*ip) {
                Ok(socket) => socket,
                Err(e) => {
                    restore_terminal();
                    error_ln(&format!("error: ICMP sockets unavailable: {e}"));
                    return 1;
                }
            };
            // Ping-mode workers run until the process exits; their stop flag
            // is never set (they self-heal on errors instead).
            let stop = Arc::new(AtomicBool::new(false));
            let (cadence, delay) =
                worker_schedule(cli.interval_mode, cli.interval_ms, i, ping_hosts.len());
            let spec = ping::WorkerSpec {
                peer: *ip,
                is_v6: ip.is_ipv6(),
                interval: cadence,
                start_delay: delay,
                restart_delay: ping::RESTART_DELAY,
            };
            if let Err(e) = ping::spawn_worker(i, socket, spec, stop, tx.clone()) {
                restore_terminal();
                error_ln(&format!("error: failed to spawn ping workers: {e}"));
                return 1;
            }
        }
    }
    // Live pingers for trace hops, tagged by group; a group's pingers are
    // stopped when its re-trace resets the hop list.
    let mut hop_flags: Vec<(usize, Arc<AtomicBool>)> = Vec::new();

    let mut app = match trace_target {
        Some(target) => App::new_trace(target, group_labels, cli.details, cli.legend),
        None => App::new_ping(
            ping_hosts.into_iter().map(|(label, _)| label).collect(),
            cli.details,
            cli.legend,
        ),
    };

    loop {
        match pump_once(
            &mut app,
            &mut runners,
            &rx,
            &tx,
            cli.interval_ms,
            &mut hop_flags,
            &mut terminal,
        ) {
            Ok(true) => return 0,
            Ok(false) => {}
            Err(e) => return io_error_exit(e),
        }
    }
}

/// One event-loop tick: drain crossterm events within the tick (crossterm
/// 0.29 has no event::ready; poll(Duration::ZERO) drains), drain worker
/// events, poll every traceroute runner (target header, hops, exit
/// classification, re-trace timer), redraw. Returns whether to quit.
fn pump_once(
    app: &mut App,
    runners: &mut [TraceRunner],
    rx: &mpsc::Receiver<WorkerEvent>,
    tx: &mpsc::Sender<WorkerEvent>,
    interval_ms: u64,
    hop_flags: &mut Vec<(usize, Arc<AtomicBool>)>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<bool> {
    if event::poll(TICK)? {
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Resize(..) => {} // the draw below picks up the new size
                _ => {}
            }
        }
    }
    while let Ok(ev) = rx.try_recv() {
        match ev {
            WorkerEvent::Started { host } => app.clear_fatal(host),
            WorkerEvent::Sample { host, sample } => app.push_sample(host, sample),
            WorkerEvent::Paused { host, message } => app.set_fatal(host, message),
        }
    }
    for (group, runner) in runners.iter_mut().enumerate() {
        while let Some(ev) = runner.next_event() {
            match ev {
                TraceEvent::Target(info) => app.set_group_label(group, target_label(&info)),
                TraceEvent::Hop(hop) => {
                    if let Some((ping_id, ip)) = app.push_hop(group, hop) {
                        spawn_hop_pinger(app, ping_id, ip, group, interval_ms, tx, hop_flags);
                    }
                }
                TraceEvent::StderrLine(line) => app.trace_stderr(group, line),
                TraceEvent::Failed(reason) => app.trace_failed(group, reason),
                TraceEvent::Reset => {
                    // Stop only this group's pingers; the others keep running.
                    let (ours, rest): (Vec<_>, Vec<_>) =
                        hop_flags.drain(..).partition(|(g, _)| *g == group);
                    for (_, flag) in ours {
                        flag.store(true, Ordering::Relaxed);
                    }
                    *hop_flags = rest;
                    app.reset_trace(group);
                }
                TraceEvent::Done(code) => app.trace_finished(group, code),
            }
        }
        app.set_retry_secs(group, runner.retry_in_secs());
    }
    terminal.draw(|f| ui::render(f, app))?;
    Ok(app.quit)
}

/// Spawn the live pinger for a discovered hop; socket or thread failures mark
/// the hop fatal instead of killing the run. Hops ping every interval
/// regardless of --interval-modes: they appear over time, so no fixed
/// rotation can be assigned up front.
fn spawn_hop_pinger(
    app: &mut App,
    ping_id: usize,
    ip: IpAddr,
    group: usize,
    interval_ms: u64,
    tx: &mpsc::Sender<WorkerEvent>,
    hop_flags: &mut Vec<(usize, Arc<AtomicBool>)>,
) {
    match ping::create_socket(ip) {
        Ok(socket) => {
            let stop = Arc::new(AtomicBool::new(false));
            let interval = Duration::from_millis(interval_ms);
            let spec = ping::WorkerSpec {
                peer: ip,
                is_v6: ip.is_ipv6(),
                interval,
                start_delay: interval,
                restart_delay: ping::RESTART_DELAY,
            };
            match ping::spawn_worker(ping_id, socket, spec, stop.clone(), tx.clone()) {
                Ok(()) => hop_flags.push((group, stop)),
                Err(e) => app.set_fatal(ping_id, format!("worker: {e}")),
            }
        }
        Err(e) => app.set_fatal(ping_id, format!("ICMP sockets unavailable: {e}")),
    }
}

/// Per-worker (send cadence, start delay) for the selected interval mode.
/// burst: everyone sends together every interval, after one interval's delay.
/// sequence: hosts rotate one send per interval, so each host's own cadence
/// is host_count * interval, and host i first sends after (i+1) intervals.
fn worker_schedule(
    mode: IntervalMode,
    interval_ms: u64,
    host: usize,
    count: usize,
) -> (Duration, Duration) {
    match mode {
        IntervalMode::Burst => (
            Duration::from_millis(interval_ms),
            Duration::from_millis(interval_ms),
        ),
        IntervalMode::Sequence => (
            Duration::from_millis(interval_ms.saturating_mul(count as u64)),
            Duration::from_millis(interval_ms.saturating_mul(host as u64 + 1)),
        ),
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
fn host_label(argv: &str, ip: &IpAddr) -> String {
    let name = if argv.parse::<IpAddr>().is_ok() {
        reverse_name(&ip.to_string())
    } else {
        Some(argv.to_string())
    };
    match name {
        Some(n) if !n.is_empty() && n != ip.to_string() => format!("{ip} ({n})"),
        _ => ip.to_string(),
    }
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
    fn labels_are_ip_with_name() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        // A hostname argv is the name; an address argv would reverse-lookup.
        assert_eq!(host_label("localhost", &v4), "127.0.0.1 (localhost)");
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
    fn sequence_schedule_rotates_one_send_per_interval() {
        // 3 hosts, 500 ms interval: host i first sends after (i+1) intervals
        // and then every 3 intervals — consecutive sends stay 500 ms apart.
        assert_eq!(
            worker_schedule(IntervalMode::Sequence, 500, 0, 3),
            (Duration::from_millis(1500), Duration::from_millis(500))
        );
        assert_eq!(
            worker_schedule(IntervalMode::Sequence, 500, 1, 3),
            (Duration::from_millis(1500), Duration::from_millis(1000))
        );
        assert_eq!(
            worker_schedule(IntervalMode::Sequence, 500, 2, 3),
            (Duration::from_millis(1500), Duration::from_millis(1500))
        );
        // A single host degenerates to burst-like timing.
        assert_eq!(
            worker_schedule(IntervalMode::Sequence, 500, 0, 1),
            (Duration::from_millis(500), Duration::from_millis(500))
        );
    }
}
