//! Hand-rolled argv parser: only five flags plus help, strict duplicate and
//! unknown-flag handling. Every exit-code rule is unit-tested inline.

pub const USAGE: &str = "\
USAGE: pingering [--details] [--legend] [--compact] [-n] [-1] [-4|-6]
                 [--interval <ms>] [--interval-mode <mode>]
                 [--interface <name>] [--bar-style <chars>]
                 [--color-style <spec>] [--trace <host>] <hosts...>

  --details        show the detail line under each graph (hidden by
                   default; toggle with d)
  --legend         show the key bindings and each graph's scale (hidden
                   by default)
  --compact        one line per host: ip/name, bars, and the detail line
                   side by side (bars shrink to fit)
  -n               do not resolve names: trace hops show bare IPs
  -1               at most one address per family per name (default: all
                   addresses)
  -4               IPv4 only (default: both families)
  -6               IPv6 only (default: both families)
  --interval <ms>  ping cadence in milliseconds (default 1000)
  --interval-mode <mode>
                   how hosts share the cadence: burst sends all pings
                   together every interval (default); sequence sends one
                   host per interval, rotating; random sends one host per
                   interval in random order (also randomizes trace hops'
                   first-send phase). Trace hops otherwise ping every
                   interval
  --interface <name>
                   bind ping sockets and traceroute to this interface
  --bar-style <chars>
                   bar height ramp, low to high (2-16 characters;
                   default ▁▂▃▄▅▆▇█)
  --bar-colors <list>
                   comma-separated colors for the ramp levels, low to high
                   (2-16; overrides --color-style bars and the latency
                   bands)
  --color-style <spec>
                   comma-separated key=value colors for title, bars,
                   details, legend, scale, hop, ip, name, errors, message;
                   values: red, green, blue, yellow, magenta, cyan, white,
                   gray, dark-gray, orange, black, #rrggbb, or default
  --trace <host>   traceroute mode: one target, no positional hosts; every
                   hop with an address is live-pinged, and the trace
                   repeats after 60 s when nothing is found or traceroute
                   fails
  -h, --help       show this help and exit

Keys: t traceroute (first host), 1 single-family, h legend, s scale,
      c compact, d details, q / Ctrl+C quit.
";

/// Ping cadence when --interval is not given.
pub const DEFAULT_INTERVAL_MS: u64 = 1000;

/// How multiple hosts share the ping cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalMode {
    /// Every host sends together after each interval.
    Burst,
    /// Hosts take turns; one send per interval, so each host's own cadence
    /// is host_count * interval.
    Sequence,
    /// Like sequence, but the rotation order is randomized (also randomizes
    /// the trace hops' first-send phase).
    Random,
}

/// Address families selected on argv; the default is both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
    Both,
}

/// What the user asked for on the command line.
#[derive(Debug, PartialEq, Eq)]
pub struct Cli {
    pub details: bool,
    /// `--legend`: show the key bindings and each graph's scale.
    pub legend: bool,
    /// `--compact`: one line per host (ip/name, bars, detail line).
    pub compact: bool,
    /// `-n`: skip name resolution (trace hops show bare IPs).
    pub no_resolve: bool,
    /// `-1`: at most one address per family per name.
    pub single: bool,
    /// `--interval`: ping cadence in milliseconds.
    pub interval_ms: u64,
    /// `--interval-mode`: how hosts share the cadence.
    pub interval_mode: IntervalMode,
    /// `--interface`: bind sockets and traceroute to this interface.
    pub interface: Option<String>,
    /// `--bar-style`: custom bar height ramp, low to high.
    pub bar_style: Option<String>,
    /// `--bar-colors`: colors for the ramp levels, low to high.
    pub bar_colors: Option<String>,
    /// `--color-style`: raw per-element color spec, parsed by the app.
    pub color_style: Option<String>,
    pub family: Family,
    pub mode: CliMode,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CliMode {
    /// Ping the positional hosts continuously.
    Ping { hosts: Vec<String> },
    /// One-shot traceroute to the flag's target.
    Trace { target: String },
}

/// Parse outcome: `--help`, a runnable command, or a usage error message
/// (the caller prints it with the usage block and exits 1).
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Help,
    Run(Cli),
    Error(String),
}

pub fn parse(args: &[String]) -> Parsed {
    let mut details = false;
    let mut legend = false;
    let mut compact = false;
    let mut no_resolve = false;
    let mut single = false;
    let mut v4 = false;
    let mut v6 = false;
    let mut interval_ms: Option<u64> = None;
    let mut interval_mode: Option<IntervalMode> = None;
    let mut interface: Option<String> = None;
    let mut bar_style: Option<String> = None;
    let mut bar_colors: Option<String> = None;
    let mut color_style: Option<String> = None;
    let mut trace: Option<String> = None;
    let mut hosts: Vec<String> = Vec::new();
    let mut flags_done = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        if flags_done {
            // Everything after `--` is a host, whatever it looks like.
            if let Err(e) = push_host(arg, trace.is_some(), &mut hosts) {
                return Parsed::Error(e);
            }
            continue;
        }
        match arg {
            "--" => flags_done = true,
            "-h" | "--help" => return Parsed::Help,
            "--details" => {
                if details {
                    return Parsed::Error("duplicate --details".into());
                }
                details = true;
            }
            "--legend" => {
                if legend {
                    return Parsed::Error("duplicate --legend".into());
                }
                legend = true;
            }
            "--compact" => {
                if compact {
                    return Parsed::Error("duplicate --compact".into());
                }
                compact = true;
            }
            "--interface" => {
                if interface.is_some() {
                    return Parsed::Error("duplicate --interface".into());
                }
                let Some(value) = args.get(i) else {
                    return Parsed::Error("--interface requires a value".into());
                };
                i += 1;
                if value.starts_with('-') || value.is_empty() {
                    return Parsed::Error(format!(
                        "--interface value must be an interface name: '{value}'"
                    ));
                }
                interface = Some(value.clone());
            }
            "--bar-style" => {
                if bar_style.is_some() {
                    return Parsed::Error("duplicate --bar-style".into());
                }
                let Some(value) = args.get(i) else {
                    return Parsed::Error("--bar-style requires a value".into());
                };
                i += 1;
                let chars = value.chars().count();
                if value.starts_with('-') || !(2..=16).contains(&chars) {
                    return Parsed::Error(format!(
                        "--bar-style needs 2-16 ramp characters, low to high: '{value}'"
                    ));
                }
                bar_style = Some(value.clone());
            }
            "--bar-colors" => {
                if bar_colors.is_some() {
                    return Parsed::Error("duplicate --bar-colors".into());
                }
                let Some(value) = args.get(i) else {
                    return Parsed::Error("--bar-colors requires a value".into());
                };
                i += 1;
                if value.starts_with('-') || value.trim().is_empty() {
                    return Parsed::Error(format!(
                        "--bar-colors value must be comma-separated colors: '{value}'"
                    ));
                }
                bar_colors = Some(value.clone());
            }
            "--color-style" => {
                if color_style.is_some() {
                    return Parsed::Error("duplicate --color-style".into());
                }
                let Some(value) = args.get(i) else {
                    return Parsed::Error("--color-style requires a value".into());
                };
                i += 1;
                if value.starts_with('-') || value.trim().is_empty() {
                    return Parsed::Error(format!(
                        "--color-style value must be key=value pairs: '{value}'"
                    ));
                }
                color_style = Some(value.clone());
            }
            "-1" => {
                if single {
                    return Parsed::Error("duplicate -1".into());
                }
                single = true;
            }
            "-n" => {
                if no_resolve {
                    return Parsed::Error("duplicate -n".into());
                }
                no_resolve = true;
            }
            "-4" => {
                if v4 {
                    return Parsed::Error("duplicate -4".into());
                }
                v4 = true;
            }
            "-6" => {
                if v6 {
                    return Parsed::Error("duplicate -6".into());
                }
                v6 = true;
            }
            "--interval" => {
                if interval_ms.is_some() {
                    return Parsed::Error("duplicate --interval".into());
                }
                let Some(value) = args.get(i) else {
                    return Parsed::Error("--interval requires a value".into());
                };
                i += 1;
                if value.starts_with('-') {
                    return Parsed::Error(format!(
                        "--interval value must not start with '-': '{value}'"
                    ));
                }
                match value.parse::<u64>() {
                    Ok(0) => return Parsed::Error("--interval must be at least 1 ms".into()),
                    Ok(ms) => interval_ms = Some(ms),
                    Err(_) => return Parsed::Error(format!("invalid --interval value '{value}'")),
                }
            }
            "--interval-mode" => {
                if interval_mode.is_some() {
                    return Parsed::Error("duplicate --interval-mode".into());
                }
                let Some(value) = args.get(i) else {
                    return Parsed::Error("--interval-mode requires a value".into());
                };
                i += 1;
                if value.starts_with('-') {
                    return Parsed::Error(format!(
                        "--interval-mode value must not start with '-': '{value}'"
                    ));
                }
                match value.as_str() {
                    "burst" => interval_mode = Some(IntervalMode::Burst),
                    "sequence" => interval_mode = Some(IntervalMode::Sequence),
                    "random" => interval_mode = Some(IntervalMode::Random),
                    _ => {
                        return Parsed::Error(format!(
                            "invalid --interval-mode mode '{value}' (burst, sequence or random)"
                        ));
                    }
                }
            }
            "--trace" => {
                if trace.is_some() {
                    return Parsed::Error("duplicate --trace".into());
                }
                if !hosts.is_empty() {
                    return Parsed::Error(
                        "positional hosts cannot be combined with --trace".into(),
                    );
                }
                let Some(value) = args.get(i) else {
                    return Parsed::Error("--trace requires a value".into());
                };
                i += 1;
                if value.starts_with('-') {
                    return Parsed::Error(format!(
                        "--trace value must not start with '-': '{value}'"
                    ));
                }
                if value.is_empty() {
                    return Parsed::Error("--trace value must not be empty".into());
                }
                trace = Some(value.clone());
            }
            _ if arg.starts_with('-') => return Parsed::Error(format!("unknown flag '{arg}'")),
            _ => {
                if let Err(e) = push_host(arg, trace.is_some(), &mut hosts) {
                    return Parsed::Error(e);
                }
            }
        }
    }
    let family = match (v4, v6) {
        (true, false) => Family::V4,
        (false, true) => Family::V6,
        // No flag, or both flags: both families.
        _ => Family::Both,
    };
    let interval_ms = interval_ms.unwrap_or(DEFAULT_INTERVAL_MS);
    let interval_mode = interval_mode.unwrap_or(IntervalMode::Burst);
    match trace {
        Some(target) => Parsed::Run(Cli {
            details,
            legend,
            compact,
            no_resolve,
            single,
            interval_ms,
            interval_mode,
            interface,
            bar_style,
            bar_colors,
            color_style,
            family,
            mode: CliMode::Trace { target },
        }),
        None if hosts.is_empty() => {
            Parsed::Error("no hosts given: name at least one host, or use --trace <host>".into())
        }
        None => Parsed::Run(Cli {
            details,
            legend,
            compact,
            no_resolve,
            single,
            interval_ms,
            interval_mode,
            interface,
            bar_style,
            bar_colors,
            color_style,
            family,
            mode: CliMode::Ping { hosts },
        }),
    }
}

fn push_host(arg: &str, tracing: bool, hosts: &mut Vec<String>) -> Result<(), String> {
    if tracing {
        return Err("positional hosts cannot be combined with --trace".into());
    }
    if arg.is_empty() {
        return Err("host must not be empty".into());
    }
    hosts.push(arg.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Parsed {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_details_and_hosts() {
        match parse_strs(&["--details", "a.example", "b.example"]) {
            Parsed::Run(Cli {
                details,
                legend: _,
                compact: _,
                no_resolve: _,
                single: _,
                interval_ms,
                interval_mode,
                interface: _,
                bar_style: _,
                bar_colors: _,
                color_style: _,
                family: _,
                mode: CliMode::Ping { hosts },
            }) => {
                assert!(details);
                assert_eq!(hosts, ["a.example", "b.example"]);
                assert_eq!(interval_ms, DEFAULT_INTERVAL_MS);
                assert_eq!(interval_mode, IntervalMode::Burst);
            }
            other => panic!("expected a ping run, got {other:?}"),
        }
    }

    #[test]
    fn legend_and_single_flags_parse() {
        match parse_strs(&["--legend", "-1", "h"]) {
            Parsed::Run(Cli { legend, single, .. }) => {
                assert!(legend);
                assert!(single);
            }
            other => panic!("expected a ping run, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_legend_and_single_are_usage_errors() {
        assert!(matches!(
            parse_strs(&["--legend", "--legend", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(parse_strs(&["-1", "-1", "h"]), Parsed::Error(_)));
    }

    #[test]
    fn parses_compact_interface_bar_style_color_style() {
        match parse_strs(&[
            "--compact",
            "--interface",
            "eth0",
            "--bar-style",
            ".:-=+*#@",
            "--color-style",
            "title=red,bars=cyan,details=green,legend=gray,ip=yellow,name=blue",
            "h",
        ]) {
            Parsed::Run(Cli {
                compact,
                interface,
                bar_style,
                color_style,
                ..
            }) => {
                assert!(compact);
                assert_eq!(interface.as_deref(), Some("eth0"));
                assert_eq!(bar_style.as_deref(), Some(".:-=+*#@"));
                assert_eq!(
                    color_style.as_deref(),
                    Some("title=red,bars=cyan,details=green,legend=gray,ip=yellow,name=blue")
                );
            }
            other => panic!("expected a ping run, got {other:?}"),
        }
    }

    #[test]
    fn bad_compact_interface_bar_style_color_style_are_usage_errors() {
        assert!(matches!(
            parse_strs(&["--compact", "--compact", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(parse_strs(&["--interface"]), Parsed::Error(_)));
        assert!(matches!(
            parse_strs(&["--interface", "eth0", "--interface", "eth1", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["--bar-style", "a", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["--bar-style", "abcdefghijklmnop", "h"]),
            Parsed::Run(_)
        ));
        assert!(matches!(
            parse_strs(&["--bar-style", "abcdefghijklmnopq", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(parse_strs(&["--color-style"]), Parsed::Error(_)));
    }

    #[test]
    fn parses_interval_flags() {
        match parse_strs(&["--interval", "250", "--interval-mode", "sequence", "h"]) {
            Parsed::Run(Cli {
                interval_ms,
                interval_mode,
                ..
            }) => {
                assert_eq!(interval_ms, 250);
                assert_eq!(interval_mode, IntervalMode::Sequence);
            }
            other => panic!("expected a ping run, got {other:?}"),
        }
        assert!(matches!(
            parse_strs(&["--interval-mode", "random", "h"]),
            Parsed::Run(_)
        ));
        assert!(matches!(
            parse_strs(&["--interval", "10", "h"]),
            Parsed::Run(_)
        ));
    }

    #[test]
    fn bad_interval_values_are_usage_errors() {
        assert!(matches!(parse_strs(&["--interval"]), Parsed::Error(_)));
        assert!(matches!(
            parse_strs(&["--interval", "0", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["--interval", "abc", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["--interval", "-5", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["--interval", "1", "--interval", "2", "h"]),
            Parsed::Error(_)
        ));
    }

    #[test]
    fn bad_interval_modes_are_usage_errors() {
        assert!(matches!(parse_strs(&["--interval-mode"]), Parsed::Error(_)));
        assert!(matches!(
            parse_strs(&["--interval-mode", "both", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["--interval-mode", "-x", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["--interval-mode", "burst", "--interval-mode", "burst", "h"]),
            Parsed::Error(_)
        ));
    }

    #[test]
    fn parses_bar_colors() {
        match parse_strs(&["--bar-colors", "green,yellow,#ff8800", "h"]) {
            Parsed::Run(Cli { bar_colors, .. }) => {
                assert_eq!(bar_colors.as_deref(), Some("green,yellow,#ff8800"));
            }
            other => panic!("expected a ping run, got {other:?}"),
        }
        assert!(matches!(
            parse_strs(&["--bar-colors", "red", "--bar-colors", "red", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(parse_strs(&["--bar-colors"]), Parsed::Error(_)));
    }

    #[test]
    fn family_flags_parse() {
        let family = |args: &[&str]| match parse_strs(args) {
            Parsed::Run(Cli { family, .. }) => family,
            other => panic!("expected a run, got {other:?}"),
        };
        assert_eq!(family(&["h"]), Family::Both);
        assert_eq!(family(&["-4", "h"]), Family::V4);
        assert_eq!(family(&["-6", "h"]), Family::V6);
        assert_eq!(family(&["-4", "-6", "h"]), Family::Both);
        assert_eq!(family(&["-6", "--trace", "t"]), Family::V6);
    }

    #[test]
    fn duplicate_family_flags_are_usage_errors() {
        assert!(matches!(parse_strs(&["-4", "-4", "h"]), Parsed::Error(_)));
        assert!(matches!(parse_strs(&["-6", "-6", "h"]), Parsed::Error(_)));
    }

    #[test]
    fn parses_n_flag() {
        match parse_strs(&["-n", "--trace", "t.example"]) {
            Parsed::Run(Cli {
                no_resolve,
                mode: CliMode::Trace { target },
                ..
            }) => {
                assert!(no_resolve);
                assert_eq!(target, "t.example");
            }
            other => panic!("expected a trace run, got {other:?}"),
        }
        // Harmless in ping mode (workers never resolve).
        assert!(matches!(parse_strs(&["-n", "h.example"]), Parsed::Run(_)));
    }

    #[test]
    fn duplicate_n_is_usage_error() {
        assert!(matches!(parse_strs(&["-n", "-n", "h"]), Parsed::Error(_)));
    }

    #[test]
    fn help_exits_zero_for_both_forms() {
        assert_eq!(parse_strs(&["-h"]), Parsed::Help);
        assert_eq!(parse_strs(&["--help"]), Parsed::Help);
    }

    #[test]
    fn zero_hosts_is_usage_error() {
        assert!(matches!(parse_strs(&[]), Parsed::Error(_)));
    }

    #[test]
    fn unknown_flag_is_usage_error() {
        assert!(matches!(parse_strs(&["-x"]), Parsed::Error(_)));
        assert!(matches!(parse_strs(&["--interval", "2"]), Parsed::Error(_)));
    }

    #[test]
    fn trace_without_value_is_usage_error() {
        assert!(matches!(parse_strs(&["--trace"]), Parsed::Error(_)));
    }

    #[test]
    fn trace_flaglike_value_is_usage_error() {
        assert!(matches!(parse_strs(&["--trace", "-h"]), Parsed::Error(_)));
        assert!(matches!(
            parse_strs(&["--trace", "--details"]),
            Parsed::Error(_)
        ));
    }

    #[test]
    fn positionals_with_trace_is_usage_error() {
        assert!(matches!(
            parse_strs(&["--trace", "t.example", "h.example"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["h.example", "--trace", "t.example"]),
            Parsed::Error(_)
        ));
    }

    #[test]
    fn duplicate_flags_are_usage_errors() {
        assert!(matches!(
            parse_strs(&["--details", "--details", "h"]),
            Parsed::Error(_)
        ));
        assert!(matches!(
            parse_strs(&["--trace", "a", "--trace", "b"]),
            Parsed::Error(_)
        ));
    }

    #[test]
    fn empty_string_host_or_trace_value_is_usage_error() {
        assert!(matches!(parse_strs(&[""]), Parsed::Error(_)));
        assert!(matches!(parse_strs(&["h", ""]), Parsed::Error(_)));
        assert!(matches!(parse_strs(&["--trace", ""]), Parsed::Error(_)));
    }

    #[test]
    fn double_dash_ends_flag_parsing() {
        match parse_strs(&["--", "-x", "--details"]) {
            Parsed::Run(Cli {
                details,
                legend: _,
                compact: _,
                no_resolve: _,
                single: _,
                interval_ms: _,
                interval_mode: _,
                interface: _,
                bar_style: _,
                bar_colors: _,
                color_style: _,
                family: _,
                mode: CliMode::Ping { hosts },
            }) => {
                assert!(!details);
                assert_eq!(hosts, ["-x", "--details"]);
            }
            other => panic!("expected hosts after --, got {other:?}"),
        }
        assert!(matches!(
            parse_strs(&["--trace", "t", "--"]),
            Parsed::Run(_)
        ));
    }

    #[test]
    fn trace_mode_parses_with_details() {
        match parse_strs(&["--details", "--trace", "t.example"]) {
            Parsed::Run(Cli {
                details,
                legend: _,
                compact: _,
                no_resolve: _,
                single: _,
                interval_ms: _,
                interval_mode: _,
                interface: _,
                bar_style: _,
                bar_colors: _,
                color_style: _,
                family: _,
                mode: CliMode::Trace { target },
            }) => {
                assert!(details);
                assert_eq!(target, "t.example");
            }
            other => panic!("expected a trace run, got {other:?}"),
        }
    }
}
