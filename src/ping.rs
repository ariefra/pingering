//! Ping worker: one std::thread per host over unprivileged ICMP datagram
//! (ping) sockets, IPv4 and IPv6 alike (the kernel ping module assigns the
//! ident and computes the checksum for both families — probe-verified).
//! Hand-built echo packets, per-sample monotonic RTT, caller-provided cadence
//! and start delay (--interval / --interval-modes); events over std mpsc. No
//! parsing, no subprocess, no per-sample allocation (reused stack buffers).
//!
//! Self-healing: a network error, or more than [`LOSS_RESTART_LIMIT`]
//! consecutive lost pings, pauses the worker for [`RESTART_DELAY`], rebuilds
//! its socket, and resumes — the label row shows the reason until the
//! restart's Started event clears it. Failed rebuilds wait again.

use std::io::{self, ErrorKind};
use std::mem::MaybeUninit;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::app::Sample;

/// Fixed 64-byte echo packet: 8-byte ICMP header + 56-byte payload.
const PACKET_LEN: usize = 64;
/// Reply window bound: a worker never waits longer than this for one probe's
/// reply, even when its cadence is slower.
const REPLY_WINDOW_MAX: Duration = Duration::from_secs(1);
/// Consecutive losses after which the worker pauses and rebuilds (the ask:
/// all pings lost more than 6 times).
pub const LOSS_RESTART_LIMIT: u32 = 6;
/// Pause before a paused or failed worker restarts.
pub const RESTART_DELAY: Duration = Duration::from_secs(60);

/// Echo-request type byte per family; replies carry the +1 counterpart.
const ECHO_REQUEST_V4: u8 = 8;
const ECHO_REPLY_V4: u8 = 0;
const ECHO_REQUEST_V6: u8 = 128;
const ECHO_REPLY_V6: u8 = 129;

/// Everything a worker needs beyond its socket: where to ping, its family,
/// and the timing (cadence, start delay, restart pause).
pub struct WorkerSpec {
    pub peer: IpAddr,
    pub is_v6: bool,
    pub interval: Duration,
    pub start_delay: Duration,
    pub restart_delay: Duration,
}

/// Events a worker emits; `host` indexes the host/pinger id.
pub enum WorkerEvent {
    /// The worker is (re)up with a working socket; clears a paused state.
    Started {
        host: usize,
    },
    Sample {
        host: usize,
        sample: Sample,
    },
    /// The worker hit a network error or sustained loss and paused; the
    /// message says why and how long until the restart.
    Paused {
        host: usize,
        message: String,
    },
}

/// Create a connected ICMP datagram (ping) socket for `peer`, IPv4 or IPv6.
///
/// Linux ping sockets require `net.ipv4.ping_group_range` to admit the uid;
/// otherwise this fails with EPERM and the caller reports it.
pub fn create_socket(peer: IpAddr) -> io::Result<Socket> {
    let (domain, protocol, addr) = match peer {
        IpAddr::V4(v4) => (
            Domain::IPV4,
            Protocol::ICMPV4,
            SockAddr::from(SocketAddr::from((v4, 0))),
        ),
        IpAddr::V6(v6) => (
            Domain::IPV6,
            Protocol::ICMPV6,
            SockAddr::from(std::net::SocketAddrV6::new(v6, 0, 0, 0)),
        ),
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(protocol))?;
    socket.set_read_timeout(Some(REPLY_WINDOW_MAX))?;
    socket.connect(&addr)?;
    Ok(socket)
}

/// Spawn one worker thread for an already-connected socket (workers never
/// touch DNS; resolution happened once in the pre-flight). `stop` ends the
/// worker promptly when set — used to drop hop pingers on a re-trace.
pub fn spawn_worker(
    host: usize,
    socket: Socket,
    spec: WorkerSpec,
    stop: Arc<AtomicBool>,
    tx: Sender<WorkerEvent>,
) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("ping-{host}"))
        .spawn(move || run_worker(host, socket, spec, stop, tx))
        .map(|_| ())
}

/// Worker loop: wait out the start delay, then send echo seq=k at t0, wait
/// for its reply within the reply window — this worker's cadence, capped at
/// [`REPLY_WINDOW_MAX`] so the next scheduled send is never late — (timeout
/// -> lost sample; late replies for expired seqs are filtered out —
/// conservative loss accounting), sleep to t0 + `interval`, repeat. Exits
/// promptly once `stop` is set.
///
/// Sessions: one session owns a healthy socket. A network error, or more
/// than [`LOSS_RESTART_LIMIT`] consecutive losses, ends the session — the
/// worker pauses for `restart_delay`, rebuilds the socket, and starts a new
/// session (a fresh kernel ident is learned then).
pub fn run_worker(
    host: usize,
    mut socket: Socket,
    spec: WorkerSpec,
    stop: Arc<AtomicBool>,
    tx: Sender<WorkerEvent>,
) {
    if tx.send(WorkerEvent::Started { host }).is_err() {
        return;
    }
    if !wait_delay(&stop, spec.start_delay) {
        return;
    }
    let mut out = [0u8; PACKET_LEN];
    let mut reply = [MaybeUninit::<u8>::uninit(); PACKET_LEN];
    let mut seq: u16 = 0;
    'sessions: loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut learned_ident: Option<u16> = None;
        let mut losses: u32 = 0;
        let reply_window = REPLY_WINDOW_MAX.min(spec.interval);
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            seq = seq.wrapping_add(1);
            build_echo(&mut out, seq, spec.is_v6);
            let t0 = Instant::now();
            if let Err(e) = socket.send(&out) {
                let _ = tx.send(WorkerEvent::Paused {
                    host,
                    message: format!(
                        "network error: {e}; restarting in {} s",
                        spec.restart_delay.as_secs()
                    ),
                });
                socket = match rebuild_paused(&spec, host, &stop, &tx) {
                    Some(socket) => socket,
                    None => return,
                };
                continue 'sessions;
            }
            let mut rtt_ms = None;
            let deadline = t0 + reply_window;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                if socket.set_read_timeout(Some(deadline - now)).is_err() {
                    break;
                }
                match socket.recv(&mut reply) {
                    Ok(n) if n >= 8 => {
                        // SAFETY: recv initialized the first n >= 8 bytes of `reply`.
                        let header: [u8; 8] = unsafe {
                            [
                                reply[0].assume_init(),
                                reply[1].assume_init(),
                                reply[2].assume_init(),
                                reply[3].assume_init(),
                                reply[4].assume_init(),
                                reply[5].assume_init(),
                                reply[6].assume_init(),
                                reply[7].assume_init(),
                            ]
                        };
                        if accept_reply(&header, seq, &mut learned_ident, spec.is_v6) {
                            // Measured when the reply arrived, not when recv
                            // was queued — the earlier stale-timestamp
                            // variant read near-zero for every host.
                            rtt_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
                            break;
                        }
                        // Short of a match (late/foreign datagram): keep waiting.
                    }
                    Ok(_) => {} // short datagram: not an echo reply
                    Err(e)
                        if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                    {
                        break; // reply window expired -> lost sample
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerEvent::Paused {
                            host,
                            message: format!(
                                "network error: {e}; restarting in {} s",
                                spec.restart_delay.as_secs()
                            ),
                        });
                        socket = match rebuild_paused(&spec, host, &stop, &tx) {
                            Some(socket) => socket,
                            None => return,
                        };
                        continue 'sessions;
                    }
                }
            }
            if rtt_ms.is_some() {
                losses = 0;
            } else {
                losses += 1;
                if losses > LOSS_RESTART_LIMIT {
                    let _ = tx.send(WorkerEvent::Paused {
                        host,
                        message: format!(
                            "{losses} pings lost in a row; restarting in {} s",
                            spec.restart_delay.as_secs()
                        ),
                    });
                    socket = match rebuild_paused(&spec, host, &stop, &tx) {
                        Some(socket) => socket,
                        None => return,
                    };
                    continue 'sessions;
                }
            }
            if tx
                .send(WorkerEvent::Sample {
                    host,
                    sample: Sample { rtt_ms },
                })
                .is_err()
            {
                return;
            }
            let elapsed = t0.elapsed();
            if elapsed < spec.interval {
                thread::sleep(spec.interval - elapsed);
            }
        }
    }
}

/// Sleep until `delay` has passed, in small slices so a stop is honored
/// promptly; false when a stop was requested meanwhile.
fn wait_delay(stop: &AtomicBool, delay: Duration) -> bool {
    let until = Instant::now() + delay;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= until {
            return true;
        }
        thread::sleep((until - now).min(Duration::from_millis(100)));
    }
    false
}

/// Wait out the restart delay, then rebuild the worker's socket; every
/// failed attempt waits again. Returns None when a stop was requested while
/// waiting.
fn rebuild_paused(
    spec: &WorkerSpec,
    host: usize,
    stop: &AtomicBool,
    tx: &Sender<WorkerEvent>,
) -> Option<Socket> {
    loop {
        if !wait_delay(stop, spec.restart_delay) {
            return None;
        }
        match create_socket(spec.peer) {
            Ok(socket) => {
                let _ = tx.send(WorkerEvent::Started { host });
                return Some(socket);
            }
            Err(e) => {
                let _ = tx.send(WorkerEvent::Paused {
                    host,
                    message: format!(
                        "rebuild failed: {e}; waiting {} s",
                        spec.restart_delay.as_secs()
                    ),
                });
            }
        }
    }
}

/// Reply filter: accept only echo replies of the family's type for
/// `expected_seq`; before the first reply only seq is matched, afterwards the
/// kernel-assigned ident must match what we learned.
fn accept_reply(
    header: &[u8],
    expected_seq: u16,
    learned_ident: &mut Option<u16>,
    is_v6: bool,
) -> bool {
    let reply_type = if is_v6 { ECHO_REPLY_V6 } else { ECHO_REPLY_V4 };
    if header.len() < 8 || header[0] != reply_type {
        return false; // not an echo reply of this family
    }
    let ident = u16::from_be_bytes([header[4], header[5]]);
    let seq = u16::from_be_bytes([header[6], header[7]]);
    if seq != expected_seq {
        return false; // late reply for an expired seq, or unrelated
    }
    if learned_ident.is_some_and(|learned| ident != learned) {
        return false;
    }
    *learned_ident = Some(ident);
    true
}

/// Build an echo request into `buf`: type 8 (IPv4) or 128 (IPv6), code 0,
/// ident 0 and checksum 0 — the kernel ping socket rewrites both (verified
/// for both families) — seq at the tail.
fn build_echo(buf: &mut [u8; PACKET_LEN], seq: u16, is_v6: bool) {
    buf.fill(0);
    buf[0] = if is_v6 {
        ECHO_REQUEST_V6
    } else {
        ECHO_REQUEST_V4
    };
    buf[1] = 0;
    buf[6..8].copy_from_slice(&seq.to_be_bytes());
    let sum = checksum(buf);
    buf[2..4].copy_from_slice(&sum.to_be_bytes());
}

/// ICMP checksum (RFC 1071 ones' complement over 16-bit big-endian words).
/// The kernel recomputes it on ping sockets, so the value here is only a
/// well-formed placeholder.
fn checksum(data: &[u8]) -> u16 {
    let (words, rest) = data.as_chunks::<2>();
    let mut sum = 0u32;
    for word in words {
        sum += u16::from_be_bytes(*word) as u32;
    }
    if let [last] = rest {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::mpsc::channel;

    #[test]
    fn checksum_known_good_packet() {
        // Echo request, ident 0, seq 1, zero payload: the word sum is
        // 0x0801, checksum its complement (hand-computed; layout probe-
        // verified end-to-end this session).
        let mut buf = [0u8; PACKET_LEN];
        build_echo(&mut buf, 1, false);
        assert_eq!([buf[0], buf[1]], [8, 0]);
        assert_eq!(u16::from_be_bytes([buf[6], buf[7]]), 1);
        assert_eq!([buf[2], buf[3]], [0xF7, 0xFE]);
        // Folding the complete packet (checksum included) must yield 0.
        assert_eq!(checksum(&buf), 0);
    }

    #[test]
    fn v6_echo_request_uses_type_128() {
        let mut buf = [0u8; PACKET_LEN];
        build_echo(&mut buf, 7, true);
        assert_eq!(buf[0], 128);
        assert_eq!(u16::from_be_bytes([buf[6], buf[7]]), 7);
    }

    #[test]
    fn reply_filter_accepts_and_learns_kernel_ident() {
        let mut learned = None;
        let first = [0u8, 0, 0, 0, 0xF1, 0x2E, 0, 5]; // type 0, ident 61742, seq 5
        assert!(accept_reply(&first, 5, &mut learned, false));
        assert_eq!(learned, Some(0xF12E));
        let second = [0u8, 0, 0, 0, 0xF1, 0x2E, 0, 6];
        assert!(accept_reply(&second, 6, &mut learned, false));
    }

    #[test]
    fn reply_filter_accepts_v6_replies_only_as_v6() {
        let mut learned = None;
        let v6_reply = [129u8, 0, 0, 0, 0x35, 0xC3, 0, 5]; // type 129, ident 13763, seq 5
        assert!(!accept_reply(&v6_reply, 5, &mut learned, false));
        assert!(accept_reply(&v6_reply, 5, &mut learned, true));
        assert_eq!(learned, Some(0x35C3));
        let v4_reply = [0u8, 0, 0, 0, 0xF1, 0x2E, 0, 6];
        assert!(!accept_reply(&v4_reply, 6, &mut learned, true));
    }

    #[test]
    fn reply_filter_rejects_wrong_type_seq_and_ident() {
        let mut learned = Some(0xF12E);
        let request = [8u8, 0, 0, 0, 0xF1, 0x2E, 0, 6];
        assert!(!accept_reply(&request, 6, &mut learned, false));
        let late = [0u8, 0, 0, 0, 0xF1, 0x2E, 0, 9];
        assert!(!accept_reply(&late, 6, &mut learned, false));
        let foreign_ident = [0u8, 0, 0, 0, 0x12, 0x34, 0, 6];
        assert!(!accept_reply(&foreign_ident, 6, &mut learned, false));
        assert!(!accept_reply(&[0u8; 4], 6, &mut learned, false)); // too short
    }

    #[test]
    fn worker_pauses_and_restarts_on_sustained_loss() {
        // TEST-NET-1 blackholes: connect and send succeed, no reply ever
        // comes — so losses accumulate past the limit and the worker must
        // pause, rebuild its socket, and resume, repeatedly.
        let peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let socket = match create_socket(peer) {
            Ok(socket) => socket,
            Err(e) => panic!("no route to the TEST-NET blackhole: {e}"),
        };
        let spec = WorkerSpec {
            peer,
            is_v6: false,
            interval: Duration::from_millis(20),
            start_delay: Duration::ZERO,
            restart_delay: Duration::from_millis(80),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        spawn_worker(0, socket, spec, Arc::clone(&stop), tx).unwrap();
        let deadline = Instant::now() + Duration::from_millis(1500);
        let (mut paused, mut restarted) = (false, false);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(WorkerEvent::Paused { message, .. }) => {
                    paused = true;
                    assert!(
                        message.contains("pings lost in a row"),
                        "loss-triggered pause: {message}"
                    );
                }
                Ok(WorkerEvent::Started { .. }) if paused => restarted = true,
                Ok(_) => {}
                Err(_) => {}
            }
        }
        stop.store(true, Ordering::Relaxed);
        assert!(paused, "sustained loss must trigger a pause");
        assert!(restarted, "the worker must restart after the pause");
    }
}
