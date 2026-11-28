use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{
    LinkProtocol, LinkSample, LinkTarget, LinkTargetReport, LinkTaskInput, MAX_LINK_ATTEMPTS,
    MAX_LINK_CONCURRENCY, MAX_LINK_TARGETS, MAX_LINK_TIMEOUT_MS, MIN_LINK_ATTEMPTS,
    MIN_LINK_CONCURRENCY, MIN_LINK_TIMEOUT_MS, ProbeError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::logger::{info, warn};

const LOG_MOD: &str = "c5";
const INTER_ATTEMPT_DELAY_MS: u64 = 50;

static CLIENT_HELLO_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn validate_link_input(input: &LinkTaskInput) -> Result<(), ProbeError> {
    if input.targets.is_empty() {
        return Err(ProbeError::InvalidInput(
            "targets must not be empty".into(),
        ));
    }
    if input.targets.len() > MAX_LINK_TARGETS {
        return Err(ProbeError::InvalidInput(format!(
            "targets count {} exceeds maximum {MAX_LINK_TARGETS}",
            input.targets.len()
        )));
    }
    if !(MIN_LINK_ATTEMPTS..=MAX_LINK_ATTEMPTS).contains(&input.attempts_per_target) {
        return Err(ProbeError::InvalidInput(format!(
            "attempts_per_target {} out of range {MIN_LINK_ATTEMPTS}..={MAX_LINK_ATTEMPTS}",
            input.attempts_per_target
        )));
    }
    if !(MIN_LINK_TIMEOUT_MS..=MAX_LINK_TIMEOUT_MS).contains(&input.timeout_ms) {
        return Err(ProbeError::InvalidInput(format!(
            "timeout_ms {} out of range {MIN_LINK_TIMEOUT_MS}..={MAX_LINK_TIMEOUT_MS}",
            input.timeout_ms
        )));
    }
    if !(MIN_LINK_CONCURRENCY..=MAX_LINK_CONCURRENCY).contains(&input.max_concurrency) {
        return Err(ProbeError::InvalidInput(format!(
            "max_concurrency {} out of range {MIN_LINK_CONCURRENCY}..={MAX_LINK_CONCURRENCY}",
            input.max_concurrency
        )));
    }

    for (idx, target) in input.targets.iter().enumerate() {
        if target.label.trim().is_empty() {
            return Err(ProbeError::InvalidInput(format!(
                "target[{idx}] label must not be empty"
            )));
        }
        let ip_raw = target.host_ip.trim();
        if ip_raw.parse::<IpAddr>().is_err() {
            return Err(ProbeError::InvalidInput(format!(
                "target[{idx}] ({}) host_ip {:?} must be an IPv4 or IPv6 literal (DNS names are disallowed)",
                target.label.trim(),
                target.host_ip
            )));
        }
        if target.port == 0 {
            return Err(ProbeError::InvalidInput(format!(
                "target[{idx}] ({}) port must be 1..=65535",
                target.label.trim()
            )));
        }
        if let LinkProtocol::TlsSni { sni } = &target.protocol {
            let trimmed = sni.trim();
            if trimmed.is_empty() || trimmed.len() > 255 {
                return Err(ProbeError::InvalidInput(format!(
                    "target[{idx}] ({}) TLS SNI must be 1..=255 bytes",
                    target.label.trim()
                )));
            }
            if !trimmed
                .bytes()
                .all(|b| b.is_ascii_graphic() && !b.is_ascii_whitespace())
            {
                return Err(ProbeError::InvalidInput(format!(
                    "target[{idx}] ({}) TLS SNI {:?} contains invalid characters",
                    target.label.trim(),
                    sni
                )));
            }
        }
    }

    Ok(())
}

pub async fn run_link_probe<F>(
    targets: &[LinkTarget],
    attempts_per_target: u32,
    timeout_ms: u64,
    max_concurrency: u32,
    cancel: CancellationToken,
    mut on_target_done: F,
) -> Result<Vec<LinkTargetReport>, ProbeError>
where
    F: FnMut(usize, usize, &LinkTargetReport),
{
    let validation_input = LinkTaskInput {
        targets: targets.to_vec(),
        attempts_per_target,
        timeout_ms,
        max_concurrency,
    };
    validate_link_input(&validation_input)?;

    let total = targets.len();
    info(
        LOG_MOD,
        format!(
            "starting link probe targets={total} attempts={attempts_per_target} timeout_ms={timeout_ms} max_concurrency={max_concurrency}"
        ),
    );

    if cancel.is_cancelled() {
        return Err(ProbeError::Cancelled);
    }

    let semaphore = Arc::new(Semaphore::new(max_concurrency as usize));
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(usize, LinkTargetReport)>();
    let mut join_set: JoinSet<Result<(usize, LinkTargetReport), ProbeError>> = JoinSet::new();

    for (idx, target) in targets.iter().cloned().enumerate() {
        let sem = Arc::clone(&semaphore);
        let target_cancel = cancel.clone();
        let tx = done_tx.clone();
        join_set.spawn(async move {
            let _permit = tokio::select! {
                _ = target_cancel.cancelled() => return Err(ProbeError::Cancelled),
                res = sem.acquire_owned() => {
                    res.map_err(|_| ProbeError::Internal("link probe semaphore closed".into()))?
                }
            };
            let report = probe_single_target(&target, attempts_per_target, timeout_ms, &target_cancel).await?;
            let _ = tx.send((idx, report.clone()));
            Ok((idx, report))
        });
    }
    drop(done_tx);

    let mut ordered_reports: Vec<Option<LinkTargetReport>> = vec![None; total];
    let mut completed_count = 0usize;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                join_set.abort_all();
                while join_set.join_next().await.is_some() {}
                warn(LOG_MOD, "link probe cancelled; aborted active target tasks");
                return Err(ProbeError::Cancelled);
            }
            maybe_done = done_rx.recv() => {
                if let Some((idx, report)) = maybe_done {
                    if ordered_reports[idx].is_none() {
                        ordered_reports[idx] = Some(report);
                        completed_count += 1;
                        if let Some(ref rep) = ordered_reports[idx] {
                            on_target_done(completed_count, total, rep);
                        }
                    }
                } else {
                    break;
                }
            }
            maybe_join = join_set.join_next() => {
                match maybe_join {
                    Some(Ok(Ok((idx, report)))) => {
                        if ordered_reports[idx].is_none() {
                            ordered_reports[idx] = Some(report);
                            completed_count += 1;
                            if let Some(ref rep) = ordered_reports[idx] {
                                on_target_done(completed_count, total, rep);
                            }
                        }
                    }
                    Some(Ok(Err(ProbeError::Cancelled))) => {
                        join_set.abort_all();
                        while join_set.join_next().await.is_some() {}
                        return Err(ProbeError::Cancelled);
                    }
                    Some(Ok(Err(err))) => {
                        join_set.abort_all();
                        while join_set.join_next().await.is_some() {}
                        return Err(err);
                    }
                    Some(Err(join_err)) => {
                        join_set.abort_all();
                        while join_set.join_next().await.is_some() {}
                        return Err(ProbeError::Internal(format!("link probe task panicked: {join_err}")));
                    }
                    None => {
                        if completed_count == total {
                            break;
                        }
                    }
                }
            }
        }
    }

    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(Ok((idx, report))) => {
                if ordered_reports[idx].is_none() {
                    ordered_reports[idx] = Some(report);
                    completed_count += 1;
                    if let Some(ref rep) = ordered_reports[idx] {
                        on_target_done(completed_count, total, rep);
                    }
                }
            }
            Ok(Err(err)) => return Err(err),
            Err(join_err) => {
                return Err(ProbeError::Internal(format!(
                    "link probe task panicked: {join_err}"
                )));
            }
        }
    }

    let reports: Vec<LinkTargetReport> = ordered_reports
        .into_iter()
        .enumerate()
        .map(|(idx, item)| {
            item.unwrap_or_else(|| {
                let t = &targets[idx];
                LinkTargetReport {
                    label: t.label.trim().to_string(),
                    host_ip: t.host_ip.trim().to_string(),
                    port: t.port,
                    protocol: t.protocol.clone(),
                    reachable: false,
                    sent: 0,
                    received: 0,
                    loss_ratio: 1.0,
                    avg_rtt_ms: None,
                    min_rtt_ms: None,
                    max_rtt_ms: None,
                    samples: Vec::new(),
                }
            })
        })
        .collect();

    Ok(reports)
}

async fn probe_single_target(
    target: &LinkTarget,
    attempts_per_target: u32,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<LinkTargetReport, ProbeError> {
    let ip: IpAddr = target
        .host_ip
        .trim()
        .parse()
        .map_err(|err| ProbeError::InvalidInput(format!("invalid host_ip: {err}")))?;
    let addr = SocketAddr::new(ip, target.port);
    let label = target.label.trim().to_string();
    let normalized_protocol = match &target.protocol {
        LinkProtocol::Tcp => LinkProtocol::Tcp,
        LinkProtocol::TlsSni { sni } => LinkProtocol::TlsSni {
            sni: sni.trim().to_string(),
        },
    };

    let mut samples = Vec::with_capacity(attempts_per_target as usize);

    for seq in 1..=attempts_per_target {
        if cancel.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }

        let sample = probe_one_attempt(seq, addr, &normalized_protocol, timeout_ms, cancel).await?;
        if let Some(ref err_msg) = sample.error {
            warn(
                LOG_MOD,
                format!(
                    "target={label} addr={addr} seq={seq}/{attempts_per_target} failed: {err_msg}"
                ),
            );
        }
        samples.push(sample);

        if seq < attempts_per_target {
            tokio::select! {
                _ = cancel.cancelled() => return Err(ProbeError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(INTER_ATTEMPT_DELAY_MS)) => {}
            }
        }
    }

    let sent = samples.len() as u32;
    let rtts: Vec<f32> = samples.iter().filter_map(|s| s.rtt_ms).collect();
    let received = rtts.len() as u32;
    let reachable = received > 0;
    let loss_ratio = if sent == 0 {
        1.0
    } else {
        (sent - received) as f32 / sent as f32
    };

    let (avg_rtt_ms, min_rtt_ms, max_rtt_ms) = if rtts.is_empty() {
        (None, None, None)
    } else {
        let sum: f32 = rtts.iter().sum();
        let min_v = rtts.iter().copied().fold(f32::INFINITY, f32::min);
        let max_v = rtts.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        (Some(sum / rtts.len() as f32), Some(min_v), Some(max_v))
    };

    info(
        LOG_MOD,
        format!(
            "target={label} addr={addr} reachable={reachable} received={received}/{sent} loss={:.1}% avg_rtt={:?}",
            loss_ratio * 100.0,
            avg_rtt_ms
        ),
    );

    Ok(LinkTargetReport {
        label,
        host_ip: ip.to_string(),
        port: target.port,
        protocol: normalized_protocol,
        reachable,
        sent,
        received,
        loss_ratio,
        avg_rtt_ms,
        min_rtt_ms,
        max_rtt_ms,
        samples,
    })
}

async fn probe_one_attempt(
    seq: u32,
    addr: SocketAddr,
    protocol: &LinkProtocol,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<LinkSample, ProbeError> {
    let total_budget = Duration::from_millis(timeout_ms);
    let started = Instant::now();

    let connect_res = tokio::select! {
        _ = cancel.cancelled() => return Err(ProbeError::Cancelled),
        res = tokio::time::timeout(total_budget, TcpStream::connect(addr)) => res,
    };

    let mut stream = match connect_res {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            return Ok(LinkSample {
                seq,
                rtt_ms: None,
                tcp_rtt_ms: None,
                error: Some(format!("tcp connect failed: {err}")),
            });
        }
        Err(_) => {
            return Ok(LinkSample {
                seq,
                rtt_ms: None,
                tcp_rtt_ms: None,
                error: Some(format!("tcp connect timed out after {timeout_ms}ms")),
            });
        }
    };

    let _ = stream.set_nodelay(true);
    let tcp_rtt_ms = started.elapsed().as_secs_f32() * 1000.0;

    match protocol {
        LinkProtocol::Tcp => Ok(LinkSample {
            seq,
            rtt_ms: Some(tcp_rtt_ms),
            tcp_rtt_ms: Some(tcp_rtt_ms),
            error: None,
        }),
        LinkProtocol::TlsSni { sni } => {
            let remaining = total_budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(LinkSample {
                    seq,
                    rtt_ms: None,
                    tcp_rtt_ms: Some(tcp_rtt_ms),
                    error: Some(format!("tls handshake timed out after {timeout_ms}ms")),
                });
            }

            let client_hello = build_tls_client_hello(sni);
            let tls_res = tokio::select! {
                _ = cancel.cancelled() => return Err(ProbeError::Cancelled),
                res = tokio::time::timeout(remaining, perform_tls_hello(&mut stream, &client_hello)) => res,
            };

            match tls_res {
                Ok(Ok(())) => {
                    let total_rtt_ms = started.elapsed().as_secs_f32() * 1000.0;
                    Ok(LinkSample {
                        seq,
                        rtt_ms: Some(total_rtt_ms),
                        tcp_rtt_ms: Some(tcp_rtt_ms),
                        error: None,
                    })
                }
                Ok(Err(err_msg)) => Ok(LinkSample {
                    seq,
                    rtt_ms: None,
                    tcp_rtt_ms: Some(tcp_rtt_ms),
                    error: Some(err_msg),
                }),
                Err(_) => Ok(LinkSample {
                    seq,
                    rtt_ms: None,
                    tcp_rtt_ms: Some(tcp_rtt_ms),
                    error: Some(format!("tls handshake timed out after {timeout_ms}ms")),
                }),
            }
        }
    }
}

async fn perform_tls_hello(stream: &mut TcpStream, client_hello: &[u8]) -> Result<(), String> {
    stream
        .write_all(client_hello)
        .await
        .map_err(|err| format!("tls write failed: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| format!("tls flush failed: {err}"))?;

    let mut rec_hdr = [0u8; 5];
    stream
        .read_exact(&mut rec_hdr)
        .await
        .map_err(|err| format!("tls read record header failed: {err}"))?;

    let content_type = rec_hdr[0];
    let ver_major = rec_hdr[1];
    let rec_len = u16::from_be_bytes([rec_hdr[3], rec_hdr[4]]);

    match content_type {
        // TLS Handshake record
        0x16 => {
            if ver_major != 0x03 || rec_len < 4 {
                return Err(format!(
                    "invalid tls handshake record (ver=0x{:02x}{:02x}, len={rec_len})",
                    rec_hdr[1], rec_hdr[2]
                ));
            }
            let mut hs_hdr = [0u8; 4];
            stream
                .read_exact(&mut hs_hdr)
                .await
                .map_err(|err| format!("tls read handshake header failed: {err}"))?;
            if hs_hdr[0] == 0x02 {
                Ok(())
            } else {
                Err(format!(
                    "unexpected TLS handshake message type 0x{:02x}",
                    hs_hdr[0]
                ))
            }
        }
        // TLS Alert record
        0x15 => {
            if rec_len >= 2 {
                let mut alert = [0u8; 2];
                stream
                    .read_exact(&mut alert)
                    .await
                    .map_err(|err| format!("tls read alert failed: {err}"))?;
                Err(format!("tls alert (level={}, code={})", alert[0], alert[1]))
            } else {
                Err("tls alert (truncated)".to_string())
            }
        }
        other => Err(format!(
            "unexpected non-TLS response (record_type=0x{other:02x})"
        )),
    }
}

pub fn build_tls_client_hello(sni: &str) -> Vec<u8> {
    let sni_bytes = sni.trim().as_bytes();
    let seq = CLIENT_HELLO_SEQ.fetch_add(1, Ordering::Relaxed);

    let mut exts = Vec::with_capacity(160);

    // 1. server_name (0x0000)
    let sni_name_len = sni_bytes.len() as u16;
    let sni_list_len = sni_name_len + 3;
    let sni_ext_len = sni_list_len + 2;
    exts.extend_from_slice(&0x0000u16.to_be_bytes());
    exts.extend_from_slice(&sni_ext_len.to_be_bytes());
    exts.extend_from_slice(&sni_list_len.to_be_bytes());
    exts.push(0x00); // host_name
    exts.extend_from_slice(&sni_name_len.to_be_bytes());
    exts.extend_from_slice(sni_bytes);

    // 2. ec_point_formats (0x000b): uncompressed (0x00)
    exts.extend_from_slice(&[0x00, 0x0b, 0x00, 0x02, 0x01, 0x00]);

    // 3. supported_groups (0x000a): x25519 (0x001d), secp256r1 (0x0017), secp384r1 (0x0018)
    exts.extend_from_slice(&[
        0x00, 0x0a, 0x00, 0x08, 0x00, 0x06, 0x00, 0x1d, 0x00, 0x17, 0x00, 0x18,
    ]);

    // 4. alpn (0x0010): "h2", "http/1.1"
    exts.extend_from_slice(&[
        0x00, 0x10, 0x00, 0x0e, 0x00, 0x0c, 0x02, b'h', b'2', 0x08, b'h', b't', b't', b'p',
        b'/', b'1', b'.', b'1',
    ]);

    // 5. signature_algorithms (0x000d)
    exts.extend_from_slice(&[
        0x00, 0x0d, 0x00, 0x0e, 0x00, 0x0c, 0x04, 0x03, 0x08, 0x04, 0x04, 0x01, 0x05, 0x03,
        0x08, 0x05, 0x05, 0x01,
    ]);

    // 6. supported_versions (0x002b): TLS 1.3 (0x0304), TLS 1.2 (0x0303)
    exts.extend_from_slice(&[0x00, 0x2b, 0x00, 0x05, 0x04, 0x03, 0x04, 0x03, 0x03]);

    // 7. key_share (0x0033): x25519 (0x001d) with standard Curve25519 basepoint [9, 0, ...]
    exts.extend_from_slice(&[0x00, 0x33, 0x00, 0x26, 0x00, 0x24, 0x00, 0x1d, 0x00, 0x20]);
    let mut x25519_pub = [0u8; 32];
    x25519_pub[0] = 0x09;
    exts.extend_from_slice(&x25519_pub);

    let mut body = Vec::with_capacity(96 + exts.len());
    // legacy_version = TLS 1.2 (0x0303)
    body.extend_from_slice(&[0x03, 0x03]);

    // 32-byte client random
    let mut random = [0x42u8; 32];
    random[..8].copy_from_slice(&seq.to_be_bytes());
    body.extend_from_slice(&random);

    // 32-byte legacy_session_id for TLS 1.3 middlebox compatibility
    body.push(32);
    let mut session_id = [0x24u8; 32];
    session_id[..8].copy_from_slice(&seq.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    body.extend_from_slice(&session_id);

    // cipher_suites (8 suites = 16 bytes)
    let cipher_suites: &[u8] = &[
        0x13, 0x01, // TLS_AES_128_GCM_SHA256
        0x13, 0x02, // TLS_AES_256_GCM_SHA384
        0x13, 0x03, // TLS_CHACHA20_POLY1305_SHA256
        0xc0, 0x2f, // TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
        0xc0, 0x2b, // TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
        0xc0, 0x30, // TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
        0xc0, 0x2c, // TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
        0xcc, 0xa8, // TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
    ];
    body.extend_from_slice(&(cipher_suites.len() as u16).to_be_bytes());
    body.extend_from_slice(cipher_suites);

    // legacy_compression_methods: [null]
    body.extend_from_slice(&[0x01, 0x00]);

    // extensions
    body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
    body.extend_from_slice(&exts);

    // Handshake header (type=0x01 ClientHello, 24-bit length)
    let hs_len = body.len() as u32;
    let mut record = Vec::with_capacity(9 + body.len());
    let rec_len = (4 + body.len()) as u16;
    record.extend_from_slice(&[0x16, 0x03, 0x01]);
    record.extend_from_slice(&rec_len.to_be_bytes());
    record.push(0x01);
    record.push(((hs_len >> 16) & 0xff) as u8);
    record.push(((hs_len >> 8) & 0xff) as u8);
    record.push((hs_len & 0xff) as u8);
    record.extend_from_slice(&body);
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_target() -> LinkTarget {
        LinkTarget {
            label: "cf".into(),
            host_ip: "1.1.1.1".into(),
            port: 443,
            protocol: LinkProtocol::TlsSni {
                sni: "cloudflare.com".into(),
            },
        }
    }

    #[test]
    fn validate_link_input_enforces_ip_literal_and_bounds() {
        let valid = LinkTaskInput {
            targets: vec![base_target()],
            attempts_per_target: 3,
            timeout_ms: 1500,
            max_concurrency: 2,
        };
        assert!(validate_link_input(&valid).is_ok());

        let mut domain_ip = valid.clone();
        domain_ip.targets[0].host_ip = "cloudflare.com".into();
        assert!(matches!(
            validate_link_input(&domain_ip),
            Err(ProbeError::InvalidInput(_))
        ));

        let mut empty_targets = valid.clone();
        empty_targets.targets.clear();
        assert!(matches!(
            validate_link_input(&empty_targets),
            Err(ProbeError::InvalidInput(_))
        ));

        let mut bad_attempts = valid.clone();
        bad_attempts.attempts_per_target = 21;
        assert!(matches!(
            validate_link_input(&bad_attempts),
            Err(ProbeError::InvalidInput(_))
        ));

        let mut bad_concurrency = valid.clone();
        bad_concurrency.max_concurrency = 6;
        assert!(matches!(
            validate_link_input(&bad_concurrency),
            Err(ProbeError::InvalidInput(_))
        ));

        let mut bad_timeout = valid.clone();
        bad_timeout.timeout_ms = 200;
        assert!(matches!(
            validate_link_input(&bad_timeout),
            Err(ProbeError::InvalidInput(_))
        ));

        let mut empty_sni = valid;
        empty_sni.targets[0].protocol = LinkProtocol::TlsSni { sni: "  ".into() };
        assert!(matches!(
            validate_link_input(&empty_sni),
            Err(ProbeError::InvalidInput(_))
        ));
    }

    #[test]
    fn client_hello_contains_valid_lengths_and_sni() {
        let hello = build_tls_client_hello("example.org");
        assert_eq!(hello[0], 0x16);
        assert_eq!(hello[1], 0x03);
        assert_eq!(hello[2], 0x01);
        let rec_len = u16::from_be_bytes([hello[3], hello[4]]) as usize;
        assert_eq!(rec_len, hello.len() - 5);
        assert_eq!(hello[5], 0x01);
        let hs_len =
            ((hello[6] as usize) << 16) | ((hello[7] as usize) << 8) | (hello[8] as usize);
        assert_eq!(hs_len, rec_len - 4);
        assert!(hello.windows(11).any(|w| w == b"example.org"));
    }
}
