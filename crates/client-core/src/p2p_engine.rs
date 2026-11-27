use crate::logger;
use protocol::{
    AddrEchoReq, Candidate, CandidateKind, EchoMode, IpFamily, LatencyPoint, P2pDirectMsg,
    P2pLatencyTier, P2pMsgKind, P2pReport, P2pStabilityGrade, P2pVerdict, ProbeError, PunchStrategy,
    S2WireMessage, MAX_PACKET_RATE_HZ, MAX_PUNCH_TIMEOUT_MS, MAX_UDP_PACKET_BYTES,
    MAX_WINDOW_SECS, MIN_PACKET_RATE_HZ, MIN_PUNCH_TIMEOUT_MS, MIN_WINDOW_SECS,
};
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MODULE_TAG: &str = "c3";
const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const STUN_MSG_BINDING_REQUEST: u16 = 0x0001;
const STUN_MSG_BINDING_RESPONSE: u16 = 0x0101;
const STUN_ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const STUN_ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const STUN_HEADER_LEN: usize = 20;
const PUNCH_BURST_INTERVAL_MS: u64 = 100;
const V4_DELAY_AFTER_V6_MS: u64 = 150;
const RECV_BUF_LEN: usize = 2048;

static SEQ_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct SessionHandle {
    v4_sock: Option<Arc<UdpSocket>>,
    v6_sock: Option<Arc<UdpSocket>>,
}

impl SessionHandle {
    pub fn local_addrs(&self) -> (Option<SocketAddr>, Option<SocketAddr>) {
        (
            self.v4_sock.as_ref().and_then(|s| s.local_addr().ok()),
            self.v6_sock.as_ref().and_then(|s| s.local_addr().ok()),
        )
    }
}

#[derive(Debug, Clone)]
pub enum P2pEvent {
    PunchSucceeded {
        peer_addr: SocketAddr,
        ip_family: IpFamily,
        elapsed_ms: u32,
    },
    LatencyPoint(LatencyPoint),
}

fn next_unique_u64() -> u64 {
    let seq = SEQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    let mut mix = nanos ^ pid.rotate_left(19) ^ seq.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    mix = (mix ^ (mix >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mix = (mix ^ (mix >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mix ^= mix >> 31;
    // Keep strictly injective low 40 bits from atomic sequence + entropy in high 24 bits.
    (seq & 0x0000_00ff_ffff_ffff) | (mix & 0xffff_ff00_0000_0000)
}

fn next_stun_tx_id() -> [u8; 12] {
    let seq = SEQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0);
    let prefix = nanos ^ std::process::id();
    let mut tx_id = [0u8; 12];
    tx_id[..4].copy_from_slice(&prefix.to_be_bytes());
    tx_id[4..12].copy_from_slice(&seq.to_be_bytes());
    tx_id
}

fn build_stun_binding_request(tx_id: [u8; 12]) -> [u8; STUN_HEADER_LEN] {
    let mut pkt = [0u8; STUN_HEADER_LEN];
    pkt[0..2].copy_from_slice(&STUN_MSG_BINDING_REQUEST.to_be_bytes());
    pkt[2..4].copy_from_slice(&0u16.to_be_bytes());
    pkt[4..8].copy_from_slice(&STUN_MAGIC_COOKIE);
    pkt[8..20].copy_from_slice(&tx_id);
    pkt
}

fn parse_stun_binding_response(
    buf: &[u8],
    expected_tx_id: &[u8; 12],
) -> Option<SocketAddr> {
    if buf.len() < STUN_HEADER_LEN {
        return None;
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    if msg_type != STUN_MSG_BINDING_RESPONSE {
        return None;
    }
    if buf[4..8] != STUN_MAGIC_COOKIE {
        return None;
    }
    if &buf[8..20] != expected_tx_id {
        return None;
    }
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = STUN_HEADER_LEN.checked_add(msg_len)?;
    if end > buf.len() {
        return None;
    }

    let mut xor_mapped: Option<SocketAddr> = None;
    let mut mapped: Option<SocketAddr> = None;
    let mut offset = STUN_HEADER_LEN;

    while offset + 4 <= end {
        let attr_type = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let attr_len = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        let val_start = offset + 4;
        let val_end = match val_start.checked_add(attr_len) {
            Some(v) if v <= end => v,
            _ => break,
        };
        let val = &buf[val_start..val_end];

        match attr_type {
            STUN_ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(addr) = parse_stun_addr_attr(val, Some(expected_tx_id)) {
                    xor_mapped = Some(addr);
                }
            }
            STUN_ATTR_MAPPED_ADDRESS => {
                if let Some(addr) = parse_stun_addr_attr(val, None) {
                    mapped = Some(addr);
                }
            }
            _ => {}
        }

        let padded = (attr_len + 3) & !3;
        offset = match val_start.checked_add(padded) {
            Some(next) => next,
            None => break,
        };
    }

    xor_mapped.or(mapped)
}

fn parse_stun_addr_attr(val: &[u8], xor_tx_id: Option<&[u8; 12]>) -> Option<SocketAddr> {
    if val.len() < 4 {
        return None;
    }
    let family = val[1];
    let raw_port = u16::from_be_bytes([val[2], val[3]]);
    let port = match xor_tx_id {
        Some(_) => raw_port ^ u16::from_be_bytes([STUN_MAGIC_COOKIE[0], STUN_MAGIC_COOKIE[1]]),
        None => raw_port,
    };
    if port == 0 {
        return None;
    }

    match family {
        0x01 if val.len() >= 8 => {
            let mut octets = [val[4], val[5], val[6], val[7]];
            if xor_tx_id.is_some() {
                for (b, c) in octets.iter_mut().zip(STUN_MAGIC_COOKIE.iter()) {
                    *b ^= *c;
                }
            }
            let ip = Ipv4Addr::from(octets);
            if ip.is_unspecified() {
                return None;
            }
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 if val.len() >= 20 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&val[4..20]);
            if let Some(tx_id) = xor_tx_id {
                let mut xor_key = [0u8; 16];
                xor_key[..4].copy_from_slice(&STUN_MAGIC_COOKIE);
                xor_key[4..16].copy_from_slice(tx_id);
                for (b, k) in octets.iter_mut().zip(xor_key.iter()) {
                    *b ^= *k;
                }
            }
            let ip = Ipv6Addr::from(octets);
            if ip.is_unspecified() || is_ipv4_mapped_v6(ip) {
                return None;
            }
            Some(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => None,
    }
}

fn is_ipv4_mapped_v6(ip: Ipv6Addr) -> bool {
    ip.segments()[..6] == [0, 0, 0, 0, 0, 0xffff]
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    let is_link_local = (seg[0] & 0xffc0) == 0xfe80;
    let is_site_local = (seg[0] & 0xffc0) == 0xfec0;
    let is_ula = (seg[0] & 0xfe00) == 0xfc00;
    let is_global_unicast = (seg[0] & 0xe000) == 0x2000;
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && !is_link_local
        && !is_site_local
        && !is_ula
        && !is_ipv4_mapped_v6(ip)
        && is_global_unicast
}

fn is_usable_local_v4(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_loopback() && !ip.is_link_local() && !ip.is_multicast()
}

fn discover_local_v4_ip() -> Option<Ipv4Addr> {
    for probe_target in ["223.5.5.5:80", "8.8.8.8:80"] {
        let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if sock.connect(probe_target).is_err() {
            continue;
        }
        if let Ok(SocketAddr::V4(local)) = sock.local_addr() {
            let ip = *local.ip();
            if is_usable_local_v4(ip) {
                return Some(ip);
            }
        }
    }
    None
}

fn discover_local_v6_ip() -> Option<Ipv6Addr> {
    for probe_target in ["[2400:3200::1]:80", "[2606:4700:4700::1111]:80"] {
        let Ok(sock) = std::net::UdpSocket::bind("[::]:0") else {
            continue;
        };
        if sock.connect(probe_target).is_err() {
            continue;
        }
        if let Ok(SocketAddr::V6(local)) = sock.local_addr() {
            let ip = *local.ip();
            if is_global_v6(ip) {
                return Some(ip);
            }
        }
    }
    None
}

fn encode_s2_echo_req(nonce: u64, token: Option<&str>) -> Result<Vec<u8>, ProbeError> {
    let msg = S2WireMessage::AddrEchoReq(AddrEchoReq {
        nonce,
        token: token.map(str::to_string),
        mode: EchoMode::Normal,
    });
    let bytes = serde_json::to_vec(&msg)
        .map_err(|e| ProbeError::Internal(format!("failed to serialize AddrEchoReq: {e}")))?;
    if bytes.len() > MAX_UDP_PACKET_BYTES {
        logger::warn(
            MODULE_TAG,
            format!(
                "AddrEchoReq exceeds MAX_UDP_PACKET_BYTES ({} > {})",
                bytes.len(),
                MAX_UDP_PACKET_BYTES
            ),
        );
        return Err(ProbeError::InvalidInput(format!(
            "AddrEchoReq payload {} exceeds {} bytes",
            bytes.len(),
            MAX_UDP_PACKET_BYTES
        )));
    }
    Ok(bytes)
}

fn encode_p2p_msg(msg: &P2pDirectMsg) -> Result<Vec<u8>, ProbeError> {
    let bytes = serde_json::to_vec(msg)
        .map_err(|e| ProbeError::Internal(format!("failed to serialize P2pDirectMsg: {e}")))?;
    if bytes.len() > MAX_UDP_PACKET_BYTES {
        logger::warn(
            MODULE_TAG,
            format!(
                "P2pDirectMsg exceeds MAX_UDP_PACKET_BYTES ({} > {})",
                bytes.len(),
                MAX_UDP_PACKET_BYTES
            ),
        );
        return Err(ProbeError::InvalidInput(format!(
            "P2pDirectMsg payload {} exceeds {} bytes",
            bytes.len(),
            MAX_UDP_PACKET_BYTES
        )));
    }
    Ok(bytes)
}

fn drain_socket(sock: &UdpSocket, label: &str) {
    let mut buf = [0u8; RECV_BUF_LEN];
    let mut drained = 0usize;
    for _ in 0..256 {
        match sock.try_recv_from(&mut buf) {
            Ok((_n, _from)) => {
                drained += 1;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                logger::warn(MODULE_TAG, format!("drain_socket({label}) error: {e}"));
                break;
            }
        }
    }
    if drained > 0 {
        logger::info(
            MODULE_TAG,
            format!("drained {drained} stale packets from {label}"),
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum ProbeTargetRole {
    EchoV4,
    EchoV6,
    PublicStun,
}

#[derive(Debug, Clone)]
enum ProbeKind {
    Echo { nonce: u64, payload: Vec<u8> },
    Stun { tx_id: [u8; 12], payload: [u8; STUN_HEADER_LEN] },
}

#[derive(Debug, Clone)]
struct PendingProbe {
    target: SocketAddr,
    kind: ProbeKind,
}

struct AbortOnDrop<T>(Vec<JoinHandle<T>>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

async fn resolve_target_str(target: &str) -> Result<Vec<SocketAddr>, String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("empty target address".to_string());
    }
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Ok(vec![addr]);
    }
    let addrs = tokio::net::lookup_host(trimmed)
        .await
        .map_err(|e| format!("lookup_host({trimmed}) failed: {e}"))?;
    Ok(addrs.collect())
}

async fn recv_from_opt(
    sock: Option<&UdpSocket>,
    buf: &mut [u8],
) -> std::io::Result<(usize, SocketAddr)> {
    match sock {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

fn candidate_priority(kind: CandidateKind) -> u8 {
    match kind {
        CandidateKind::HostGlobalV6 => 0,
        CandidateKind::ServerReflexiveV6 => 1,
        CandidateKind::ServerReflexiveV4 => 2,
        CandidateKind::HostLocal => 3,
    }
}

fn finalize_candidates(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by_key(|c| candidate_priority(c.kind));
    let mut seen_addrs = HashSet::new();
    let mut out = Vec::with_capacity(candidates.len());
    for c in candidates {
        if seen_addrs.insert(c.addr.clone()) {
            out.push(c);
        }
    }
    out
}

fn validate_observed_addr(raw: &str, expected_family: IpFamily) -> Option<SocketAddr> {
    let parsed: SocketAddr = raw.parse().ok()?;
    if parsed.port() == 0 || parsed.ip().is_unspecified() {
        return None;
    }
    match (expected_family, parsed) {
        (IpFamily::Ipv4, SocketAddr::V4(_)) => Some(parsed),
        (IpFamily::Ipv6, SocketAddr::V6(v6)) if !is_ipv4_mapped_v6(*v6.ip()) => Some(parsed),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_prepare_recv(
    family: IpFamily,
    data: &[u8],
    from: SocketAddr,
    sent_echo_nonces: &HashSet<u64>,
    sent_stun_txs: &HashSet<[u8; 12]>,
    pending_echo: &mut HashMap<u64, PendingProbe>,
    pending_stun: &mut HashMap<[u8; 12], PendingProbe>,
    reflexive_candidates: &mut Vec<Candidate>,
) {
    if data.first().copied() == Some(b'{') {
        match serde_json::from_slice::<S2WireMessage>(data) {
            Ok(S2WireMessage::AddrEchoResp(resp)) => {
                if !sent_echo_nonces.contains(&resp.nonce) {
                    logger::warn(
                        MODULE_TAG,
                        format!("unexpected AddrEchoResp nonce={} from {from}", resp.nonce),
                    );
                    return;
                }
                pending_echo.remove(&resp.nonce);
                match validate_observed_addr(&resp.observed_addr, family) {
                    Some(addr) => {
                        let kind = match family {
                            IpFamily::Ipv4 => CandidateKind::ServerReflexiveV4,
                            IpFamily::Ipv6 => CandidateKind::ServerReflexiveV6,
                        };
                        reflexive_candidates.push(Candidate {
                            kind,
                            addr: addr.to_string(),
                        });
                        logger::info(
                            MODULE_TAG,
                            format!(
                                "observed reflexive addr {addr} ({family:?}) from echo {from}"
                            ),
                        );
                    }
                    None => {
                        logger::warn(
                            MODULE_TAG,
                            format!(
                                "invalid observed_addr {:?} for {family:?} from {from}",
                                resp.observed_addr
                            ),
                        );
                    }
                }
            }
            Ok(other) => {
                logger::warn(
                    MODULE_TAG,
                    format!("unexpected S2 wire message from {from}: {other:?}"),
                );
            }
            Err(e) => {
                logger::warn(
                    MODULE_TAG,
                    format!("invalid JSON packet from {from} during prepare_session: {e}"),
                );
            }
        }
        return;
    }

    if data.len() >= STUN_HEADER_LEN {
        let mut tx_id = [0u8; 12];
        tx_id.copy_from_slice(&data[8..20]);
        if sent_stun_txs.contains(&tx_id) {
            if let Some(addr) = parse_stun_binding_response(data, &tx_id) {
                let valid = match (family, addr) {
                    (IpFamily::Ipv4, SocketAddr::V4(_)) => true,
                    (IpFamily::Ipv6, SocketAddr::V6(v6)) => !is_ipv4_mapped_v6(*v6.ip()),
                    _ => false,
                };
                if valid {
                    pending_stun.remove(&tx_id);
                    let kind = match family {
                        IpFamily::Ipv4 => CandidateKind::ServerReflexiveV4,
                        IpFamily::Ipv6 => CandidateKind::ServerReflexiveV6,
                    };
                    reflexive_candidates.push(Candidate {
                        kind,
                        addr: addr.to_string(),
                    });
                    logger::info(
                        MODULE_TAG,
                        format!("observed reflexive addr {addr} ({family:?}) from STUN {from}"),
                    );
                } else {
                    logger::warn(
                        MODULE_TAG,
                        format!(
                            "mismatched family STUN response addr {addr} for {family:?} from {from}"
                        ),
                    );
                }
                return;
            }
        }
    }

    logger::warn(
        MODULE_TAG,
        format!("unrecognized UDP packet ({} bytes) from {from} during prepare_session", data.len()),
    );
}

pub async fn prepare_session(
    echo_udp_v4: Option<&str>,
    echo_udp_v6: Option<&str>,
    public_stun_servers: &[String],
    token: Option<&str>,
    timeout_ms: u64,
    cancel: CancellationToken,
) -> Result<(SessionHandle, Vec<Candidate>), ProbeError> {
    prepare_session_with_strategy(
        echo_udp_v4,
        echo_udp_v6,
        public_stun_servers,
        token,
        timeout_ms,
        PunchStrategy::StandardUdpV6First,
        cancel,
    )
    .await
}

pub async fn prepare_session_with_strategy(
    echo_udp_v4: Option<&str>,
    echo_udp_v6: Option<&str>,
    public_stun_servers: &[String],
    token: Option<&str>,
    timeout_ms: u64,
    strategy: PunchStrategy,
    cancel: CancellationToken,
) -> Result<(SessionHandle, Vec<Candidate>), ProbeError> {
    if cancel.is_cancelled() {
        return Err(ProbeError::Cancelled);
    }

    // Pre-validate AddrEchoReq size against MAX_UDP_PACKET_BYTES when token is supplied.
    let _ = encode_s2_echo_req(0, token)?;

    let allow_v4 = !matches!(strategy, PunchStrategy::Ipv6Only);
    let allow_v6 = !matches!(strategy, PunchStrategy::Ipv4Only);

    let v4_sock = if allow_v4 {
        match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                logger::warn(
                    MODULE_TAG,
                    format!("failed to bind IPv4 UDP socket 0.0.0.0:0, continuing without IPv4: {e}"),
                );
                None
            }
        }
    } else {
        None
    };

    let v6_sock = if allow_v6 {
        match UdpSocket::bind("[::]:0").await {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                logger::warn(
                    MODULE_TAG,
                    format!("failed to bind IPv6 UDP socket [::]:0, continuing without IPv6: {e}"),
                );
                None
            }
        }
    } else {
        None
    };

    if strategy == PunchStrategy::Ipv4Only && v4_sock.is_none() {
        logger::error(
            MODULE_TAG,
            "IPv4-only strategy requested but IPv4 UDP socket failed to bind in prepare_session",
        );
        return Err(ProbeError::Internal(
            "IPv4-only strategy requested but IPv4 UDP socket failed to bind".into(),
        ));
    }
    if strategy == PunchStrategy::Ipv6Only && v6_sock.is_none() {
        logger::error(
            MODULE_TAG,
            "IPv6-only strategy requested but IPv6 UDP socket failed to bind in prepare_session",
        );
        return Err(ProbeError::Internal(
            "IPv6-only strategy requested but IPv6 UDP socket failed to bind".into(),
        ));
    }
    if v4_sock.is_none() && v6_sock.is_none() {
        logger::error(
            MODULE_TAG,
            "failed to bind both IPv4 and IPv6 UDP sockets in prepare_session",
        );
        return Err(ProbeError::Internal(
            "failed to bind IPv4 and IPv6 UDP sockets".to_string(),
        ));
    }

    let local_v4_addr = v4_sock.as_ref().and_then(|s| s.local_addr().ok());
    let local_v6_addr = v6_sock.as_ref().and_then(|s| s.local_addr().ok());
    logger::info(
        MODULE_TAG,
        format!("prepare_session bound sockets v4={local_v4_addr:?} v6={local_v6_addr:?}"),
    );

    let mut host_candidates = Vec::new();
    if let Some(local_v4) = local_v4_addr {
        match discover_local_v4_ip() {
            Some(ip) => {
                let addr = SocketAddr::new(IpAddr::V4(ip), local_v4.port());
                host_candidates.push(Candidate {
                    kind: CandidateKind::HostLocal,
                    addr: addr.to_string(),
                });
            }
            None => {
                logger::warn(
                    MODULE_TAG,
                    "local IPv4 routing interface discovery via UDP connect fallback returned none",
                );
            }
        }
    }
    if let Some(local_v6) = local_v6_addr {
        match discover_local_v6_ip() {
            Some(ip) => {
                let addr = SocketAddr::new(IpAddr::V6(ip), local_v6.port());
                host_candidates.push(Candidate {
                    kind: CandidateKind::HostGlobalV6,
                    addr: addr.to_string(),
                });
            }
            None => {
                logger::warn(
                    MODULE_TAG,
                    "local global IPv6 routing interface discovery via UDP connect fallback returned none",
                );
            }
        }
    }

    let filtered_echo_v4 = echo_udp_v4.filter(|_| allow_v4);
    let filtered_echo_v6 = echo_udp_v6.filter(|_| allow_v6);
    let had_external_targets =
        filtered_echo_v4.is_some() || filtered_echo_v6.is_some() || !public_stun_servers.is_empty();

    if !had_external_targets {
        let final_candidates = finalize_candidates(host_candidates);
        logger::info(
            MODULE_TAG,
            format!(
                "prepare_session completed with {} host-only candidates",
                final_candidates.len()
            ),
        );
        return Ok((SessionHandle { v4_sock, v6_sock }, final_candidates));
    }

    let mut raw_targets: Vec<(ProbeTargetRole, String)> = Vec::new();
    if let Some(v4) = filtered_echo_v4 {
        raw_targets.push((ProbeTargetRole::EchoV4, v4.to_string()));
    }
    if let Some(v6) = filtered_echo_v6 {
        raw_targets.push((ProbeTargetRole::EchoV6, v6.to_string()));
    }
    for stun in public_stun_servers {
        raw_targets.push((ProbeTargetRole::PublicStun, stun.clone()));
    }

    let (dns_tx, mut dns_rx) = mpsc::channel::<(ProbeTargetRole, String, Result<Vec<SocketAddr>, String>)>(
        raw_targets.len().max(1),
    );
    let mut dns_tasks = AbortOnDrop(Vec::new());
    let mut outstanding_dns = 0usize;

    let mut resolved_initial: Vec<(ProbeTargetRole, String, Vec<SocketAddr>)> = Vec::new();
    for (role, raw) in raw_targets {
        let trimmed = raw.trim();
        if let Ok(addr) = trimmed.parse::<SocketAddr>() {
            resolved_initial.push((role, raw, vec![addr]));
        } else {
            outstanding_dns += 1;
            let tx = dns_tx.clone();
            dns_tasks.0.push(tokio::spawn(async move {
                let res = resolve_target_str(&raw).await;
                let _ = tx.send((role, raw, res)).await;
            }));
        }
    }
    drop(dns_tx);

    let mut v4_sent_echo_nonces: HashSet<u64> = HashSet::new();
    let mut v6_sent_echo_nonces: HashSet<u64> = HashSet::new();
    let mut v4_sent_stun_txs: HashSet<[u8; 12]> = HashSet::new();
    let mut v6_sent_stun_txs: HashSet<[u8; 12]> = HashSet::new();

    let mut v4_pending_echo: HashMap<u64, PendingProbe> = HashMap::new();
    let mut v6_pending_echo: HashMap<u64, PendingProbe> = HashMap::new();
    let mut v4_pending_stun: HashMap<[u8; 12], PendingProbe> = HashMap::new();
    let mut v6_pending_stun: HashMap<[u8; 12], PendingProbe> = HashMap::new();

    let token_owned = token.map(str::to_string);

    let dispatch_resolved = |role: ProbeTargetRole,
                             raw: &str,
                             addrs: Vec<SocketAddr>,
                             v4_sock: Option<&Arc<UdpSocket>>,
                             v6_sock: Option<&Arc<UdpSocket>>| {
        let mut to_send: Vec<(IpFamily, PendingProbe)> = Vec::new();
        match role {
            ProbeTargetRole::EchoV4 => {
                if let Some(addr) = addrs.into_iter().find(|a| a.is_ipv4()) {
                    if v4_sock.is_some() {
                        let nonce = next_unique_u64();
                        if let Ok(payload) = encode_s2_echo_req(nonce, token_owned.as_deref()) {
                            to_send.push((
                                IpFamily::Ipv4,
                                PendingProbe {
                                    target: addr,
                                    kind: ProbeKind::Echo { nonce, payload },
                                },
                            ));
                        }
                    } else {
                        logger::warn(
                            MODULE_TAG,
                            format!("skipping echo_udp_v4 {raw}: IPv4 socket unavailable"),
                        );
                    }
                } else {
                    logger::warn(
                        MODULE_TAG,
                        format!("echo_udp_v4 {raw:?} resolved to no IPv4 address"),
                    );
                }
            }
            ProbeTargetRole::EchoV6 => {
                if let Some(addr) = addrs
                    .into_iter()
                    .find(|a| matches!(a, SocketAddr::V6(v6) if !is_ipv4_mapped_v6(*v6.ip())))
                {
                    if v6_sock.is_some() {
                        let nonce = next_unique_u64();
                        if let Ok(payload) = encode_s2_echo_req(nonce, token_owned.as_deref()) {
                            to_send.push((
                                IpFamily::Ipv6,
                                PendingProbe {
                                    target: addr,
                                    kind: ProbeKind::Echo { nonce, payload },
                                },
                            ));
                        }
                    } else {
                        logger::warn(
                            MODULE_TAG,
                            format!("skipping echo_udp_v6 {raw}: IPv6 socket unavailable"),
                        );
                    }
                } else {
                    logger::warn(
                        MODULE_TAG,
                        format!("echo_udp_v6 {raw:?} resolved to no IPv6 address"),
                    );
                }
            }
            ProbeTargetRole::PublicStun => {
                let first_v4 = addrs.iter().copied().find(|a| a.is_ipv4());
                let first_v6 = addrs
                    .iter()
                    .copied()
                    .find(|a| matches!(a, SocketAddr::V6(v6) if !is_ipv4_mapped_v6(*v6.ip())));
                if let (Some(addr), Some(_)) = (first_v4, v4_sock) {
                    let tx_id = next_stun_tx_id();
                    let payload = build_stun_binding_request(tx_id);
                    to_send.push((
                        IpFamily::Ipv4,
                        PendingProbe {
                            target: addr,
                            kind: ProbeKind::Stun { tx_id, payload },
                        },
                    ));
                }
                if let (Some(addr), Some(_)) = (first_v6, v6_sock) {
                    let tx_id = next_stun_tx_id();
                    let payload = build_stun_binding_request(tx_id);
                    to_send.push((
                        IpFamily::Ipv6,
                        PendingProbe {
                            target: addr,
                            kind: ProbeKind::Stun { tx_id, payload },
                        },
                    ));
                }
            }
        }
        to_send
    };

    let mut initial_sends = Vec::new();
    for (role, raw, addrs) in resolved_initial {
        initial_sends.extend(dispatch_resolved(
            role,
            &raw,
            addrs,
            v4_sock.as_ref(),
            v6_sock.as_ref(),
        ));
    }

    for (family, probe) in initial_sends {
        send_probe_and_record(
            family,
            probe,
            v4_sock.as_deref(),
            v6_sock.as_deref(),
            &mut v4_pending_echo,
            &mut v6_pending_echo,
            &mut v4_pending_stun,
            &mut v6_pending_stun,
            &mut v4_sent_echo_nonces,
            &mut v6_sent_echo_nonces,
            &mut v4_sent_stun_txs,
            &mut v6_sent_stun_txs,
        )
        .await;
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let half_ms = (timeout_ms / 2).clamp(100, 600);
    let mut retry_at = if timeout_ms > 150 {
        Some(Instant::now() + Duration::from_millis(half_ms))
    } else {
        None
    };

    let mut reflexive_candidates = Vec::new();
    let mut v4_buf = [0u8; RECV_BUF_LEN];
    let mut v6_buf = [0u8; RECV_BUF_LEN];

    loop {
        let all_probes_done = v4_pending_echo.is_empty()
            && v6_pending_echo.is_empty()
            && v4_pending_stun.is_empty()
            && v6_pending_stun.is_empty();
        if outstanding_dns == 0 && all_probes_done {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                drop(dns_tasks);
                drop(v4_sock);
                drop(v6_sock);
                logger::info(MODULE_TAG, "prepare_session cancelled while probing candidates");
                return Err(ProbeError::Cancelled);
            }
            dns_msg = dns_rx.recv(), if outstanding_dns > 0 => {
                match dns_msg {
                    Some((role, raw, Ok(addrs))) => {
                        outstanding_dns = outstanding_dns.saturating_sub(1);
                        let sends = dispatch_resolved(
                            role,
                            &raw,
                            addrs,
                            v4_sock.as_ref(),
                            v6_sock.as_ref(),
                        );
                        for (family, probe) in sends {
                            send_probe_and_record(
                                family,
                                probe,
                                v4_sock.as_deref(),
                                v6_sock.as_deref(),
                                &mut v4_pending_echo,
                                &mut v6_pending_echo,
                                &mut v4_pending_stun,
                                &mut v6_pending_stun,
                                &mut v4_sent_echo_nonces,
                                &mut v6_sent_echo_nonces,
                                &mut v4_sent_stun_txs,
                                &mut v6_sent_stun_txs,
                            ).await;
                        }
                    }
                    Some((_role, raw, Err(err))) => {
                        outstanding_dns = outstanding_dns.saturating_sub(1);
                        logger::warn(MODULE_TAG, format!("DNS resolution failed for {raw:?}: {err}"));
                    }
                    None => {
                        outstanding_dns = 0;
                    }
                }
            }
            res_v6 = recv_from_opt(v6_sock.as_deref(), &mut v6_buf) => {
                match res_v6 {
                    Ok((n, from)) => {
                        handle_prepare_recv(
                            IpFamily::Ipv6,
                            &v6_buf[..n],
                            from,
                            &v6_sent_echo_nonces,
                            &v6_sent_stun_txs,
                            &mut v6_pending_echo,
                            &mut v6_pending_stun,
                            &mut reflexive_candidates,
                        );
                    }
                    Err(e) => {
                        logger::warn(MODULE_TAG, format!("v6_sock recv_from error in prepare_session: {e}"));
                    }
                }
            }
            res_v4 = recv_from_opt(v4_sock.as_deref(), &mut v4_buf) => {
                match res_v4 {
                    Ok((n, from)) => {
                        handle_prepare_recv(
                            IpFamily::Ipv4,
                            &v4_buf[..n],
                            from,
                            &v4_sent_echo_nonces,
                            &v4_sent_stun_txs,
                            &mut v4_pending_echo,
                            &mut v4_pending_stun,
                            &mut reflexive_candidates,
                        );
                    }
                    Err(e) => {
                        logger::warn(MODULE_TAG, format!("v4_sock recv_from error in prepare_session: {e}"));
                    }
                }
            }
            _ = sleep_until_opt(retry_at) => {
                retry_at = None;
                resend_pending(v4_sock.as_deref(), &v4_pending_echo, &v4_pending_stun).await;
                resend_pending(v6_sock.as_deref(), &v6_pending_echo, &v6_pending_stun).await;
            }
            _ = tokio::time::sleep_until(deadline) => {
                break;
            }
        }
    }

    drop(dns_tasks);

    if let Some(s) = v4_sock.as_deref() {
        drain_socket(s, "prepare_v4");
    }
    if let Some(s) = v6_sock.as_deref() {
        drain_socket(s, "prepare_v6");
    }

    if reflexive_candidates.is_empty() {
        logger::warn(
            MODULE_TAG,
            "prepare_session failed: external echo/STUN targets provided but zero reflexive candidates succeeded",
        );
        drop(v4_sock);
        drop(v6_sock);
        return Err(ProbeError::ServerUnreachable(
            "no echo or public STUN server responded".to_string(),
        ));
    }

    let mut all_candidates = host_candidates;
    all_candidates.extend(reflexive_candidates);
    let final_candidates = finalize_candidates(all_candidates);

    logger::info(
        MODULE_TAG,
        format!(
            "prepare_session succeeded with {} candidates",
            final_candidates.len()
        ),
    );
    Ok((SessionHandle { v4_sock, v6_sock }, final_candidates))
}

#[allow(clippy::too_many_arguments)]
async fn send_probe_and_record(
    family: IpFamily,
    probe: PendingProbe,
    v4_sock: Option<&UdpSocket>,
    v6_sock: Option<&UdpSocket>,
    v4_pending_echo: &mut HashMap<u64, PendingProbe>,
    v6_pending_echo: &mut HashMap<u64, PendingProbe>,
    v4_pending_stun: &mut HashMap<[u8; 12], PendingProbe>,
    v6_pending_stun: &mut HashMap<[u8; 12], PendingProbe>,
    v4_sent_echo_nonces: &mut HashSet<u64>,
    v6_sent_echo_nonces: &mut HashSet<u64>,
    v4_sent_stun_txs: &mut HashSet<[u8; 12]>,
    v6_sent_stun_txs: &mut HashSet<[u8; 12]>,
) {
    let sock = match family {
        IpFamily::Ipv4 => v4_sock,
        IpFamily::Ipv6 => v6_sock,
    };
    let Some(sock) = sock else {
        return;
    };

    let send_res = match &probe.kind {
        ProbeKind::Echo { payload, .. } => sock.send_to(payload, probe.target).await,
        ProbeKind::Stun { payload, .. } => sock.send_to(payload, probe.target).await,
    };

    if let Err(e) = send_res {
        logger::warn(
            MODULE_TAG,
            format!("failed to send {family:?} probe to {}: {e}", probe.target),
        );
        return;
    }

    match (&probe.kind, family) {
        (ProbeKind::Echo { nonce, .. }, IpFamily::Ipv4) => {
            v4_sent_echo_nonces.insert(*nonce);
            v4_pending_echo.insert(*nonce, probe);
        }
        (ProbeKind::Echo { nonce, .. }, IpFamily::Ipv6) => {
            v6_sent_echo_nonces.insert(*nonce);
            v6_pending_echo.insert(*nonce, probe);
        }
        (ProbeKind::Stun { tx_id, .. }, IpFamily::Ipv4) => {
            v4_sent_stun_txs.insert(*tx_id);
            v4_pending_stun.insert(*tx_id, probe);
        }
        (ProbeKind::Stun { tx_id, .. }, IpFamily::Ipv6) => {
            v6_sent_stun_txs.insert(*tx_id);
            v6_pending_stun.insert(*tx_id, probe);
        }
    }
}

async fn resend_pending(
    sock: Option<&UdpSocket>,
    pending_echo: &HashMap<u64, PendingProbe>,
    pending_stun: &HashMap<[u8; 12], PendingProbe>,
) {
    let Some(sock) = sock else {
        return;
    };
    for probe in pending_echo.values() {
        if let ProbeKind::Echo { payload, .. } = &probe.kind {
            if let Err(e) = sock.send_to(payload, probe.target).await {
                logger::warn(
                    MODULE_TAG,
                    format!("retry echo send_to {} failed: {e}", probe.target),
                );
            }
        }
    }
    for probe in pending_stun.values() {
        if let ProbeKind::Stun { payload, .. } = &probe.kind {
            if let Err(e) = sock.send_to(payload, probe.target).await {
                logger::warn(
                    MODULE_TAG,
                    format!("retry STUN send_to {} failed: {e}", probe.target),
                );
            }
        }
    }
}

pub async fn keepalive_session(
    handle: &SessionHandle,
    echo_udp_v4: Option<&str>,
    echo_udp_v6: Option<&str>,
    public_stun_servers: &[String],
    token: Option<&str>,
) -> Result<(), ProbeError> {
    if handle.v4_sock.is_none() && handle.v6_sock.is_none() {
        return Err(ProbeError::Internal(
            "session handle holds no active sockets".to_string(),
        ));
    }

    let _ = encode_s2_echo_req(0, token)?;

    let mut attempted = 0usize;
    let mut succeeded = 0usize;

    if let (Some(target), Some(sock)) = (echo_udp_v4, handle.v4_sock.as_deref()) {
        attempted += 1;
        match tokio::time::timeout(Duration::from_millis(500), resolve_target_str(target)).await {
            Ok(Ok(addrs)) => {
                if let Some(addr) = addrs.into_iter().find(|a| a.is_ipv4()) {
                    let payload = encode_s2_echo_req(next_unique_u64(), token)?;
                    if let Err(e) = sock.send_to(&payload, addr).await {
                        logger::warn(
                            MODULE_TAG,
                            format!("keepalive echo_udp_v4 send_to {addr} failed: {e}"),
                        );
                    } else {
                        succeeded += 1;
                    }
                } else {
                    logger::warn(
                        MODULE_TAG,
                        format!("keepalive echo_udp_v4 {target:?} resolved to no IPv4 address"),
                    );
                }
            }
            Ok(Err(e)) => {
                logger::warn(
                    MODULE_TAG,
                    format!("keepalive echo_udp_v4 resolve failed: {e}"),
                );
            }
            Err(_) => {
                logger::warn(
                    MODULE_TAG,
                    format!("keepalive echo_udp_v4 resolve timed out for {target:?}"),
                );
            }
        }
    }

    if let (Some(target), Some(sock)) = (echo_udp_v6, handle.v6_sock.as_deref()) {
        attempted += 1;
        match tokio::time::timeout(Duration::from_millis(500), resolve_target_str(target)).await {
            Ok(Ok(addrs)) => {
                if let Some(addr) = addrs
                    .into_iter()
                    .find(|a| matches!(a, SocketAddr::V6(v6) if !is_ipv4_mapped_v6(*v6.ip())))
                {
                    let payload = encode_s2_echo_req(next_unique_u64(), token)?;
                    if let Err(e) = sock.send_to(&payload, addr).await {
                        logger::warn(
                            MODULE_TAG,
                            format!("keepalive echo_udp_v6 send_to {addr} failed: {e}"),
                        );
                    } else {
                        succeeded += 1;
                    }
                } else {
                    logger::warn(
                        MODULE_TAG,
                        format!("keepalive echo_udp_v6 {target:?} resolved to no IPv6 address"),
                    );
                }
            }
            Ok(Err(e)) => {
                logger::warn(
                    MODULE_TAG,
                    format!("keepalive echo_udp_v6 resolve failed: {e}"),
                );
            }
            Err(_) => {
                logger::warn(
                    MODULE_TAG,
                    format!("keepalive echo_udp_v6 resolve timed out for {target:?}"),
                );
            }
        }
    }

    let mut sent_v4_stun = handle.v4_sock.is_none();
    let mut sent_v6_stun = handle.v6_sock.is_none();

    for stun in public_stun_servers {
        if sent_v4_stun && sent_v6_stun {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(500), resolve_target_str(stun)).await {
            Ok(Ok(addrs)) => {
                if !sent_v4_stun {
                    if let (Some(addr), Some(sock)) = (
                        addrs.iter().copied().find(|a| a.is_ipv4()),
                        handle.v4_sock.as_deref(),
                    ) {
                        attempted += 1;
                        let pkt = build_stun_binding_request(next_stun_tx_id());
                        if let Err(e) = sock.send_to(&pkt, addr).await {
                            logger::warn(
                                MODULE_TAG,
                                format!("keepalive STUN v4 send_to {addr} failed: {e}"),
                            );
                        } else {
                            succeeded += 1;
                            sent_v4_stun = true;
                        }
                    }
                }
                if !sent_v6_stun {
                    if let (Some(addr), Some(sock)) = (
                        addrs.iter().copied().find(
                            |a| matches!(a, SocketAddr::V6(v6) if !is_ipv4_mapped_v6(*v6.ip())),
                        ),
                        handle.v6_sock.as_deref(),
                    ) {
                        attempted += 1;
                        let pkt = build_stun_binding_request(next_stun_tx_id());
                        if let Err(e) = sock.send_to(&pkt, addr).await {
                            logger::warn(
                                MODULE_TAG,
                                format!("keepalive STUN v6 send_to {addr} failed: {e}"),
                            );
                        } else {
                            succeeded += 1;
                            sent_v6_stun = true;
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                logger::warn(
                    MODULE_TAG,
                    format!("keepalive STUN resolve failed for {stun:?}: {e}"),
                );
            }
            Err(_) => {
                logger::warn(
                    MODULE_TAG,
                    format!("keepalive STUN resolve timed out for {stun:?}"),
                );
            }
        }
    }

    tokio::task::yield_now().await;
    if let Some(s) = handle.v4_sock.as_deref() {
        drain_socket(s, "keepalive_v4");
    }
    if let Some(s) = handle.v6_sock.as_deref() {
        drain_socket(s, "keepalive_v6");
    }

    logger::info(
        MODULE_TAG,
        format!("keepalive_session sent {succeeded}/{attempted} refresh packets"),
    );

    if attempted > 0 && succeeded == 0 {
        return Err(ProbeError::ServerUnreachable(
            "keepalive_session failed to send any UDP refresh packet".to_string(),
        ));
    }

    Ok(())
}

pub fn abort_session(handle: SessionHandle) {
    let (v4_addr, v6_addr) = handle.local_addrs();
    drop(handle);
    logger::info(
        MODULE_TAG,
        format!("abort_session dropped sockets (v4={v4_addr:?}, v6={v6_addr:?})"),
    );
}

fn partition_peer_candidates(
    candidates: &[Candidate],
    has_v4: bool,
    has_v6: bool,
) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
    let mut v6_targets = Vec::new();
    let mut v4_targets = Vec::new();
    let mut seen = HashSet::new();

    for c in candidates {
        let Ok(addr) = c.addr.parse::<SocketAddr>() else {
            logger::warn(
                MODULE_TAG,
                format!("ignoring unparseable peer candidate {:?}", c.addr),
            );
            continue;
        };
        if addr.port() == 0 || addr.ip().is_unspecified() {
            logger::warn(
                MODULE_TAG,
                format!("ignoring invalid peer candidate address {addr}"),
            );
            continue;
        }
        if !seen.insert(addr) {
            continue;
        }
        match addr {
            SocketAddr::V6(v6) => {
                if is_ipv4_mapped_v6(*v6.ip()) {
                    logger::warn(
                        MODULE_TAG,
                        format!("ignoring IPv4-mapped IPv6 peer candidate {addr}"),
                    );
                    continue;
                }
                if has_v6 {
                    v6_targets.push(addr);
                }
            }
            SocketAddr::V4(_) => {
                if has_v4 {
                    v4_targets.push(addr);
                }
            }
        }
    }

    (v6_targets, v4_targets)
}

fn is_valid_src_for_family(from: SocketAddr, family: IpFamily) -> bool {
    if from.port() == 0 || from.ip().is_unspecified() {
        return false;
    }
    match (family, from) {
        (IpFamily::Ipv4, SocketAddr::V4(_)) => true,
        (IpFamily::Ipv6, SocketAddr::V6(v6)) => !is_ipv4_mapped_v6(*v6.ip()),
        _ => false,
    }
}

async fn send_punch_burst(
    sock: Option<&UdpSocket>,
    targets: &[SocketAddr],
    session_id: &str,
    seq: u32,
    sent_offset_ms: u32,
) {
    let Some(sock) = sock else {
        return;
    };
    if targets.is_empty() {
        return;
    }
    let msg = P2pDirectMsg {
        session_id: session_id.to_string(),
        kind: P2pMsgKind::Punch,
        seq,
        sent_offset_ms,
    };
    let Ok(bytes) = encode_p2p_msg(&msg) else {
        return;
    };
    for &target in targets {
        if let Err(e) = sock.send_to(&bytes, target).await {
            logger::warn(
                MODULE_TAG,
                format!("punch send_to {target} failed: {e}"),
            );
        }
    }
}

fn elapsed_u32(since: Instant) -> u32 {
    since.elapsed().as_millis().min(u128::from(u32::MAX)) as u32
}

async fn send_p2p_reply(
    sock: &UdpSocket,
    target: SocketAddr,
    session_id: &str,
    kind: P2pMsgKind,
    seq: u32,
    sent_offset_ms: u32,
) {
    let msg = P2pDirectMsg {
        session_id: session_id.to_string(),
        kind,
        seq,
        sent_offset_ms,
    };
    if let Ok(bytes) = encode_p2p_msg(&msg) {
        if let Err(e) = sock.send_to(&bytes, target).await {
            logger::warn(
                MODULE_TAG,
                format!("reply {kind:?} send_to {target} failed: {e}"),
            );
        }
    }
}

fn parse_valid_direct_msg(
    data: &[u8],
    from: SocketAddr,
    family: IpFamily,
    session_id: &str,
) -> Option<P2pDirectMsg> {
    if !is_valid_src_for_family(from, family) {
        logger::warn(
            MODULE_TAG,
            format!("ignoring packet from mismatched address {from} on {family:?} socket"),
        );
        return None;
    }
    let msg = match serde_json::from_slice::<P2pDirectMsg>(data) {
        Ok(m) => m,
        Err(e) => {
            logger::warn(
                MODULE_TAG,
                format!("ignoring malformed P2pDirectMsg from {from}: {e}"),
            );
            return None;
        }
    };
    if msg.session_id != session_id {
        logger::warn(
            MODULE_TAG,
            format!(
                "ignoring P2pDirectMsg with mismatched session_id {:?} from {from}",
                msg.session_id
            ),
        );
        return None;
    }
    Some(msg)
}

#[allow(clippy::too_many_arguments)]
async fn handle_phase_b_packet(
    family: IpFamily,
    sock: &UdpSocket,
    from: SocketAddr,
    msg: P2pDirectMsg,
    session_id: &str,
    punch_start: Instant,
    next_punch_seq: &mut u32,
) -> Option<(SocketAddr, IpFamily, P2pDirectMsg)> {
    let now_offset = elapsed_u32(punch_start);
    match msg.kind {
        P2pMsgKind::Punch => {
            send_p2p_reply(
                sock,
                from,
                session_id,
                P2pMsgKind::PunchAck,
                msg.seq,
                now_offset,
            )
            .await;
            let reply_punch_seq = *next_punch_seq;
            *next_punch_seq = next_punch_seq.saturating_add(1);
            send_p2p_reply(
                sock,
                from,
                session_id,
                P2pMsgKind::Punch,
                reply_punch_seq,
                now_offset,
            )
            .await;
            None
        }
        P2pMsgKind::PunchAck | P2pMsgKind::Ping => Some((from, family, msg)),
        P2pMsgKind::Pong => {
            logger::warn(
                MODULE_TAG,
                format!("ignoring unexpected Pong seq={} during Phase B from {from}", msg.seq),
            );
            None
        }
    }
}

struct SentRecord {
    sent_at: Instant,
    sent_offset_ms: u32,
    rtt_ms: Option<f32>,
}

const SPIKE_OFFSET_MS: f32 = 30.0;

fn percentile_nearest_rank(sorted: &[f32], q: f64) -> f32 {
    let idx = ((q * sorted.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[idx]
}

fn classify_latency_tier(avg_rtt_ms: Option<f32>) -> Option<P2pLatencyTier> {
    let avg = avg_rtt_ms.filter(|v| v.is_finite())?;
    Some(if avg < 30.0 {
        P2pLatencyTier::UltraLow
    } else if avg < 80.0 {
        P2pLatencyTier::Low
    } else if avg <= 150.0 {
        P2pLatencyTier::Medium
    } else {
        P2pLatencyTier::High
    })
}

fn classify_stability_grade(
    punched: bool,
    received: u32,
    loss_ratio: f32,
    iqr_ms: Option<f32>,
    tail_bloat_ms: Option<f32>,
    spike_ratio: Option<f32>,
) -> P2pStabilityGrade {
    if !punched || received == 0 {
        return P2pStabilityGrade::Failed;
    }
    let iqr = iqr_ms.unwrap_or(f32::INFINITY);
    let tail = tail_bloat_ms.unwrap_or(f32::INFINITY);
    let spike = spike_ratio.unwrap_or(f32::INFINITY);

    if loss_ratio == 0.0 && iqr <= 8.0 && tail <= 20.0 && spike <= 0.02 {
        P2pStabilityGrade::RockSolid
    } else if loss_ratio <= 0.01 && iqr <= 15.0 && tail <= 35.0 && spike <= 0.05 {
        P2pStabilityGrade::Good
    } else if loss_ratio <= 0.03 && iqr <= 25.0 && tail <= 60.0 && spike <= 0.12 {
        P2pStabilityGrade::MildJitter
    } else {
        P2pStabilityGrade::SevereJitter
    }
}

fn stability_to_verdict(grade: P2pStabilityGrade) -> P2pVerdict {
    match grade {
        P2pStabilityGrade::RockSolid => P2pVerdict::Excellent,
        P2pStabilityGrade::Good | P2pStabilityGrade::MildJitter => P2pVerdict::Good,
        P2pStabilityGrade::SevereJitter => P2pVerdict::Unstable,
        P2pStabilityGrade::Failed => P2pVerdict::Failed,
    }
}

fn summarize_report(
    active_peer_addr: SocketAddr,
    ip_family: IpFamily,
    punch_elapsed_ms: u32,
    sent: u32,
    series: Vec<LatencyPoint>,
) -> P2pReport {
    let rtts: Vec<f32> = series.iter().filter_map(|p| p.rtt_ms).collect();
    let received = rtts.len() as u32;
    let loss_ratio = if sent == 0 {
        1.0
    } else {
        sent.saturating_sub(received) as f32 / sent as f32
    };

    let (avg_rtt_ms, p50_rtt_ms, p95_rtt_ms, jitter_ms, iqr_ms, tail_bloat_ms, spike_ratio) =
        if rtts.is_empty() {
            (None, None, None, None, None, None, None)
        } else {
            let sum: f64 = rtts.iter().map(|&v| f64::from(v)).sum();
            let avg = (sum / rtts.len() as f64) as f32;

            let mut sorted = rtts.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let p25 = percentile_nearest_rank(&sorted, 0.25);
            let p50 = percentile_nearest_rank(&sorted, 0.50);
            let p75 = percentile_nearest_rank(&sorted, 0.75);
            let p95 = percentile_nearest_rank(&sorted, 0.95);
            let iqr = (p75 - p25).max(0.0);
            let tail_bloat = (p95 - p50).max(0.0);
            let spike_thresh = p50 + SPIKE_OFFSET_MS;
            let spikes = rtts.iter().filter(|&&v| v > spike_thresh).count();
            let spike = spikes as f32 / rtts.len() as f32;

            let jitter = if rtts.len() == 1 {
                0.0
            } else {
                let diff_sum: f64 = rtts
                    .windows(2)
                    .map(|w| f64::from((w[1] - w[0]).abs()))
                    .sum();
                (diff_sum / (rtts.len() - 1) as f64) as f32
            };

            (
                Some(avg),
                Some(p50),
                Some(p95),
                Some(jitter),
                Some(iqr),
                Some(tail_bloat),
                Some(spike),
            )
        };

    let latency_tier = classify_latency_tier(avg_rtt_ms);
    let stability_grade =
        classify_stability_grade(true, received, loss_ratio, iqr_ms, tail_bloat_ms, spike_ratio);
    let verdict = stability_to_verdict(stability_grade);
    let failure_reason = if received == 0 {
        Some("no_pong_received".to_string())
    } else {
        None
    };

    P2pReport {
        punched: true,
        peer_label: None,
        active_peer_addr: Some(active_peer_addr.to_string()),
        ip_family: Some(ip_family),
        punch_elapsed_ms: Some(punch_elapsed_ms),
        sent,
        received,
        loss_ratio,
        avg_rtt_ms,
        p50_rtt_ms,
        p95_rtt_ms,
        jitter_ms,
        iqr_ms,
        tail_bloat_ms,
        spike_ratio,
        latency_tier,
        stability_grade: Some(stability_grade),
        series,
        verdict,
        failure_reason,
        interrupted_reason: None,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_probe<F>(
    handle: SessionHandle,
    session_id: &str,
    peer_candidates: &[Candidate],
    start_delay_ms: u32,
    punch_timeout_ms: u32,
    window_secs: u32,
    packet_rate_hz: u32,
    strategy: PunchStrategy,
    mut on_event: F,
    cancel: CancellationToken,
) -> Result<P2pReport, ProbeError>
where
    F: FnMut(P2pEvent) + Send,
{
    if session_id.trim().is_empty() {
        logger::warn(MODULE_TAG, "execute_probe rejected empty session_id");
        drop(handle);
        return Err(ProbeError::InvalidInput(
            "session_id must not be empty".to_string(),
        ));
    }
    if !(MIN_PUNCH_TIMEOUT_MS..=MAX_PUNCH_TIMEOUT_MS).contains(&punch_timeout_ms) {
        logger::warn(
            MODULE_TAG,
            format!(
                "execute_probe rejected punch_timeout_ms={punch_timeout_ms} (expected {MIN_PUNCH_TIMEOUT_MS}..={MAX_PUNCH_TIMEOUT_MS})"
            ),
        );
        drop(handle);
        return Err(ProbeError::InvalidInput(format!(
            "punch_timeout_ms {punch_timeout_ms} out of bounds {MIN_PUNCH_TIMEOUT_MS}..={MAX_PUNCH_TIMEOUT_MS}"
        )));
    }
    if !(MIN_WINDOW_SECS..=MAX_WINDOW_SECS).contains(&window_secs) {
        logger::warn(
            MODULE_TAG,
            format!(
                "execute_probe rejected window_secs={window_secs} (expected {MIN_WINDOW_SECS}..={MAX_WINDOW_SECS})"
            ),
        );
        drop(handle);
        return Err(ProbeError::InvalidInput(format!(
            "window_secs {window_secs} out of bounds {MIN_WINDOW_SECS}..={MAX_WINDOW_SECS}"
        )));
    }
    if !(MIN_PACKET_RATE_HZ..=MAX_PACKET_RATE_HZ).contains(&packet_rate_hz) {
        logger::warn(
            MODULE_TAG,
            format!(
                "execute_probe rejected packet_rate_hz={packet_rate_hz} (expected {MIN_PACKET_RATE_HZ}..={MAX_PACKET_RATE_HZ})"
            ),
        );
        drop(handle);
        return Err(ProbeError::InvalidInput(format!(
            "packet_rate_hz {packet_rate_hz} out of bounds {MIN_PACKET_RATE_HZ}..={MAX_PACKET_RATE_HZ}"
        )));
    }

    // Verify worst-case serialized P2pDirectMsg fits MAX_UDP_PACKET_BYTES.
    let worst_case = P2pDirectMsg {
        session_id: session_id.to_string(),
        kind: P2pMsgKind::PunchAck,
        seq: u32::MAX,
        sent_offset_ms: u32::MAX,
    };
    let _ = encode_p2p_msg(&worst_case)?;

    if cancel.is_cancelled() {
        drop(handle);
        logger::info(
            MODULE_TAG,
            format!("execute_probe cancelled before start (session_id={session_id})"),
        );
        return Err(ProbeError::Cancelled);
    }

    let SessionHandle { mut v4_sock, mut v6_sock } = handle;
    match strategy {
        PunchStrategy::StandardUdpV6First => {}
        PunchStrategy::Ipv4Only => {
            v6_sock = None;
        }
        PunchStrategy::Ipv6Only => {
            v4_sock = None;
        }
    }
    if v4_sock.is_none() && v6_sock.is_none() {
        return Err(ProbeError::Internal(
            "session handle holds no active sockets for requested strategy".to_string(),
        ));
    }

    // Phase A: synchronized start delay
    if start_delay_ms > 0 {
        logger::info(
            MODULE_TAG,
            format!(
                "execute_probe Phase A sleeping {start_delay_ms}ms (session_id={session_id})"
            ),
        );
        tokio::select! {
            _ = cancel.cancelled() => {
                drop(v4_sock);
                drop(v6_sock);
                logger::info(MODULE_TAG, format!("execute_probe cancelled in Phase A (session_id={session_id})"));
                return Err(ProbeError::Cancelled);
            }
            _ = tokio::time::sleep(Duration::from_millis(u64::from(start_delay_ms))) => {}
        }
    }

    // Phase B: Punching
    let (v6_targets, v4_targets) =
        partition_peer_candidates(peer_candidates, v4_sock.is_some(), v6_sock.is_some());
    if v6_targets.is_empty() && v4_targets.is_empty() {
        drop(v4_sock);
        drop(v6_sock);
        logger::warn(
            MODULE_TAG,
            format!(
                "execute_probe Phase B failed fast: no compatible peer candidates for strategy {strategy:?} (session_id={session_id})"
            ),
        );
        return Ok(P2pReport {
            punched: false,
            peer_label: None,
            active_peer_addr: None,
            ip_family: None,
            punch_elapsed_ms: None,
            sent: 0,
            received: 0,
            loss_ratio: 1.0,
            avg_rtt_ms: None,
            p50_rtt_ms: None,
            p95_rtt_ms: None,
            jitter_ms: None,
            iqr_ms: None,
            tail_bloat_ms: None,
            spike_ratio: None,
            latency_tier: None,
            stability_grade: Some(P2pStabilityGrade::Failed),
            series: Vec::new(),
            verdict: P2pVerdict::Failed,
            failure_reason: Some("no_compatible_candidates".to_string()),
            interrupted_reason: None,
        });
    }

    let punch_start = Instant::now();
    let punch_deadline = punch_start + Duration::from_millis(u64::from(punch_timeout_ms));
    let burst_interval = Duration::from_millis(PUNCH_BURST_INTERVAL_MS);

    let mut v4_unlocked = match strategy {
        PunchStrategy::StandardUdpV6First => v6_targets.is_empty() || v6_sock.is_none(),
        PunchStrategy::Ipv4Only => true,
        PunchStrategy::Ipv6Only => false,
    };
    let mut v4_unlock_at =
        if matches!(strategy, PunchStrategy::Ipv6Only) || v4_unlocked || v4_targets.is_empty() {
            None
        } else {
            Some(punch_start + Duration::from_millis(V4_DELAY_AFTER_V6_MS))
        };

    let mut next_punch_seq: u32 = 1;
    logger::info(
        MODULE_TAG,
        format!(
            "execute_probe Phase B started (session_id={session_id}, v6_targets={}, v4_targets={}, v4_delayed={})",
            v6_targets.len(),
            v4_targets.len(),
            !v4_unlocked
        ),
    );

    if !v6_targets.is_empty() {
        send_punch_burst(
            v6_sock.as_deref(),
            &v6_targets,
            session_id,
            next_punch_seq,
            0,
        )
        .await;
        next_punch_seq = next_punch_seq.saturating_add(1);
    }
    if v4_unlocked && !v4_targets.is_empty() {
        send_punch_burst(
            v4_sock.as_deref(),
            &v4_targets,
            session_id,
            next_punch_seq,
            0,
        )
        .await;
        next_punch_seq = next_punch_seq.saturating_add(1);
    }

    let mut next_burst_at = punch_start + burst_interval;
    let mut v6_buf = [0u8; RECV_BUF_LEN];
    let mut v4_buf = [0u8; RECV_BUF_LEN];
    let mut confirmed: Option<(SocketAddr, IpFamily, P2pDirectMsg)> = None;

    while confirmed.is_none() {
        if Instant::now() >= punch_deadline {
            break;
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                drop(v4_sock);
                drop(v6_sock);
                logger::info(MODULE_TAG, format!("execute_probe cancelled in Phase B (session_id={session_id})"));
                return Err(ProbeError::Cancelled);
            }
            res_v6 = recv_from_opt(v6_sock.as_deref(), &mut v6_buf) => {
                match res_v6 {
                    Ok((n, from)) => {
                        if let Some(msg) = parse_valid_direct_msg(&v6_buf[..n], from, IpFamily::Ipv6, session_id) {
                            if let Some(s) = v6_sock.as_deref() {
                                confirmed = handle_phase_b_packet(
                                    IpFamily::Ipv6,
                                    s,
                                    from,
                                    msg,
                                    session_id,
                                    punch_start,
                                    &mut next_punch_seq,
                                ).await;
                            }
                        }
                    }
                    Err(e) => {
                        logger::warn(MODULE_TAG, format!("v6_sock recv_from error in Phase B: {e}"));
                    }
                }
            }
            res_v4 = recv_from_opt(v4_sock.as_deref(), &mut v4_buf) => {
                match res_v4 {
                    Ok((n, from)) => {
                        if let Some(msg) = parse_valid_direct_msg(&v4_buf[..n], from, IpFamily::Ipv4, session_id) {
                            if let Some(s) = v4_sock.as_deref() {
                                let v4_candidate = handle_phase_b_packet(
                                    IpFamily::Ipv4,
                                    s,
                                    from,
                                    msg,
                                    session_id,
                                    punch_start,
                                    &mut next_punch_seq,
                                ).await;
                                if let Some(v4_match) = v4_candidate {
                                    // Prefer IPv6 if a valid IPv6 confirmation also arrived in the same instant.
                                    let mut v6_match = None;
                                    if let Some(s6) = v6_sock.as_deref() {
                                        for _ in 0..16 {
                                            match s6.try_recv_from(&mut v6_buf) {
                                                Ok((n6, from6)) => {
                                                    if let Some(msg6) = parse_valid_direct_msg(&v6_buf[..n6], from6, IpFamily::Ipv6, session_id) {
                                                        if let Some(found6) = handle_phase_b_packet(
                                                            IpFamily::Ipv6,
                                                            s6,
                                                            from6,
                                                            msg6,
                                                            session_id,
                                                            punch_start,
                                                            &mut next_punch_seq,
                                                        ).await {
                                                            v6_match = Some(found6);
                                                            break;
                                                        }
                                                    }
                                                }
                                                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                                                Err(_) => break,
                                            }
                                        }
                                    }
                                    confirmed = v6_match.or(Some(v4_match));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        logger::warn(MODULE_TAG, format!("v4_sock recv_from error in Phase B: {e}"));
                    }
                }
            }
            _ = sleep_until_opt(v4_unlock_at) => {
                v4_unlocked = true;
                v4_unlock_at = None;
                let offset = elapsed_u32(punch_start);
                send_punch_burst(
                    v4_sock.as_deref(),
                    &v4_targets,
                    session_id,
                    next_punch_seq,
                    offset,
                ).await;
                next_punch_seq = next_punch_seq.saturating_add(1);
            }
            _ = tokio::time::sleep_until(next_burst_at) => {
                let offset = elapsed_u32(punch_start);
                if !v6_targets.is_empty() {
                    send_punch_burst(
                        v6_sock.as_deref(),
                        &v6_targets,
                        session_id,
                        next_punch_seq,
                        offset,
                    ).await;
                }
                if v4_unlocked && !v4_targets.is_empty() {
                    send_punch_burst(
                        v4_sock.as_deref(),
                        &v4_targets,
                        session_id,
                        next_punch_seq,
                        offset,
                    ).await;
                }
                next_punch_seq = next_punch_seq.saturating_add(1);
                next_burst_at = Instant::now() + burst_interval;
            }
            _ = tokio::time::sleep_until(punch_deadline) => {
                break;
            }
        }
    }

    let Some((active_peer_addr, ip_family, trigger_msg)) = confirmed else {
        drop(v4_sock);
        drop(v6_sock);
        logger::warn(
            MODULE_TAG,
            format!("execute_probe Phase B punch_timeout after {punch_timeout_ms}ms (session_id={session_id})"),
        );
        return Ok(P2pReport {
            punched: false,
            peer_label: None,
            active_peer_addr: None,
            ip_family: None,
            punch_elapsed_ms: None,
            sent: 0,
            received: 0,
            loss_ratio: 1.0,
            avg_rtt_ms: None,
            p50_rtt_ms: None,
            p95_rtt_ms: None,
            jitter_ms: None,
            iqr_ms: None,
            tail_bloat_ms: None,
            spike_ratio: None,
            latency_tier: None,
            stability_grade: Some(P2pStabilityGrade::Failed),
            series: Vec::new(),
            verdict: P2pVerdict::Failed,
            failure_reason: Some("punch_timeout".to_string()),
            interrupted_reason: None,
        });
    };

    let active_sock = match ip_family {
        IpFamily::Ipv6 => {
            v4_sock.take();
            v6_sock.take().ok_or_else(|| {
                ProbeError::Internal("missing v6_sock after IPv6 punch confirmation".to_string())
            })?
        }
        IpFamily::Ipv4 => {
            v6_sock.take();
            v4_sock.take().ok_or_else(|| {
                ProbeError::Internal("missing v4_sock after IPv4 punch confirmation".to_string())
            })?
        }
    };

    let punch_elapsed_ms = elapsed_u32(punch_start);
    let now_offset = punch_elapsed_ms;
    send_p2p_reply(
        &active_sock,
        active_peer_addr,
        session_id,
        P2pMsgKind::PunchAck,
        trigger_msg.seq,
        now_offset,
    )
    .await;
    if trigger_msg.kind == P2pMsgKind::Ping {
        send_p2p_reply(
            &active_sock,
            active_peer_addr,
            session_id,
            P2pMsgKind::Pong,
            trigger_msg.seq,
            trigger_msg.sent_offset_ms,
        )
        .await;
    }

    logger::info(
        MODULE_TAG,
        format!(
            "execute_probe punch succeeded (session_id={session_id}, peer={active_peer_addr}, family={ip_family:?}, elapsed_ms={punch_elapsed_ms})"
        ),
    );
    on_event(P2pEvent::PunchSucceeded {
        peer_addr: active_peer_addr,
        ip_family,
        elapsed_ms: punch_elapsed_ms,
    });

    // Phase C: Fixed-window latency sampling
    let total_packets = window_secs * packet_rate_hz;
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(packet_rate_hz));
    let sample_start = Instant::now();
    let mut records: Vec<SentRecord> = Vec::with_capacity(total_packets as usize);
    let mut next_seq: u32 = 1;
    let mut next_send_at = Some(sample_start);
    let mut grace_deadline: Option<Instant> = None;
    let mut max_observed_rtt_ms: f32 = 0.0;
    let mut received_count: u32 = 0;
    let mut recv_buf = [0u8; RECV_BUF_LEN];

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                drop(active_sock);
                logger::info(MODULE_TAG, format!("execute_probe cancelled in Phase C (session_id={session_id})"));
                return Err(ProbeError::Cancelled);
            }
            recv_res = active_sock.recv_from(&mut recv_buf) => {
                match recv_res {
                    Ok((n, from)) => {
                        if let Some(msg) = parse_valid_direct_msg(&recv_buf[..n], from, ip_family, session_id) {
                            match msg.kind {
                                P2pMsgKind::Punch => {
                                    send_p2p_reply(
                                        &active_sock,
                                        from,
                                        session_id,
                                        P2pMsgKind::PunchAck,
                                        msg.seq,
                                        elapsed_u32(sample_start),
                                    ).await;
                                }
                                P2pMsgKind::PunchAck => {}
                                P2pMsgKind::Ping => {
                                    send_p2p_reply(
                                        &active_sock,
                                        from,
                                        session_id,
                                        P2pMsgKind::Pong,
                                        msg.seq,
                                        msg.sent_offset_ms,
                                    ).await;
                                }
                                P2pMsgKind::Pong => {
                                    if msg.seq >= 1 && (msg.seq as usize) <= records.len() {
                                        let rec = &mut records[(msg.seq - 1) as usize];
                                        if rec.rtt_ms.is_none() {
                                            let rtt_ms = (rec.sent_at.elapsed().as_secs_f64() * 1000.0) as f32;
                                            rec.rtt_ms = Some(rtt_ms);
                                            received_count += 1;
                                            if rtt_ms > max_observed_rtt_ms {
                                                max_observed_rtt_ms = rtt_ms;
                                            }
                                            on_event(P2pEvent::LatencyPoint(LatencyPoint {
                                                seq: msg.seq,
                                                elapsed_ms: rec.sent_offset_ms,
                                                rtt_ms: Some(rtt_ms),
                                            }));
                                            if next_send_at.is_none() && received_count >= total_packets {
                                                break;
                                            }
                                        }
                                    } else {
                                        logger::warn(
                                            MODULE_TAG,
                                            format!("ignoring Pong with out-of-range seq={} from {from}", msg.seq),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        logger::warn(MODULE_TAG, format!("active_sock recv_from error in Phase C: {e}"));
                    }
                }
            }
            _ = sleep_until_opt(next_send_at) => {
                let sent_at = Instant::now();
                let sent_offset_ms = elapsed_u32(sample_start);
                let ping = P2pDirectMsg {
                    session_id: session_id.to_string(),
                    kind: P2pMsgKind::Ping,
                    seq: next_seq,
                    sent_offset_ms,
                };
                if let Ok(bytes) = encode_p2p_msg(&ping) {
                    if let Err(e) = active_sock.send_to(&bytes, active_peer_addr).await {
                        logger::warn(
                            MODULE_TAG,
                            format!("Phase C Ping seq={next_seq} send_to {active_peer_addr} failed: {e}"),
                        );
                    }
                }
                records.push(SentRecord {
                    sent_at,
                    sent_offset_ms,
                    rtt_ms: None,
                });
                next_seq += 1;
                if next_seq <= total_packets {
                    next_send_at = Some(sample_start + interval * (next_seq - 1));
                } else {
                    next_send_at = None;
                    if received_count >= total_packets {
                        break;
                    }
                    let min_grace = (interval * 2).max(Duration::from_millis(400));
                    let dynamic_grace = Duration::from_secs_f64(
                        (f64::from(max_observed_rtt_ms) * 2.0 / 1000.0).clamp(0.4, 1.5),
                    );
                    let grace = min_grace.max(dynamic_grace).min(Duration::from_millis(1500));
                    grace_deadline = Some(Instant::now() + grace);
                }
            }
            _ = sleep_until_opt(grace_deadline) => {
                break;
            }
        }
    }

    drop(active_sock);

    let mut series = Vec::with_capacity(records.len());
    for (idx, rec) in records.iter().enumerate() {
        let seq = (idx + 1) as u32;
        let point = LatencyPoint {
            seq,
            elapsed_ms: rec.sent_offset_ms,
            rtt_ms: rec.rtt_ms,
        };
        if rec.rtt_ms.is_none() {
            on_event(P2pEvent::LatencyPoint(point.clone()));
        }
        series.push(point);
    }

    let report = summarize_report(
        active_peer_addr,
        ip_family,
        punch_elapsed_ms,
        total_packets,
        series,
    );
    logger::info(
        MODULE_TAG,
        format!(
            "execute_probe Phase C complete (session_id={session_id}, sent={}, received={}, loss={:.4}, verdict={:?})",
            report.sent, report.received, report.loss_ratio, report.verdict
        ),
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn build_mock_stun_xor_response(tx_id: [u8; 12], peer: SocketAddr) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&STUN_MSG_BINDING_RESPONSE.to_be_bytes());
        let attr_len: u16 = match peer {
            SocketAddr::V4(_) => 8,
            SocketAddr::V6(_) => 20,
        };
        let total_attr_bytes = 4 + attr_len;
        out.extend_from_slice(&total_attr_bytes.to_be_bytes());
        out.extend_from_slice(&STUN_MAGIC_COOKIE);
        out.extend_from_slice(&tx_id);

        out.extend_from_slice(&STUN_ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        out.extend_from_slice(&attr_len.to_be_bytes());
        out.push(0);
        let xport = peer.port() ^ u16::from_be_bytes([STUN_MAGIC_COOKIE[0], STUN_MAGIC_COOKIE[1]]);
        match peer {
            SocketAddr::V4(v4) => {
                out.push(0x01);
                out.extend_from_slice(&xport.to_be_bytes());
                let octets = v4.ip().octets();
                for i in 0..4 {
                    out.push(octets[i] ^ STUN_MAGIC_COOKIE[i]);
                }
            }
            SocketAddr::V6(v6) => {
                out.push(0x02);
                out.extend_from_slice(&xport.to_be_bytes());
                let mut xor_key = [0u8; 16];
                xor_key[..4].copy_from_slice(&STUN_MAGIC_COOKIE);
                xor_key[4..16].copy_from_slice(&tx_id);
                let octets = v6.ip().octets();
                for i in 0..16 {
                    out.push(octets[i] ^ xor_key[i]);
                }
            }
        }
        out
    }

    fn build_mock_stun_mapped_response(tx_id: [u8; 12], peer: SocketAddr) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&STUN_MSG_BINDING_RESPONSE.to_be_bytes());
        let attr_len: u16 = match peer {
            SocketAddr::V4(_) => 8,
            SocketAddr::V6(_) => 20,
        };
        let total_attr_bytes = 4 + attr_len;
        out.extend_from_slice(&total_attr_bytes.to_be_bytes());
        out.extend_from_slice(&STUN_MAGIC_COOKIE);
        out.extend_from_slice(&tx_id);

        out.extend_from_slice(&STUN_ATTR_MAPPED_ADDRESS.to_be_bytes());
        out.extend_from_slice(&attr_len.to_be_bytes());
        out.push(0);
        match peer {
            SocketAddr::V4(v4) => {
                out.push(0x01);
                out.extend_from_slice(&peer.port().to_be_bytes());
                out.extend_from_slice(&v4.ip().octets());
            }
            SocketAddr::V6(v6) => {
                out.push(0x02);
                out.extend_from_slice(&peer.port().to_be_bytes());
                out.extend_from_slice(&v6.ip().octets());
            }
        }
        out
    }

    #[test]
    fn stun_request_and_response_parser_roundtrip() {
        let tx_id = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let req = build_stun_binding_request(tx_id);
        assert_eq!(req.len(), 20);
        assert_eq!(&req[4..8], &STUN_MAGIC_COOKIE);
        assert_eq!(&req[8..20], &tx_id);

        let v4_peer: SocketAddr = "198.51.100.42:54321".parse().unwrap();
        let xor_v4 = build_mock_stun_xor_response(tx_id, v4_peer);
        assert_eq!(parse_stun_binding_response(&xor_v4, &tx_id), Some(v4_peer));

        let v6_peer: SocketAddr = "[2001:db8:1234::5678]:41234".parse().unwrap();
        let xor_v6 = build_mock_stun_xor_response(tx_id, v6_peer);
        assert_eq!(parse_stun_binding_response(&xor_v6, &tx_id), Some(v6_peer));

        let mapped_v4 = build_mock_stun_mapped_response(tx_id, v4_peer);
        assert_eq!(parse_stun_binding_response(&mapped_v4, &tx_id), Some(v4_peer));

        let mapped_v6 = build_mock_stun_mapped_response(tx_id, v6_peer);
        assert_eq!(parse_stun_binding_response(&mapped_v6, &tx_id), Some(v6_peer));

        let wrong_tx = [9u8; 12];
        assert_eq!(parse_stun_binding_response(&xor_v4, &wrong_tx), None);
    }

    #[test]
    fn global_v6_filtering_rejects_non_global_ranges() {
        assert!(!is_global_v6(Ipv6Addr::UNSPECIFIED));
        assert!(!is_global_v6(Ipv6Addr::LOCALHOST));
        assert!(!is_global_v6("fe80::1".parse().unwrap()));
        assert!(!is_global_v6("fc00::1".parse().unwrap()));
        assert!(!is_global_v6("fd12:3456:789a::1".parse().unwrap()));
        assert!(!is_global_v6("::ffff:192.0.2.1".parse().unwrap()));
        assert!(!is_global_v6("ff02::1".parse().unwrap()));
        assert!(is_global_v6("2400:3200::1".parse().unwrap()));
        assert!(is_global_v6("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn verdict_thresholds_and_stats_math() {
        // RockSolid + UltraLow: loss == 0%, iqr <= 8ms, tail <= 20ms, spike <= 2%
        let excellent_series: Vec<LatencyPoint> = (1..=20)
            .map(|seq| LatencyPoint {
                seq,
                elapsed_ms: seq * 100,
                rtt_ms: Some(15.0 + (seq % 2) as f32 * 2.0),
            })
            .collect();
        let rep = summarize_report(
            "127.0.0.1:40000".parse().unwrap(),
            IpFamily::Ipv4,
            12,
            20,
            excellent_series,
        );
        assert_eq!(rep.verdict, P2pVerdict::Excellent);
        assert_eq!(rep.stability_grade, Some(P2pStabilityGrade::RockSolid));
        assert_eq!(rep.latency_tier, Some(P2pLatencyTier::UltraLow));
        assert_eq!(rep.received, 20);
        assert_eq!(rep.loss_ratio, 0.0);
        assert_eq!(rep.p50_rtt_ms, Some(15.0));
        assert_eq!(rep.p95_rtt_ms, Some(17.0));
        assert_eq!(rep.iqr_ms, Some(2.0));
        assert_eq!(rep.tail_bloat_ms, Some(2.0));
        assert_eq!(rep.spike_ratio, Some(0.0));
        assert_eq!(rep.jitter_ms, Some(2.0));

        // Latency tier boundaries (<30, 30-80, 80-150, >150)
        assert_eq!(classify_latency_tier(None), None);
        assert_eq!(classify_latency_tier(Some(29.9)), Some(P2pLatencyTier::UltraLow));
        assert_eq!(classify_latency_tier(Some(30.0)), Some(P2pLatencyTier::Low));
        assert_eq!(classify_latency_tier(Some(79.9)), Some(P2pLatencyTier::Low));
        assert_eq!(classify_latency_tier(Some(80.0)), Some(P2pLatencyTier::Medium));
        assert_eq!(classify_latency_tier(Some(150.0)), Some(P2pLatencyTier::Medium));
        assert_eq!(classify_latency_tier(Some(150.1)), Some(P2pLatencyTier::High));

        // Cross-ocean high-RTT steady link stays RockSolid
        assert_eq!(
            classify_stability_grade(true, 200, 0.0, Some(3.0), Some(5.0), Some(0.0)),
            P2pStabilityGrade::RockSolid
        );
        // Exact RockSolid edge
        assert_eq!(
            classify_stability_grade(true, 200, 0.0, Some(8.0), Some(20.0), Some(0.02)),
            P2pStabilityGrade::RockSolid
        );
        // Good edge
        assert_eq!(
            classify_stability_grade(true, 99, 0.01, Some(15.0), Some(35.0), Some(0.05)),
            P2pStabilityGrade::Good
        );
        // MildJitter edge (e.g. mobile wireless round 1: 0% loss, iqr=12.6, tail=48.8, spike=10%)
        assert_eq!(
            classify_stability_grade(true, 200, 0.0, Some(12.64), Some(48.79), Some(0.10)),
            P2pStabilityGrade::MildJitter
        );
        assert_eq!(
            classify_stability_grade(true, 97, 0.03, Some(25.0), Some(60.0), Some(0.12)),
            P2pStabilityGrade::MildJitter
        );
        // SevereJitter cases
        assert_eq!(
            classify_stability_grade(true, 96, 0.04, Some(5.0), Some(10.0), Some(0.0)),
            P2pStabilityGrade::SevereJitter
        );
        assert_eq!(
            classify_stability_grade(true, 100, 0.0, Some(25.1), Some(10.0), Some(0.0)),
            P2pStabilityGrade::SevereJitter
        );
        assert_eq!(
            classify_stability_grade(true, 100, 0.0, Some(10.0), Some(60.1), Some(0.0)),
            P2pStabilityGrade::SevereJitter
        );
        assert_eq!(
            classify_stability_grade(true, 100, 0.0, Some(10.0), Some(10.0), Some(0.121)),
            P2pStabilityGrade::SevereJitter
        );
        // Failed cases
        assert_eq!(
            classify_stability_grade(true, 0, 1.0, None, None, None),
            P2pStabilityGrade::Failed
        );
        assert_eq!(
            classify_stability_grade(false, 5, 0.0, Some(1.0), Some(1.0), Some(0.0)),
            P2pStabilityGrade::Failed
        );

        // Single sample jitter/iqr/tail/spike = 0.0, avg/p50/p95 = sample
        let single = summarize_report(
            "127.0.0.1:40000".parse().unwrap(),
            IpFamily::Ipv4,
            5,
            1,
            vec![LatencyPoint {
                seq: 1,
                elapsed_ms: 0,
                rtt_ms: Some(42.5),
            }],
        );
        assert_eq!(single.jitter_ms, Some(0.0));
        assert_eq!(single.p50_rtt_ms, Some(42.5));
        assert_eq!(single.p95_rtt_ms, Some(42.5));
        assert_eq!(single.avg_rtt_ms, Some(42.5));
        assert_eq!(single.iqr_ms, Some(0.0));
        assert_eq!(single.tail_bloat_ms, Some(0.0));
        assert_eq!(single.spike_ratio, Some(0.0));
        assert_eq!(single.latency_tier, Some(P2pLatencyTier::Low));
        assert_eq!(single.stability_grade, Some(P2pStabilityGrade::RockSolid));
        assert_eq!(single.verdict, P2pVerdict::Excellent);
    }

    #[tokio::test]
    async fn execute_probe_rejects_out_of_range_inputs() {
        let cancel = CancellationToken::new();

        // Bad window_secs (below min & above max)
        for bad_window in [MIN_WINDOW_SECS - 1, MAX_WINDOW_SECS + 1] {
            let (h, _) = prepare_session(None, None, &[], None, 500, cancel.clone())
                .await
                .unwrap();
            let err = execute_probe(
                h,
                "sess-1",
                &[],
                0,
                MIN_PUNCH_TIMEOUT_MS,
                bad_window,
                MIN_PACKET_RATE_HZ,
                PunchStrategy::StandardUdpV6First,
                |_| {},
                cancel.clone(),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ProbeError::InvalidInput(_)));
        }

        // Bad packet_rate_hz
        for bad_rate in [MIN_PACKET_RATE_HZ - 1, MAX_PACKET_RATE_HZ + 1] {
            let (h, _) = prepare_session(None, None, &[], None, 500, cancel.clone())
                .await
                .unwrap();
            let err = execute_probe(
                h,
                "sess-2",
                &[],
                0,
                MIN_PUNCH_TIMEOUT_MS,
                MIN_WINDOW_SECS,
                bad_rate,
                PunchStrategy::StandardUdpV6First,
                |_| {},
                cancel.clone(),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ProbeError::InvalidInput(_)));
        }

        // Bad punch_timeout_ms
        for bad_timeout in [MIN_PUNCH_TIMEOUT_MS - 1, MAX_PUNCH_TIMEOUT_MS + 1] {
            let (h, _) = prepare_session(None, None, &[], None, 500, cancel.clone())
                .await
                .unwrap();
            let err = execute_probe(
                h,
                "sess-3",
                &[],
                0,
                bad_timeout,
                MIN_WINDOW_SECS,
                MIN_PACKET_RATE_HZ,
                PunchStrategy::StandardUdpV6First,
                |_| {},
                cancel.clone(),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ProbeError::InvalidInput(_)));
        }

        // Empty session_id
        let (h, _) = prepare_session(None, None, &[], None, 500, cancel.clone())
            .await
            .unwrap();
        let err = execute_probe(
            h,
            "   ",
            &[],
            0,
            MIN_PUNCH_TIMEOUT_MS,
            MIN_WINDOW_SECS,
            MIN_PACKET_RATE_HZ,
            PunchStrategy::StandardUdpV6First,
            |_| {},
            cancel.clone(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProbeError::InvalidInput(_)));
    }

    async fn spawn_mock_s2_and_stun() -> (
        String,
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        CancellationToken,
    ) {
        let s2_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let s2_addr = s2_sock.local_addr().unwrap().to_string();
        let stun_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun_addr = stun_sock.local_addr().unwrap().to_string();

        let s2_hits = Arc::new(AtomicUsize::new(0));
        let stun_hits = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();

        let s2_hits_cl = s2_hits.clone();
        let stop_s2 = stop.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                tokio::select! {
                    _ = stop_s2.cancelled() => break,
                    res = s2_sock.recv_from(&mut buf) => {
                        if let Ok((n, from)) = res {
                            if let Ok(S2WireMessage::AddrEchoReq(req)) = serde_json::from_slice(&buf[..n]) {
                                s2_hits_cl.fetch_add(1, Ordering::SeqCst);
                                let resp = S2WireMessage::AddrEchoResp(protocol::AddrEchoResp {
                                    nonce: req.nonce,
                                    observed_addr: from.to_string(),
                                    responder_id: Some("mock-s2".into()),
                                });
                                let bytes = serde_json::to_vec(&resp).unwrap();
                                let _ = s2_sock.send_to(&bytes, from).await;
                            }
                        }
                    }
                }
            }
        });

        let stun_hits_cl = stun_hits.clone();
        let stop_stun = stop.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                tokio::select! {
                    _ = stop_stun.cancelled() => break,
                    res = stun_sock.recv_from(&mut buf) => {
                        if let Ok((n, from)) = res {
                            if n >= STUN_HEADER_LEN
                                && u16::from_be_bytes([buf[0], buf[1]]) == STUN_MSG_BINDING_REQUEST
                                && buf[4..8] == STUN_MAGIC_COOKIE
                            {
                                stun_hits_cl.fetch_add(1, Ordering::SeqCst);
                                let mut tx_id = [0u8; 12];
                                tx_id.copy_from_slice(&buf[8..20]);
                                let resp = build_mock_stun_xor_response(tx_id, from);
                                let _ = stun_sock.send_to(&resp, from).await;
                            }
                        }
                    }
                }
            }
        });

        (s2_addr, stun_addr, s2_hits, stun_hits, stop)
    }

    #[tokio::test]
    async fn prepare_keepalive_and_abort_session_with_mock_responders() {
        let (s2_addr, stun_addr, s2_hits, stun_hits, stop) = spawn_mock_s2_and_stun().await;
        let cancel = CancellationToken::new();

        let (handle, candidates) = prepare_session(
            Some(&s2_addr),
            None,
            std::slice::from_ref(&stun_addr),
            Some("tok-abc"),
            1500,
            cancel.clone(),
        )
        .await
        .unwrap();

        let (v4_local, _v6_local) = handle.local_addrs();
        let v4_local = v4_local.expect("v4 socket must be bound");
        let expected_srflx = format!("127.0.0.1:{}", v4_local.port());

        let srflx_count = candidates
            .iter()
            .filter(|c| c.kind == CandidateKind::ServerReflexiveV4)
            .count();
        assert_eq!(srflx_count, 1, "duplicate reflexive candidates should be deduplicated");
        assert!(candidates
            .iter()
            .any(|c| c.kind == CandidateKind::ServerReflexiveV4 && c.addr == expected_srflx));

        let hits_before_s2 = s2_hits.load(Ordering::SeqCst);
        let hits_before_stun = stun_hits.load(Ordering::SeqCst);

        keepalive_session(
            &handle,
            Some(&s2_addr),
            None,
            &[stun_addr],
            Some("tok-abc"),
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(s2_hits.load(Ordering::SeqCst) > hits_before_s2);
        assert!(stun_hits.load(Ordering::SeqCst) > hits_before_stun);

        abort_session(handle);

        // Unreachable external server should return ServerUnreachable.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap().to_string();
        let err = prepare_session(
            Some(&silent_addr),
            None,
            &[],
            None,
            150,
            cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProbeError::ServerUnreachable(_)));

        stop.cancel();
    }

    #[tokio::test]
    async fn dual_task_localhost_punch_and_fixed_window_sampling() {
        let (s2_addr, _stun_addr, _s2_hits, _stun_hits, stop) = spawn_mock_s2_and_stun().await;
        let cancel = CancellationToken::new();

        let (handle_a, cand_a) = prepare_session(
            Some(&s2_addr),
            None,
            &[],
            None,
            1500,
            cancel.clone(),
        )
        .await
        .unwrap();
        let (handle_b, cand_b) = prepare_session(
            Some(&s2_addr),
            None,
            &[],
            None,
            1500,
            cancel.clone(),
        )
        .await
        .unwrap();

        let cand_a_srflx: Vec<Candidate> = cand_a
            .into_iter()
            .filter(|c| c.kind == CandidateKind::ServerReflexiveV4)
            .collect();
        let cand_b_srflx: Vec<Candidate> = cand_b
            .into_iter()
            .filter(|c| c.kind == CandidateKind::ServerReflexiveV4)
            .collect();

        let mut events_a = Vec::new();
        let mut events_b = Vec::new();

        let (res_a, res_b) = tokio::join!(
            execute_probe(
                handle_a,
                "sess-dual-1",
                &cand_b_srflx,
                40,
                3000,
                5,
                5,
                PunchStrategy::StandardUdpV6First,
                |ev| events_a.push(ev),
                cancel.clone(),
            ),
            execute_probe(
                handle_b,
                "sess-dual-1",
                &cand_a_srflx,
                40,
                3000,
                5,
                5,
                PunchStrategy::StandardUdpV6First,
                |ev| events_b.push(ev),
                cancel.clone(),
            ),
        );

        stop.cancel();

        let rep_a = res_a.unwrap();
        let rep_b = res_b.unwrap();

        for (label, rep, evs) in [("A", &rep_a, &events_a), ("B", &rep_b, &events_b)] {
            assert!(rep.punched, "peer {label} should punch");
            assert_eq!(rep.ip_family, Some(IpFamily::Ipv4));
            assert!(rep.active_peer_addr.is_some());
            assert!(rep.punch_elapsed_ms.is_some());
            assert_eq!(rep.sent, 25);
            assert_eq!(rep.received, 25);
            assert_eq!(rep.loss_ratio, 0.0);
            assert!(rep.avg_rtt_ms.is_some());
            assert!(rep.p95_rtt_ms.is_some());
            assert!(rep.jitter_ms.is_some());
            assert_eq!(rep.series.len(), 25);
            assert_eq!(rep.verdict, P2pVerdict::Excellent);
            assert!(rep.failure_reason.is_none());

            let punch_events = evs
                .iter()
                .filter(|e| matches!(e, P2pEvent::PunchSucceeded { .. }))
                .count();
            let point_events = evs
                .iter()
                .filter(|e| matches!(e, P2pEvent::LatencyPoint(_)))
                .count();
            assert_eq!(punch_events, 1);
            assert_eq!(point_events, 25);
            for (idx, pt) in rep.series.iter().enumerate() {
                assert_eq!(pt.seq, (idx + 1) as u32);
                assert!(pt.rtt_ms.is_some());
            }
        }
    }

    #[tokio::test]
    async fn blackhole_peer_returns_punch_timeout_report() {
        let blackhole = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap().to_string();
        let cancel = CancellationToken::new();

        let (handle, _) = prepare_session(None, None, &[], None, 500, cancel.clone())
            .await
            .unwrap();

        let rep = execute_probe(
            handle,
            "sess-blackhole",
            &[Candidate {
                kind: CandidateKind::ServerReflexiveV4,
                addr: blackhole_addr,
            }],
            0,
            MIN_PUNCH_TIMEOUT_MS,
            MIN_WINDOW_SECS,
            MIN_PACKET_RATE_HZ,
            PunchStrategy::StandardUdpV6First,
            |_| {},
            cancel,
        )
        .await
        .unwrap();

        assert!(!rep.punched);
        assert_eq!(rep.verdict, P2pVerdict::Failed);
        assert_eq!(rep.failure_reason.as_deref(), Some("punch_timeout"));
        assert_eq!(rep.sent, 0);
        assert_eq!(rep.received, 0);
        assert_eq!(rep.loss_ratio, 1.0);
        assert!(rep.series.is_empty());
    }

    #[tokio::test]
    async fn mid_window_peer_drop_finishes_fixed_window_and_marks_losses() {
        let (s2_addr, _stun_addr, _s2_hits, _stun_hits, stop) = spawn_mock_s2_and_stun().await;
        let cancel = CancellationToken::new();
        let (handle, _) = prepare_session(Some(&s2_addr), None, &[], None, 1000, cancel.clone())
            .await
            .unwrap();

        let peer_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer_sock.local_addr().unwrap().to_string();
        let stop_peer = stop.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                tokio::select! {
                    _ = stop_peer.cancelled() => break,
                    res = peer_sock.recv_from(&mut buf) => {
                        let Ok((n, from)) = res else { break };
                        let Ok(msg) = serde_json::from_slice::<P2pDirectMsg>(&buf[..n]) else {
                            continue;
                        };
                        match msg.kind {
                            P2pMsgKind::Punch => {
                                let ack = P2pDirectMsg {
                                    session_id: msg.session_id,
                                    kind: P2pMsgKind::PunchAck,
                                    seq: msg.seq,
                                    sent_offset_ms: 0,
                                };
                                let bytes = serde_json::to_vec(&ack).unwrap();
                                let _ = peer_sock.send_to(&bytes, from).await;
                            }
                            P2pMsgKind::Ping if msg.seq <= 20 => {
                                let pong = P2pDirectMsg {
                                    session_id: msg.session_id,
                                    kind: P2pMsgKind::Pong,
                                    seq: msg.seq,
                                    sent_offset_ms: msg.sent_offset_ms,
                                };
                                let bytes = serde_json::to_vec(&pong).unwrap();
                                let _ = peer_sock.send_to(&bytes, from).await;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        let mut seen_events = Vec::new();
        let rep = execute_probe(
            handle,
            "sess-partial-loss",
            &[Candidate {
                kind: CandidateKind::ServerReflexiveV4,
                addr: peer_addr,
            }],
            0,
            3000,
            5,
            5,
            PunchStrategy::StandardUdpV6First,
            |ev| seen_events.push(ev),
            cancel,
        )
        .await
        .unwrap();

        stop.cancel();

        assert!(rep.punched);
        assert_eq!(rep.sent, 25);
        assert_eq!(rep.received, 20);
        assert!((rep.loss_ratio - 0.2).abs() < 1e-6);
        assert_eq!(rep.verdict, P2pVerdict::Unstable);
        assert_eq!(rep.series.len(), 25);
        for pt in &rep.series[..20] {
            assert!(pt.rtt_ms.is_some());
        }
        for pt in &rep.series[20..] {
            assert!(pt.rtt_ms.is_none());
        }
        let lost_emitted = seen_events
            .iter()
            .filter(|e| matches!(e, P2pEvent::LatencyPoint(pt) if pt.rtt_ms.is_none()))
            .count();
        assert_eq!(lost_emitted, 5);
    }

    #[test]
    fn candidate_ordering_dedup_and_v4_mapped_filtering() {
        let ordered = finalize_candidates(vec![
            Candidate {
                kind: CandidateKind::HostLocal,
                addr: "192.168.1.5:5000".into(),
            },
            Candidate {
                kind: CandidateKind::ServerReflexiveV4,
                addr: "198.51.100.1:5000".into(),
            },
            Candidate {
                kind: CandidateKind::ServerReflexiveV4,
                addr: "198.51.100.1:5000".into(),
            },
            Candidate {
                kind: CandidateKind::ServerReflexiveV6,
                addr: "[2400:3200::1]:6000".into(),
            },
            Candidate {
                kind: CandidateKind::HostGlobalV6,
                addr: "[2400:3200::1]:6000".into(),
            },
        ]);
        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].kind, CandidateKind::HostGlobalV6);
        assert_eq!(ordered[1].kind, CandidateKind::ServerReflexiveV4);
        assert_eq!(ordered[2].kind, CandidateKind::HostLocal);

        let (v6_targets, v4_targets) = partition_peer_candidates(
            &[
                Candidate {
                    kind: CandidateKind::ServerReflexiveV6,
                    addr: "[::ffff:192.0.2.1]:4000".into(),
                },
                Candidate {
                    kind: CandidateKind::ServerReflexiveV6,
                    addr: "[2001:db8::1]:4000".into(),
                },
                Candidate {
                    kind: CandidateKind::ServerReflexiveV4,
                    addr: "198.51.100.2:4000".into(),
                },
            ],
            true,
            true,
        );
        assert_eq!(v6_targets.len(), 1);
        assert_eq!(
            v6_targets[0],
            "[2001:db8::1]:4000".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(v4_targets.len(), 1);
        assert_eq!(
            v4_targets[0],
            "198.51.100.2:4000".parse::<SocketAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn execute_probe_cancellation_aborts_cleanly() {
        let cancel = CancellationToken::new();
        let (handle, _) = prepare_session(None, None, &[], None, 500, cancel.clone())
            .await
            .unwrap();

        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            cancel_trigger.cancel();
        });

        let err = execute_probe(
            handle,
            "sess-cancel",
            &[Candidate {
                kind: CandidateKind::ServerReflexiveV4,
                addr: "127.0.0.1:9".into(),
            }],
            500,
            MIN_PUNCH_TIMEOUT_MS,
            MIN_WINDOW_SECS,
            MIN_PACKET_RATE_HZ,
            PunchStrategy::StandardUdpV6First,
            |_| {},
            cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(err, ProbeError::Cancelled);
    }

    #[tokio::test]
    async fn prepare_session_respects_ipv4_only_and_ipv6_only_strategy() {
        let cancel = CancellationToken::new();

        let (handle_v4, cands_v4) = prepare_session_with_strategy(
            None,
            Some("[::1]:9"),
            &[],
            None,
            500,
            PunchStrategy::Ipv4Only,
            cancel.clone(),
        )
        .await
        .unwrap();
        let (v4_local, v6_local) = handle_v4.local_addrs();
        assert!(v4_local.is_some());
        assert!(v6_local.is_none(), "Ipv4Only must not bind IPv6 socket");
        assert!(cands_v4.iter().all(|c| {
            matches!(
                c.kind,
                CandidateKind::HostLocal | CandidateKind::ServerReflexiveV4
            )
        }));
        abort_session(handle_v4);

        if let Ok(probe_v6) = UdpSocket::bind("[::]:0").await {
            drop(probe_v6);
            let (handle_v6, cands_v6) = prepare_session_with_strategy(
                Some("127.0.0.1:9"),
                None,
                &[],
                None,
                500,
                PunchStrategy::Ipv6Only,
                cancel,
            )
            .await
            .unwrap();
            let (v4_local, v6_local) = handle_v6.local_addrs();
            assert!(v4_local.is_none(), "Ipv6Only must not bind IPv4 socket");
            assert!(v6_local.is_some());
            assert!(!cands_v6.iter().any(|c| {
                matches!(
                    c.kind,
                    CandidateKind::HostLocal | CandidateKind::ServerReflexiveV4
                )
            }));
            abort_session(handle_v6);
        }
    }

    #[tokio::test]
    async fn execute_probe_ipv4_only_ignores_v6_candidates_and_punches_v4() {
        let cancel = CancellationToken::new();
        let stop = CancellationToken::new();

        let peer_v4 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_v4_addr = peer_v4.local_addr().unwrap().to_string();
        let stop_peer = stop.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                tokio::select! {
                    _ = stop_peer.cancelled() => break,
                    res = peer_v4.recv_from(&mut buf) => {
                        let Ok((n, from)) = res else { break };
                        let Ok(msg) = serde_json::from_slice::<P2pDirectMsg>(&buf[..n]) else {
                            continue;
                        };
                        match msg.kind {
                            P2pMsgKind::Punch => {
                                let ack = P2pDirectMsg {
                                    session_id: msg.session_id,
                                    kind: P2pMsgKind::PunchAck,
                                    seq: msg.seq,
                                    sent_offset_ms: 0,
                                };
                                let bytes = serde_json::to_vec(&ack).unwrap();
                                let _ = peer_v4.send_to(&bytes, from).await;
                            }
                            P2pMsgKind::Ping => {
                                let pong = P2pDirectMsg {
                                    session_id: msg.session_id,
                                    kind: P2pMsgKind::Pong,
                                    seq: msg.seq,
                                    sent_offset_ms: msg.sent_offset_ms,
                                };
                                let bytes = serde_json::to_vec(&pong).unwrap();
                                let _ = peer_v4.send_to(&bytes, from).await;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        let v6_blackhole_opt = UdpSocket::bind("[::1]:0").await.ok();
        let v6_addr = v6_blackhole_opt
            .as_ref()
            .map(|s| s.local_addr().unwrap().to_string())
            .unwrap_or_else(|| "[2001:db8::1]:40000".to_string());

        let (handle, _) = prepare_session(None, None, &[], None, 500, cancel.clone())
            .await
            .unwrap();

        let rep = execute_probe(
            handle,
            "sess-v4-only",
            &[
                Candidate {
                    kind: CandidateKind::ServerReflexiveV6,
                    addr: v6_addr,
                },
                Candidate {
                    kind: CandidateKind::ServerReflexiveV4,
                    addr: peer_v4_addr,
                },
            ],
            0,
            MIN_PUNCH_TIMEOUT_MS,
            MIN_WINDOW_SECS,
            MIN_PACKET_RATE_HZ,
            PunchStrategy::Ipv4Only,
            |_| {},
            cancel,
        )
        .await
        .unwrap();

        stop.cancel();

        assert!(rep.punched);
        assert_eq!(rep.ip_family, Some(IpFamily::Ipv4));
        assert_eq!(rep.sent, MIN_WINDOW_SECS * MIN_PACKET_RATE_HZ);
        assert_eq!(rep.received, MIN_WINDOW_SECS * MIN_PACKET_RATE_HZ);

        if let Some(v6_bh) = v6_blackhole_opt {
            let mut buf = [0u8; 64];
            assert!(matches!(
                v6_bh.try_recv_from(&mut buf),
                Err(ref e) if e.kind() == ErrorKind::WouldBlock
            ));
        }
    }

    #[tokio::test]
    async fn execute_probe_ipv6_only_ignores_v4_candidates() {
        let cancel = CancellationToken::new();
        let stop = CancellationToken::new();

        let peer_v4 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_v4_addr = peer_v4.local_addr().unwrap().to_string();
        let v4_hits = Arc::new(AtomicUsize::new(0));
        let v4_hits_cl = v4_hits.clone();
        let stop_peer = stop.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                tokio::select! {
                    _ = stop_peer.cancelled() => break,
                    res = peer_v4.recv_from(&mut buf) => {
                        let Ok((n, from)) = res else { break };
                        v4_hits_cl.fetch_add(1, Ordering::SeqCst);
                        if let Ok(msg) = serde_json::from_slice::<P2pDirectMsg>(&buf[..n]) {
                            if msg.kind == P2pMsgKind::Punch {
                                let ack = P2pDirectMsg {
                                    session_id: msg.session_id,
                                    kind: P2pMsgKind::PunchAck,
                                    seq: msg.seq,
                                    sent_offset_ms: 0,
                                };
                                let bytes = serde_json::to_vec(&ack).unwrap();
                                let _ = peer_v4.send_to(&bytes, from).await;
                            }
                        }
                    }
                }
            }
        });

        let v4_only_handle = SessionHandle {
            v4_sock: Some(Arc::new(UdpSocket::bind("0.0.0.0:0").await.unwrap())),
            v6_sock: None,
        };
        let err = execute_probe(
            v4_only_handle,
            "sess-v6-missing-sock",
            &[Candidate {
                kind: CandidateKind::ServerReflexiveV4,
                addr: peer_v4_addr.clone(),
            }],
            0,
            MIN_PUNCH_TIMEOUT_MS,
            MIN_WINDOW_SECS,
            MIN_PACKET_RATE_HZ,
            PunchStrategy::Ipv6Only,
            |_| {},
            cancel.clone(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProbeError::Internal(_)));

        if let Ok(v6_sock) = UdpSocket::bind("[::]:0").await {
            let dual_handle = SessionHandle {
                v4_sock: Some(Arc::new(UdpSocket::bind("0.0.0.0:0").await.unwrap())),
                v6_sock: Some(Arc::new(v6_sock)),
            };
            let rep = execute_probe(
                dual_handle,
                "sess-v6-only",
                &[Candidate {
                    kind: CandidateKind::ServerReflexiveV4,
                    addr: peer_v4_addr,
                }],
                0,
                MIN_PUNCH_TIMEOUT_MS,
                MIN_WINDOW_SECS,
                MIN_PACKET_RATE_HZ,
                PunchStrategy::Ipv6Only,
                |_| {},
                cancel,
            )
            .await
            .unwrap();

            assert!(!rep.punched);
            assert_eq!(rep.verdict, P2pVerdict::Failed);
            assert_eq!(
                rep.failure_reason.as_deref(),
                Some("no_compatible_candidates")
            );
        }

        stop.cancel();
        assert_eq!(v4_hits.load(Ordering::SeqCst), 0);
    }
}
