# pingering

`pingering` is a terminal ping with a live one-line bar graph per host, in
the spirit of gping: each sample is one block character whose shape scales
with latency, scrolling right-to-left under the host label, with an opt-in
one-line detail block under the graph. A host resolving to both IPv4 and
IPv6 is pinged on both. `--trace <host>` traces the route (one trace per
selected family) and then live-pings every hop that has an address,
mtr-style.

## Build

```
cargo build --release
# binary: target/release/pingering
```

## Usage

```
USAGE: pingering [--details] [--legend] [--compact] [-n] [-1] [-4|-6]
                 [--interval <ms>] [--interval-mode <mode>]
                 [--interface <name>] [--bar-style <chars>]
                 [--bar-colors <list>] [--color-style <spec>]
                 [--trace <host>] <hosts...>
```

Ping several hosts at once (argv order):

```
pingering --details 127.0.0.1 localhost 9.9.9.9
```

Traceroute a target and live-ping every hop (dual-stack targets trace on
both families):

```
pingering --trace 9.9.9.9
pingering -n --trace 9.9.9.9          # bare hop IPs, no name resolution
pingering -6 --trace 9.9.9.9          # IPv6 only
```

## Bar graph

One line per host, anchored at the left edge and at most 40 columns wide:
every sample is one character whose height scales with latency against an
auto-snapped scale (`▁ ▂ ▃ ▄ ▅ ▆ ▇ █`); the line fills left-to-right and
scrolls once the window is full. Color is relative to the running average of
the host:

| Latency vs average | Color |
| --- | --- |
| ≤ 0.5× average | green |
| 0.5×–1× average | yellow |
| 1×–1.5× average | orange |
| > 1.5× average | red |
| lost sample | blank |

## Layout

Hosts stack top-down and each takes only the rows it needs: a label row
(`ip (name)`), the graph line, and — when shown — one detail line:

```
Current 45.32 ms  Min 39.50  Avg 45.12  Max 67.90  Recv 532/824  Loss 35.4%
```

Hidden detail lines collapse: with details off each host is just two rows.

## Flags

| Flag | Meaning |
| --- | --- |
| `--details` | show the detail line under each graph. **Hidden by default**; `d` toggles it at any time. |
| `--legend` | show the key bindings (header) and each graph's scale. **Hidden by default**. |
| `-n` | do not resolve names: trace hops show bare IPs |
| `-1` | at most one address per family per name (default: all addresses) |
| `-4` | IPv4 only (default: both families) |
| `-6` | IPv6 only (default: both families) |
| `--interval <ms>` | ping cadence in milliseconds (default 1000) |
| `--interval-mode <mode>` | `burst` (default): all pings sent together after each interval; `sequence`: hosts take turns, one send per interval; `random`: like sequence but in random order (also jitters trace hops' first send). Trace hops otherwise ping every interval. |
| `--bar-colors <list>` | comma-separated colors for the ramp levels, low to high (2–16; overrides `--color-style bars` and the latency bands) |
| `--interface <name>` | bind ping sockets and traceroute to this interface (its address becomes the source) |
| `--bar-style <chars>` | bar height ramp, low to high, 2–16 characters (default `▁▂▃▄▅▆▇█`) |
| `--color-style <spec>` | per-element colors: comma-separated `key=value` with keys `title`, `bars`, `details`, `legend`, `scale`, `hop`, `ip`, `name`, `errors`, `message`; values are color names (`red`, `green`, `blue`, `yellow`, `magenta`, `cyan`, `white`, `gray`, `dark-gray`, `orange`, `black`), `#rrggbb`, or `default`. `bars` replaces the latency bands with one color; `scale` colors the scale hints; `hop` colors the trace hop index; `errors` colors pause/fatal messages; `message` colors the trace failure footer. |
| `--compact` | one line per host: `ip (name)`, the bar window (shrunk to fit), and the compact detail block (`Cur 45.50ms  Avg 45.12  Loss 35.4%`) side by side; respects `--details`/`d` |
| `--trace <host>` | trace mode: exactly one target, no positional hosts |
| `-h`, `--help` | print usage and exit 0 |

## Trace mode

- One trace per address family the target resolves to; each group shows its
  target as `ip (name)` (refined from traceroute's own output).
- Every hop that reports an address gets its own live ping graph, IPv4 or
  IPv6 as discovered; unresponsive hops (no address) are not shown.
- Hop names come from traceroute's reverse DNS; `-n` skips resolution.
- If a trace finds no responsive hop, or traceroute fails (interface error),
  the reason is shown in the footer and that trace repeats after 60 s until
  it succeeds.

## Self-healing

If a pinger hits a network error, or every ping is lost more than 6 times in
a row, it pauses for 60 s — the label row shows the reason (`7 pings lost in
a row; restarting in 60 s`) — then rebuilds its socket and resumes. Failed
rebuilds wait again, retrying every 60 s until the network is back.

## Keys

| Key | Action |
| --- | --- |
| `4` | toggle the IPv4 family and re-resolve |
| `6` | toggle the IPv6 family and re-resolve (the last enabled family can't be disabled) |
| `t` | toggle traceroute mode for the first host (ping mode with hosts only) |
| `1` | toggle one-address-per-family and re-resolve the hosts |
| `h` | toggle the legend (key bindings in the header) |
| `s` | toggle the scale hints |
| `c` | toggle compact mode (one line per host) |
| `d` | toggle the detail line |
| `q`, `Ctrl+C` | quit (exit 0) |

## Requirements

- Linux with ICMP ping sockets permitted: `net.ipv4.ping_group_range` must
  include your group (e.g. `sysctl -w net.ipv4.ping_group_range="0 2147483647"`);
  otherwise socket creation fails with `error: ICMP sockets unavailable`.
  This covers both ICMPv4 and ICMPv6 ping sockets.
- `--trace` needs `traceroute` in `PATH` (e.g. `/usr/sbin/traceroute`).
