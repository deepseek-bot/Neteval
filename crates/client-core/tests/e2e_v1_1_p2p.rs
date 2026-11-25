use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client_core::orchestrator::Orchestrator;
use futures_util::{SinkExt, StreamExt};
use protocol::{
    AddrEchoResp, AssessmentState, Candidate, CandidateKind, ClientSignalMsg, P2pTaskInput,
    P2pVerdict, ProbeError, PunchStrategy, S2WireMessage, ServerSignalMsg, SignalErrorMsg,
    StartPunchMsg, WaitingForPeerMsg,
};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "neteval_e2e_p2p_{name}_{}_{seq}",
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

enum SimRoom {
    Waiting {
        peer1_id: u64,
        peer1_label: String,
        peer1_candidates: Vec<Candidate>,
        peer1_tx: mpsc::UnboundedSender<ServerSignalMsg>,
    },
    Paired {
        peer1_id: u64,
        peer2_id: u64,
        peer1_tx: Option<mpsc::UnboundedSender<ServerSignalMsg>>,
        peer2_tx: Option<mpsc::UnboundedSender<ServerSignalMsg>>,
    },
}

struct SimP2pRig {
    ws_url: String,
    echo_v4: String,
    cancel: CancellationToken,
    _blackhole_sock: Option<UdpSocket>,
}

impl SimP2pRig {
    async fn start(echo_token: Option<&str>, ws_token: Option<&str>, blackhole: bool) -> Self {
        let echo_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo_v4 = echo_sock.local_addr().unwrap().to_string();

        let (blackhole_sock, override_addr) = if blackhole {
            let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let a = s.local_addr().unwrap();
            (Some(s), Some(a))
        } else {
            (None, None)
        };

        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();
        let ws_url = format!("ws://{ws_addr}/signal");

        let cancel = CancellationToken::new();
        let expected_echo_token = echo_token.map(str::to_string);
        let expected_ws_token = ws_token.map(str::to_string);

        // S2 Echo mock
        {
            let c = cancel.clone();
            let tok = expected_echo_token;
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        res = echo_sock.recv_from(&mut buf) => {
                            let Ok((n, src)) = res else { continue };
                            let Ok(S2WireMessage::AddrEchoReq(req)) = serde_json::from_slice(&buf[..n]) else {
                                continue;
                            };
                            if let Some(exp) = tok.as_deref() {
                                if req.token.as_deref() != Some(exp) {
                                    continue;
                                }
                            }
                            let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
                                nonce: req.nonce,
                                observed_addr: src.to_string(),
                                responder_id: Some("sim-s2".into()),
                            });
                            if let Ok(bytes) = serde_json::to_vec(&resp) {
                                let _ = echo_sock.send_to(&bytes, src).await;
                            }
                        }
                    }
                }
            });
        }

        // S1 WS Signaling mock
        {
            let c = cancel.clone();
            let rooms = Arc::new(Mutex::new(HashMap::<String, SimRoom>::new()));
            let conn_seq = Arc::new(AtomicU64::new(1));
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        res = ws_listener.accept() => {
                            let Ok((stream, _)) = res else { continue };
                            let rooms = Arc::clone(&rooms);
                            let expected_token = expected_ws_token.clone();
                            let conn_id = conn_seq.fetch_add(1, Ordering::Relaxed);
                            let conn_cancel = c.clone();
                            tokio::spawn(async move {
                                handle_sim_ws_conn(
                                    stream,
                                    conn_id,
                                    rooms,
                                    expected_token,
                                    override_addr,
                                    conn_cancel,
                                ).await;
                            });
                        }
                    }
                }
            });
        }

        Self {
            ws_url,
            echo_v4,
            cancel,
            _blackhole_sock: blackhole_sock,
        }
    }
}

impl Drop for SimP2pRig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn handle_sim_ws_conn(
    stream: TcpStream,
    conn_id: u64,
    rooms: Arc<Mutex<HashMap<String, SimRoom>>>,
    expected_token: Option<String>,
    override_addr: Option<SocketAddr>,
    cancel: CancellationToken,
) {
    let Ok(mut ws) = accept_async(stream).await else {
        return;
    };

    let Some(Ok(Message::Text(first_text))) = ws.next().await else {
        return;
    };
    let Ok(ClientSignalMsg::JoinRoom(join)) = serde_json::from_str(first_text.as_str()) else {
        return;
    };

    if let Some(exp) = expected_token.as_deref() {
        if join.token.as_deref() != Some(exp) {
            let err_json = serde_json::to_string(&ServerSignalMsg::Error(SignalErrorMsg {
                error: ProbeError::Unauthorized,
            }))
            .unwrap();
            let _ = ws.send(Message::text(err_json)).await;
            let _ = ws.close(None).await;
            return;
        }
    }

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerSignalMsg>();
    let room_code = join.room_code.clone();

    {
        let mut map = rooms.lock().await;
        match map.get_mut(&room_code) {
            None => {
                map.insert(
                    room_code.clone(),
                    SimRoom::Waiting {
                        peer1_id: conn_id,
                        peer1_label: join.peer_label.clone(),
                        peer1_candidates: join.candidates,
                        peer1_tx: out_tx.clone(),
                    },
                );
                let _ = out_tx.send(ServerSignalMsg::WaitingForPeer(WaitingForPeerMsg {
                    room_code: room_code.clone(),
                    ttl_secs: 120,
                }));
            }
            Some(SimRoom::Paired { .. }) => {
                let err_json = serde_json::to_string(&ServerSignalMsg::Error(SignalErrorMsg {
                    error: ProbeError::RoomFull,
                }))
                .unwrap();
                let _ = ws.send(Message::text(err_json)).await;
                let _ = ws.close(None).await;
                return;
            }
            Some(SimRoom::Waiting { .. }) => {
                let Some(SimRoom::Waiting {
                    peer1_id,
                    peer1_label,
                    peer1_candidates,
                    peer1_tx,
                }) = map.remove(&room_code)
                else {
                    return;
                };
                let session_id = format!("sim_sess_{room_code}");
                let p1_cands_for_p2 = override_addr
                    .map(|a| {
                        vec![Candidate {
                            kind: CandidateKind::ServerReflexiveV4,
                            addr: a.to_string(),
                        }]
                    })
                    .unwrap_or(peer1_candidates);
                let p2_cands_for_p1 = override_addr
                    .map(|a| {
                        vec![Candidate {
                            kind: CandidateKind::ServerReflexiveV4,
                            addr: a.to_string(),
                        }]
                    })
                    .unwrap_or(join.candidates);

                let _ = peer1_tx.send(ServerSignalMsg::StartPunch(StartPunchMsg {
                    session_id: session_id.clone(),
                    peer_label: join.peer_label,
                    peer_candidates: p2_cands_for_p1,
                    start_delay_ms: 80,
                    punch_duration_ms: 5000,
                }));
                let _ = out_tx.send(ServerSignalMsg::StartPunch(StartPunchMsg {
                    session_id,
                    peer_label: peer1_label,
                    peer_candidates: p1_cands_for_p2,
                    start_delay_ms: 80,
                    punch_duration_ms: 5000,
                }));

                map.insert(
                    room_code.clone(),
                    SimRoom::Paired {
                        peer1_id,
                        peer2_id: conn_id,
                        peer1_tx: Some(peer1_tx),
                        peer2_tx: Some(out_tx),
                    },
                );
            }
        }
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            msg = out_rx.recv() => {
                let Some(msg) = msg else { break };
                let json = serde_json::to_string(&msg).unwrap();
                if ws.send(Message::text(json)).await.is_err() {
                    break;
                }
            }
            frame = ws.next() => {
                match frame {
                    Some(Ok(Message::Ping(_))) => {
                        let _ = ws.flush().await;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(ClientSignalMsg::LeaveRoom) = serde_json::from_str(text.as_str()) {
                            let mut map = rooms.lock().await;
                            if let Some(SimRoom::Paired {
                                peer1_id,
                                peer2_id,
                                peer1_tx,
                                peer2_tx,
                            }) = map.remove(&room_code)
                            {
                                let partner = if conn_id == peer1_id {
                                    peer2_tx
                                } else if conn_id == peer2_id {
                                    peer1_tx
                                } else {
                                    None
                                };
                                if let Some(tx) = partner {
                                    let _ = tx.send(ServerSignalMsg::PeerLeft);
                                }
                            }
                            let _ = ws.close(None).await;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
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

async fn wait_for_stage(rx: &mut watch::Receiver<AssessmentState>, target_stage: &str) {
    loop {
        let cur = rx.borrow().clone();
        if let AssessmentState::Running { stage, .. } = &cur {
            if stage == target_stage {
                return;
            }
        } else if !matches!(cur, AssessmentState::Idle) {
            panic!("reached unexpected state while waiting for {target_stage}: {cur:?}");
        }
        rx.changed().await.expect("state channel open");
    }
}

fn make_p2p_input(rig: &SimP2pRig, room: &str, label: &str, token: Option<&str>) -> P2pTaskInput {
    P2pTaskInput {
        signaling_ws_url: rig.ws_url.clone(),
        room_code: room.to_string(),
        token: token.map(str::to_string),
        peer_label: label.to_string(),
        echo_udp_v4: Some(rig.echo_v4.clone()),
        echo_udp_v6: None,
        public_stun_servers: Vec::new(),
        candidate_timeout_ms: 1500,
        pair_timeout_secs: 15,
        punch_timeout_ms: 3000,
        window_secs: 5,
        packet_rate_hz: 5,
        strategy: PunchStrategy::StandardUdpV6First,
    }
}

#[tokio::test]
async fn full_dual_peer_p2p_assessment_completes_and_saves_reports() {
    let rig = SimP2pRig::start(Some("tok-e2e"), Some("tok-e2e"), false).await;
    let dir_a = TempDir::new("full_a");
    let dir_b = TempDir::new("full_b");

    let orch_a = Orchestrator::new(&dir_a.path);
    let orch_b = Orchestrator::new(&dir_b.path);
    let mut rx_a = orch_a.subscribe();
    let mut rx_b = orch_b.subscribe();

    let id_a = orch_a
        .start_p2p_assessment(make_p2p_input(&rig, "room1234", "alice", Some("tok-e2e")))
        .await
        .unwrap();
    let id_b = orch_b
        .start_p2p_assessment(make_p2p_input(&rig, "room1234", "bob", Some("tok-e2e")))
        .await
        .unwrap();

    let (term_a, term_b) = tokio::join!(wait_for_terminal(&mut rx_a), wait_for_terminal(&mut rx_b));

    let AssessmentState::Completed(rep_a) = term_a else {
        panic!("expected peer A Completed, got {term_a:?}");
    };
    let AssessmentState::Completed(rep_b) = term_b else {
        panic!("expected peer B Completed, got {term_b:?}");
    };

    assert_eq!(rep_a.report_id, id_a);
    assert_eq!(rep_b.report_id, id_b);
    let p2p_a = rep_a.p2p_report.expect("p2p_report A");
    let p2p_b = rep_b.p2p_report.expect("p2p_report B");

    assert!(p2p_a.punched);
    assert!(p2p_b.punched);
    assert_eq!(p2p_a.peer_label.as_deref(), Some("bob"));
    assert_eq!(p2p_b.peer_label.as_deref(), Some("alice"));
    assert_eq!(p2p_a.sent, 25);
    assert_eq!(p2p_a.received, 25);
    assert_eq!(p2p_a.series.len(), 25);
    assert!(p2p_a.avg_rtt_ms.is_some());
    assert!(p2p_a.p95_rtt_ms.is_some());
    assert!(p2p_a.jitter_ms.is_some());
    assert_eq!(p2p_a.verdict, P2pVerdict::Excellent);
    assert_eq!(p2p_b.verdict, P2pVerdict::Excellent);

    let hist_a = orch_a.list_history().unwrap();
    assert_eq!(hist_a.len(), 1);
    assert!(hist_a[0].has_p2p);
    assert_eq!(hist_a[0].p2p_verdict, Some(P2pVerdict::Excellent));
    assert_eq!(orch_a.get_report(&id_a).unwrap().report_id, id_a);
}

#[tokio::test]
async fn unauthorized_and_room_full_fail_cleanly_and_release_resources() {
    let rig = SimP2pRig::start(None, Some("secret1"), true).await;
    let dir_a = TempDir::new("auth_a");
    let dir_b = TempDir::new("auth_b");
    let dir_c = TempDir::new("auth_c");

    let orch_a = Orchestrator::new(&dir_a.path);
    let orch_b = Orchestrator::new(&dir_b.path);
    let orch_c = Orchestrator::new(&dir_c.path);
    let mut rx_a = orch_a.subscribe();
    let mut rx_b = orch_b.subscribe();
    let mut rx_c = orch_c.subscribe();

    // 1. Unauthorized token rejected cleanly
    orch_c
        .start_p2p_assessment(make_p2p_input(&rig, "authroom", "mallory", Some("wrong")))
        .await
        .unwrap();
    let term_unauth = wait_for_terminal(&mut rx_c).await;
    assert_eq!(term_unauth, AssessmentState::Failed(ProbeError::Unauthorized));
    assert!(orch_c.list_history().unwrap().is_empty());

    // 2. Pair A & B with blackhole candidates, then reject 3rd peer C with RoomFull
    orch_a
        .start_p2p_assessment(make_p2p_input(&rig, "busyroom", "alice", Some("secret1")))
        .await
        .unwrap();
    orch_b
        .start_p2p_assessment(make_p2p_input(&rig, "busyroom", "bob", Some("secret1")))
        .await
        .unwrap();

    tokio::join!(
        wait_for_stage(&mut rx_a, "punching"),
        wait_for_stage(&mut rx_b, "punching")
    );

    orch_c
        .start_p2p_assessment(make_p2p_input(&rig, "busyroom", "charlie", Some("secret1")))
        .await
        .unwrap();
    let term_full = wait_for_terminal(&mut rx_c).await;
    assert_eq!(term_full, AssessmentState::Failed(ProbeError::RoomFull));
    assert!(orch_c.list_history().unwrap().is_empty());

    orch_a.cancel_current(Some("cleanup".into())).await.unwrap();
    orch_b.cancel_current(Some("cleanup".into())).await.unwrap();
}

#[tokio::test]
async fn peer_cancel_during_punching_aborts_partner_under_500ms() {
    let rig = SimP2pRig::start(None, None, true).await;
    let dir_a = TempDir::new("cancel_a");
    let dir_b = TempDir::new("cancel_b");

    let orch_a = Orchestrator::new(&dir_a.path);
    let orch_b = Orchestrator::new(&dir_b.path);
    let mut rx_a = orch_a.subscribe();
    let mut rx_b = orch_b.subscribe();

    let mut in_a = make_p2p_input(&rig, "cancelrm", "alice", None);
    in_a.punch_timeout_ms = 10_000;
    let mut in_b = make_p2p_input(&rig, "cancelrm", "bob", None);
    in_b.punch_timeout_ms = 10_000;

    orch_a.start_p2p_assessment(in_a).await.unwrap();
    orch_b.start_p2p_assessment(in_b).await.unwrap();

    tokio::join!(
        wait_for_stage(&mut rx_a, "punching"),
        wait_for_stage(&mut rx_b, "punching")
    );

    let started = Instant::now();
    orch_a
        .cancel_current(Some("peer_a_abort".into()))
        .await
        .unwrap();
    let cancel_elapsed = started.elapsed();
    assert!(
        cancel_elapsed < Duration::from_millis(500),
        "cancel took {cancel_elapsed:?}"
    );

    let term_a = wait_for_terminal(&mut rx_a).await;
    assert_eq!(
        term_a,
        AssessmentState::Cancelled {
            reason: Some("peer_a_abort".into())
        }
    );

    let term_b = tokio::time::timeout(Duration::from_millis(500), wait_for_terminal(&mut rx_b))
        .await
        .expect("partner B should receive PeerLeft and abort within 500ms");
    assert_eq!(term_b, AssessmentState::Failed(ProbeError::PeerLeft));
    assert!(orch_a.list_history().unwrap().is_empty());
    assert!(orch_b.list_history().unwrap().is_empty());
}

#[tokio::test]
async fn punch_timeout_transitions_to_completed_failed_and_saves_report() {
    let rig = SimP2pRig::start(None, None, true).await;
    let dir_a = TempDir::new("timeout_a");
    let dir_b = TempDir::new("timeout_b");

    let orch_a = Orchestrator::new(&dir_a.path);
    let orch_b = Orchestrator::new(&dir_b.path);
    let mut rx_a = orch_a.subscribe();
    let mut rx_b = orch_b.subscribe();

    orch_a
        .start_p2p_assessment(make_p2p_input(&rig, "timeout1", "alice", None))
        .await
        .unwrap();
    orch_b
        .start_p2p_assessment(make_p2p_input(&rig, "timeout1", "bob", None))
        .await
        .unwrap();

    let (term_a, term_b) = tokio::join!(wait_for_terminal(&mut rx_a), wait_for_terminal(&mut rx_b));

    for (term, orch) in [(term_a, &orch_a), (term_b, &orch_b)] {
        let AssessmentState::Completed(report) = term else {
            panic!("punch_timeout after pairing must reach Completed, got {term:?}");
        };
        let p2p = report.p2p_report.expect("p2p_report present");
        assert!(!p2p.punched);
        assert_eq!(p2p.verdict, P2pVerdict::Failed);
        assert_eq!(p2p.failure_reason.as_deref(), Some("punch_timeout"));
        let hist = orch.list_history().unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].p2p_verdict, Some(P2pVerdict::Failed));
    }
}
