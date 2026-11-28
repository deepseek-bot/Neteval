use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use client_core::link_prober;
use client_core::orchestrator::Orchestrator;
use protocol::{AssessmentState, LinkProtocol, LinkTarget, LinkTaskInput, ProbeError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

static NEXT_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let seq = NEXT_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "neteval_e2e_v14_{name}_{}_{seq}",
            std::process::id()
        ));
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

async fn wait_for_terminal(rx: &mut watch::Receiver<AssessmentState>) -> AssessmentState {
    loop {
        let cur = rx.borrow().clone();
        match cur {
            AssessmentState::Completed(_)
            | AssessmentState::Cancelled { .. }
            | AssessmentState::Failed(_) => return cur,
            AssessmentState::Idle | AssessmentState::Running { .. } => {
                rx.changed().await.expect("state channel open");
            }
        }
    }
}

#[tokio::test]
async fn link_probe_tcp_and_persistence() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_task = tokio::spawn(async move {
        while let Ok((_stream, _)) = listener.accept().await {}
    });

    let dir = TempDir::new("tcp_persist");
    let orch = Orchestrator::new(&dir.path);
    let mut rx = orch.subscribe();

    let input = LinkTaskInput {
        targets: vec![LinkTarget {
            label: "local-tcp".into(),
            host_ip: addr.ip().to_string(),
            port: addr.port(),
            protocol: LinkProtocol::Tcp,
        }],
        attempts_per_target: 3,
        timeout_ms: 1500,
        max_concurrency: 2,
    };

    let task_id = orch.start_link_assessment(input).await.unwrap();
    assert!(task_id.starts_with("link_"));

    let term = wait_for_terminal(&mut rx).await;
    accept_task.abort();

    let report = match term {
        AssessmentState::Completed(r) => r,
        other => panic!("expected Completed, got {other:?}"),
    };
    assert_eq!(report.report_id, task_id);
    assert!(report.storage_warning.is_none());

    let links = report.link_reports.expect("link_reports missing");
    assert_eq!(links.len(), 1);
    let t = &links[0];
    assert_eq!(t.label, "local-tcp");
    assert!(t.reachable);
    assert_eq!(t.sent, 3);
    assert_eq!(t.received, 3);
    assert_eq!(t.loss_ratio, 0.0);
    assert!(t.avg_rtt_ms.is_some());
    assert!(t.min_rtt_ms.is_some());
    assert!(t.max_rtt_ms.is_some());

    let history = orch.list_history().unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].has_link);
    assert_eq!(history[0].link_target_count, Some(1));
    assert_eq!(history[0].link_reachable_count, Some(1));
}

#[tokio::test]
async fn link_probe_tls_sni_sends_sni_and_accepts_server_hello() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed_payloads: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let payloads_ref = Arc::clone(&observed_payloads);

    let srv = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let p_ref = Arc::clone(&payloads_ref);
            tokio::spawn(async move {
                let mut hdr = [0u8; 5];
                if stream.read_exact(&mut hdr).await.is_err() {
                    return;
                }
                let rec_len = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
                let mut body = vec![0u8; rec_len];
                if stream.read_exact(&mut body).await.is_err() {
                    return;
                }
                p_ref.lock().unwrap().push(body);
                // Minimal TLS Handshake record containing ServerHello (type=0x02)
                let server_hello = [
                    0x16, 0x03, 0x03, 0x00, 0x08, // record header
                    0x02, 0x00, 0x00, 0x04, // ServerHello handshake header
                    0x03, 0x03, 0x00, 0x00, // dummy body
                ];
                let _ = stream.write_all(&server_hello).await;
            });
        }
    });

    let targets = vec![LinkTarget {
        label: "mock-tls".into(),
        host_ip: addr.ip().to_string(),
        port: addr.port(),
        protocol: LinkProtocol::TlsSni {
            sni: "edge.example.com".into(),
        },
    }];

    let reports = link_prober::run_link_probe(
        &targets,
        2,
        1500,
        1,
        CancellationToken::new(),
        |_, _, _| {},
    )
    .await
    .unwrap();
    srv.abort();

    assert_eq!(reports.len(), 1);
    let rep = &reports[0];
    assert!(rep.reachable);
    assert_eq!(rep.sent, 2);
    assert_eq!(rep.received, 2);
    assert_eq!(rep.loss_ratio, 0.0);
    assert!(rep.samples.iter().all(|s| s.tcp_rtt_ms.is_some() && s.rtt_ms.is_some()));

    let payloads = observed_payloads.lock().unwrap();
    assert_eq!(payloads.len(), 2);
    for buf in payloads.iter() {
        assert!(
            buf.windows(b"edge.example.com".len())
                .any(|w| w == b"edge.example.com"),
            "ClientHello did not carry SNI hostname"
        );
    }
}

#[tokio::test]
async fn link_probe_isolates_failures_across_targets() {
    // 1. Healthy TCP target
    let ok_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ok_addr = ok_listener.local_addr().unwrap();
    let ok_srv = tokio::spawn(async move {
        while let Ok((_s, _)) = ok_listener.accept().await {}
    });

    // 2. TLS Alert responder
    let alert_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let alert_addr = alert_listener.local_addr().unwrap();
    let alert_srv = tokio::spawn(async move {
        while let Ok((mut s, _)) = alert_listener.accept().await {
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]).await;
        }
    });

    // 3. Non-TLS plaintext responder
    let plain_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plain_addr = plain_listener.local_addr().unwrap();
    let plain_srv = tokio::spawn(async move {
        while let Ok((mut s, _)) = plain_listener.accept().await {
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        }
    });

    // 4. Closed port
    let closed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_addr = closed_listener.local_addr().unwrap();
    drop(closed_listener);

    let targets = vec![
        LinkTarget {
            label: "closed".into(),
            host_ip: closed_addr.ip().to_string(),
            port: closed_addr.port(),
            protocol: LinkProtocol::Tcp,
        },
        LinkTarget {
            label: "alert".into(),
            host_ip: alert_addr.ip().to_string(),
            port: alert_addr.port(),
            protocol: LinkProtocol::TlsSni {
                sni: "alert.test".into(),
            },
        },
        LinkTarget {
            label: "non-tls".into(),
            host_ip: plain_addr.ip().to_string(),
            port: plain_addr.port(),
            protocol: LinkProtocol::TlsSni {
                sni: "plain.test".into(),
            },
        },
        LinkTarget {
            label: "healthy".into(),
            host_ip: ok_addr.ip().to_string(),
            port: ok_addr.port(),
            protocol: LinkProtocol::Tcp,
        },
    ];

    let reports = link_prober::run_link_probe(
        &targets,
        1,
        1000,
        4,
        CancellationToken::new(),
        |_, _, _| {},
    )
    .await
    .unwrap();

    ok_srv.abort();
    alert_srv.abort();
    plain_srv.abort();

    assert_eq!(reports.len(), 4);
    assert_eq!(reports[0].label, "closed");
    assert!(!reports[0].reachable);
    assert!(reports[0].samples[0]
        .error
        .as_deref()
        .is_some_and(|e| e.contains("tcp connect")));

    assert_eq!(reports[1].label, "alert");
    assert!(!reports[1].reachable);
    assert!(reports[1].samples[0]
        .error
        .as_deref()
        .is_some_and(|e| e.contains("tls alert")));

    assert_eq!(reports[2].label, "non-tls");
    assert!(!reports[2].reachable);
    assert!(reports[2].samples[0]
        .error
        .as_deref()
        .is_some_and(|e| e.contains("non-TLS")));

    assert_eq!(reports[3].label, "healthy");
    assert!(reports[3].reachable);
    assert_eq!(reports[3].loss_ratio, 0.0);
}

#[tokio::test]
async fn link_probe_enforces_max_concurrency() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let in_flight_srv = Arc::clone(&in_flight);
    let peak_srv = Arc::clone(&peak);

    let srv = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let cur = Arc::clone(&in_flight_srv);
            let max_seen = Arc::clone(&peak_srv);
            tokio::spawn(async move {
                let active = cur.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(active, Ordering::SeqCst);
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(120)).await;
                let server_hello = [
                    0x16, 0x03, 0x03, 0x00, 0x08, 0x02, 0x00, 0x00, 0x04, 0x03, 0x03, 0x00, 0x00,
                ];
                let _ = stream.write_all(&server_hello).await;
                cur.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });

    let targets: Vec<LinkTarget> = (1..=4)
        .map(|i| LinkTarget {
            label: format!("t-{i}"),
            host_ip: addr.ip().to_string(),
            port: addr.port(),
            protocol: LinkProtocol::TlsSni {
                sni: "conc.test".into(),
            },
        })
        .collect();

    let reports = link_prober::run_link_probe(
        &targets,
        1,
        2000,
        2,
        CancellationToken::new(),
        |_, _, _| {},
    )
    .await
    .unwrap();
    srv.abort();

    assert_eq!(reports.len(), 4);
    assert!(reports.iter().all(|r| r.reachable));
    assert!(
        peak.load(Ordering::SeqCst) <= 2,
        "observed concurrency {} > 2",
        peak.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn link_probe_cancel_stops_under_500ms() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let dir = TempDir::new("cancel");
    let orch = Orchestrator::new(&dir.path);
    let mut rx = orch.subscribe();

    let input = LinkTaskInput {
        targets: vec![LinkTarget {
            label: "hung-tls".into(),
            host_ip: addr.ip().to_string(),
            port: addr.port(),
            protocol: LinkProtocol::TlsSni {
                sni: "hung.test".into(),
            },
        }],
        attempts_per_target: 3,
        timeout_ms: 5000,
        max_concurrency: 1,
    };

    orch.start_link_assessment(input).await.unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;

    let started = Instant::now();
    orch.cancel_current(Some("user_abort".into()))
        .await
        .unwrap();
    let elapsed = started.elapsed();
    srv.abort();

    assert!(elapsed < Duration::from_millis(500), "cancel took {elapsed:?}");
    let term = wait_for_terminal(&mut rx).await;
    assert_eq!(
        term,
        AssessmentState::Cancelled {
            reason: Some("user_abort".into())
        }
    );
}

#[tokio::test]
async fn link_probe_rejects_dns_names_and_invalid_bounds() {
    let dir = TempDir::new("invalid");
    let orch = Orchestrator::new(&dir.path);

    let err = orch
        .start_link_assessment(LinkTaskInput {
            targets: vec![LinkTarget {
                label: "bad".into(),
                host_ip: "cloudflare.com".into(),
                port: 443,
                protocol: LinkProtocol::Tcp,
            }],
            attempts_per_target: 3,
            timeout_ms: 1000,
            max_concurrency: 1,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::InvalidInput(_)));
}
