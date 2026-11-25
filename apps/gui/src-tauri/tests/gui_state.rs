use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use client_core::orchestrator::Orchestrator;
use neteval_gui_lib::GuiState;
use protocol::{
    AddrEchoResp, AssessmentState, EchoMode, IpFamily, LatencyPoint, LivePeerInfo, NatFiltering,
    NatMapping, NatStatus, NatTaskInput, P2pTaskInput, ProbeError, S2WireMessage,
};
use tokio::net::UdpSocket;

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "neteval_gui_{name}_{}_{seq}",
            std::process::id()
        ));
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn gui_state_delegates_to_orchestrator_and_history() {
    let tmp = TempDir::new("state");
    let gui = GuiState::new(Orchestrator::new(&tmp.0));
    assert_eq!(gui.current_state(), AssessmentState::Idle);
    assert!(gui.history().unwrap().is_empty());
    assert!(matches!(
        gui.report("missing_1"),
        Err(ProbeError::InvalidInput(ref msg)) if msg.contains("report_not_found")
    ));
}

#[tokio::test]
async fn gui_state_rejects_invalid_p2p_input_via_orchestrator() {
    let tmp = TempDir::new("invalid_p2p");
    let gui = GuiState::new(Orchestrator::new(&tmp.0));
    let err = gui
        .start_p2p(P2pTaskInput {
            signaling_ws_url: "ws://127.0.0.1:34781".into(),
            room_code: "x".into(),
            token: None,
            peer_label: "tester".into(),
            echo_udp_v4: None,
            echo_udp_v6: None,
            public_stun_servers: vec![],
            candidate_timeout_ms: 1000,
            pair_timeout_secs: 10,
            punch_timeout_ms: 5000,
            window_secs: 10,
            packet_rate_hz: 10,
            strategy: Default::default(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::InvalidInput(_)));
}

#[test]
fn assessment_state_and_probe_error_serialize_with_expected_tags() {
    let running = AssessmentState::Running {
        task_id: "p2p_1".into(),
        stage: "sampling".into(),
        progress_pct: 60,
        live_point: Some(LatencyPoint {
            seq: 2,
            elapsed_ms: 200,
            rtt_ms: Some(15.25),
        }),
        live_peer: Some(LivePeerInfo {
            room_code: Some("abcd".into()),
            peer_label: Some("bob".into()),
            ip_family: Some(IpFamily::Ipv4),
            active_peer_addr: Some("198.51.100.2:40000".into()),
            punch_elapsed_ms: Some(42),
        }),
    };
    let v = serde_json::to_value(&running).unwrap();
    assert_eq!(v["running"]["stage"], "sampling");
    assert_eq!(v["running"]["live_peer"]["peer_label"], "bob");

    let err_v = serde_json::to_value(ProbeError::InvalidInput("bad".into())).unwrap();
    assert_eq!(err_v["invalid_input"], "bad");
    assert_eq!(
        serde_json::to_value(ProbeError::RoomFull).unwrap(),
        serde_json::json!("room_full")
    );
}

async fn spawn_mock_s2_pair() -> (String, String, Vec<tokio::task::JoinHandle<()>>) {
    let p1 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let p2 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let p1_addr = p1.local_addr().unwrap().to_string();
    let p2_addr = p2.local_addr().unwrap().to_string();

    let p1_rx = p1.clone();
    let p2_for_alt = p2.clone();
    let h1 = tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            let Ok((len, peer)) = p1_rx.recv_from(&mut buf).await else {
                break;
            };
            if let Ok(S2WireMessage::AddrEchoReq(req)) = serde_json::from_slice(&buf[..len]) {
                let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
                    nonce: req.nonce,
                    observed_addr: peer.to_string(),
                    responder_id: Some("mock-p1".into()),
                });
                let bytes = serde_json::to_vec(&resp).unwrap();
                let sender = if req.mode == EchoMode::ReplyFromAltPort {
                    &p2_for_alt
                } else {
                    &p1_rx
                };
                let _ = sender.send_to(&bytes, peer).await;
            }
        }
    });

    let p2_rx = p2.clone();
    let h2 = tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            let Ok((len, peer)) = p2_rx.recv_from(&mut buf).await else {
                break;
            };
            if let Ok(S2WireMessage::AddrEchoReq(req)) = serde_json::from_slice(&buf[..len]) {
                let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
                    nonce: req.nonce,
                    observed_addr: peer.to_string(),
                    responder_id: Some("mock-p2".into()),
                });
                let bytes = serde_json::to_vec(&resp).unwrap();
                let _ = p2_rx.send_to(&bytes, peer).await;
            }
        }
    });

    (p1_addr, p2_addr, vec![h1, h2])
}

#[tokio::test]
async fn gui_state_runs_nat_assessment_end_to_end_and_persists_report() {
    let (p1_addr, p2_addr, handles) = spawn_mock_s2_pair().await;
    let tmp = TempDir::new("e2e_nat");
    let gui = GuiState::new(Orchestrator::new(&tmp.0));

    let task_id = gui
        .start_nat(NatTaskInput {
            public_stun_servers: vec![],
            primary_v4_p1: Some(p1_addr),
            primary_v4_p2: Some(p2_addr),
            primary_v6: None,
            helper_v4_p1: None,
            token: None,
            timeout_ms: 800,
        })
        .await
        .expect("start_nat should succeed");
    assert!(task_id.starts_with("nat_"));

    let report = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match gui.current_state() {
                AssessmentState::Completed(rep) => return rep,
                AssessmentState::Failed(err) => panic!("NAT task failed unexpectedly: {err:?}"),
                AssessmentState::Cancelled { reason } => panic!("NAT task cancelled: {reason:?}"),
                AssessmentState::Idle | AssessmentState::Running { .. } => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("NAT task timed out; last_state={:?}", gui.current_state()));

    for h in handles {
        h.abort();
    }

    let nat = report.nat_report.as_ref().expect("nat_report missing");
    assert_eq!(nat.status, NatStatus::CompleteSingleVps);
    assert_eq!(nat.mapping, NatMapping::EndpointIndependent);
    assert_eq!(nat.filtering, NatFiltering::ConeNat1OrNat2);

    let history = gui.history().expect("list history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].report_id, report.report_id);

    let loaded = gui.report(&report.report_id).expect("load persisted report");
    assert_eq!(loaded.report_id, report.report_id);

    gui.remove_report(&report.report_id).expect("delete persisted report");
    assert!(gui.history().expect("list after delete").is_empty());
}
