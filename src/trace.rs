//! Traceroute runner: spawns `traceroute -4|-6 -q 1 -w 2 -m 30 <target>` (one
//! runner per address family; with `-n` when name resolution is disabled)
//! with LC_ALL=C, streams the resolved target header and Hop events from
//! stdout on a reader thread, captures stderr, and classifies the exit: hops
//! found -> Done (their live pingers keep running); no hop with an address,
//! or an operational error (exit >= 2, signal death, spawn failure) ->
//! Failed, and the trace is repeated after [`RETRY_DELAY`]. The child is
//! killed on drop (quit/panic).

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

/// Wait before a failed trace is repeated.
const RETRY_DELAY: Duration = Duration::from_secs(60);

/// One traceroute hop, parsed from a `-q 1` output line.
#[derive(Debug, Clone, PartialEq)]
pub struct Hop {
    pub idx: usize,
    /// Hop address as printed (empty for a lost hop with no address).
    pub addr: String,
    /// Reverse-DNS name traceroute resolved for the hop, unless `-n`.
    pub name: Option<String>,
    /// Min of the hop's probe latencies in ms (`-q 1`: the single probe).
    pub latency_ms: Option<f64>,
    pub lost: bool,
}

/// The target as traceroute itself resolved it, from the output header.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetInfo {
    pub addr: String,
    pub name: Option<String>,
}

/// Events streamed from the traceroute child and the runner's own decisions.
pub enum TraceEvent {
    Target(TargetInfo),
    Hop(Hop),
    /// One stderr line; the app keeps the tail for the failure footer.
    StderrLine(String),
    /// The trace failed; the reason is shown while the runner waits to re-trace.
    Failed(String),
    /// A re-trace is starting: drop previous hops (the caller stops their pingers).
    Reset,
    /// The child finished without a retry-worthy error; hop pinging continues.
    Done(i32),
}

pub struct TraceRunner {
    target: String,
    v6: bool,
    no_resolve: bool,
    /// Some while the child runs; killed and reaped on drop.
    child: Option<Child>,
    events: Receiver<TraceEvent>,
    /// The runner pushes its own decisions (Failed/Reset/Done) here too.
    tx: Sender<TraceEvent>,
    /// Hops with an address seen in the current trace (shared with the reader).
    ip_hops: Arc<AtomicUsize>,
    retry_at: Option<Instant>,
}

impl TraceRunner {
    /// Spawn traceroute for one address family. The pre-flight has already
    /// verified the binary exists in PATH; spawn failures after that are
    /// terminal errors.
    pub fn spawn(target: &str, v6: bool, no_resolve: bool) -> std::io::Result<TraceRunner> {
        let (tx, rx) = channel();
        let ip_hops = Arc::new(AtomicUsize::new(0));
        let mut child = Self::spawn_child(target, v6, no_resolve)?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        Self::start_readers(&tx, stdout, stderr, Arc::clone(&ip_hops))?;
        Ok(TraceRunner {
            target: target.to_string(),
            v6,
            no_resolve,
            child: Some(child),
            events: rx,
            tx,
            ip_hops,
            retry_at: None,
        })
    }

    fn spawn_child(target: &str, v6: bool, no_resolve: bool) -> std::io::Result<Child> {
        let mut cmd = Command::new("traceroute");
        cmd.args(["-q", "1", "-w", "2", "-m", "30"]);
        cmd.arg(if v6 { "-6" } else { "-4" });
        if no_resolve {
            cmd.arg("-n");
        }
        cmd.arg(target)
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn()
    }

    fn start_readers(
        tx: &Sender<TraceEvent>,
        stdout: ChildStdout,
        stderr: ChildStderr,
        ip_hops: Arc<AtomicUsize>,
    ) -> std::io::Result<()> {
        let tx_out = tx.clone();
        thread::Builder::new()
            .name("trace-stdout".into())
            .spawn(move || {
                // The first line is the resolved-target header; the rest are
                // hop lines.
                let mut header_seen = false;
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if !header_seen {
                        header_seen = true;
                        if let Some(info) = parse_target_line(&line)
                            && tx_out.send(TraceEvent::Target(info)).is_err()
                        {
                            return;
                        }
                        continue;
                    }
                    let Some(hop) = parse_hop(&line) else {
                        continue;
                    };
                    if !hop.addr.is_empty() {
                        ip_hops.fetch_add(1, Ordering::Relaxed);
                    }
                    if tx_out.send(TraceEvent::Hop(hop)).is_err() {
                        return;
                    }
                }
            })?;
        let tx_err = tx.clone();
        thread::Builder::new()
            .name("trace-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if tx_err.send(TraceEvent::StderrLine(line)).is_err() {
                        return;
                    }
                }
            })?;
        Ok(())
    }

    /// Drain one event; also drives child-exit classification and the
    /// re-trace timer. Non-blocking.
    pub fn next_event(&mut self) -> Option<TraceEvent> {
        if let Some(child) = self.child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            self.child = None;
            let code = status.code().unwrap_or(-1);
            match classify_exit(code, self.ip_hops.load(Ordering::Relaxed)) {
                Outcome::Done => {
                    let _ = self.tx.send(TraceEvent::Done(code));
                }
                Outcome::Retry(reason) => {
                    self.retry_at = Some(Instant::now() + RETRY_DELAY);
                    let _ = self.tx.send(TraceEvent::Failed(reason.to_string()));
                }
            }
        }
        if self.child.is_none() && self.retry_at.is_some_and(|at| Instant::now() >= at) {
            self.retry_at = None;
            self.ip_hops.store(0, Ordering::Relaxed);
            let _ = self.tx.send(TraceEvent::Reset);
            match Self::spawn_child(&self.target, self.v6, self.no_resolve) {
                Ok(mut child) => {
                    let stdout = child.stdout.take().expect("stdout was piped");
                    let stderr = child.stderr.take().expect("stderr was piped");
                    if let Err(e) =
                        Self::start_readers(&self.tx, stdout, stderr, Arc::clone(&self.ip_hops))
                    {
                        self.retry_at = Some(Instant::now() + RETRY_DELAY);
                        let _ = self.tx.send(TraceEvent::Failed(format!(
                            "failed to start traceroute readers: {e}"
                        )));
                    }
                    self.child = Some(child);
                }
                Err(e) => {
                    self.retry_at = Some(Instant::now() + RETRY_DELAY);
                    let _ = self.tx.send(TraceEvent::Failed(format!(
                        "failed to spawn traceroute: {e}"
                    )));
                }
            }
        }
        self.events.try_recv().ok()
    }

    /// Seconds until the scheduled re-trace, while one is pending.
    pub fn retry_in_secs(&self) -> Option<u64> {
        self.retry_at
            .map(|at| at.saturating_duration_since(Instant::now()).as_secs())
    }
}

impl Drop for TraceRunner {
    fn drop(&mut self) {
        // Kill the child on quit/panic/drop and reap it so it does not linger.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Exit classification: Done keeps the found hops pinged; Retry schedules a
/// re-trace. traceroute exit codes (Butskoy): 0 all probes replied, 1 some
/// probes lost, >= 2 operational errors (net unreachable, bad usage, ...);
/// a signal death has no code (reported as -1). "No address at all" always
/// retries whatever the code — nothing was found to ping.
enum Outcome {
    Done,
    Retry(&'static str),
}

fn classify_exit(code: i32, ip_hops: usize) -> Outcome {
    if ip_hops == 0 {
        return Outcome::Retry("no responsive hops found");
    }
    match code {
        0 | 1 => Outcome::Done,
        _ => Outcome::Retry("traceroute failed"),
    }
}

/// Parse one traceroute line into a Hop. Deliberately lenient token scan (no
/// regex). Forms handled (LC_ALL=C, Butskoy traceroute):
///   ` 1  127.0.0.1  0.592 ms`                     (with -n, or unresolved)
///   ` 1  gateway.local (192.168.1.1)  0.592 ms`   (resolved: name (ip))
///   ` 3  *` / ` 3  * * *`                         (lost)
///   ` 2  10.0.0.1  30.000 ms !H`                  (trailing !-markers ignored)
/// Header and blank lines yield None.
pub fn parse_hop(line: &str) -> Option<Hop> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let idx: usize = toks.first()?.parse().ok()?;
    let second = *toks.get(1)?;
    if second == "*" {
        return Some(Hop {
            idx,
            addr: String::new(),
            name: None,
            latency_ms: None,
            lost: true,
        });
    }
    // Reached hop: `name (ip)` (two tokens) or a bare address token.
    let (addr, name) = match paren_addr(second) {
        Some(ip) => (ip, None),
        None => match toks.get(2).and_then(|t| paren_addr(t)) {
            Some(ip) => (ip, Some(second.to_string())),
            None => (second.to_string(), None),
        },
    };
    // Latency: scan the rest for `<number> ms`; trailing !-markers ignored.
    let mut latency_ms: Option<f64> = None;
    let mut prev = "";
    for token in &toks[1..] {
        if *token == "ms" {
            let number = prev.split_once('!').map_or(prev, |(n, _)| n);
            if let Ok(value) = number.parse::<f64>() {
                latency_ms = Some(latency_ms.map_or(value, |cur| cur.min(value)));
            }
        }
        prev = token;
    }
    Some(Hop {
        idx,
        addr,
        name,
        latency_ms,
        lost: false,
    })
}

/// The address inside a `(192.168.1.1)` token.
fn paren_addr(token: &str) -> Option<String> {
    let inner = token.strip_prefix('(')?.strip_suffix(')')?;
    (!inner.is_empty()).then(|| inner.to_string())
}

/// The output header `traceroute to <host> (<ip>), 30 hops max, ...` — the
/// target as traceroute resolved it. Without resolution (`-n`, or a failed
/// reverse lookup) the host token is the bare address and no name is known.
pub fn parse_target_line(line: &str) -> Option<TargetInfo> {
    let rest = line.strip_prefix("traceroute to ")?;
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let host = toks.first()?.trim_end_matches(',');
    if host.is_empty() {
        return None;
    }
    let ip = toks
        .get(1)
        .and_then(|t| paren_addr(t.trim_end_matches(',')));
    Some(TargetInfo {
        addr: ip.clone().unwrap_or_else(|| host.to_string()),
        name: ip.map(|_| host.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reached_hop_without_name() {
        // Fixture: /usr/sbin/traceroute -n -q 1 -w 2 127.0.0.1 (Butskoy 2.1.6).
        let hop = parse_hop(" 1  127.0.0.1  0.592 ms").unwrap();
        assert_eq!(hop.idx, 1);
        assert_eq!(hop.addr, "127.0.0.1");
        assert_eq!(hop.name, None);
        assert_eq!(hop.latency_ms, Some(0.592));
        assert!(!hop.lost);
    }

    #[test]
    fn parses_named_hop() {
        let hop = parse_hop(" 1  gateway.local (192.168.1.1)  0.592 ms").unwrap();
        assert_eq!(hop.addr, "192.168.1.1");
        assert_eq!(hop.name.as_deref(), Some("gateway.local"));
        assert_eq!(hop.latency_ms, Some(0.592));
        assert!(!hop.lost);
    }

    #[test]
    fn named_hop_without_reply_still_has_the_address() {
        let hop = parse_hop(" 3  host.example (10.0.0.9)  *").unwrap();
        assert_eq!(hop.addr, "10.0.0.9");
        assert_eq!(hop.name.as_deref(), Some("host.example"));
        assert_eq!(hop.latency_ms, None);
        assert!(!hop.lost);
    }

    #[test]
    fn parses_lost_hop() {
        // Fixture: the same command against 1.1.1.1 (unreplied probes).
        let hop = parse_hop(" 3  *").unwrap();
        assert_eq!(hop.idx, 3);
        assert!(hop.lost);
        assert_eq!(hop.addr, "");
        assert_eq!(hop.name, None);
        assert_eq!(hop.latency_ms, None);
    }

    #[test]
    fn ignores_unreachable_markers() {
        let hop = parse_hop(" 2  10.0.0.1  30.000 ms !H").unwrap();
        assert_eq!(hop.addr, "10.0.0.1");
        assert_eq!(hop.latency_ms, Some(30.0));
        assert!(!hop.lost);
    }

    #[test]
    fn ignores_header_and_blank_lines() {
        assert_eq!(
            parse_hop("traceroute to 127.0.0.1 (127.0.0.1), 30 hops max, 60 byte packets"),
            None
        );
        assert_eq!(parse_hop(""), None);
        assert_eq!(parse_hop("   "), None);
    }

    #[test]
    fn multi_star_line_is_lost() {
        let hop = parse_hop(" 5  * * *").unwrap();
        assert!(hop.lost);
    }

    #[test]
    fn exit_classification() {
        // Route found: 0 (all replied) and 1 (some probes lost) are Done.
        assert!(matches!(classify_exit(0, 3), Outcome::Done));
        assert!(matches!(classify_exit(1, 1), Outcome::Done));
        // Nothing to ping: retry whatever the code says.
        assert!(matches!(classify_exit(0, 0), Outcome::Retry(_)));
        assert!(matches!(classify_exit(1, 0), Outcome::Retry(_)));
        // Operational errors (>= 2) and signal death (-1): retry.
        assert!(matches!(classify_exit(2, 5), Outcome::Retry(_)));
        assert!(matches!(classify_exit(-1, 2), Outcome::Retry(_)));
    }

    #[test]
    fn parses_target_header_with_name() {
        // Fixture: traceroute (Butskoy 2.1.6) header with a resolved target.
        let info = parse_target_line(
            "traceroute to dns9.quad9.net (9.9.9.9), 30 hops max, 60 byte packets",
        )
        .unwrap();
        assert_eq!(info.addr, "9.9.9.9");
        assert_eq!(info.name.as_deref(), Some("dns9.quad9.net"));
    }

    #[test]
    fn parses_target_header_without_name() {
        // With -n (or a failed reverse lookup) traceroute prints no parens;
        // when the reverse name equals the address it is kept as printed and
        // the caller drops the redundant name.
        let info = parse_target_line("traceroute to 9.9.9.9 (9.9.9.9), 30 hops max").unwrap();
        assert_eq!(info.addr, "9.9.9.9");
        assert_eq!(info.name.as_deref(), Some("9.9.9.9"));
        let info = parse_target_line("traceroute to 9.9.9.9, 30 hops max").unwrap();
        assert_eq!(info.addr, "9.9.9.9");
        assert_eq!(info.name, None);
    }

    #[test]
    fn non_header_lines_are_not_targets() {
        assert_eq!(parse_target_line(" 1  127.0.0.1  0.592 ms"), None);
        assert_eq!(parse_target_line(""), None);
    }
}
