use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use client_core::logger;
use client_core::orchestrator::Orchestrator;
use protocol::{
    AssessmentReport, AssessmentState, DEFAULT_LINK_ATTEMPTS, DEFAULT_LINK_CONCURRENCY,
    DEFAULT_LINK_TIMEOUT_MS, DEFAULT_PACKET_RATE_HZ, DEFAULT_PUBLIC_STUN_SERVERS,
    DEFAULT_PUNCH_TIMEOUT_MS, DEFAULT_WINDOW_SECS, IpFamily, LinkProtocol, LinkTarget,
    LinkTaskInput, NatFiltering, NatMapping, NatStatus, NatTaskInput, P2pLatencyTier,
    P2pStabilityGrade, P2pTaskInput, P2pVerdict, ProbeError, PunchStrategy, ROOM_WAIT_TTL_SECS,
};

#[derive(Debug, Parser)]
#[command(name = "neteval-cli", version, about = "NetEval NAT & P2P Diagnostic CLI")]
struct Cli {
    #[arg(long, default_value = "logs/latest.log", global = true)]
    log_file: PathBuf,

    #[arg(long, default_value_t = false, global = true)]
    truncate_log: bool,

    #[arg(long, default_value_os_t = default_data_dir(), global = true)]
    data_dir: PathBuf,

    #[arg(long, default_value_t = false, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run local NAT profiling using public STUN and/or self-hosted S2 server
    Nat(NatArgs),
    /// Run dual-peer P2P hole-punching and latency curve probe via S1 signaling server
    P2p(P2pArgs),
    /// Run direct TCP / TLS+SNI link probe against explicit IP targets (bypasses system DNS)
    Link(LinkArgs),
    /// Manage saved local assessment reports
    History(HistoryArgs),
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliPunchStrategy {
    V6First,
    Ipv4Only,
    Ipv6Only,
}

impl From<CliPunchStrategy> for PunchStrategy {
    fn from(value: CliPunchStrategy) -> Self {
        match value {
            CliPunchStrategy::V6First => PunchStrategy::StandardUdpV6First,
            CliPunchStrategy::Ipv4Only => PunchStrategy::Ipv4Only,
            CliPunchStrategy::Ipv6Only => PunchStrategy::Ipv6Only,
        }
    }
}

#[derive(Debug, Args)]
struct P2pArgs {
    /// Signaling server WebSocket URL (e.g. ws://127.0.0.1:34780)
    #[arg(long)]
    ws: String,

    /// Room code (4..=16 ASCII alphanumeric characters)
    #[arg(long)]
    room: String,

    /// Peer display label shared with partner
    #[arg(long, default_value = "peer")]
    label: String,

    /// Optional pre-shared access token for S1/S2
    #[arg(long)]
    token: Option<String>,

    /// Optional self-hosted S2 IPv4 UDP echo endpoint (host:port)
    #[arg(long)]
    echo_v4: Option<String>,

    /// Optional self-hosted S2 IPv6 UDP echo endpoint (host:port)
    #[arg(long)]
    echo_v6: Option<String>,

    /// Public RFC 5389 STUN servers (repeatable). Defaults to built-in STUN nodes unless --no-default-stun is set.
    #[arg(long = "stun")]
    stun_servers: Vec<String>,

    /// Disable built-in public STUN server list
    #[arg(long, default_value_t = false)]
    no_default_stun: bool,

    /// Candidate discovery timeout in milliseconds
    #[arg(long, default_value_t = 2500)]
    candidate_timeout_ms: u64,

    /// Room pairing timeout in seconds (1..=120)
    #[arg(long, default_value_t = ROOM_WAIT_TTL_SECS)]
    pair_timeout_secs: u64,

    /// Simultaneous hole-punching timeout in milliseconds (3000..=15000)
    #[arg(long, default_value_t = DEFAULT_PUNCH_TIMEOUT_MS)]
    punch_timeout_ms: u32,

    /// Fixed latency sampling window duration in seconds (5..=60)
    #[arg(long, default_value_t = DEFAULT_WINDOW_SECS)]
    window_secs: u32,

    /// Packet sampling rate in Hz (1..=20)
    #[arg(long, default_value_t = DEFAULT_PACKET_RATE_HZ)]
    rate_hz: u32,

    /// P2P hole-punching IP stack strategy
    #[arg(long, value_enum, default_value_t = CliPunchStrategy::V6First)]
    strategy: CliPunchStrategy,
}

#[derive(Debug, Args)]
struct NatArgs {
    /// Public RFC 5389 STUN servers (repeatable). Defaults to built-in STUN nodes unless --no-default-stun is set.
    #[arg(long = "stun")]
    stun_servers: Vec<String>,

    /// Disable built-in public STUN server list
    #[arg(long, default_value_t = false)]
    no_default_stun: bool,

    /// Self-hosted S2 primary IPv4 port 1 (host:port)
    #[arg(long)]
    server_p1: Option<String>,

    /// Self-hosted S2 primary IPv4 port 2 (host:port)
    #[arg(long)]
    server_p2: Option<String>,

    /// Self-hosted S2 IPv6 endpoint (host:port)
    #[arg(long)]
    server_v6: Option<String>,

    /// Optional helper S2 IPv4 endpoint (host:port) for NAT1 vs NAT2 distinction
    #[arg(long)]
    helper_p1: Option<String>,

    /// Optional pre-shared token for self-hosted S2
    #[arg(long)]
    token: Option<String>,

    /// Per-step probe timeout in milliseconds
    #[arg(long, default_value_t = 2500)]
    timeout_ms: u64,
}

#[derive(Debug, Args)]
struct LinkArgs {
    /// Target endpoint spec: [LABEL=]IP:PORT[@SNI] (repeatable, e.g. CF=1.1.1.1:443@cloudflare.com)
    #[arg(long = "target", required = true)]
    targets: Vec<String>,

    /// Number of connection attempts per target (1..=20)
    #[arg(long, default_value_t = DEFAULT_LINK_ATTEMPTS)]
    attempts: u32,

    /// Per-attempt connect/TLS timeout in milliseconds (500..=15000)
    #[arg(long, default_value_t = DEFAULT_LINK_TIMEOUT_MS)]
    timeout_ms: u64,

    /// Maximum concurrent target probes (1..=5)
    #[arg(long, default_value_t = DEFAULT_LINK_CONCURRENCY)]
    concurrency: u32,
}

#[derive(Debug, Args)]
struct HistoryArgs {
    #[command(subcommand)]
    action: HistoryAction,
}

#[derive(Debug, Subcommand)]
enum HistoryAction {
    /// List saved reports (newest first)
    List,
    /// Show full report details by ID
    Show { id: String },
    /// Delete a saved report by ID
    Delete { id: String },
}

fn default_data_dir() -> PathBuf {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        if !local_app_data.is_empty() {
            return PathBuf::from(local_app_data).join("NetEval").join("reports");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".neteval").join("reports");
        }
    }
    PathBuf::from(".neteval-data").join("reports")
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(err) = logger::init_logger(&cli.log_file, cli.truncate_log, false) {
        eprintln!(
            "ERROR [0] [cli] failed to initialize log file {}: {err}",
            cli.log_file.display()
        );
    }
    logger::info(
        "cli",
        format!(
            "starting neteval-cli data_dir={} json={}",
            cli.data_dir.display(),
            cli.json
        ),
    );

    let orchestrator = Orchestrator::new(&cli.data_dir);

    match cli.command {
        Commands::Nat(args) => run_nat_command(orchestrator, args, cli.json).await,
        Commands::P2p(args) => run_p2p_command(orchestrator, args, cli.json).await,
        Commands::Link(args) => run_link_command(orchestrator, args, cli.json).await,
        Commands::History(args) => run_history_command(orchestrator, args, cli.json),
    }
}

fn resolve_stun_list(stun_servers: Vec<String>, no_default_stun: bool) -> Vec<String> {
    if !stun_servers.is_empty() {
        stun_servers
    } else if no_default_stun {
        Vec::new()
    } else {
        DEFAULT_PUBLIC_STUN_SERVERS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }
}

async fn run_nat_command(orchestrator: Orchestrator, args: NatArgs, json: bool) -> ExitCode {
    let public_stun_servers = resolve_stun_list(args.stun_servers, args.no_default_stun);

    let input = NatTaskInput {
        public_stun_servers,
        primary_v4_p1: args.server_p1,
        primary_v4_p2: args.server_p2,
        primary_v6: args.server_v6,
        helper_v4_p1: args.helper_p1,
        token: args.token,
        timeout_ms: args.timeout_ms,
        require_vps: false,
    };

    let mut rx = orchestrator.subscribe();
    if let Err(err) = orchestrator.start_nat_assessment(input).await {
        logger::error("cli", format!("failed to start NAT assessment: {err}"));
        print_error(&err, json);
        return ExitCode::FAILURE;
    }

    observe_assessment(&orchestrator, &mut rx, json).await
}

async fn run_p2p_command(orchestrator: Orchestrator, args: P2pArgs, json: bool) -> ExitCode {
    let public_stun_servers = resolve_stun_list(args.stun_servers, args.no_default_stun);

    let input = P2pTaskInput {
        signaling_ws_url: args.ws,
        room_code: args.room,
        token: args.token,
        peer_label: args.label,
        echo_udp_v4: args.echo_v4,
        echo_udp_v6: args.echo_v6,
        public_stun_servers,
        candidate_timeout_ms: args.candidate_timeout_ms,
        pair_timeout_secs: args.pair_timeout_secs,
        punch_timeout_ms: args.punch_timeout_ms,
        window_secs: args.window_secs,
        packet_rate_hz: args.rate_hz,
        strategy: args.strategy.into(),
    };

    let mut rx = orchestrator.subscribe();
    if let Err(err) = orchestrator.start_p2p_assessment(input).await {
        logger::error("cli", format!("failed to start P2P assessment: {err}"));
        print_error(&err, json);
        return ExitCode::FAILURE;
    }

    observe_assessment(&orchestrator, &mut rx, json).await
}

fn parse_link_target_spec(spec: &str) -> Result<LinkTarget, ProbeError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(ProbeError::InvalidInput("empty --target spec".into()));
    }

    let (custom_label, addr_part) = match trimmed.split_once('=') {
        Some((lbl, rest)) => {
            let lbl = lbl.trim();
            if lbl.is_empty() {
                return Err(ProbeError::InvalidInput(format!(
                    "empty label in --target {spec:?}"
                )));
            }
            (Some(lbl.to_string()), rest.trim())
        }
        None => (None, trimmed),
    };

    let (endpoint, protocol) = match addr_part.split_once('@') {
        Some((ep, sni)) => {
            let sni = sni.trim();
            if sni.is_empty() {
                return Err(ProbeError::InvalidInput(format!(
                    "empty SNI after '@' in --target {spec:?}"
                )));
            }
            (
                ep.trim(),
                LinkProtocol::TlsSni {
                    sni: sni.to_string(),
                },
            )
        }
        None => (addr_part, LinkProtocol::Tcp),
    };

    let socket_addr: SocketAddr = endpoint.parse().map_err(|err| {
        ProbeError::InvalidInput(format!(
            "invalid target endpoint {endpoint:?} (must be IPv4:PORT or [IPv6]:PORT): {err}"
        ))
    })?;

    let label = custom_label.unwrap_or_else(|| endpoint.to_string());
    Ok(LinkTarget {
        label,
        host_ip: socket_addr.ip().to_string(),
        port: socket_addr.port(),
        protocol,
    })
}

async fn run_link_command(orchestrator: Orchestrator, args: LinkArgs, json: bool) -> ExitCode {
    let mut parsed_targets = Vec::with_capacity(args.targets.len());
    for raw in &args.targets {
        match parse_link_target_spec(raw) {
            Ok(t) => parsed_targets.push(t),
            Err(err) => {
                logger::error("cli", format!("invalid --target {raw:?}: {err}"));
                print_error(&err, json);
                return ExitCode::FAILURE;
            }
        }
    }

    let input = LinkTaskInput {
        targets: parsed_targets,
        attempts_per_target: args.attempts,
        timeout_ms: args.timeout_ms,
        max_concurrency: args.concurrency,
    };

    let mut rx = orchestrator.subscribe();
    if let Err(err) = orchestrator.start_link_assessment(input).await {
        logger::error("cli", format!("failed to start link assessment: {err}"));
        print_error(&err, json);
        return ExitCode::FAILURE;
    }

    observe_assessment(&orchestrator, &mut rx, json).await
}

async fn observe_assessment(
    orchestrator: &Orchestrator,
    rx: &mut tokio::sync::watch::Receiver<AssessmentState>,
    json: bool,
) -> ExitCode {
    let cancel_orch = orchestrator.clone();
    let ctrl_c_handle = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            logger::info("cli", "received ctrl_c signal, cancelling current task");
            let _ = cancel_orch
                .cancel_current(Some("user_ctrl_c".to_string()))
                .await;
        }
    });

    let mut last_stage = String::new();
    let exit_code = loop {
        let state = rx.borrow().clone();
        match state {
            AssessmentState::Idle => {}
            AssessmentState::Running {
                stage,
                progress_pct,
                live_point,
                ..
            } => {
                if !json {
                    if stage != last_stage {
                        eprintln!("[{progress_pct:>3}%] {stage}...");
                        last_stage = stage;
                    } else if let Some(pt) = live_point {
                        match pt.rtt_ms {
                            Some(rtt) => {
                                eprintln!("  sample #{:<4} t={}ms rtt={rtt:.2}ms", pt.seq, pt.elapsed_ms)
                            }
                            None => eprintln!("  sample #{:<4} t={}ms LOST", pt.seq, pt.elapsed_ms),
                        }
                    }
                }
            }
            AssessmentState::Completed(report) => {
                print_report(&report, json);
                break ExitCode::SUCCESS;
            }
            AssessmentState::Cancelled { reason } => {
                let msg = reason.unwrap_or_else(|| "cancelled".to_string());
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "cancelled",
                            "reason": msg
                        }))
                        .unwrap_or_else(|_| "{\"status\":\"cancelled\"}".into())
                    );
                } else {
                    eprintln!("Assessment cancelled: {msg}");
                }
                break ExitCode::FAILURE;
            }
            AssessmentState::Failed(err) => {
                print_error(&err, json);
                break ExitCode::FAILURE;
            }
        }

        if rx.changed().await.is_err() {
            let err = ProbeError::Internal("orchestrator state channel closed".into());
            print_error(&err, json);
            break ExitCode::FAILURE;
        }
    };

    ctrl_c_handle.abort();
    exit_code
}

fn run_history_command(orchestrator: Orchestrator, args: HistoryArgs, json: bool) -> ExitCode {
    match args.action {
        HistoryAction::List => match orchestrator.list_history() {
            Ok(items) => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".into())
                    );
                } else if items.is_empty() {
                    println!("No saved reports.");
                } else {
                    for item in items {
                        let nat_status = item
                            .nat_status
                            .map(format_nat_status)
                            .unwrap_or("none");
                        let p2p_verdict = item
                            .p2p_verdict
                            .map(format_p2p_verdict)
                            .unwrap_or("none");
                        let link_summary = match (item.link_reachable_count, item.link_target_count)
                        {
                            (Some(ok), Some(total)) => format!("{ok}/{total}"),
                            _ => "none".to_string(),
                        };
                        println!(
                            "{}  created_unix_ms={}  nat_status={}  p2p_verdict={}  link={}",
                            item.report_id,
                            item.created_unix_ms,
                            nat_status,
                            p2p_verdict,
                            link_summary
                        );
                    }
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                print_error(&err, json);
                ExitCode::FAILURE
            }
        },
        HistoryAction::Show { id } => match orchestrator.get_report(&id) {
            Ok(report) => {
                print_report(&report, json);
                ExitCode::SUCCESS
            }
            Err(err) => {
                print_error(&err, json);
                ExitCode::FAILURE
            }
        },
        HistoryAction::Delete { id } => match orchestrator.delete_report(&id) {
            Ok(()) => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "deleted": id })
                    );
                } else {
                    println!("Deleted report {id}");
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                print_error(&err, json);
                ExitCode::FAILURE
            }
        },
    }
}

fn print_report(report: &AssessmentReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into())
        );
        return;
    }

    println!("Report ID     : {}", report.report_id);
    println!("Created (ms)  : {}", report.created_unix_ms);
    if let Some(warning) = &report.storage_warning {
        println!("Storage Warn  : {warning}");
    }
    if let Some(nat) = &report.nat_report {
        println!("Status        : {}", format_nat_status(nat.status));
        println!(
            "Public IPv4   : {}",
            nat.public_ipv4.as_deref().unwrap_or("-")
        );
        println!(
            "Public IPv6   : {}",
            nat.public_ipv6.as_deref().unwrap_or("-")
        );
        println!("IPv6 Reachable: {}", nat.ipv6_reachable);
        println!("Mapping       : {}", format_mapping(nat.mapping));
        println!("Filtering     : {}", format_filtering(nat.filtering));
        println!("Difficulty    : {}", nat.estimated_difficulty);
        if !nat.notes.is_empty() {
            println!("Notes:");
            for note in &nat.notes {
                println!("  - {note}");
            }
        }
    }
    if let Some(p2p) = &report.p2p_report {
        println!("P2P Punched   : {}", p2p.punched);
        println!("Peer Label    : {}", p2p.peer_label.as_deref().unwrap_or("-"));
        println!(
            "Peer Addr     : {}",
            p2p.active_peer_addr.as_deref().unwrap_or("-")
        );
        println!(
            "IP Family     : {}",
            p2p.ip_family.map(format_ip_family).unwrap_or("-")
        );
        println!(
            "Punch Time    : {}",
            p2p.punch_elapsed_ms
                .map(|ms| format!("{ms}ms"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "Packets       : {}/{} (loss={:.1}%)",
            p2p.received,
            p2p.sent,
            p2p.loss_ratio * 100.0
        );
        println!(
            "Avg RTT       : {}",
            p2p.avg_rtt_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "P50 RTT       : {}",
            p2p.p50_rtt_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "P95 RTT       : {}",
            p2p.p95_rtt_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "IQR (P75-P25) : {}",
            p2p.iqr_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "Tail (P95-P50): {}",
            p2p.tail_bloat_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "Jitter        : {}",
            p2p.jitter_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "Spike (>P50+30): {}",
            p2p.spike_ratio
                .map(|v| format!("{:.1}%", v * 100.0))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "Latency Tier  : {}",
            p2p.latency_tier
                .map(format_latency_tier)
                .unwrap_or("-")
        );
        println!(
            "Stability     : {}",
            p2p.stability_grade
                .map(format_stability_grade)
                .unwrap_or("-")
        );
        println!("Verdict       : {}", format_p2p_verdict(p2p.verdict));
        if let Some(reason) = &p2p.failure_reason {
            println!("Failure Reason: {reason}");
        }
        if let Some(reason) = &p2p.interrupted_reason {
            println!("Interrupted   : {reason}");
        }
    }
    if let Some(links) = &report.link_reports {
        println!("Link Targets  : {}", links.len());
        for t in links {
            let proto = match &t.protocol {
                LinkProtocol::Tcp => "tcp".to_string(),
                LinkProtocol::TlsSni { sni } => format!("tls_sni({sni})"),
            };
            let avg = t
                .avg_rtt_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into());
            let min = t
                .min_rtt_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into());
            let max = t
                .max_rtt_ms
                .map(|v| format!("{v:.2}ms"))
                .unwrap_or_else(|| "-".into());
            println!(
                "  - {} ({}:{}, {}) reachable={} ok={}/{} loss={:.1}% avg={} min={} max={}",
                t.label,
                t.host_ip,
                t.port,
                proto,
                t.reachable,
                t.received,
                t.sent,
                t.loss_ratio * 100.0,
                avg,
                min,
                max
            );
            for s in &t.samples {
                if let Some(err) = &s.error {
                    println!("      #{} error={err}", s.seq);
                }
            }
        }
    }
}

fn format_ip_family(f: IpFamily) -> &'static str {
    match f {
        IpFamily::Ipv4 => "ipv4",
        IpFamily::Ipv6 => "ipv6",
    }
}

fn format_latency_tier(t: P2pLatencyTier) -> &'static str {
    match t {
        P2pLatencyTier::UltraLow => "ultra_low",
        P2pLatencyTier::Low => "low",
        P2pLatencyTier::Medium => "medium",
        P2pLatencyTier::High => "high",
    }
}

fn format_stability_grade(g: P2pStabilityGrade) -> &'static str {
    match g {
        P2pStabilityGrade::RockSolid => "rock_solid",
        P2pStabilityGrade::Good => "good",
        P2pStabilityGrade::MildJitter => "mild_jitter",
        P2pStabilityGrade::SevereJitter => "severe_jitter",
        P2pStabilityGrade::Failed => "failed",
    }
}

fn format_p2p_verdict(v: P2pVerdict) -> &'static str {
    match v {
        P2pVerdict::Excellent => "excellent",
        P2pVerdict::Good => "good",
        P2pVerdict::Unstable => "unstable",
        P2pVerdict::Failed => "failed",
    }
}

fn print_error(err: &ProbeError, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "error": err }))
                .unwrap_or_else(|_| format!("{{\"error\":\"{err}\"}}"))
        );
    } else {
        eprintln!("Error: {err}");
    }
}

fn format_nat_status(s: NatStatus) -> &'static str {
    match s {
        NatStatus::CompleteFull => "complete_full",
        NatStatus::CompleteSingleVps => "complete_single_vps",
        NatStatus::PublicStunOnly => "public_stun_only",
        NatStatus::Unreachable => "unreachable",
    }
}

fn format_mapping(m: NatMapping) -> &'static str {
    match m {
        NatMapping::EndpointIndependent => "endpoint_independent",
        NatMapping::AddressOrPortDependentSymmetric => "address_or_port_dependent_symmetric",
        NatMapping::Unknown => "unknown",
    }
}

fn format_filtering(f: NatFiltering) -> &'static str {
    match f {
        NatFiltering::FullConeNat1 => "full_cone_nat1",
        NatFiltering::AddressRestrictedNat2 => "address_restricted_nat2",
        NatFiltering::PortRestrictedNat3 => "port_restricted_nat3",
        NatFiltering::ConeNat1OrNat2 => "cone_nat1_or_nat2",
        NatFiltering::NotTestedPublicStun => "not_tested_public_stun",
        NatFiltering::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_link_target_spec_handles_tcp_tls_label_and_ipv6() {
        let tcp = parse_link_target_spec("127.0.0.1:8080").unwrap();
        assert_eq!(tcp.label, "127.0.0.1:8080");
        assert_eq!(tcp.host_ip, "127.0.0.1");
        assert_eq!(tcp.port, 8080);
        assert_eq!(tcp.protocol, LinkProtocol::Tcp);

        let tls = parse_link_target_spec("CF-1.1.1.1=1.1.1.1:443@cloudflare.com").unwrap();
        assert_eq!(tls.label, "CF-1.1.1.1");
        assert_eq!(tls.host_ip, "1.1.1.1");
        assert_eq!(tls.port, 443);
        assert_eq!(
            tls.protocol,
            LinkProtocol::TlsSni {
                sni: "cloudflare.com".into()
            }
        );

        let v6 = parse_link_target_spec("CF-v6=[2606:4700:4700::1111]:443@cloudflare.com").unwrap();
        assert_eq!(v6.label, "CF-v6");
        assert_eq!(v6.host_ip, "2606:4700:4700::1111");
        assert_eq!(v6.port, 443);

        assert!(parse_link_target_spec("cloudflare.com:443").is_err());
        assert!(parse_link_target_spec("=1.1.1.1:443").is_err());
        assert!(parse_link_target_spec("1.1.1.1:443@").is_err());
    }
}
