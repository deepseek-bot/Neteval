use std::collections::HashSet;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use client_core::nat_prober::{build_stun_binding_response, parse_stun_binding_request};
use client_core::orchestrator::Orchestrator;
use protocol::{
    AddrEchoResp, AssessmentState, EchoMode, NatFiltering, NatMapping, NatStatus, NatTaskInput,
    ProbeError, S2WireMessage, TASK_ALREADY_RUNNING_MSG,
};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "neteval_e2e_{name}_{}_{seq}",
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

#[derive(Clone, Copy)]
enum SimNatProfile {
    Nat1FullCone,
    Nat2AddressRestricted,
    Nat3PortRestricted,
    Nat4Symmetric,
}

struct SimNatRig {
    stun1_addr: SocketAddr,
    stun2_addr: SocketAddr,
    p1_addr: SocketAddr,
    p2_addr: SocketAddr,
    helper_addr: SocketAddr,
    v6_addr: Option<SocketAddr>,
    cancel: CancellationToken,
}

impl SimNatRig {
    async fn start(profile: SimNatProfile, token: Option<&str>, enable_v6: bool) -> Self {
        let stun1_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let stun2_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let p1_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let p2_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let helper_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());

        let stun1_addr = stun1_sock.local_addr().unwrap();
        let stun2_addr = stun2_sock.local_addr().unwrap();
        let p1_addr = p1_sock.local_addr().unwrap();
        let p2_addr = p2_sock.local_addr().unwrap();
        let helper_addr = helper_sock.local_addr().unwrap();

        let v6_sock = if enable_v6 {
            UdpSocket::bind("[::1]:0").await.ok().map(Arc::new)
        } else {
            None
        };
        let v6_addr = v6_sock.as_ref().and_then(|s| s.local_addr().ok());

        let cancel = CancellationToken::new();
        let contacted_by_client = Arc::new(Mutex::new(HashSet::<SocketAddr>::new()));
        let expected_token = token.map(str::to_string);

        // STUN 1
        {
            let sock = Arc::clone(&stun1_sock);
            let c = cancel.clone();
            let contacted = Arc::clone(&contacted_by_client);
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        res = sock.recv_from(&mut buf) => {
                            if let Ok((len, src)) = res {
                                contacted.lock().unwrap().insert(stun1_addr);
                                if let Some(txn) = parse_stun_binding_request(&buf[..len]) {
                                    let mapped = simulated_mapped_addr(profile, 0);
                                    let resp = build_stun_binding_response(txn, mapped);
                                    let _ = sock.send_to(&resp, src).await;
                                }
                            }
                        }
                    }
                }
            });
        }

        // STUN 2
        {
            let sock = Arc::clone(&stun2_sock);
            let c = cancel.clone();
            let contacted = Arc::clone(&contacted_by_client);
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        res = sock.recv_from(&mut buf) => {
                            if let Ok((len, src)) = res {
                                contacted.lock().unwrap().insert(stun2_addr);
                                if let Some(txn) = parse_stun_binding_request(&buf[..len]) {
                                    let mapped = simulated_mapped_addr(profile, 1);
                                    let resp = build_stun_binding_response(txn, mapped);
                                    let _ = sock.send_to(&resp, src).await;
                                }
                            }
                        }
                    }
                }
            });
        }

        // Primary S2 P1 (also dispatches to P2 and Helper according to NAT profile + pinhole rules)
        {
            let sock = Arc::clone(&p1_sock);
            let p2 = Arc::clone(&p2_sock);
            let helper = Arc::clone(&helper_sock);
            let c = cancel.clone();
            let contacted = Arc::clone(&contacted_by_client);
            let tok = expected_token.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        res = sock.recv_from(&mut buf) => {
                            let Ok((len, src)) = res else { continue };
                            contacted.lock().unwrap().insert(p1_addr);
                            let Ok(S2WireMessage::AddrEchoReq(req)) = serde_json::from_slice(&buf[..len]) else {
                                continue;
                            };
                            if let Some(exp) = tok.as_deref() {
                                if req.token.as_deref() != Some(exp) {
                                    continue;
                                }
                            }
                            let mapped = simulated_mapped_addr(profile, 2);
                            match req.mode {
                                EchoMode::Normal => {
                                    let msg = S2WireMessage::AddrEchoResp(AddrEchoResp {
                                        nonce: req.nonce,
                                        observed_addr: mapped.to_string(),
                                        responder_id: Some("sim-p1".into()),
                                    });
                                    let bytes = serde_json::to_vec(&msg).unwrap();
                                    let _ = sock.send_to(&bytes, src).await;
                                }
                                EchoMode::ReplyFromAltPort => {
                                    let pinhole_to_p2 = contacted.lock().unwrap().contains(&p2_addr);
                                    let allow = match profile {
                                        SimNatProfile::Nat1FullCone
                                        | SimNatProfile::Nat2AddressRestricted => true,
                                        SimNatProfile::Nat3PortRestricted
                                        | SimNatProfile::Nat4Symmetric => pinhole_to_p2,
                                    };
                                    if allow {
                                        let msg = S2WireMessage::AddrEchoResp(AddrEchoResp {
                                            nonce: req.nonce,
                                            observed_addr: mapped.to_string(),
                                            responder_id: Some("sim-p2".into()),
                                        });
                                        let bytes = serde_json::to_vec(&msg).unwrap();
                                        let _ = p2.send_to(&bytes, src).await;
                                    }
                                }
                                EchoMode::ReplyFromHelperIp => {
                                    let pinhole_to_helper = contacted.lock().unwrap().contains(&helper_addr);
                                    let allow = match profile {
                                        SimNatProfile::Nat1FullCone => true,
                                        SimNatProfile::Nat2AddressRestricted
                                        | SimNatProfile::Nat3PortRestricted
                                        | SimNatProfile::Nat4Symmetric => pinhole_to_helper,
                                    };
                                    if allow {
                                        let msg = S2WireMessage::AddrEchoResp(AddrEchoResp {
                                            nonce: req.nonce,
                                            observed_addr: mapped.to_string(),
                                            responder_id: Some("sim-helper".into()),
                                        });
                                        let bytes = serde_json::to_vec(&msg).unwrap();
                                        let _ = helper.send_to(&bytes, src).await;
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }

        // Primary S2 P2 (records direct pinhole if contacted by client)
        {
            let sock = Arc::clone(&p2_sock);
            let c = cancel.clone();
            let contacted = Arc::clone(&contacted_by_client);
            let tok = expected_token.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        res = sock.recv_from(&mut buf) => {
                            let Ok((len, src)) = res else { continue };
                            contacted.lock().unwrap().insert(p2_addr);
                            if let Ok(S2WireMessage::AddrEchoReq(req)) = serde_json::from_slice(&buf[..len]) {
                                if let Some(exp) = tok.as_deref() {
                                    if req.token.as_deref() != Some(exp) {
                                        continue;
                                    }
                                }
                                let mapped = simulated_mapped_addr(profile, 3);
                                let msg = S2WireMessage::AddrEchoResp(AddrEchoResp {
                                    nonce: req.nonce,
                                    observed_addr: mapped.to_string(),
                                    responder_id: Some("sim-p2".into()),
                                });
                                let bytes = serde_json::to_vec(&msg).unwrap();
                                let _ = sock.send_to(&bytes, src).await;
                            }
                        }
                    }
                }
            });
        }

        // Helper S2 (records direct pinhole if contacted by client)
        {
            let sock = Arc::clone(&helper_sock);
            let c = cancel.clone();
            let contacted = Arc::clone(&contacted_by_client);
            let tok = expected_token;
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        res = sock.recv_from(&mut buf) => {
                            let Ok((len, src)) = res else { continue };
                            contacted.lock().unwrap().insert(helper_addr);
                            if let Ok(S2WireMessage::AddrEchoReq(req)) = serde_json::from_slice(&buf[..len]) {
                                if let Some(exp) = tok.as_deref() {
                                    if req.token.as_deref() != Some(exp) {
                                        continue;
                                    }
                                }
                                let mapped = simulated_mapped_addr(profile, 4);
                                let msg = S2WireMessage::AddrEchoResp(AddrEchoResp {
                                    nonce: req.nonce,
                                    observed_addr: mapped.to_string(),
                                    responder_id: Some("sim-helper".into()),
                                });
                                let bytes = serde_json::to_vec(&msg).unwrap();
                                let _ = sock.send_to(&bytes, src).await;
                            }
                        }
                    }
                }
            });
        }

        // Optional IPv6 S2
        if let Some(sock) = v6_sock {
            let c = cancel.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        res = sock.recv_from(&mut buf) => {
                            let Ok((len, src)) = res else { continue };
                            if let Ok(S2WireMessage::AddrEchoReq(req)) = serde_json::from_slice(&buf[..len]) {
                                let mapped = SocketAddr::new(
                                    IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0x42)),
                                    49000,
                                );
                                let msg = S2WireMessage::AddrEchoResp(AddrEchoResp {
                                    nonce: req.nonce,
                                    observed_addr: mapped.to_string(),
                                    responder_id: Some("sim-v6".into()),
                                });
                                let bytes = serde_json::to_vec(&msg).unwrap();
                                let _ = sock.send_to(&bytes, src).await;
                            }
                        }
                    }
                }
            });
        }

        Self {
            stun1_addr,
            stun2_addr,
            p1_addr,
            p2_addr,
            helper_addr,
            v6_addr,
            cancel,
        }
    }
}

impl Drop for SimNatRig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn simulated_mapped_addr(profile: SimNatProfile, endpoint_idx: u16) -> SocketAddr {
    let base_port = match profile {
        SimNatProfile::Nat4Symmetric => 41000 + endpoint_idx,
        _ => 41000,
    };
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 88)), base_port)
}

async fn run_to_terminal(orch: &Orchestrator, input: NatTaskInput) -> AssessmentState {
    let mut rx = orch.subscribe();
    orch.start_nat_assessment(input).await.unwrap();
    loop {
        let cur = rx.borrow().clone();
        match cur {
            AssessmentState::Completed(_)
            | AssessmentState::Cancelled { .. }
            | AssessmentState::Failed(_) => return cur,
            AssessmentState::Idle | AssessmentState::Running { .. } => {
                rx.changed().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn e2e_public_stun_only_cone_symmetric_and_v6() {
    let dir = TempDir::new("stun_only");
    let orch = Orchestrator::new(&dir.path);

    let rig_cone = SimNatRig::start(SimNatProfile::Nat2AddressRestricted, None, true).await;
    let cone_state = run_to_terminal(
        &orch,
        NatTaskInput {
            public_stun_servers: vec![
                rig_cone.stun1_addr.to_string(),
                rig_cone.stun2_addr.to_string(),
            ],
            primary_v6: rig_cone.v6_addr.map(|a| a.to_string()),
            timeout_ms: 600,
            ..Default::default()
        },
    )
    .await;

    let AssessmentState::Completed(rep) = cone_state else {
        panic!("expected Completed, got {cone_state:?}");
    };
    let nat = rep.nat_report.unwrap();
    assert_eq!(nat.status, NatStatus::PublicStunOnly);
    assert_eq!(nat.mapping, NatMapping::EndpointIndependent);
    assert_eq!(nat.filtering, NatFiltering::NotTestedPublicStun);
    assert_eq!(nat.public_ipv4.as_deref(), Some("203.0.113.88"));
    if rig_cone.v6_addr.is_some() {
        assert!(nat.ipv6_reachable);
        assert_eq!(nat.public_ipv6.as_deref(), Some("2001:db8::42"));
    }

    let rig_sym = SimNatRig::start(SimNatProfile::Nat4Symmetric, None, false).await;
    let sym_state = run_to_terminal(
        &orch,
        NatTaskInput {
            public_stun_servers: vec![
                rig_sym.stun1_addr.to_string(),
                rig_sym.stun2_addr.to_string(),
            ],
            timeout_ms: 600,
            ..Default::default()
        },
    )
    .await;
    let AssessmentState::Completed(sym_rep) = sym_state else {
        panic!("expected Completed, got {sym_state:?}");
    };
    let sym_nat = sym_rep.nat_report.unwrap();
    assert_eq!(sym_nat.status, NatStatus::PublicStunOnly);
    assert_eq!(
        sym_nat.mapping,
        NatMapping::AddressOrPortDependentSymmetric
    );
    assert_eq!(sym_nat.filtering, NatFiltering::NotTestedPublicStun);
}

#[tokio::test]
async fn e2e_single_vps_mode_distinguishes_cone12_vs_nat3() {
    let dir = TempDir::new("single_vps");
    let orch = Orchestrator::new(&dir.path);

    let rig_nat2 = SimNatRig::start(SimNatProfile::Nat2AddressRestricted, Some("tok"), false).await;
    let state_12 = run_to_terminal(
        &orch,
        NatTaskInput {
            public_stun_servers: vec![rig_nat2.stun1_addr.to_string()],
            primary_v4_p1: Some(rig_nat2.p1_addr.to_string()),
            primary_v4_p2: Some(rig_nat2.p2_addr.to_string()),
            token: Some("tok".into()),
            timeout_ms: 400,
            ..Default::default()
        },
    )
    .await;
    let AssessmentState::Completed(r12) = state_12 else {
        panic!("expected Completed, got {state_12:?}");
    };
    let n12 = r12.nat_report.unwrap();
    assert_eq!(n12.status, NatStatus::CompleteSingleVps);
    assert_eq!(n12.mapping, NatMapping::EndpointIndependent);
    assert_eq!(n12.filtering, NatFiltering::ConeNat1OrNat2);

    let rig_nat3 = SimNatRig::start(SimNatProfile::Nat3PortRestricted, Some("tok"), false).await;
    let state_3 = run_to_terminal(
        &orch,
        NatTaskInput {
            public_stun_servers: vec![rig_nat3.stun1_addr.to_string()],
            primary_v4_p1: Some(rig_nat3.p1_addr.to_string()),
            primary_v4_p2: Some(rig_nat3.p2_addr.to_string()),
            token: Some("tok".into()),
            timeout_ms: 400,
            ..Default::default()
        },
    )
    .await;
    let AssessmentState::Completed(r3) = state_3 else {
        panic!("expected Completed, got {state_3:?}");
    };
    let n3 = r3.nat_report.unwrap();
    assert_eq!(n3.status, NatStatus::CompleteSingleVps);
    assert_eq!(n3.mapping, NatMapping::EndpointIndependent);
    assert_eq!(n3.filtering, NatFiltering::PortRestrictedNat3);
}

#[tokio::test]
async fn e2e_complete_full_mode_distinguishes_nat1_nat2_nat3_nat4() {
    let dir = TempDir::new("full_mode");
    let orch = Orchestrator::new(&dir.path);

    let cases = [
        (
            SimNatProfile::Nat1FullCone,
            NatMapping::EndpointIndependent,
            NatFiltering::FullConeNat1,
        ),
        (
            SimNatProfile::Nat2AddressRestricted,
            NatMapping::EndpointIndependent,
            NatFiltering::AddressRestrictedNat2,
        ),
        (
            SimNatProfile::Nat3PortRestricted,
            NatMapping::EndpointIndependent,
            NatFiltering::PortRestrictedNat3,
        ),
        (
            SimNatProfile::Nat4Symmetric,
            NatMapping::AddressOrPortDependentSymmetric,
            NatFiltering::PortRestrictedNat3,
        ),
    ];

    for (profile, expected_mapping, expected_filtering) in cases {
        let rig = SimNatRig::start(profile, Some("s3cr3t"), false).await;
        let state = run_to_terminal(
            &orch,
            NatTaskInput {
                public_stun_servers: vec![rig.stun1_addr.to_string()],
                primary_v4_p1: Some(rig.p1_addr.to_string()),
                primary_v4_p2: Some(rig.p2_addr.to_string()),
                helper_v4_p1: Some(rig.helper_addr.to_string()),
                token: Some("s3cr3t".into()),
                timeout_ms: 350,
                ..Default::default()
            },
        )
        .await;
        let AssessmentState::Completed(rep) = state else {
            panic!("expected Completed, got {state:?}");
        };
        let nat = rep.nat_report.unwrap();
        assert_eq!(nat.status, NatStatus::CompleteFull);
        assert_eq!(nat.mapping, expected_mapping);
        assert_eq!(nat.filtering, expected_filtering);
    }

    // Also verify all 4 reports persisted in C6 via C1 history APIs
    let list = orch.list_history().unwrap();
    assert_eq!(list.len(), 4);
    let first_id = list[0].report_id.clone();
    assert_eq!(orch.get_report(&first_id).unwrap().report_id, first_id);
    orch.delete_report(&first_id).unwrap();
    assert_eq!(orch.list_history().unwrap().len(), 3);
}

#[tokio::test]
async fn e2e_unreachable_mutex_cancel_and_storage_fault_tolerance() {
    // 1. All nodes unreachable -> Failed(ServerUnreachable)
    let dir = TempDir::new("faults");
    let orch = Orchestrator::new(&dir.path);
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let silent_addr = silent.local_addr().unwrap();

    let state = run_to_terminal(
        &orch,
        NatTaskInput {
            public_stun_servers: vec![silent_addr.to_string()],
            timeout_ms: 150,
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(
        state,
        AssessmentState::Failed(ProbeError::ServerUnreachable(_))
    ));
    assert!(orch.list_history().unwrap().is_empty());

    // 2. Single-task mutex + <=500ms cancellation
    let mut rx = orch.subscribe();
    orch.start_nat_assessment(NatTaskInput {
        public_stun_servers: vec![silent_addr.to_string()],
        timeout_ms: 5000,
        ..Default::default()
    })
    .await
    .unwrap();

    let dup = orch
        .start_nat_assessment(NatTaskInput {
            public_stun_servers: vec![silent_addr.to_string()],
            timeout_ms: 500,
            ..Default::default()
        })
        .await;
    assert_eq!(
        dup,
        Err(ProbeError::InvalidInput(TASK_ALREADY_RUNNING_MSG.into()))
    );

    let t0 = Instant::now();
    orch.cancel_current(Some("e2e_cancel".into())).await.unwrap();
    assert!(t0.elapsed() < Duration::from_millis(500));
    loop {
        let cur = rx.borrow().clone();
        if matches!(cur, AssessmentState::Cancelled { .. }) {
            break;
        }
        rx.changed().await.unwrap();
    }

    // 3. Storage failure preserves report with storage_warning
    let blocked_dir = TempDir::new("blocked_store");
    fs::write(&blocked_dir.path, b"file-blocking-dir").unwrap();
    let blocked_orch = Orchestrator::new(&blocked_dir.path);
    let rig = SimNatRig::start(SimNatProfile::Nat1FullCone, None, false).await;
    let completed = run_to_terminal(
        &blocked_orch,
        NatTaskInput {
            public_stun_servers: vec![
                rig.stun1_addr.to_string(),
                rig.stun2_addr.to_string(),
            ],
            timeout_ms: 400,
            ..Default::default()
        },
    )
    .await;
    let AssessmentState::Completed(rep) = completed else {
        panic!("expected Completed even when store fails, got {completed:?}");
    };
    assert!(rep.nat_report.is_some());
    assert!(rep.storage_warning.is_some());
}
