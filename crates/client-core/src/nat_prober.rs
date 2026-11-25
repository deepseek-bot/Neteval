use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AddrEchoReq, AddrEchoResp, EchoMode, MAX_UDP_PACKET_BYTES, NatFiltering, NatMapping, NatReport,
    NatStatus, NatTaskInput, ProbeError, S2WireMessage,
};
use tokio::net::{UdpSocket, lookup_host};
use tokio::time::{self, Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::logger::{error, info, warn};

const LOG_MOD: &str = "c4";
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const STUN_MAGIC_COOKIE_BYTES: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const STUN_BINDING_REQUEST: u16 = 0x0001;
const STUN_BINDING_RESPONSE: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const FAMILY_IPV4: u8 = 0x01;
const FAMILY_IPV6: u8 = 0x02;
const DNS_TIMEOUT_MIN_MS: u64 = 500;
const DNS_TIMEOUT_MAX_MS: u64 = 5000;

static BOOT_SEED: OnceLock<u64> = OnceLock::new();
static SEQ_COUNTER: AtomicU64 = AtomicU64::new(1);

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn next_nonce() -> u64 {
    let seed = *BOOT_SEED.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x517c_c1b7_2722_0a95);
        let pid = std::process::id() as u64;
        splitmix64(nanos ^ pid.rotate_left(32))
    });
    let seq = SEQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    splitmix64(seed.wrapping_add(seq))
}

fn next_txn_id() -> [u8; 12] {
    let hi = next_nonce().to_be_bytes();
    let lo = next_nonce().to_be_bytes();
    let mut txn = [0u8; 12];
    txn[0..8].copy_from_slice(&hi);
    txn[8..12].copy_from_slice(&lo[0..4]);
    txn
}

/// Builds a 20-byte RFC 5389 STUN Binding Request.
pub fn build_stun_binding_request(txn_id: [u8; 12]) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf[0..2].copy_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
    buf[2..4].copy_from_slice(&0u16.to_be_bytes());
    buf[4..8].copy_from_slice(&STUN_MAGIC_COOKIE_BYTES);
    buf[8..20].copy_from_slice(&txn_id);
    buf
}

/// Parses an RFC 5389 STUN Binding Request header and returns its 12-byte transaction ID.
pub fn parse_stun_binding_request(buf: &[u8]) -> Option<[u8; 12]> {
    if buf.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    if msg_type != STUN_BINDING_REQUEST {
        return None;
    }
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if 20 + msg_len > buf.len() {
        return None;
    }
    let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if cookie != STUN_MAGIC_COOKIE {
        return None;
    }
    let mut txn_id = [0u8; 12];
    txn_id.copy_from_slice(&buf[8..20]);
    Some(txn_id)
}

/// Builds an RFC 5389 STUN Binding Success Response with `XOR-MAPPED-ADDRESS`.
pub fn build_stun_binding_response(txn_id: [u8; 12], mapped: SocketAddr) -> Vec<u8> {
    let x_port = (mapped.port() ^ ((STUN_MAGIC_COOKIE >> 16) as u16)).to_be_bytes();
    let attr_val_len: u16 = match mapped {
        SocketAddr::V4(_) => 8,
        SocketAddr::V6(_) => 20,
    };
    let msg_len = 4 + attr_val_len;

    let mut buf = Vec::with_capacity(20 + msg_len as usize);
    buf.extend_from_slice(&STUN_BINDING_RESPONSE.to_be_bytes());
    buf.extend_from_slice(&msg_len.to_be_bytes());
    buf.extend_from_slice(&STUN_MAGIC_COOKIE_BYTES);
    buf.extend_from_slice(&txn_id);

    buf.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    buf.extend_from_slice(&attr_val_len.to_be_bytes());
    buf.push(0x00);

    match mapped {
        SocketAddr::V4(v4) => {
            buf.push(FAMILY_IPV4);
            buf.extend_from_slice(&x_port);
            let octets = v4.ip().octets();
            for i in 0..4 {
                buf.push(octets[i] ^ STUN_MAGIC_COOKIE_BYTES[i]);
            }
        }
        SocketAddr::V6(v6) => {
            buf.push(FAMILY_IPV6);
            buf.extend_from_slice(&x_port);
            let octets = v6.ip().octets();
            let mut key = [0u8; 16];
            key[0..4].copy_from_slice(&STUN_MAGIC_COOKIE_BYTES);
            key[4..16].copy_from_slice(&txn_id);
            for i in 0..16 {
                buf.push(octets[i] ^ key[i]);
            }
        }
    }

    buf
}

/// Parses an RFC 5389 STUN Binding Success Response, preferring `XOR-MAPPED-ADDRESS`
/// and falling back to `MAPPED-ADDRESS` across both IPv4 and IPv6.
pub fn parse_stun_binding_response(buf: &[u8], expected_txn_id: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    if msg_type != STUN_BINDING_RESPONSE {
        return None;
    }
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = 20usize.checked_add(msg_len)?;
    if end > buf.len() {
        return None;
    }
    let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if cookie != STUN_MAGIC_COOKIE {
        return None;
    }
    if &buf[8..20] != expected_txn_id {
        return None;
    }

    let mut offset = 20usize;
    let mut mapped_fallback: Option<SocketAddr> = None;

    while offset + 4 <= end {
        let attr_type = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let attr_len = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        let val_start = offset + 4;
        let val_end = val_start.checked_add(attr_len)?;
        if val_end > end {
            return None;
        }
        let val = &buf[val_start..val_end];

        if attr_type == ATTR_XOR_MAPPED_ADDRESS {
            if let Some(addr) = parse_stun_addr_attr(val, Some(expected_txn_id)) {
                return Some(addr);
            }
        } else if attr_type == ATTR_MAPPED_ADDRESS && mapped_fallback.is_none() {
            mapped_fallback = parse_stun_addr_attr(val, None);
        }

        let padded_len = (attr_len + 3) & !3;
        offset = match val_start.checked_add(padded_len) {
            Some(next) => next,
            None => break,
        };
    }

    mapped_fallback
}

fn parse_stun_addr_attr(val: &[u8], xor_txn_id: Option<&[u8; 12]>) -> Option<SocketAddr> {
    if val.len() < 4 {
        return None;
    }
    let family = val[1];
    let raw_port = u16::from_be_bytes([val[2], val[3]]);
    let port = match xor_txn_id {
        Some(_) => raw_port ^ ((STUN_MAGIC_COOKIE >> 16) as u16),
        None => raw_port,
    };

    match family {
        FAMILY_IPV4 => {
            if val.len() != 8 {
                return None;
            }
            let mut octets = [val[4], val[5], val[6], val[7]];
            if xor_txn_id.is_some() {
                for i in 0..4 {
                    octets[i] ^= STUN_MAGIC_COOKIE_BYTES[i];
                }
            }
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        FAMILY_IPV6 => {
            if val.len() != 20 {
                return None;
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&val[4..20]);
            if let Some(txn_id) = xor_txn_id {
                let mut key = [0u8; 16];
                key[0..4].copy_from_slice(&STUN_MAGIC_COOKIE_BYTES);
                key[4..16].copy_from_slice(txn_id);
                for i in 0..16 {
                    octets[i] ^= key[i];
                }
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        _ => None,
    }
}

/// Runs the C4 NAT profiler using a reused IPv4 UDP socket with anti-pinhole-poisoning ordering
/// and a separate IPv6 reachability socket.
pub async fn run_nat_probe(
    input: &NatTaskInput,
    cancel: CancellationToken,
) -> Result<NatReport, ProbeError> {
    validate_input(input)?;
    if cancel.is_cancelled() {
        info(LOG_MOD, "NAT probe cancelled before starting");
        return Err(ProbeError::Cancelled);
    }

    info(
        LOG_MOD,
        format!(
            "Starting NAT probe: stun_servers={}, has_p1={}, has_p2={}, has_helper={}, has_v6={}, timeout_ms={}",
            input.public_stun_servers.len(),
            input.primary_v4_p1.is_some(),
            input.primary_v4_p2.is_some(),
            input.helper_v4_p1.is_some(),
            input.primary_v6.is_some(),
            input.timeout_ms,
        ),
    );

    let mut endpoints = resolve_all_endpoints(input, &cancel).await?;
    let mut notes = std::mem::take(&mut endpoints.notes);

    let has_any_v4_target = !endpoints.v4_stun.is_empty()
        || endpoints.primary_v4_p1.is_some()
        || endpoints.primary_v4_p2.is_some()
        || endpoints.helper_v4_p1.is_some();

    let mut v4_observations: Vec<V4Observation> = Vec::new();
    let mut p1_observed: Option<SocketAddr> = None;
    let mut alt_port_ok = false;
    let mut helper_ip_ok = false;

    if has_any_v4_target {
        match bind_udp(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0), &cancel).await? {
            Ok(v4_sock) => {
                let step_a = run_step_a_baseline(
                    &v4_sock,
                    &endpoints,
                    &input.token,
                    input.timeout_ms,
                    &cancel,
                )
                .await?;
                v4_observations = step_a.v4_observations;
                p1_observed = step_a.p1_observed;
                let stun_responded_count = step_a.stun_responded_count;

                if !endpoints.v4_stun.is_empty() || endpoints.stun_dns_failures > 0 {
                    let configured_stun = endpoints.v4_stun.len() + endpoints.stun_dns_failures;
                    notes.push(format!(
                        "IPv4 public STUN reachable: {stun_responded_count}/{configured_stun}"
                    ));
                }

                if let Some(p1_addr) = endpoints.primary_v4_p1 {
                    if p1_observed.is_some() {
                        if input.primary_v4_p2.is_some() {
                            alt_port_ok = run_filtering_probe(
                                &v4_sock,
                                p1_addr,
                                EchoMode::ReplyFromAltPort,
                                &input.token,
                                input.timeout_ms,
                                &cancel,
                            )
                            .await?;
                            info(LOG_MOD, format!("Step B ReplyFromAltPort result: {alt_port_ok}"));
                        }

                        if input.helper_v4_p1.is_some() {
                            helper_ip_ok = run_filtering_probe(
                                &v4_sock,
                                p1_addr,
                                EchoMode::ReplyFromHelperIp,
                                &input.token,
                                input.timeout_ms,
                                &cancel,
                            )
                            .await?;
                            info(
                                LOG_MOD,
                                format!("Step C ReplyFromHelperIp result: {helper_ip_ok}"),
                            );
                        }
                    } else {
                        warn(
                            LOG_MOD,
                            format!("primary_v4_p1 ({p1_addr}) timed out; skipping filtering tests"),
                        );
                        notes.push("primary_v4_p1 did not respond; filtering tests skipped".into());
                    }
                }

                if count_distinct_ips(&v4_observations) < 2 {
                    run_step_d_supplement(
                        &v4_sock,
                        &endpoints,
                        &input.token,
                        input.timeout_ms,
                        &cancel,
                        &mut v4_observations,
                        &mut notes,
                    )
                    .await?;
                }
            }
            Err(e) => {
                error(LOG_MOD, format!("Failed to bind IPv4 UDP socket: {e}"));
                notes.push(format!("IPv4 probe failed (socket bind error: {e})"));
            }
        }
    }

    let public_ipv6 = run_ipv6_probe(
        &endpoints,
        &input.token,
        input.timeout_ms,
        &cancel,
        &mut notes,
    )
    .await?;

    let report = classify_nat_report(ClassificationInput {
        input,
        v4_observations,
        p1_observed,
        alt_port_ok,
        helper_ip_ok,
        public_ipv6,
        notes,
    });

    info(
        LOG_MOD,
        format!(
            "NAT probe complete: status={:?}, mapping={:?}, filtering={:?}, difficulty={}, ipv6_reachable={}",
            report.status,
            report.mapping,
            report.filtering,
            report.estimated_difficulty,
            report.ipv6_reachable,
        ),
    );

    Ok(report)
}

fn validate_input(input: &NatTaskInput) -> Result<(), ProbeError> {
    if input.timeout_ms == 0 {
        warn(LOG_MOD, "Validation failed: timeout_ms must be > 0");
        return Err(ProbeError::InvalidInput("timeout_ms must be > 0".into()));
    }
    if input.public_stun_servers.is_empty()
        && input.primary_v4_p1.is_none()
        && input.primary_v6.is_none()
    {
        warn(LOG_MOD, "Validation failed: no probe endpoints configured");
        return Err(ProbeError::InvalidInput(
            "no probe endpoints configured".into(),
        ));
    }
    if input.primary_v4_p2.is_some() && input.primary_v4_p1.is_none() {
        warn(
            LOG_MOD,
            "Validation failed: primary_v4_p2 requires primary_v4_p1",
        );
        return Err(ProbeError::InvalidInput(
            "primary_v4_p2 requires primary_v4_p1".into(),
        ));
    }
    if input.helper_v4_p1.is_some() && input.primary_v4_p1.is_none() {
        warn(
            LOG_MOD,
            "Validation failed: helper_v4_p1 requires primary_v4_p1",
        );
        return Err(ProbeError::InvalidInput(
            "helper_v4_p1 requires primary_v4_p1".into(),
        ));
    }
    Ok(())
}

struct ResolvedEndpoints {
    v4_stun: Vec<(String, SocketAddr)>,
    v6_stun: Vec<(String, SocketAddr)>,
    primary_v4_p1: Option<SocketAddr>,
    primary_v4_p2: Option<SocketAddr>,
    helper_v4_p1: Option<SocketAddr>,
    primary_v6: Option<SocketAddr>,
    stun_dns_failures: usize,
    notes: Vec<String>,
}

async fn resolve_host_bounded(
    target: String,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<Result<Vec<SocketAddr>, String>, ProbeError> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Ok(Err("empty endpoint string".into()));
    }
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Ok(Ok(vec![addr]));
    }

    tokio::select! {
        _ = cancel.cancelled() => Err(ProbeError::Cancelled),
        res = time::timeout(timeout, lookup_host(trimmed)) => {
            match res {
                Ok(Ok(addrs)) => {
                    let list: Vec<SocketAddr> = addrs.collect();
                    if list.is_empty() {
                        Ok(Err("resolved to zero addresses".into()))
                    } else {
                        Ok(Ok(list))
                    }
                }
                Ok(Err(e)) => Ok(Err(format!("dns lookup failed: {e}"))),
                Err(_) => Ok(Err("dns lookup timed out".into())),
            }
        }
    }
}

async fn resolve_optional_endpoint(
    role: &str,
    raw: &Option<String>,
    want_v4: bool,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(Option<SocketAddr>, Option<String>), ProbeError> {
    let Some(s) = raw else {
        return Ok((None, None));
    };
    match resolve_host_bounded(s.clone(), timeout, cancel.clone()).await? {
        Ok(addrs) => {
            let picked = addrs.into_iter().find(|a| a.is_ipv4() == want_v4);
            if picked.is_none() {
                let family = if want_v4 { "IPv4" } else { "IPv6" };
                warn(
                    LOG_MOD,
                    format!("{role} ({s}) resolved without any {family} address"),
                );
                return Ok((
                    None,
                    Some(format!("{role} unreachable (no {family} address resolved)")),
                ));
            }
            Ok((picked, None))
        }
        Err(reason) => {
            warn(LOG_MOD, format!("{role} ({s}) resolution failed: {reason}"));
            Ok((None, Some(format!("{role} unreachable ({reason})"))))
        }
    }
}

async fn resolve_all_endpoints(
    input: &NatTaskInput,
    cancel: &CancellationToken,
) -> Result<ResolvedEndpoints, ProbeError> {
    let dns_timeout = Duration::from_millis(
        input
            .timeout_ms
            .clamp(DNS_TIMEOUT_MIN_MS, DNS_TIMEOUT_MAX_MS),
    );

    let mut stun_handles = Vec::with_capacity(input.public_stun_servers.len());
    for server in &input.public_stun_servers {
        let server_clone = server.clone();
        let cancel_clone = cancel.clone();
        stun_handles.push((
            server.clone(),
            tokio::spawn(async move {
                resolve_host_bounded(server_clone, dns_timeout, cancel_clone).await
            }),
        ));
    }

    let (p1_res, p2_res, helper_res, v6_res) = tokio::join!(
        resolve_optional_endpoint(
            "primary_v4_p1",
            &input.primary_v4_p1,
            true,
            dns_timeout,
            cancel,
        ),
        resolve_optional_endpoint(
            "primary_v4_p2",
            &input.primary_v4_p2,
            true,
            dns_timeout,
            cancel,
        ),
        resolve_optional_endpoint(
            "helper_v4_p1",
            &input.helper_v4_p1,
            true,
            dns_timeout,
            cancel,
        ),
        resolve_optional_endpoint("primary_v6", &input.primary_v6, false, dns_timeout, cancel),
    );

    let mut notes = Vec::new();
    let (primary_v4_p1, p1_note) = p1_res?;
    let (primary_v4_p2, p2_note) = p2_res?;
    let (helper_v4_p1, helper_note) = helper_res?;
    let (primary_v6, v6_note) = v6_res?;
    for note in [p1_note, p2_note, helper_note, v6_note].into_iter().flatten() {
        notes.push(note);
    }

    let mut v4_stun = Vec::new();
    let mut v6_stun = Vec::new();
    let mut seen_v4_addrs: HashSet<SocketAddr> = HashSet::new();
    let mut seen_v6_addrs: HashSet<SocketAddr> = HashSet::new();
    let mut stun_dns_failures = 0usize;

    for (server_label, mut handle) in stun_handles {
        let joined = tokio::select! {
            _ = cancel.cancelled() => {
                handle.abort();
                return Err(ProbeError::Cancelled);
            }
            res = &mut handle => res,
        };
        match joined {
            Ok(Ok(Ok(addrs))) => {
                for addr in addrs {
                    match addr {
                        SocketAddr::V4(_) => {
                            if seen_v4_addrs.insert(addr) {
                                v4_stun.push((server_label.clone(), addr));
                            }
                        }
                        SocketAddr::V6(_) => {
                            if seen_v6_addrs.insert(addr) {
                                v6_stun.push((server_label.clone(), addr));
                            }
                        }
                    }
                }
            }
            Ok(Ok(Err(reason))) => {
                stun_dns_failures += 1;
                warn(
                    LOG_MOD,
                    format!("STUN server {server_label} resolution failed: {reason}"),
                );
            }
            Ok(Err(ProbeError::Cancelled)) => return Err(ProbeError::Cancelled),
            Ok(Err(e)) => return Err(e),
            Err(join_err) => {
                stun_dns_failures += 1;
                warn(
                    LOG_MOD,
                    format!("STUN server {server_label} resolver task error: {join_err}"),
                );
            }
        }
    }

    Ok(ResolvedEndpoints {
        v4_stun,
        v6_stun,
        primary_v4_p1,
        primary_v4_p2,
        helper_v4_p1,
        primary_v6,
        stun_dns_failures,
        notes,
    })
}

async fn bind_udp(
    addr: SocketAddr,
    cancel: &CancellationToken,
) -> Result<std::io::Result<UdpSocket>, ProbeError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(ProbeError::Cancelled),
        res = UdpSocket::bind(addr) => Ok(res),
    }
}

async fn send_udp(
    sock: &UdpSocket,
    payload: &[u8],
    target: SocketAddr,
    label: &str,
    cancel: &CancellationToken,
) -> Result<bool, ProbeError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(ProbeError::Cancelled),
        res = sock.send_to(payload, target) => match res {
            Ok(_) => Ok(true),
            Err(e) => {
                warn(LOG_MOD, format!("UDP send_to {label} ({target}) failed: {e}"));
                Ok(false)
            }
        }
    }
}

fn encode_addr_echo_req(
    nonce: u64,
    token: &Option<String>,
    mode: EchoMode,
) -> Result<Vec<u8>, ProbeError> {
    let msg = S2WireMessage::AddrEchoReq(AddrEchoReq {
        nonce,
        token: token.clone(),
        mode,
    });
    let bytes = serde_json::to_vec(&msg).map_err(|e| {
        error(LOG_MOD, format!("Failed to serialize AddrEchoReq: {e}"));
        ProbeError::Internal(format!("serialize AddrEchoReq: {e}"))
    })?;
    if bytes.len() > MAX_UDP_PACKET_BYTES {
        warn(
            LOG_MOD,
            format!(
                "AddrEchoReq payload len {} exceeds MAX_UDP_PACKET_BYTES ({})",
                bytes.len(),
                MAX_UDP_PACKET_BYTES
            ),
        );
    }
    Ok(bytes)
}

#[derive(Debug, Clone)]
struct V4Observation {
    label: String,
    endpoint: SocketAddr,
    mapped: SocketAddr,
}

fn is_valid_v4_mapped(mapped: SocketAddr) -> bool {
    mapped.is_ipv4() && mapped.port() != 0 && !mapped.ip().is_unspecified()
}

fn record_v4_observation(
    obs: &mut Vec<V4Observation>,
    label: String,
    endpoint: SocketAddr,
    mapped: SocketAddr,
) {
    if !is_valid_v4_mapped(mapped) {
        warn(
            LOG_MOD,
            format!("Ignoring invalid v4 mapped address {mapped} from {label} ({endpoint})"),
        );
        return;
    }
    if !obs.iter().any(|existing| existing.endpoint == endpoint) {
        obs.push(V4Observation {
            label,
            endpoint,
            mapped,
        });
    }
}

fn count_distinct_ips(obs: &[V4Observation]) -> usize {
    obs.iter()
        .map(|o| o.endpoint.ip())
        .collect::<HashSet<IpAddr>>()
        .len()
}

fn is_transient_recv_err(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
    )
}

struct StepAResult {
    v4_observations: Vec<V4Observation>,
    p1_observed: Option<SocketAddr>,
    stun_responded_count: usize,
}

async fn run_step_a_baseline(
    sock: &UdpSocket,
    endpoints: &ResolvedEndpoints,
    token: &Option<String>,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<StepAResult, ProbeError> {
    info(
        LOG_MOD,
        format!(
            "Step A baseline: querying {} v4 STUN server(s) and primary_v4_p1={}",
            endpoints.v4_stun.len(),
            endpoints.primary_v4_p1.is_some()
        ),
    );

    let mut pending_stun: HashMap<[u8; 12], (String, SocketAddr, [u8; 20])> = HashMap::new();
    for (label, addr) in &endpoints.v4_stun {
        let txn_id = next_txn_id();
        let pkt = build_stun_binding_request(txn_id);
        if send_udp(sock, &pkt, *addr, label, cancel).await? {
            pending_stun.insert(txn_id, (label.clone(), *addr, pkt));
        }
    }

    let p1_nonce = next_nonce();
    let p1_payload = if endpoints.primary_v4_p1.is_some() {
        Some(encode_addr_echo_req(p1_nonce, token, EchoMode::Normal)?)
    } else {
        None
    };

    let mut p1_pending = false;
    if let (Some(p1_addr), Some(payload)) = (endpoints.primary_v4_p1, &p1_payload) {
        p1_pending = send_udp(sock, payload, p1_addr, "primary_v4_p1", cancel).await?;
    }

    let mut v4_observations = Vec::new();
    let mut p1_observed = None;
    let mut stun_responded_count = 0usize;

    if pending_stun.is_empty() && !p1_pending {
        return Ok(StepAResult {
            v4_observations,
            p1_observed,
            stun_responded_count,
        });
    }

    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms);
    let can_retry = timeout_ms >= 2;
    let retry_at = start + Duration::from_millis((timeout_ms / 2).max(1));
    let mut retried = !can_retry;

    let retry_sleep = time::sleep_until(retry_at);
    let deadline_sleep = time::sleep_until(deadline);
    tokio::pin!(retry_sleep);
    tokio::pin!(deadline_sleep);

    let mut buf = [0u8; 1500];

    while !pending_stun.is_empty() || p1_pending {
        if cancel.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                info(LOG_MOD, "NAT probe cancelled during Step A");
                return Err(ProbeError::Cancelled);
            }
            _ = &mut deadline_sleep => {
                break;
            }
            _ = &mut retry_sleep, if !retried => {
                retried = true;
                for (label, addr, pkt) in pending_stun.values() {
                    let _ = send_udp(sock, pkt, *addr, label, cancel).await?;
                }
                if p1_pending {
                    if let (Some(p1_addr), Some(payload)) = (endpoints.primary_v4_p1, &p1_payload) {
                        let _ = send_udp(sock, payload, p1_addr, "primary_v4_p1", cancel).await?;
                    }
                }
            }
            recv_res = sock.recv_from(&mut buf) => {
                match recv_res {
                    Ok((len, src)) => {
                        let pkt = &buf[..len];
                        if len >= 20 && pkt[4..8] == STUN_MAGIC_COOKIE_BYTES {
                            let mut txn_id = [0u8; 12];
                            txn_id.copy_from_slice(&pkt[8..20]);
                            if let Some((label, target_addr, _)) = pending_stun.get(&txn_id).cloned() {
                                match parse_stun_binding_response(pkt, &txn_id) {
                                    Some(mapped) if is_valid_v4_mapped(mapped) => {
                                        pending_stun.remove(&txn_id);
                                        stun_responded_count += 1;
                                        info(
                                            LOG_MOD,
                                            format!("Step A STUN response from {label} ({src}): mapped={mapped}"),
                                        );
                                        record_v4_observation(
                                            &mut v4_observations,
                                            label,
                                            target_addr,
                                            mapped,
                                        );
                                    }
                                    Some(mapped) => {
                                        warn(
                                            LOG_MOD,
                                            format!("Ignoring invalid v4 STUN mapped address {mapped} from {src}"),
                                        );
                                    }
                                    None => {
                                        warn(LOG_MOD, format!("Malformed STUN response from {src}"));
                                    }
                                }
                            } else {
                                warn(LOG_MOD, format!("STUN packet with unexpected txn_id from {src}"));
                            }
                        } else if let Ok(S2WireMessage::AddrEchoResp(resp)) =
                            serde_json::from_slice::<S2WireMessage>(pkt)
                        {
                            if p1_pending && resp.nonce == p1_nonce {
                                match resp.observed_addr.parse::<SocketAddr>() {
                                    Ok(mapped) if is_valid_v4_mapped(mapped) => {
                                        p1_pending = false;
                                        p1_observed = Some(mapped);
                                        info(
                                            LOG_MOD,
                                            format!("Step A primary_v4_p1 response from {src}: observed={mapped}"),
                                        );
                                        if let Some(p1_addr) = endpoints.primary_v4_p1 {
                                            record_v4_observation(
                                                &mut v4_observations,
                                                "primary_v4_p1".into(),
                                                p1_addr,
                                                mapped,
                                            );
                                        }
                                    }
                                    Ok(mapped) => {
                                        warn(
                                            LOG_MOD,
                                            format!("Ignoring invalid v4 observed_addr {mapped} from {src}"),
                                        );
                                    }
                                    Err(e) => {
                                        warn(
                                            LOG_MOD,
                                            format!("Invalid observed_addr '{}' from {src}: {e}", resp.observed_addr),
                                        );
                                    }
                                }
                            } else {
                                warn(
                                    LOG_MOD,
                                    format!("Ignoring AddrEchoResp with mismatched/late nonce from {src}"),
                                );
                            }
                        } else {
                            warn(
                                LOG_MOD,
                                format!("Ignoring unrecognized UDP packet ({len} bytes) from {src} in Step A"),
                            );
                        }
                    }
                    Err(e) if is_transient_recv_err(&e) => {
                        warn(LOG_MOD, format!("Transient UDP recv error in Step A (continuing): {e}"));
                    }
                    Err(e) => {
                        warn(LOG_MOD, format!("UDP recv error in Step A: {e}"));
                        break;
                    }
                }
            }
        }
    }

    Ok(StepAResult {
        v4_observations,
        p1_observed,
        stun_responded_count,
    })
}

async fn run_s2_echo_probe(
    sock: &UdpSocket,
    target: SocketAddr,
    label: &str,
    mode: EchoMode,
    token: &Option<String>,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<Option<AddrEchoResp>, ProbeError> {
    let nonce = next_nonce();
    let payload = encode_addr_echo_req(nonce, token, mode)?;

    if !send_udp(sock, &payload, target, label, cancel).await? {
        return Ok(None);
    }

    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms);
    let can_retry = timeout_ms >= 2;
    let retry_at = start + Duration::from_millis((timeout_ms / 2).max(1));
    let mut retried = !can_retry;

    let retry_sleep = time::sleep_until(retry_at);
    let deadline_sleep = time::sleep_until(deadline);
    tokio::pin!(retry_sleep);
    tokio::pin!(deadline_sleep);

    let mut buf = [0u8; 1500];

    loop {
        if cancel.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                info(LOG_MOD, format!("NAT probe cancelled during {label}"));
                return Err(ProbeError::Cancelled);
            }
            _ = &mut deadline_sleep => {
                return Ok(None);
            }
            _ = &mut retry_sleep, if !retried => {
                retried = true;
                let _ = send_udp(sock, &payload, target, label, cancel).await?;
            }
            recv_res = sock.recv_from(&mut buf) => {
                match recv_res {
                    Ok((len, src)) => {
                        let pkt = &buf[..len];
                        match serde_json::from_slice::<S2WireMessage>(pkt) {
                            Ok(S2WireMessage::AddrEchoResp(resp)) if resp.nonce == nonce => {
                                info(
                                    LOG_MOD,
                                    format!("{label} response received from {src}: observed={}", resp.observed_addr),
                                );
                                return Ok(Some(resp));
                            }
                            Ok(_) => {
                                warn(
                                    LOG_MOD,
                                    format!("Ignoring non-matching S2 message from {src} during {label}"),
                                );
                            }
                            Err(_) => {
                                warn(
                                    LOG_MOD,
                                    format!("Ignoring non-S2 packet ({len} bytes) from {src} during {label}"),
                                );
                            }
                        }
                    }
                    Err(e) if is_transient_recv_err(&e) => {
                        warn(LOG_MOD, format!("Transient UDP recv error in {label} (continuing): {e}"));
                    }
                    Err(e) => {
                        warn(LOG_MOD, format!("UDP recv error in {label}: {e}"));
                        return Ok(None);
                    }
                }
            }
        }
    }
}

async fn run_filtering_probe(
    sock: &UdpSocket,
    p1_addr: SocketAddr,
    mode: EchoMode,
    token: &Option<String>,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<bool, ProbeError> {
    let label = match mode {
        EchoMode::ReplyFromAltPort => "Step B ReplyFromAltPort",
        EchoMode::ReplyFromHelperIp => "Step C ReplyFromHelperIp",
        EchoMode::Normal => "NormalEcho",
    };
    let resp = run_s2_echo_probe(sock, p1_addr, label, mode, token, timeout_ms, cancel).await?;
    Ok(resp.is_some())
}

async fn run_step_d_supplement(
    sock: &UdpSocket,
    endpoints: &ResolvedEndpoints,
    token: &Option<String>,
    timeout_ms: u64,
    cancel: &CancellationToken,
    v4_observations: &mut Vec<V4Observation>,
    notes: &mut Vec<String>,
) -> Result<(), ProbeError> {
    info(
        LOG_MOD,
        format!(
            "Step D fallback: distinct IPv4 endpoint IPs={} (< 2); supplementing mapping observations",
            count_distinct_ips(v4_observations)
        ),
    );

    if let Some(p2_addr) = endpoints.primary_v4_p2 {
        if !v4_observations.iter().any(|o| o.endpoint == p2_addr) {
            match run_s2_echo_probe(
                sock,
                p2_addr,
                "Step D primary_v4_p2 Normal",
                EchoMode::Normal,
                token,
                timeout_ms,
                cancel,
            )
            .await?
            {
                Some(resp) => match resp.observed_addr.parse::<SocketAddr>() {
                    Ok(mapped) => record_v4_observation(
                        v4_observations,
                        "primary_v4_p2".into(),
                        p2_addr,
                        mapped,
                    ),
                    Err(e) => warn(
                        LOG_MOD,
                        format!("Invalid observed_addr '{}' from primary_v4_p2: {e}", resp.observed_addr),
                    ),
                },
                None => {
                    warn(LOG_MOD, format!("Step D primary_v4_p2 ({p2_addr}) did not respond"));
                    notes.push("primary_v4_p2 did not respond to Normal mapping probe".into());
                }
            }
        }
    }

    if count_distinct_ips(v4_observations) < 2 {
        if let Some(helper_addr) = endpoints.helper_v4_p1 {
            if !v4_observations.iter().any(|o| o.endpoint == helper_addr) {
                match run_s2_echo_probe(
                    sock,
                    helper_addr,
                    "Step D helper_v4_p1 Normal",
                    EchoMode::Normal,
                    token,
                    timeout_ms,
                    cancel,
                )
                .await?
                {
                    Some(resp) => match resp.observed_addr.parse::<SocketAddr>() {
                        Ok(mapped) => record_v4_observation(
                            v4_observations,
                            "helper_v4_p1".into(),
                            helper_addr,
                            mapped,
                        ),
                        Err(e) => warn(
                            LOG_MOD,
                            format!(
                                "Invalid observed_addr '{}' from helper_v4_p1: {e}",
                                resp.observed_addr
                            ),
                        ),
                    },
                    None => {
                        warn(
                            LOG_MOD,
                            format!("Step D helper_v4_p1 ({helper_addr}) did not respond"),
                        );
                        notes.push("helper_v4_p1 did not respond to Normal mapping probe".into());
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_ipv6_probe(
    endpoints: &ResolvedEndpoints,
    token: &Option<String>,
    timeout_ms: u64,
    cancel: &CancellationToken,
    notes: &mut Vec<String>,
) -> Result<Option<String>, ProbeError> {
    if endpoints.primary_v6.is_none() && endpoints.v6_stun.is_empty() {
        return Ok(None);
    }

    let v6_sock = match bind_udp(
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        cancel,
    )
    .await?
    {
        Ok(sock) => sock,
        Err(e) => {
            info(
                LOG_MOD,
                format!("IPv6 UDP socket bind failed (host may lack IPv6): {e}"),
            );
            notes.push("IPv6 unavailable (socket bind failed)".into());
            return Ok(None);
        }
    };

    if let Some(v6_addr) = endpoints.primary_v6 {
        info(LOG_MOD, format!("Probing primary_v6 ({v6_addr})"));
        if let Some(resp) = run_s2_echo_probe(
            &v6_sock,
            v6_addr,
            "IPv6 primary_v6",
            EchoMode::Normal,
            token,
            timeout_ms,
            cancel,
        )
        .await?
        {
            match resp.observed_addr.parse::<SocketAddr>() {
                Ok(SocketAddr::V6(sa)) if sa.port() != 0 && !sa.ip().is_unspecified() => {
                    notes.push("IPv6 reachable via primary_v6".into());
                    return Ok(Some(sa.ip().to_string()));
                }
                Ok(other) => {
                    warn(
                        LOG_MOD,
                        format!("Unexpected observed_addr {other} from primary_v6"),
                    );
                }
                Err(e) => {
                    warn(
                        LOG_MOD,
                        format!("Invalid observed_addr '{}' from primary_v6: {e}", resp.observed_addr),
                    );
                }
            }
        } else {
            warn(LOG_MOD, format!("primary_v6 ({v6_addr}) did not respond"));
            notes.push("primary_v6 did not respond".into());
        }
    }

    if !endpoints.v6_stun.is_empty() {
        info(
            LOG_MOD,
            format!(
                "Probing {} IPv6 public STUN server(s)",
                endpoints.v6_stun.len()
            ),
        );
        if let Some(v6_ip) =
            run_ipv6_stun_probe(&v6_sock, &endpoints.v6_stun, timeout_ms, cancel).await?
        {
            notes.push("IPv6 reachable via public STUN".into());
            return Ok(Some(v6_ip));
        }
        notes.push("IPv6 public STUN servers unreachable".into());
    }

    Ok(None)
}

async fn run_ipv6_stun_probe(
    sock: &UdpSocket,
    targets: &[(String, SocketAddr)],
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<Option<String>, ProbeError> {
    let mut pending: HashMap<[u8; 12], (String, SocketAddr, [u8; 20])> = HashMap::new();
    for (label, addr) in targets {
        let txn_id = next_txn_id();
        let pkt = build_stun_binding_request(txn_id);
        if send_udp(sock, &pkt, *addr, label, cancel).await? {
            pending.insert(txn_id, (label.clone(), *addr, pkt));
        }
    }

    if pending.is_empty() {
        info(LOG_MOD, "All IPv6 STUN sends failed (host may lack IPv6 route)");
        return Ok(None);
    }

    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms);
    let can_retry = timeout_ms >= 2;
    let retry_at = start + Duration::from_millis((timeout_ms / 2).max(1));
    let mut retried = !can_retry;

    let retry_sleep = time::sleep_until(retry_at);
    let deadline_sleep = time::sleep_until(deadline);
    tokio::pin!(retry_sleep);
    tokio::pin!(deadline_sleep);

    let mut buf = [0u8; 1500];

    loop {
        if cancel.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                info(LOG_MOD, "NAT probe cancelled during IPv6 STUN probe");
                return Err(ProbeError::Cancelled);
            }
            _ = &mut deadline_sleep => {
                return Ok(None);
            }
            _ = &mut retry_sleep, if !retried => {
                retried = true;
                for (label, addr, pkt) in pending.values() {
                    let _ = send_udp(sock, pkt, *addr, label, cancel).await?;
                }
            }
            recv_res = sock.recv_from(&mut buf) => {
                match recv_res {
                    Ok((len, src)) => {
                        let pkt = &buf[..len];
                        if len >= 20 && pkt[4..8] == STUN_MAGIC_COOKIE_BYTES {
                            let mut txn_id = [0u8; 12];
                            txn_id.copy_from_slice(&pkt[8..20]);
                            if pending.contains_key(&txn_id) {
                                if let Some(SocketAddr::V6(sa)) = parse_stun_binding_response(pkt, &txn_id) {
                                    if sa.port() != 0 && !sa.ip().is_unspecified() {
                                        info(LOG_MOD, format!("IPv6 STUN response from {src}: mapped={sa}"));
                                        return Ok(Some(sa.ip().to_string()));
                                    }
                                }
                                warn(LOG_MOD, format!("Ignoring invalid/non-v6 STUN response from {src}"));
                            }
                        } else {
                            warn(LOG_MOD, format!("Ignoring non-STUN packet ({len} bytes) on v6 socket from {src}"));
                        }
                    }
                    Err(e) if is_transient_recv_err(&e) => {
                        warn(LOG_MOD, format!("Transient UDP recv error on v6 socket (continuing): {e}"));
                    }
                    Err(e) => {
                        warn(LOG_MOD, format!("UDP recv error on v6 socket: {e}"));
                        return Ok(None);
                    }
                }
            }
        }
    }
}

struct ClassificationInput<'a> {
    input: &'a NatTaskInput,
    v4_observations: Vec<V4Observation>,
    p1_observed: Option<SocketAddr>,
    alt_port_ok: bool,
    helper_ip_ok: bool,
    public_ipv6: Option<String>,
    notes: Vec<String>,
}

fn classify_nat_report(ctx: ClassificationInput<'_>) -> NatReport {
    let ClassificationInput {
        input,
        v4_observations,
        p1_observed,
        alt_port_ok,
        helper_ip_ok,
        public_ipv6,
        mut notes,
    } = ctx;

    let ipv6_reachable = public_ipv6.is_some();

    let public_ipv4 = p1_observed
        .or_else(|| v4_observations.first().map(|o| o.mapped))
        .map(|sa| sa.ip().to_string());

    if v4_observations.is_empty() && !ipv6_reachable {
        notes.push("No IPv4 or IPv6 probe endpoints responded".into());
        return NatReport {
            status: NatStatus::Unreachable,
            public_ipv4: None,
            public_ipv6: None,
            ipv6_reachable: false,
            mapping: NatMapping::Unknown,
            filtering: NatFiltering::Unknown,
            estimated_difficulty: "unreachable".into(),
            notes,
        };
    }

    let distinct_ips = count_distinct_ips(&v4_observations);
    let mapping = if distinct_ips >= 2 {
        let first = v4_observations[0].mapped;
        if v4_observations.iter().all(|o| o.mapped == first) {
            NatMapping::EndpointIndependent
        } else {
            NatMapping::AddressOrPortDependentSymmetric
        }
    } else if v4_observations.len() >= 2 {
        notes.push(
            "Same-IP-only mapping fallback: public STUN/helper endpoints unreachable or unconfigured; address-dependent mapping cannot be ruled out".into(),
        );
        let first = v4_observations[0].mapped;
        if v4_observations.iter().all(|o| o.mapped == first) {
            NatMapping::EndpointIndependent
        } else {
            NatMapping::AddressOrPortDependentSymmetric
        }
    } else {
        if v4_observations.len() == 1 {
            notes.push("Only 1 IPv4 endpoint responded; cannot determine NAT mapping".into());
        } else {
            notes.push("No IPv4 endpoints responded; IPv6 only".into());
        }
        NatMapping::Unknown
    };

    let p1_responded = p1_observed.is_some();
    let (status, filtering) = if p1_responded
        && input.primary_v4_p2.is_some()
        && input.helper_v4_p1.is_some()
    {
        let filtering = if helper_ip_ok {
            NatFiltering::FullConeNat1
        } else if alt_port_ok {
            NatFiltering::AddressRestrictedNat2
        } else {
            NatFiltering::PortRestrictedNat3
        };
        (NatStatus::CompleteFull, filtering)
    } else if p1_responded && input.primary_v4_p2.is_some() {
        let filtering = if alt_port_ok {
            NatFiltering::ConeNat1OrNat2
        } else {
            NatFiltering::PortRestrictedNat3
        };
        (NatStatus::CompleteSingleVps, filtering)
    } else {
        let filtering = if v4_observations.is_empty() {
            NatFiltering::Unknown
        } else {
            NatFiltering::NotTestedPublicStun
        };
        (NatStatus::PublicStunOnly, filtering)
    };

    let estimated_difficulty = if v4_observations.is_empty() && ipv6_reachable {
        "ipv6_only"
    } else {
        match mapping {
            NatMapping::AddressOrPortDependentSymmetric => "hard (NAT4 Symmetric)",
            NatMapping::EndpointIndependent => match filtering {
                NatFiltering::FullConeNat1 => "easy (NAT1 Full Cone)",
                NatFiltering::ConeNat1OrNat2 => "easy (NAT1/NAT2 Cone)",
                NatFiltering::AddressRestrictedNat2 => "moderate (NAT2 Address-Restricted)",
                NatFiltering::PortRestrictedNat3 => "moderate-hard (NAT3 Port-Restricted)",
                NatFiltering::NotTestedPublicStun | NatFiltering::Unknown => {
                    "cone (NAT1-3, filtering untested)"
                }
            },
            NatMapping::Unknown => "unknown",
        }
    }
    .to_string();

    if !v4_observations.is_empty() {
        let summary = v4_observations
            .iter()
            .map(|o| format!("{}={}", o.label, o.mapped))
            .collect::<Vec<_>>()
            .join(", ");
        notes.push(format!("Observed IPv4 mappings: {summary}"));
    }

    NatReport {
        status,
        public_ipv4,
        public_ipv6,
        ipv6_reachable,
        mapping,
        filtering,
        estimated_difficulty,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio test runtime")
            .block_on(fut)
    }

    #[test]
    fn stun_codec_roundtrip_v4_and_v6() {
        let txn_id = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let req = build_stun_binding_request(txn_id);
        assert_eq!(parse_stun_binding_request(&req), Some(txn_id));

        let v4_mapped: SocketAddr = "203.0.113.42:54321".parse().unwrap();
        let v4_resp = build_stun_binding_response(txn_id, v4_mapped);
        assert_eq!(parse_stun_binding_response(&v4_resp, &txn_id), Some(v4_mapped));

        let v6_mapped: SocketAddr = "[2001:db8::1234:5678]:43210".parse().unwrap();
        let v6_resp = build_stun_binding_response(txn_id, v6_mapped);
        assert_eq!(parse_stun_binding_response(&v6_resp, &txn_id), Some(v6_mapped));

        let wrong_txn = [99u8; 12];
        assert_eq!(parse_stun_binding_response(&v4_resp, &wrong_txn), None);
    }

    #[test]
    fn stun_response_prefers_xor_mapped_and_handles_padding() {
        let txn_id = [0xAA; 12];
        let mut pkt = Vec::new();
        // Header: type=0x0101, placeholder len, magic cookie, txn_id
        pkt.extend_from_slice(&STUN_BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&STUN_MAGIC_COOKIE_BYTES);
        pkt.extend_from_slice(&txn_id);

        // SOFTWARE attr (0x8022) len=5 ("abcde") + 3 padding bytes
        pkt.extend_from_slice(&0x8022u16.to_be_bytes());
        pkt.extend_from_slice(&5u16.to_be_bytes());
        pkt.extend_from_slice(b"abcde\0\0\0");

        // MAPPED-ADDRESS (0x0001): 198.51.100.1:1111
        pkt.extend_from_slice(&ATTR_MAPPED_ADDRESS.to_be_bytes());
        pkt.extend_from_slice(&8u16.to_be_bytes());
        pkt.extend_from_slice(&[0x00, FAMILY_IPV4]);
        pkt.extend_from_slice(&1111u16.to_be_bytes());
        pkt.extend_from_slice(&[198, 51, 100, 1]);

        // Test fallback when only MAPPED-ADDRESS is present
        let msg_len_fallback = (pkt.len() - 20) as u16;
        pkt[2..4].copy_from_slice(&msg_len_fallback.to_be_bytes());
        assert_eq!(
            parse_stun_binding_response(&pkt, &txn_id),
            Some("198.51.100.1:1111".parse().unwrap())
        );

        // Append XOR-MAPPED-ADDRESS (0x0020): 203.0.113.9:2222
        let xor_resp = build_stun_binding_response(txn_id, "203.0.113.9:2222".parse().unwrap());
        pkt.extend_from_slice(&xor_resp[20..]);
        let msg_len_both = (pkt.len() - 20) as u16;
        pkt[2..4].copy_from_slice(&msg_len_both.to_be_bytes());

        assert_eq!(
            parse_stun_binding_response(&pkt, &txn_id),
            Some("203.0.113.9:2222".parse().unwrap())
        );
    }

    #[test]
    fn validation_rejects_invalid_inputs() {
        block_on(async {
            let cancel = CancellationToken::new();

            let zero_timeout = NatTaskInput {
                timeout_ms: 0,
                ..Default::default()
            };
            assert!(matches!(
                run_nat_probe(&zero_timeout, cancel.clone()).await,
                Err(ProbeError::InvalidInput(_))
            ));

            let empty = NatTaskInput {
                public_stun_servers: vec![],
                primary_v4_p1: None,
                primary_v4_p2: None,
                primary_v6: None,
                helper_v4_p1: None,
                token: None,
                timeout_ms: 500,
            };
            assert!(matches!(
                run_nat_probe(&empty, cancel.clone()).await,
                Err(ProbeError::InvalidInput(_))
            ));

            let p2_without_p1 = NatTaskInput {
                public_stun_servers: vec!["127.0.0.1:3478".into()],
                primary_v4_p1: None,
                primary_v4_p2: Some("127.0.0.1:4002".into()),
                primary_v6: None,
                helper_v4_p1: None,
                token: None,
                timeout_ms: 500,
            };
            assert!(matches!(
                run_nat_probe(&p2_without_p1, cancel.clone()).await,
                Err(ProbeError::InvalidInput(_))
            ));

            let helper_without_p1 = NatTaskInput {
                public_stun_servers: vec!["127.0.0.1:3478".into()],
                primary_v4_p1: None,
                primary_v4_p2: None,
                primary_v6: None,
                helper_v4_p1: Some("127.0.0.1:4003".into()),
                token: None,
                timeout_ms: 500,
            };
            assert!(matches!(
                run_nat_probe(&helper_without_p1, cancel).await,
                Err(ProbeError::InvalidInput(_))
            ));
        });
    }

    #[test]
    fn cancellation_returns_quickly() {
        block_on(async {
            let dead_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let dead_addr = dead_sock.local_addr().unwrap();
            drop(dead_sock);

            let input = NatTaskInput {
                public_stun_servers: vec![dead_addr.to_string()],
                primary_v4_p1: None,
                primary_v4_p2: None,
                primary_v6: None,
                helper_v4_p1: None,
                token: None,
                timeout_ms: 5000,
            };

            let cancel = CancellationToken::new();
            let cancel_trigger = cancel.clone();
            tokio::spawn(async move {
                time::sleep(Duration::from_millis(30)).await;
                cancel_trigger.cancel();
            });

            let start = Instant::now();
            let res = run_nat_probe(&input, cancel).await;
            let elapsed = start.elapsed();

            assert_eq!(res, Err(ProbeError::Cancelled));
            assert!(
                elapsed < Duration::from_millis(500),
                "cancellation took {elapsed:?}"
            );
        });
    }

    #[test]
    fn single_vps_probe_enforces_pinhole_order_and_step_d_fallback() {
        block_on(async {
            let p1_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let p2_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let p1_addr = p1_sock.local_addr().unwrap();
            let p2_addr = p2_sock.local_addr().unwrap();

            let p2_saw_direct_before_step_b = Arc::new(AtomicBool::new(false));
            let step_b_triggered = Arc::new(AtomicBool::new(false));

            let p1_task_sock = p1_sock.clone();
            let p2_from_p1 = p2_sock.clone();
            let step_b_flag = step_b_triggered.clone();
            let p1_handle = tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    let Ok((len, src)) = p1_task_sock.recv_from(&mut buf).await else {
                        break;
                    };
                    if let Ok(S2WireMessage::AddrEchoReq(req)) =
                        serde_json::from_slice::<S2WireMessage>(&buf[..len])
                    {
                        let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
                            nonce: req.nonce,
                            observed_addr: src.to_string(),
                            responder_id: Some("p1".into()),
                        });
                        let payload = serde_json::to_vec(&resp).unwrap();
                        match req.mode {
                            EchoMode::Normal => {
                                let _ = p1_task_sock.send_to(&payload, src).await;
                            }
                            EchoMode::ReplyFromAltPort => {
                                step_b_flag.store(true, Ordering::SeqCst);
                                let _ = p2_from_p1.send_to(&payload, src).await;
                            }
                            EchoMode::ReplyFromHelperIp => {}
                        }
                    }
                }
            });

            let p2_task_sock = p2_sock.clone();
            let step_b_check = step_b_triggered.clone();
            let early_flag = p2_saw_direct_before_step_b.clone();
            let p2_handle = tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    let Ok((len, src)) = p2_task_sock.recv_from(&mut buf).await else {
                        break;
                    };
                    if !step_b_check.load(Ordering::SeqCst) {
                        early_flag.store(true, Ordering::SeqCst);
                    }
                    if let Ok(S2WireMessage::AddrEchoReq(req)) =
                        serde_json::from_slice::<S2WireMessage>(&buf[..len])
                    {
                        let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
                            nonce: req.nonce,
                            observed_addr: src.to_string(),
                            responder_id: Some("p2".into()),
                        });
                        let payload = serde_json::to_vec(&resp).unwrap();
                        let _ = p2_task_sock.send_to(&payload, src).await;
                    }
                }
            });

            let input = NatTaskInput {
                public_stun_servers: vec![],
                primary_v4_p1: Some(p1_addr.to_string()),
                primary_v4_p2: Some(p2_addr.to_string()),
                primary_v6: None,
                helper_v4_p1: None,
                token: Some("tok".into()),
                timeout_ms: 400,
            };

            let report = run_nat_probe(&input, CancellationToken::new())
                .await
                .unwrap();

            p1_handle.abort();
            p2_handle.abort();

            assert!(!p2_saw_direct_before_step_b.load(Ordering::SeqCst));
            assert_eq!(report.status, NatStatus::CompleteSingleVps);
            assert_eq!(report.mapping, NatMapping::EndpointIndependent);
            assert_eq!(report.filtering, NatFiltering::ConeNat1OrNat2);
            assert_eq!(report.estimated_difficulty, "easy (NAT1/NAT2 Cone)");
            assert_eq!(report.public_ipv4.as_deref(), Some("127.0.0.1"));
        });
    }

    #[test]
    fn classification_matrix_covers_all_variants() {
        let full_input = NatTaskInput {
            public_stun_servers: vec!["1.1.1.1:3478".into()],
            primary_v4_p1: Some("2.2.2.2:1000".into()),
            primary_v4_p2: Some("2.2.2.2:1001".into()),
            primary_v6: None,
            helper_v4_p1: Some("3.3.3.3:1000".into()),
            token: None,
            timeout_ms: 500,
        };

        let obs_eim = vec![
            V4Observation {
                label: "stun".into(),
                endpoint: "1.1.1.1:3478".parse().unwrap(),
                mapped: "9.9.9.9:5000".parse().unwrap(),
            },
            V4Observation {
                label: "p1".into(),
                endpoint: "2.2.2.2:1000".parse().unwrap(),
                mapped: "9.9.9.9:5000".parse().unwrap(),
            },
        ];

        let nat1 = classify_nat_report(ClassificationInput {
            input: &full_input,
            v4_observations: obs_eim.clone(),
            p1_observed: Some("9.9.9.9:5000".parse().unwrap()),
            alt_port_ok: true,
            helper_ip_ok: true,
            public_ipv6: None,
            notes: vec![],
        });
        assert_eq!(nat1.status, NatStatus::CompleteFull);
        assert_eq!(nat1.filtering, NatFiltering::FullConeNat1);
        assert_eq!(nat1.estimated_difficulty, "easy (NAT1 Full Cone)");

        let nat2 = classify_nat_report(ClassificationInput {
            input: &full_input,
            v4_observations: obs_eim.clone(),
            p1_observed: Some("9.9.9.9:5000".parse().unwrap()),
            alt_port_ok: true,
            helper_ip_ok: false,
            public_ipv6: None,
            notes: vec![],
        });
        assert_eq!(nat2.filtering, NatFiltering::AddressRestrictedNat2);
        assert_eq!(nat2.estimated_difficulty, "moderate (NAT2 Address-Restricted)");

        let nat3 = classify_nat_report(ClassificationInput {
            input: &full_input,
            v4_observations: obs_eim,
            p1_observed: Some("9.9.9.9:5000".parse().unwrap()),
            alt_port_ok: false,
            helper_ip_ok: false,
            public_ipv6: None,
            notes: vec![],
        });
        assert_eq!(nat3.filtering, NatFiltering::PortRestrictedNat3);
        assert_eq!(
            nat3.estimated_difficulty,
            "moderate-hard (NAT3 Port-Restricted)"
        );

        let obs_sym = vec![
            V4Observation {
                label: "stun".into(),
                endpoint: "1.1.1.1:3478".parse().unwrap(),
                mapped: "9.9.9.9:5000".parse().unwrap(),
            },
            V4Observation {
                label: "p1".into(),
                endpoint: "2.2.2.2:1000".parse().unwrap(),
                mapped: "9.9.9.9:5001".parse().unwrap(),
            },
        ];
        let nat4 = classify_nat_report(ClassificationInput {
            input: &full_input,
            v4_observations: obs_sym,
            p1_observed: Some("9.9.9.9:5001".parse().unwrap()),
            alt_port_ok: false,
            helper_ip_ok: false,
            public_ipv6: None,
            notes: vec![],
        });
        assert_eq!(nat4.mapping, NatMapping::AddressOrPortDependentSymmetric);
        assert_eq!(nat4.estimated_difficulty, "hard (NAT4 Symmetric)");

        let v6_only = classify_nat_report(ClassificationInput {
            input: &full_input,
            v4_observations: vec![],
            p1_observed: None,
            alt_port_ok: false,
            helper_ip_ok: false,
            public_ipv6: Some("2001:db8::1".into()),
            notes: vec![],
        });
        assert_eq!(v6_only.status, NatStatus::PublicStunOnly);
        assert_eq!(v6_only.filtering, NatFiltering::Unknown);
        assert_eq!(v6_only.estimated_difficulty, "ipv6_only");
        assert!(v6_only.ipv6_reachable);

        let unreachable = classify_nat_report(ClassificationInput {
            input: &full_input,
            v4_observations: vec![],
            p1_observed: None,
            alt_port_ok: false,
            helper_ip_ok: false,
            public_ipv6: None,
            notes: vec![],
        });
        assert_eq!(unreachable.status, NatStatus::Unreachable);
        assert_eq!(unreachable.estimated_difficulty, "unreachable");
    }

    #[test]
    fn full_vps_and_stun_full_cone_probe() {
        block_on(async {
            let stun_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let p1_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let p2_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let helper_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());

            let stun_addr = stun_sock.local_addr().unwrap();
            let p1_addr = p1_sock.local_addr().unwrap();
            let p2_addr = p2_sock.local_addr().unwrap();
            let helper_addr = helper_sock.local_addr().unwrap();

            let stun_handle = tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    let Ok((len, src)) = stun_sock.recv_from(&mut buf).await else {
                        break;
                    };
                    if let Some(txn_id) = parse_stun_binding_request(&buf[..len]) {
                        let resp = build_stun_binding_response(txn_id, src);
                        let _ = stun_sock.send_to(&resp, src).await;
                    }
                }
            });

            let p1_task_sock = p1_sock.clone();
            let p2_from_p1 = p2_sock.clone();
            let helper_from_p1 = helper_sock.clone();
            let p1_handle = tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    let Ok((len, src)) = p1_task_sock.recv_from(&mut buf).await else {
                        break;
                    };
                    if let Ok(S2WireMessage::AddrEchoReq(req)) =
                        serde_json::from_slice::<S2WireMessage>(&buf[..len])
                    {
                        let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
                            nonce: req.nonce,
                            observed_addr: src.to_string(),
                            responder_id: Some("p1".into()),
                        });
                        let payload = serde_json::to_vec(&resp).unwrap();
                        match req.mode {
                            EchoMode::Normal => {
                                let _ = p1_task_sock.send_to(&payload, src).await;
                            }
                            EchoMode::ReplyFromAltPort => {
                                let _ = p2_from_p1.send_to(&payload, src).await;
                            }
                            EchoMode::ReplyFromHelperIp => {
                                let _ = helper_from_p1.send_to(&payload, src).await;
                            }
                        }
                    }
                }
            });

            let p2_task_sock = p2_sock.clone();
            let p2_handle = tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    let Ok((len, src)) = p2_task_sock.recv_from(&mut buf).await else {
                        break;
                    };
                    if let Ok(S2WireMessage::AddrEchoReq(req)) =
                        serde_json::from_slice::<S2WireMessage>(&buf[..len])
                    {
                        let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
                            nonce: req.nonce,
                            observed_addr: src.to_string(),
                            responder_id: Some("p2".into()),
                        });
                        let payload = serde_json::to_vec(&resp).unwrap();
                        let _ = p2_task_sock.send_to(&payload, src).await;
                    }
                }
            });

            let helper_task_sock = helper_sock.clone();
            let helper_handle = tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    let Ok((len, src)) = helper_task_sock.recv_from(&mut buf).await else {
                        break;
                    };
                    if let Ok(S2WireMessage::AddrEchoReq(req)) =
                        serde_json::from_slice::<S2WireMessage>(&buf[..len])
                    {
                        let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
                            nonce: req.nonce,
                            observed_addr: src.to_string(),
                            responder_id: Some("helper".into()),
                        });
                        let payload = serde_json::to_vec(&resp).unwrap();
                        let _ = helper_task_sock.send_to(&payload, src).await;
                    }
                }
            });

            let input = NatTaskInput {
                public_stun_servers: vec![stun_addr.to_string()],
                primary_v4_p1: Some(p1_addr.to_string()),
                primary_v4_p2: Some(p2_addr.to_string()),
                primary_v6: None,
                helper_v4_p1: Some(helper_addr.to_string()),
                token: None,
                timeout_ms: 400,
            };

            let report = run_nat_probe(&input, CancellationToken::new())
                .await
                .unwrap();

            stun_handle.abort();
            p1_handle.abort();
            p2_handle.abort();
            helper_handle.abort();

            assert_eq!(report.status, NatStatus::CompleteFull);
            assert_eq!(report.mapping, NatMapping::EndpointIndependent);
            assert_eq!(report.filtering, NatFiltering::FullConeNat1);
            assert_eq!(report.estimated_difficulty, "easy (NAT1 Full Cone)");
            assert_eq!(report.public_ipv4.as_deref(), Some("127.0.0.1"));
        });
    }

    #[test]
    fn unreachable_endpoints_return_unreachable_status() {
        block_on(async {
            let dead_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let dead_addr = dead_sock.local_addr().unwrap();
            drop(dead_sock);

            let input = NatTaskInput {
                public_stun_servers: vec![dead_addr.to_string()],
                primary_v4_p1: None,
                primary_v4_p2: None,
                primary_v6: None,
                helper_v4_p1: None,
                token: None,
                timeout_ms: 80,
            };

            let report = run_nat_probe(&input, CancellationToken::new())
                .await
                .unwrap();

            assert_eq!(report.status, NatStatus::Unreachable);
            assert_eq!(report.mapping, NatMapping::Unknown);
            assert_eq!(report.filtering, NatFiltering::Unknown);
            assert_eq!(report.estimated_difficulty, "unreachable");
            assert!(!report.ipv6_reachable);
        });
    }
}
