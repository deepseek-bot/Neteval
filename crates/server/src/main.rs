use clap::Parser;
use protocol::{DEFAULT_PUNCH_START_DELAY_MS, DEFAULT_PUNCH_TIMEOUT_MS};
use server::log_sink;
use server::nat_echo::{run_echo_server, EchoServerConfig};
use server::signaling::{run_signaling_server, SignalingServerConfig};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(
    name = "neteval-server",
    version,
    about = "NetEval Server (S1 Signaling & S2 NAT Echo)"
)]
struct CliArgs {
    #[arg(long, default_value = "0.0.0.0:34780")]
    p1: SocketAddr,

    #[arg(long)]
    p2: Option<SocketAddr>,

    #[arg(long)]
    v6: Option<SocketAddr>,

    #[arg(long)]
    helper: Option<SocketAddr>,

    #[arg(long)]
    ws: Option<SocketAddr>,

    #[arg(long, default_value_t = DEFAULT_PUNCH_START_DELAY_MS)]
    ws_start_delay_ms: u32,

    #[arg(long, default_value_t = DEFAULT_PUNCH_TIMEOUT_MS)]
    ws_punch_duration_ms: u32,

    #[arg(long)]
    token: Option<String>,

    #[arg(long)]
    responder_id: Option<String>,

    #[arg(long, default_value = "logs/latest.log")]
    log_file: PathBuf,

    #[arg(long, default_value_t = false)]
    truncate_log: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = CliArgs::parse();

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
            "starting neteval-server p1={} p2={:?} v6={:?} helper={:?} ws={:?} ws_start_delay_ms={} ws_punch_duration_ms={} token_configured={} responder_id={:?} log_file={}",
            args.p1,
            args.p2,
            args.v6,
            args.helper,
            args.ws,
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

    let s2_config = EchoServerConfig {
        bind_p1: args.p1,
        bind_p2: args.p2,
        bind_v6: args.v6,
        helper_addr: args.helper,
        access_token: args.token.clone(),
        responder_id: args.responder_id,
    };

    let run_res = if let Some(ws_addr) = args.ws {
        let s1_config = SignalingServerConfig {
            bind_ws: ws_addr,
            access_token: args.token,
            start_delay_ms: args.ws_start_delay_ms,
            punch_duration_ms: args.ws_punch_duration_ms,
            ..Default::default()
        };
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
    } else {
        run_echo_server(s2_config, cancel).await
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
