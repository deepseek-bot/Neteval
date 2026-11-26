use clap::{Args, Parser, Subcommand};
use protocol::{DEFAULT_PUNCH_START_DELAY_MS, DEFAULT_PUNCH_TIMEOUT_MS};
use server::log_sink;
use server::nat_echo::{run_echo_server, EchoServerConfig};
use server::signaling::{run_signaling_server, SignalingServerConfig};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;

const DEFAULT_PROBE_PORT_P1: u16 = 34778;
const DEFAULT_PROBE_PORT_P2: u16 = 34779;
const DEFAULT_SIGNALING_WS_PORT: u16 = 34780;

#[derive(Debug, Parser)]
#[command(
    name = "neteval-server",
    version,
    about = "NetEval Server (S1 Signaling & S2 NAT Echo)",
    after_help = "\
Examples:
  neteval-server start
  neteval-server start ws:34780 --token secret
  neteval-server start ws:34780 --probe 34778,34779

Default ports: 34778/udp (probe p1), 34779/udp (probe p2), 34780/tcp (signaling ws)"
)]
struct CliArgs {
    #[command(subcommand)]
    command: Option<ServerCommand>,

    #[arg(long, default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PROBE_PORT_P1))]
    p1: SocketAddr,

    #[arg(long)]
    p2: Option<SocketAddr>,

    #[arg(long)]
    v6: Option<SocketAddr>,

    #[arg(long, global = true)]
    helper: Option<SocketAddr>,

    #[arg(long)]
    ws: Option<SocketAddr>,

    #[arg(long)]
    ws_v6: Option<SocketAddr>,

    #[arg(long, global = true, default_value_t = DEFAULT_PUNCH_START_DELAY_MS)]
    ws_start_delay_ms: u32,

    #[arg(long, global = true, default_value_t = DEFAULT_PUNCH_TIMEOUT_MS)]
    ws_punch_duration_ms: u32,

    #[arg(long, global = true)]
    token: Option<String>,

    #[arg(long, global = true)]
    responder_id: Option<String>,

    #[arg(long, global = true, default_value = "logs/latest.log")]
    log_file: PathBuf,

    #[arg(long, global = true, default_value_t = false)]
    truncate_log: bool,
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    /// Start signaling and UDP probe services with dual-stack defaults
    Start(StartArgs),
}

#[derive(Debug, Args)]
#[command(
    after_help = "\
Examples:
  neteval-server start
  neteval-server start ws:34780 --token secret
  neteval-server start ws:34780 --probe 34778,34779
  neteval-server start ws:34780 --no-probe

Default ports: 34778/udp (probe p1), 34779/udp (probe p2), 34780/tcp (signaling ws)"
)]
struct StartArgs {
    /// Endpoints in <proto>:<port> form (supported: ws; default: ws:34780)
    #[arg(value_name = "PROTO:PORT")]
    endpoints: Vec<String>,

    /// UDP probe/echo ports as P1 or P1,P2 (single port derives P2 = P1 + 1)
    #[arg(long, value_name = "P1[,P2]", default_value = "34778,34779")]
    probe: Option<String>,

    /// Disable UDP probe/echo server so only signaling runs
    #[arg(long, default_value_t = false)]
    no_probe: bool,

    /// IPv4 address to bind listeners on
    #[arg(long, default_value_t = Ipv4Addr::UNSPECIFIED)]
    bind_v4: Ipv4Addr,

    /// IPv6 address to bind listeners on
    #[arg(long, default_value_t = Ipv6Addr::UNSPECIFIED)]
    bind_v6: Ipv6Addr,

    /// Disable IPv6 listeners on both WebSocket signaling and UDP probe p1
    #[arg(long, default_value_t = false)]
    no_ipv6: bool,
}

fn parse_nonzero_port(raw_port: &str, context: &str) -> Result<u16, String> {
    let trimmed = raw_port.trim();
    let port: u16 = trimmed.parse().map_err(|_| {
        format!("invalid port '{trimmed}' in '{context}': expected integer in 1..=65535")
    })?;
    if port == 0 {
        return Err(format!(
            "invalid port 0 in '{context}': port must be in 1..=65535"
        ));
    }
    Ok(port)
}

fn parse_endpoints(endpoints: &[String]) -> Result<u16, String> {
    if endpoints.is_empty() {
        return Ok(DEFAULT_SIGNALING_WS_PORT);
    }

    let mut ws_port: Option<u16> = None;
    for raw in endpoints {
        let Some((proto_raw, port_raw)) = raw.split_once(':') else {
            return Err(format!(
                "invalid endpoint '{raw}': expected <proto>:<port> (supported protocols: ws)"
            ));
        };
        let proto = proto_raw.trim().to_ascii_lowercase();
        match proto.as_str() {
            "ws" => {
                let port = parse_nonzero_port(port_raw, raw)?;
                if ws_port.is_some() {
                    return Err(format!("duplicate 'ws' endpoint '{raw}'"));
                }
                ws_port = Some(port);
            }
            "" => {
                return Err(format!(
                    "invalid endpoint '{raw}': missing protocol before ':' (supported protocols: ws)"
                ));
            }
            other => {
                return Err(format!(
                    "unsupported protocol '{other}' in endpoint '{raw}'; supported protocols: ws"
                ));
            }
        }
    }

    ws_port.ok_or_else(|| "no ws endpoint configured (supported protocols: ws)".to_string())
}

fn parse_probe_ports(spec: &str) -> Result<(u16, u16), String> {
    let parts: Vec<&str> = spec.split(',').collect();
    let (p1, p2) = match parts.as_slice() {
        [p1_raw] => {
            let p1 = parse_nonzero_port(p1_raw, "--probe")?;
            let p2 = p1.checked_add(1).ok_or_else(|| {
                format!(
                    "invalid --probe '{spec}': p1 port {p1} overflows when deriving p2 = p1 + 1"
                )
            })?;
            (p1, p2)
        }
        [p1_raw, p2_raw] => {
            let p1 = parse_nonzero_port(p1_raw, "--probe")?;
            let p2 = parse_nonzero_port(p2_raw, "--probe")?;
            (p1, p2)
        }
        _ => {
            return Err(format!(
                "invalid --probe '{spec}': expected <P1> or <P1>,<P2> with ports in 1..=65535"
            ));
        }
    };

    if p1 == p2 {
        return Err(format!(
            "invalid --probe '{spec}': p1 and p2 must be distinct ports (got {p1})"
        ));
    }

    Ok((p1, p2))
}

fn resolve_start_configs(
    cli: &CliArgs,
    start: &StartArgs,
) -> Result<(Option<EchoServerConfig>, Option<SignalingServerConfig>), String> {
    let ws_port = parse_endpoints(&start.endpoints)?;

    let echo_cfg = if start.no_probe {
        None
    } else {
        let (p1_port, p2_port) = match start.probe.as_deref() {
            Some(spec) => parse_probe_ports(spec)?,
            None => (DEFAULT_PROBE_PORT_P1, DEFAULT_PROBE_PORT_P2),
        };
        Some(EchoServerConfig {
            bind_p1: SocketAddr::new(IpAddr::V4(start.bind_v4), p1_port),
            bind_p2: Some(SocketAddr::new(IpAddr::V4(start.bind_v4), p2_port)),
            bind_v6: (!start.no_ipv6).then(|| SocketAddr::new(IpAddr::V6(start.bind_v6), p1_port)),
            bind_v6_best_effort: true,
            helper_addr: cli.helper,
            access_token: cli.token.clone(),
            responder_id: cli.responder_id.clone(),
        })
    };

    let signaling_cfg = SignalingServerConfig {
        bind_ws: SocketAddr::new(IpAddr::V4(start.bind_v4), ws_port),
        bind_ws_v6: (!start.no_ipv6).then(|| SocketAddr::new(IpAddr::V6(start.bind_v6), ws_port)),
        bind_ws_v6_best_effort: true,
        access_token: cli.token.clone(),
        start_delay_ms: cli.ws_start_delay_ms,
        punch_duration_ms: cli.ws_punch_duration_ms,
        ..Default::default()
    };

    Ok((echo_cfg, Some(signaling_cfg)))
}

fn resolve_legacy_configs(cli: &CliArgs) -> (Option<EchoServerConfig>, Option<SignalingServerConfig>) {
    let echo_cfg = EchoServerConfig {
        bind_p1: cli.p1,
        bind_p2: cli.p2,
        bind_v6: cli.v6,
        bind_v6_best_effort: false,
        helper_addr: cli.helper,
        access_token: cli.token.clone(),
        responder_id: cli.responder_id.clone(),
    };

    let signaling_cfg = cli.ws.map(|ws_addr| SignalingServerConfig {
        bind_ws: ws_addr,
        bind_ws_v6: cli.ws_v6,
        bind_ws_v6_best_effort: false,
        access_token: cli.token.clone(),
        start_delay_ms: cli.ws_start_delay_ms,
        punch_duration_ms: cli.ws_punch_duration_ms,
        ..Default::default()
    });

    (Some(echo_cfg), signaling_cfg)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = CliArgs::parse();

    let (echo_cfg, signaling_cfg) = match &args.command {
        Some(ServerCommand::Start(start_args)) => match resolve_start_configs(&args, start_args) {
            Ok(cfgs) => cfgs,
            Err(err) => {
                eprintln!("ERROR [0] [s2] invalid start configuration: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => resolve_legacy_configs(&args),
    };

    if let Err(err) = log_sink::init_log(Some(&args.log_file), args.truncate_log, true) {
        eprintln!(
            "ERROR [0] [s2] failed to initialize log file {}: {err}",
            args.log_file.display()
        );
        return ExitCode::FAILURE;
    }

    log_sink::info(
        "s2",
        format!(
            "starting neteval-server mode={} p1={:?} p2={:?} v6={:?} helper={:?} ws={:?} ws_v6={:?} ws_start_delay_ms={} ws_punch_duration_ms={} token_configured={} responder_id={:?} log_file={}",
            if args.command.is_some() { "start" } else { "legacy" },
            echo_cfg.as_ref().map(|c| c.bind_p1),
            echo_cfg.as_ref().and_then(|c| c.bind_p2),
            echo_cfg.as_ref().and_then(|c| c.bind_v6),
            args.helper,
            signaling_cfg.as_ref().map(|c| c.bind_ws),
            signaling_cfg.as_ref().and_then(|c| c.bind_ws_v6),
            args.ws_start_delay_ms,
            args.ws_punch_duration_ms,
            args.token.is_some(),
            args.responder_id,
            args.log_file.display()
        ),
    );

    let cancel = CancellationToken::new();
    let sig_cancel = cancel.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                log_sink::info("s2", "received ctrl_c signal, shutting down");
                sig_cancel.cancel();
            }
            Err(err) => {
                log_sink::error("s2", format!("failed to listen for ctrl_c signal: {err}"));
            }
        }
    });

    let run_res = match (echo_cfg, signaling_cfg) {
        (Some(s2_config), Some(s1_config)) => {
            let s2_cancel = cancel.clone();
            let s1_cancel = cancel.clone();
            let err_cancel_s2 = cancel.clone();
            let err_cancel_s1 = cancel.clone();

            let s2_fut = async move {
                let res = run_echo_server(s2_config, s2_cancel).await;
                if res.is_err() {
                    err_cancel_s2.cancel();
                }
                res
            };
            let s1_fut = async move {
                let res = run_signaling_server(s1_config, s1_cancel).await;
                if res.is_err() {
                    err_cancel_s1.cancel();
                }
                res
            };

            let (s2_res, s1_res) = tokio::join!(s2_fut, s1_fut);
            s2_res.and(s1_res)
        }
        (Some(s2_config), None) => run_echo_server(s2_config, cancel).await,
        (None, Some(s1_config)) => run_signaling_server(s1_config, cancel).await,
        (None, None) => Ok(()),
    };

    match run_res {
        Ok(()) => {
            log_sink::info("s2", "neteval-server stopped cleanly");
            ExitCode::SUCCESS
        }
        Err(err) => {
            log_sink::error("s2", format!("neteval-server exited with error: {err}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_resolve(
        args: &[&str],
    ) -> Result<(Option<EchoServerConfig>, Option<SignalingServerConfig>), String> {
        let cli = CliArgs::try_parse_from(args).map_err(|e| e.to_string())?;
        match &cli.command {
            Some(ServerCommand::Start(start)) => resolve_start_configs(&cli, start),
            None => Ok(resolve_legacy_configs(&cli)),
        }
    }

    #[test]
    fn start_defaults_enable_dual_stack_and_default_ports() {
        let (echo_opt, sig_opt) = parse_and_resolve(&["neteval-server", "start"]).unwrap();
        let echo = echo_opt.expect("probe server should be enabled by default");
        let sig = sig_opt.expect("signaling server should be enabled");

        assert_eq!(echo.bind_p1, "0.0.0.0:34778".parse::<SocketAddr>().unwrap());
        assert_eq!(
            echo.bind_p2,
            Some("0.0.0.0:34779".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(
            echo.bind_v6,
            Some("[::]:34778".parse::<SocketAddr>().unwrap())
        );
        assert!(echo.bind_v6_best_effort);

        assert_eq!(sig.bind_ws, "0.0.0.0:34780".parse::<SocketAddr>().unwrap());
        assert_eq!(
            sig.bind_ws_v6,
            Some("[::]:34780".parse::<SocketAddr>().unwrap())
        );
        assert!(sig.bind_ws_v6_best_effort);
    }

    #[test]
    fn start_custom_ws_probe_and_no_ipv6() {
        let (echo_opt, sig_opt) = parse_and_resolve(&[
            "neteval-server",
            "start",
            "ws:34800",
            "--probe",
            "35000,35001",
            "--no-ipv6",
            "--token",
            "secret",
        ])
        .unwrap();

        let echo = echo_opt.unwrap();
        let sig = sig_opt.unwrap();

        assert_eq!(echo.bind_p1, "0.0.0.0:35000".parse::<SocketAddr>().unwrap());
        assert_eq!(
            echo.bind_p2,
            Some("0.0.0.0:35001".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(echo.bind_v6, None);
        assert_eq!(echo.access_token.as_deref(), Some("secret"));

        assert_eq!(sig.bind_ws, "0.0.0.0:34800".parse::<SocketAddr>().unwrap());
        assert_eq!(sig.bind_ws_v6, None);
        assert_eq!(sig.access_token.as_deref(), Some("secret"));
    }

    #[test]
    fn start_no_probe_disables_echo_server() {
        let (echo_opt, sig_opt) =
            parse_and_resolve(&["neteval-server", "--token", "tok", "start", "--no-probe"])
                .unwrap();
        assert!(echo_opt.is_none());
        let sig = sig_opt.unwrap();
        assert_eq!(sig.bind_ws, "0.0.0.0:34780".parse::<SocketAddr>().unwrap());
        assert_eq!(
            sig.bind_ws_v6,
            Some("[::]:34780".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(sig.access_token.as_deref(), Some("tok"));
    }

    #[test]
    fn start_rejects_unsupported_protocol_with_clear_error() {
        let err = parse_and_resolve(&["neteval-server", "start", "wss:34781"]).unwrap_err();
        assert!(
            err.contains("wss") && err.contains("ws"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn start_single_probe_port_and_validation_errors() {
        let (echo_opt, _) =
            parse_and_resolve(&["neteval-server", "start", "WS:34810", "--probe", "36000"])
                .unwrap();
        let echo = echo_opt.unwrap();
        assert_eq!(echo.bind_p1, "0.0.0.0:36000".parse::<SocketAddr>().unwrap());
        assert_eq!(
            echo.bind_p2,
            Some("0.0.0.0:36001".parse::<SocketAddr>().unwrap())
        );

        assert!(parse_and_resolve(&["neteval-server", "start", "--probe", "65535"]).is_err());
        assert!(parse_and_resolve(&["neteval-server", "start", "--probe", "0"]).is_err());
        assert!(parse_and_resolve(&["neteval-server", "start", "--probe", "34778,34778"]).is_err());
        assert!(
            parse_and_resolve(&["neteval-server", "start", "ws:34780", "ws:34781"]).is_err()
        );
    }

    #[test]
    fn legacy_flags_without_subcommand_remain_compatible() {
        let (echo_opt, sig_opt) = parse_and_resolve(&[
            "neteval-server",
            "--p1",
            "127.0.0.1:34780",
            "--p2",
            "127.0.0.1:34781",
            "--ws",
            "127.0.0.1:34782",
        ])
        .unwrap();

        let echo = echo_opt.unwrap();
        let sig = sig_opt.unwrap();

        assert_eq!(
            echo.bind_p1,
            "127.0.0.1:34780".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            echo.bind_p2,
            Some("127.0.0.1:34781".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(echo.bind_v6, None);
        assert!(!echo.bind_v6_best_effort);

        assert_eq!(
            sig.bind_ws,
            "127.0.0.1:34782".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(sig.bind_ws_v6, None);
        assert!(!sig.bind_ws_v6_best_effort);
    }
}
