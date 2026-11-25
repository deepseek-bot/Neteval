use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use client_core::orchestrator::Orchestrator;
use neteval_gui_lib::GuiState;
use protocol::{
    AssessmentState, IpFamily, LatencyPoint, LivePeerInfo, P2pTaskInput, ProbeError,
};

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
