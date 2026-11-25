use crate::log_sink;
use protocol::{
    AddrEchoReq, AddrEchoResp, ChangeIpHint, EchoMode, S2WireMessage, MAX_S2_ALT_REPLY_BYTES,
    MAX_UDP_PACKET_BYTES,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const LOG_MODULE: &str = "s2";
const RECV_BUFFER_BYTES: usize = 512;
const SHUTDOWN_WAIT_MS: u64 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoServerConfig {
    pub bind_p1: SocketAddr,
    pub bind_p2: Option<SocketAddr>,
    pub bind_v6: Option<SocketAddr>,
    pub helper_addr: Option<SocketAddr>,
    pub access_token: Option<String>,
    pub responder_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoServerBound {
    pub p1_addr: SocketAddr,
    pub p2_addr: Option<SocketAddr>,
    pub v6_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketRole {
    PrimaryP1,
    SecondaryP2,
    V6,
}

impl SocketRole {
    fn label(self) -> &'static str {
        match self {
            Self::PrimaryP1 => "p1",
            Self::SecondaryP2 => "p2",
            Self::V6 => "v6",
        }
    }
}

struct ServerRuntime {
    p1_sock: Arc<UdpSocket>,
    p2_sock: Option<Arc<UdpSocket>>,
    helper_addr: Option<SocketAddr>,
    access_token: Option<String>,
    responder_id: Option<String>,
}

async fn bind_socket(
    role: SocketRole,
    addr: SocketAddr,
) -> std::io::Result<(Arc<UdpSocket>, SocketAddr)> {
    let sock = UdpSocket::bind(addr).await.map_err(|err| {
        log_sink::error(
            LOG_MODULE,
            format!("failed to bind {} socket on {addr}: {err}", role.label()),
        );
        err
    })?;
    let local = sock.local_addr().map_err(|err| {
        log_sink::error(
            LOG_MODULE,
            format!("failed to read local_addr for {} socket: {err}", role.label()),
        );
        err
    })?;
    log_sink::info(
        LOG_MODULE,
        format!("bound {} socket on {local}", role.label()),
    );
    Ok((Arc::new(sock), local))
}

pub async fn start_echo_server(
    config: EchoServerConfig,
    cancel: CancellationToken,
) -> std::io::Result<(EchoServerBound, JoinHandle<()>)> {
    let (p1_sock, p1_addr) = bind_socket(SocketRole::PrimaryP1, config.bind_p1).await?;

    let (p2_sock, p2_addr) = match config.bind_p2 {
        Some(addr) => {
            let (sock, local) = bind_socket(SocketRole::SecondaryP2, addr).await?;
            (Some(sock), Some(local))
        }
        None => (None, None),
    };

    let (v6_sock, v6_addr) = match config.bind_v6 {
        Some(addr) => {
            let (sock, local) = bind_socket(SocketRole::V6, addr).await?;
            (Some(sock), Some(local))
        }
        None => (None, None),
    };

    let bound = EchoServerBound {
        p1_addr,
        p2_addr,
        v6_addr,
    };

    let runtime = Arc::new(ServerRuntime {
        p1_sock: Arc::clone(&p1_sock),
        p2_sock: p2_sock.as_ref().map(Arc::clone),
        helper_addr: config.helper_addr,
        access_token: config.access_token,
        responder_id: config.responder_id,
    });

    log_sink::info(
        LOG_MODULE,
        format!(
            "echo server started p1={} p2={:?} v6={:?} helper={:?} token_configured={}",
            bound.p1_addr,
            bound.p2_addr,
            bound.v6_addr,
            runtime.helper_addr,
            runtime.access_token.is_some()
        ),
    );

    let handle = tokio::spawn(run_coordinator(
        p1_sock,
        p2_sock,
        v6_sock,
        runtime,
        cancel,
    ));

    Ok((bound, handle))
}

pub async fn run_echo_server(
    config: EchoServerConfig,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let (_bound, handle) = start_echo_server(config, cancel).await?;
    handle.await.map_err(|err| {
        log_sink::error(
            LOG_MODULE,
            format!("echo server coordinator task failed: {err}"),
        );
        std::io::Error::other(format!("echo server coordinator task failed: {err}"))
    })
}

async fn run_coordinator(
    p1_sock: Arc<UdpSocket>,
    p2_sock: Option<Arc<UdpSocket>>,
    v6_sock: Option<Arc<UdpSocket>>,
    runtime: Arc<ServerRuntime>,
    cancel: CancellationToken,
) {
    let mut workers = Vec::new();

    workers.push(tokio::spawn(run_socket_loop(
        SocketRole::PrimaryP1,
        p1_sock,
        Arc::clone(&runtime),
        cancel.clone(),
    )));

    if let Some(p2) = p2_sock {
        workers.push(tokio::spawn(run_socket_loop(
            SocketRole::SecondaryP2,
            p2,
            Arc::clone(&runtime),
            cancel.clone(),
        )));
    }

    if let Some(v6) = v6_sock {
        workers.push(tokio::spawn(run_socket_loop(
            SocketRole::V6,
            v6,
            Arc::clone(&runtime),
            cancel.clone(),
        )));
    }

    cancel.cancelled().await;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(SHUTDOWN_WAIT_MS);
    for mut worker in workers {
        tokio::select! {
            biased;
            res = &mut worker => {
                if let Err(err) = res {
                    if !err.is_cancelled() {
                        log_sink::error(LOG_MODULE, format!("socket worker panicked: {err}"));
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                log_sink::warn(
                    LOG_MODULE,
                    format!("socket worker did not stop within {SHUTDOWN_WAIT_MS}ms, aborting"),
                );
                worker.abort();
                let _ = worker.await;
            }
        }
    }

    log_sink::info(LOG_MODULE, "echo server stopped");
}

async fn run_socket_loop(
    role: SocketRole,
    recv_sock: Arc<UdpSocket>,
    runtime: Arc<ServerRuntime>,
    cancel: CancellationToken,
) {
    let mut buf = [0u8; RECV_BUFFER_BYTES];
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                break;
            }
            recv_res = recv_sock.recv_from(&mut buf) => {
                match recv_res {
                    Ok((len, src)) => {
                        handle_datagram(role, &recv_sock, &runtime, &buf[..len], src).await;
                    }
                    Err(err) => {
                        log_sink::warn(
                            LOG_MODULE,
                            format!("{} recv_from error: {err}", role.label()),
                        );
                        if !is_transient_udp_error(&err) {
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => break,
                                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

fn is_transient_udp_error(err: &std::io::Error) -> bool {
    #[cfg(windows)]
    if err.raw_os_error() == Some(10040) {
        return true;
    }
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
    )
}

async fn handle_datagram(
    role: SocketRole,
    recv_sock: &UdpSocket,
    runtime: &ServerRuntime,
    packet: &[u8],
    src: SocketAddr,
) {
    if packet.len() > MAX_UDP_PACKET_BYTES {
        log_sink::warn(
            LOG_MODULE,
            format!(
                "oversized packet on {} from {src}: {} bytes > max {MAX_UDP_PACKET_BYTES}",
                role.label(),
                packet.len()
            ),
        );
        return;
    }

    let text = match std::str::from_utf8(packet) {
        Ok(text) => text,
        Err(err) => {
            log_sink::warn(
                LOG_MODULE,
                format!(
                    "invalid utf-8 packet on {} from {src} ({} bytes): {err}",
                    role.label(),
                    packet.len()
                ),
            );
            return;
        }
    };

    let msg: S2WireMessage = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(err) => {
            log_sink::warn(
                LOG_MODULE,
                format!(
                    "invalid json wire message on {} from {src}: {err}",
                    role.label()
                ),
            );
            return;
        }
    };

    match msg {
        S2WireMessage::AddrEchoReq(req) => {
            handle_addr_echo_req(role, recv_sock, runtime, req, src).await;
        }
        S2WireMessage::ChangeIpHint(hint) => {
            handle_change_ip_hint(role, runtime, hint, src).await;
        }
        S2WireMessage::AddrEchoResp(_) => {
            log_sink::warn(
                LOG_MODULE,
                format!(
                    "rejected inbound addr_echo_resp on {} from {src}",
                    role.label()
                ),
            );
        }
    }
}

async fn handle_addr_echo_req(
    role: SocketRole,
    recv_sock: &UdpSocket,
    runtime: &ServerRuntime,
    req: AddrEchoReq,
    src: SocketAddr,
) {
    if let Some(expected_token) = runtime.access_token.as_deref() {
        if req.token.as_deref() != Some(expected_token) {
            log_sink::warn(
                LOG_MODULE,
                format!("unauthorized addr_echo_req on {} from {src}", role.label()),
            );
            return;
        }
    }

    match req.mode {
        EchoMode::Normal => {
            let Some(payload) = encode_bounded_resp(
                req.nonce,
                src,
                runtime.responder_id.as_deref(),
                MAX_UDP_PACKET_BYTES,
            ) else {
                return;
            };
            send_udp_packet(recv_sock, &payload, src, role.label(), "addr_echo_resp").await;
        }
        EchoMode::ReplyFromAltPort => {
            let (alt_sock, alt_label) = match role {
                SocketRole::PrimaryP1 => match runtime.p2_sock.as_deref() {
                    Some(p2) => (p2, SocketRole::SecondaryP2.label()),
                    None => {
                        log_sink::warn(
                            LOG_MODULE,
                            format!(
                                "dropped reply_from_alt_port from {src} on p1: p2 socket not configured"
                            ),
                        );
                        return;
                    }
                },
                SocketRole::SecondaryP2 => {
                    (runtime.p1_sock.as_ref(), SocketRole::PrimaryP1.label())
                }
                SocketRole::V6 => {
                    log_sink::warn(
                        LOG_MODULE,
                        format!(
                            "dropped reply_from_alt_port from {src} on v6: alt port reply not available on v6"
                        ),
                    );
                    return;
                }
            };

            let Some(payload) = encode_bounded_resp(
                req.nonce,
                src,
                runtime.responder_id.as_deref(),
                MAX_S2_ALT_REPLY_BYTES,
            ) else {
                return;
            };
            send_udp_packet(
                alt_sock,
                &payload,
                src,
                alt_label,
                "addr_echo_resp(alt_port)",
            )
            .await;
        }
        EchoMode::ReplyFromHelperIp => {
            let Some(helper_addr) = runtime.helper_addr else {
                log_sink::warn(
                    LOG_MODULE,
                    format!(
                        "dropped reply_from_helper_ip from {src} on {}: helper_addr not configured",
                        role.label()
                    ),
                );
                return;
            };

            if runtime.access_token.is_none() {
                log_sink::warn(
                    LOG_MODULE,
                    format!(
                        "forwarding change_ip_hint for {src} to helper {helper_addr} without access_token configured"
                    ),
                );
            }

            let hint = S2WireMessage::ChangeIpHint(ChangeIpHint {
                token: runtime.access_token.clone(),
                nonce: req.nonce,
                target_client: src.to_string(),
            });

            let payload = match serde_json::to_vec(&hint) {
                Ok(bytes) => bytes,
                Err(err) => {
                    log_sink::error(
                        LOG_MODULE,
                        format!("failed to serialize change_ip_hint for {src}: {err}"),
                    );
                    return;
                }
            };

            if payload.len() > MAX_UDP_PACKET_BYTES {
                log_sink::warn(
                    LOG_MODULE,
                    format!(
                        "change_ip_hint ({} bytes) exceeds max {MAX_UDP_PACKET_BYTES}; dropping",
                        payload.len()
                    ),
                );
                return;
            }

            log_sink::info(
                LOG_MODULE,
                format!(
                    "forwarding change_ip_hint for client {src} nonce={} to helper {helper_addr}",
                    req.nonce
                ),
            );
            send_udp_packet(
                runtime.p1_sock.as_ref(),
                &payload,
                helper_addr,
                SocketRole::PrimaryP1.label(),
                "change_ip_hint",
            )
            .await;
        }
    }
}

async fn handle_change_ip_hint(
    role: SocketRole,
    runtime: &ServerRuntime,
    hint: ChangeIpHint,
    src: SocketAddr,
) {
    let Some(expected_token) = runtime.access_token.as_deref() else {
        log_sink::warn(
            LOG_MODULE,
            format!(
                "rejected change_ip_hint on {} from {src}: access_token must be configured",
                role.label()
            ),
        );
        return;
    };

    if hint.token.as_deref() != Some(expected_token) {
        log_sink::warn(
            LOG_MODULE,
            format!("unauthorized change_ip_hint on {} from {src}", role.label()),
        );
        return;
    }

    let target: SocketAddr = match hint.target_client.parse() {
        Ok(addr) => addr,
        Err(err) => {
            log_sink::warn(
                LOG_MODULE,
                format!(
                    "invalid target_client '{}' in change_ip_hint from {src}: {err}",
                    hint.target_client
                ),
            );
            return;
        }
    };

    if !is_valid_target_addr(target) {
        log_sink::warn(
            LOG_MODULE,
            format!("rejected disallowed target_client {target} in change_ip_hint from {src}"),
        );
        return;
    }

    let Some(payload) = encode_bounded_resp(
        hint.nonce,
        target,
        runtime.responder_id.as_deref(),
        MAX_S2_ALT_REPLY_BYTES,
    ) else {
        return;
    };

    log_sink::info(
        LOG_MODULE,
        format!(
            "sending helper addr_echo_resp nonce={} to target {target} (via hint from {src})",
            hint.nonce
        ),
    );
    send_udp_packet(
        runtime.p1_sock.as_ref(),
        &payload,
        target,
        SocketRole::PrimaryP1.label(),
        "addr_echo_resp(helper_ip)",
    )
    .await;
}

fn is_valid_v4(v4: Ipv4Addr) -> bool {
    !v4.is_unspecified() && !v4.is_multicast() && !v4.is_broadcast()
}

fn is_valid_target_addr(addr: SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(v4) => is_valid_v4(v4),
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            if let Some(mapped_v4) = v6.to_ipv4_mapped() {
                return is_valid_v4(mapped_v4);
            }
            true
        }
    }
}

fn encode_bounded_resp(
    nonce: u64,
    observed_addr: SocketAddr,
    responder_id: Option<&str>,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    let observed = observed_addr.to_string();
    let msg_with_id = S2WireMessage::AddrEchoResp(AddrEchoResp {
        nonce,
        observed_addr: observed.clone(),
        responder_id: responder_id.map(str::to_string),
    });

    match serde_json::to_vec(&msg_with_id) {
        Ok(bytes) if bytes.len() <= max_bytes => return Some(bytes),
        Ok(bytes) => {
            if responder_id.is_some() {
                log_sink::warn(
                    LOG_MODULE,
                    format!(
                        "addr_echo_resp ({} bytes) exceeds limit {max_bytes}; omitting responder_id",
                        bytes.len()
                    ),
                );
            } else {
                log_sink::warn(
                    LOG_MODULE,
                    format!(
                        "addr_echo_resp ({} bytes) exceeds limit {max_bytes}; dropping",
                        bytes.len()
                    ),
                );
                return None;
            }
        }
        Err(err) => {
            log_sink::error(LOG_MODULE, format!("failed to serialize addr_echo_resp: {err}"));
            return None;
        }
    }

    let msg_without_id = S2WireMessage::AddrEchoResp(AddrEchoResp {
        nonce,
        observed_addr: observed,
        responder_id: None,
    });

    match serde_json::to_vec(&msg_without_id) {
        Ok(bytes) if bytes.len() <= max_bytes => Some(bytes),
        Ok(bytes) => {
            log_sink::warn(
                LOG_MODULE,
                format!(
                    "addr_echo_resp without responder_id ({} bytes) exceeds limit {max_bytes}; dropping",
                    bytes.len()
                ),
            );
            None
        }
        Err(err) => {
            log_sink::error(
                LOG_MODULE,
                format!("failed to serialize addr_echo_resp without responder_id: {err}"),
            );
            None
        }
    }
}

async fn send_udp_packet(
    sock: &UdpSocket,
    payload: &[u8],
    dest: SocketAddr,
    sock_label: &str,
    msg_kind: &str,
) {
    match sock.send_to(payload, dest).await {
        Ok(sent) if sent != payload.len() => {
            log_sink::warn(
                LOG_MODULE,
                format!(
                    "partial {msg_kind} send on {sock_label} to {dest}: sent {sent}/{} bytes",
                    payload.len()
                ),
            );
        }
        Ok(_) => {}
        Err(err) => {
            log_sink::warn(
                LOG_MODULE,
                format!("failed to send {msg_kind} on {sock_label} to {dest}: {err}"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(bind_p1: SocketAddr) -> EchoServerConfig {
        EchoServerConfig {
            bind_p1,
            bind_p2: None,
            bind_v6: None,
            helper_addr: None,
            access_token: None,
            responder_id: None,
        }
    }

    async fn recv_msg_timeout(
        sock: &UdpSocket,
        timeout_ms: u64,
    ) -> Option<(S2WireMessage, SocketAddr, usize)> {
        let mut buf = [0u8; 512];
        let res = tokio::time::timeout(Duration::from_millis(timeout_ms), sock.recv_from(&mut buf))
            .await
            .ok()?
            .ok()?;
        let (len, src) = res;
        let msg = serde_json::from_slice::<S2WireMessage>(&buf[..len]).ok()?;
        Some((msg, src, len))
    }

    #[tokio::test]
    async fn normal_echo_roundtrip_and_clean_shutdown() {
        let cancel = CancellationToken::new();
        let mut cfg = test_config("127.0.0.1:0".parse().unwrap());
        cfg.responder_id = Some("s2-primary".to_string());

        let (bound, handle) = start_echo_server(cfg, cancel.clone()).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();

        let req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: 42,
            token: None,
            mode: EchoMode::Normal,
        });
        client
            .send_to(&serde_json::to_vec(&req).unwrap(), bound.p1_addr)
            .await
            .unwrap();

        let (msg, from, _len) = recv_msg_timeout(&client, 500).await.unwrap();
        assert_eq!(from, bound.p1_addr);
        match msg {
            S2WireMessage::AddrEchoResp(resp) => {
                assert_eq!(resp.nonce, 42);
                assert_eq!(resp.observed_addr, client_addr.to_string());
                assert_eq!(resp.responder_id.as_deref(), Some("s2-primary"));
            }
            other => panic!("unexpected message: {other:?}"),
        }

        let start = tokio::time::Instant::now();
        cancel.cancel();
        handle.await.unwrap();
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn invalid_oversized_and_inbound_resp_packets_are_dropped() {
        let cancel = CancellationToken::new();
        let (bound, handle) = start_echo_server(test_config("127.0.0.1:0".parse().unwrap()), cancel.clone())
            .await
            .unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let oversized = vec![b'a'; MAX_UDP_PACKET_BYTES + 1];
        client.send_to(&oversized, bound.p1_addr).await.unwrap();
        assert!(recv_msg_timeout(&client, 80).await.is_none());

        client
            .send_to(&[0xff, 0xfe, 0xfd], bound.p1_addr)
            .await
            .unwrap();
        assert!(recv_msg_timeout(&client, 80).await.is_none());

        client
            .send_to(br#"{"type":"unknown"}"#, bound.p1_addr)
            .await
            .unwrap();
        assert!(recv_msg_timeout(&client, 80).await.is_none());

        let inbound_resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
            nonce: 1,
            observed_addr: "127.0.0.1:9999".to_string(),
            responder_id: None,
        });
        client
            .send_to(&serde_json::to_vec(&inbound_resp).unwrap(), bound.p1_addr)
            .await
            .unwrap();
        assert!(recv_msg_timeout(&client, 80).await.is_none());

        let valid_req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: 77,
            token: None,
            mode: EchoMode::Normal,
        });
        client
            .send_to(&serde_json::to_vec(&valid_req).unwrap(), bound.p1_addr)
            .await
            .unwrap();
        assert!(recv_msg_timeout(&client, 500).await.is_some());

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn token_validation_and_alt_port_with_size_cap() {
        let cancel = CancellationToken::new();
        let mut cfg = test_config("127.0.0.1:0".parse().unwrap());
        cfg.bind_p2 = Some("127.0.0.1:0".parse().unwrap());
        cfg.access_token = Some("secret-token".to_string());
        cfg.responder_id = Some("x".repeat(80));

        let (bound, handle) = start_echo_server(cfg, cancel.clone()).await.unwrap();
        let p2_addr = bound.p2_addr.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let bad_req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: 10,
            token: Some("wrong".to_string()),
            mode: EchoMode::ReplyFromAltPort,
        });
        client
            .send_to(&serde_json::to_vec(&bad_req).unwrap(), bound.p1_addr)
            .await
            .unwrap();
        assert!(recv_msg_timeout(&client, 80).await.is_none());

        let good_p1_req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: 11,
            token: Some("secret-token".to_string()),
            mode: EchoMode::ReplyFromAltPort,
        });
        client
            .send_to(&serde_json::to_vec(&good_p1_req).unwrap(), bound.p1_addr)
            .await
            .unwrap();

        let (msg, from, raw_len) = recv_msg_timeout(&client, 500).await.unwrap();
        assert_eq!(from, p2_addr);
        assert!(raw_len <= MAX_S2_ALT_REPLY_BYTES);
        match msg {
            S2WireMessage::AddrEchoResp(resp) => {
                assert_eq!(resp.nonce, 11);
                assert!(resp.responder_id.is_none());
            }
            other => panic!("unexpected message: {other:?}"),
        }

        let good_p2_req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: 12,
            token: Some("secret-token".to_string()),
            mode: EchoMode::ReplyFromAltPort,
        });
        client
            .send_to(&serde_json::to_vec(&good_p2_req).unwrap(), p2_addr)
            .await
            .unwrap();

        let (msg2, from2, raw_len2) = recv_msg_timeout(&client, 500).await.unwrap();
        assert_eq!(from2, bound.p1_addr);
        assert!(raw_len2 <= MAX_S2_ALT_REPLY_BYTES);
        match msg2 {
            S2WireMessage::AddrEchoResp(resp) => assert_eq!(resp.nonce, 12),
            other => panic!("unexpected message: {other:?}"),
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn reply_from_helper_ip_and_change_ip_hint_guards() {
        let cancel = CancellationToken::new();

        let mut helper_cfg = test_config("127.0.0.1:0".parse().unwrap());
        helper_cfg.access_token = Some("shared-tok".to_string());
        helper_cfg.responder_id = Some("s2-helper".to_string());
        let (helper_bound, helper_handle) =
            start_echo_server(helper_cfg, cancel.clone()).await.unwrap();

        let mut primary_cfg = test_config("127.0.0.1:0".parse().unwrap());
        primary_cfg.helper_addr = Some(helper_bound.p1_addr);
        primary_cfg.access_token = Some("shared-tok".to_string());
        let (primary_bound, primary_handle) =
            start_echo_server(primary_cfg, cancel.clone()).await.unwrap();

        let mut open_cfg = test_config("127.0.0.1:0".parse().unwrap());
        open_cfg.access_token = None;
        let (open_bound, open_handle) =
            start_echo_server(open_cfg, cancel.clone()).await.unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();

        let req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: 500,
            token: Some("shared-tok".to_string()),
            mode: EchoMode::ReplyFromHelperIp,
        });
        client
            .send_to(&serde_json::to_vec(&req).unwrap(), primary_bound.p1_addr)
            .await
            .unwrap();

        let (msg, from, len) = recv_msg_timeout(&client, 500).await.unwrap();
        assert_eq!(from, helper_bound.p1_addr);
        assert!(len <= MAX_S2_ALT_REPLY_BYTES);
        match msg {
            S2WireMessage::AddrEchoResp(resp) => {
                assert_eq!(resp.nonce, 500);
                assert_eq!(resp.observed_addr, client_addr.to_string());
                assert_eq!(resp.responder_id.as_deref(), Some("s2-helper"));
            }
            other => panic!("unexpected message: {other:?}"),
        }

        let open_hint = S2WireMessage::ChangeIpHint(ChangeIpHint {
            token: None,
            nonce: 501,
            target_client: client_addr.to_string(),
        });
        client
            .send_to(&serde_json::to_vec(&open_hint).unwrap(), open_bound.p1_addr)
            .await
            .unwrap();
        assert!(recv_msg_timeout(&client, 80).await.is_none());

        for bad_target in ["0.0.0.0:1234", "224.0.0.1:1234", "255.255.255.255:1234", "bad-ip"] {
            let bad_hint = S2WireMessage::ChangeIpHint(ChangeIpHint {
                token: Some("shared-tok".to_string()),
                nonce: 502,
                target_client: bad_target.to_string(),
            });
            client
                .send_to(&serde_json::to_vec(&bad_hint).unwrap(), helper_bound.p1_addr)
                .await
                .unwrap();
        }
        assert!(recv_msg_timeout(&client, 80).await.is_none());

        cancel.cancel();
        helper_handle.await.unwrap();
        primary_handle.await.unwrap();
        open_handle.await.unwrap();
    }

    #[tokio::test]
    async fn v6_socket_normal_echo_and_rejects_alt_port() {
        let Ok(probe) = UdpSocket::bind("[::1]:0").await else {
            return;
        };
        drop(probe);

        let cancel = CancellationToken::new();
        let mut cfg = test_config("127.0.0.1:0".parse().unwrap());
        cfg.bind_p2 = Some("127.0.0.1:0".parse().unwrap());
        cfg.bind_v6 = Some("[::1]:0".parse().unwrap());

        let (bound, handle) = start_echo_server(cfg, cancel.clone()).await.unwrap();
        let v6_addr = bound.v6_addr.unwrap();
        let client_v6 = UdpSocket::bind("[::1]:0").await.unwrap();
        let client_v6_addr = client_v6.local_addr().unwrap();

        let alt_req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: 90,
            token: None,
            mode: EchoMode::ReplyFromAltPort,
        });
        client_v6
            .send_to(&serde_json::to_vec(&alt_req).unwrap(), v6_addr)
            .await
            .unwrap();
        assert!(recv_msg_timeout(&client_v6, 80).await.is_none());

        let normal_req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: 91,
            token: None,
            mode: EchoMode::Normal,
        });
        client_v6
            .send_to(&serde_json::to_vec(&normal_req).unwrap(), v6_addr)
            .await
            .unwrap();
        let (msg, from, _) = recv_msg_timeout(&client_v6, 500).await.unwrap();
        assert_eq!(from, v6_addr);
        match msg {
            S2WireMessage::AddrEchoResp(resp) => {
                assert_eq!(resp.nonce, 91);
                assert_eq!(resp.observed_addr, client_v6_addr.to_string());
            }
            other => panic!("unexpected message: {other:?}"),
        }

        cancel.cancel();
        handle.await.unwrap();
    }
}
