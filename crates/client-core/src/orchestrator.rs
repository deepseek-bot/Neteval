use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{
    validate_room_code, AssessmentReport, AssessmentState, DEFAULT_PUBLIC_STUN_SERVERS,
    KEEPALIVE_INTERVAL_SECS, MAX_PACKET_RATE_HZ, MAX_PUNCH_TIMEOUT_MS, MAX_WINDOW_SECS,
    MIN_PACKET_RATE_HZ, MIN_PUNCH_TIMEOUT_MS, MIN_WINDOW_SECS, NatStatus, NatTaskInput,
    P2pTaskInput, ProbeError, ReportSummary, TASK_ALREADY_RUNNING_MSG,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::local_store::LocalStore;
use crate::logger::{error, info, warn};
use crate::nat_prober;
use crate::p2p_engine::{self, P2pEvent};
use crate::signaling_client;

const LOG_MOD: &str = "c1";
const CANCEL_WAIT_MS: u64 = 400;

static TASK_SEQ: AtomicU64 = AtomicU64::new(1);

struct ActiveTask {
    task_id: String,
    cancel: CancellationToken,
    cancel_reason: Arc<Mutex<Option<String>>>,
    handle: JoinHandle<()>,
}

struct OrchestratorInner {
    store: LocalStore,
    state_tx: watch::Sender<AssessmentState>,
    active: Mutex<Option<ActiveTask>>,
}

#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<OrchestratorInner>,
}

impl Orchestrator {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let (state_tx, _rx) = watch::channel(AssessmentState::Idle);
        Self {
            inner: Arc::new(OrchestratorInner {
                store: LocalStore::new(data_dir),
                state_tx,
                active: Mutex::new(None),
            }),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<AssessmentState> {
        self.inner.state_tx.subscribe()
    }

    pub fn state(&self) -> AssessmentState {
        self.inner.state_tx.borrow().clone()
    }

    pub fn data_dir(&self) -> &std::path::Path {
        self.inner.store.dir()
    }

    pub fn default_nat_input() -> NatTaskInput {
        NatTaskInput {
            public_stun_servers: DEFAULT_PUBLIC_STUN_SERVERS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            ..NatTaskInput::default()
        }
    }

    pub async fn start_nat_assessment(&self, input: NatTaskInput) -> Result<String, ProbeError> {
        let mut active_guard = self.inner.active.lock().map_err(|_| {
            error(LOG_MOD, "orchestrator active mutex poisoned");
            ProbeError::Internal("orchestrator lock poisoned".into())
        })?;

        if active_guard.is_some() {
            warn(
                LOG_MOD,
                "rejected start_nat_assessment: task_already_running",
            );
            return Err(ProbeError::InvalidInput(TASK_ALREADY_RUNNING_MSG.into()));
        }

        let created_unix_ms = now_unix_ms();
        let seq = TASK_SEQ.fetch_add(1, Ordering::Relaxed);
        let task_id = format!("nat_{created_unix_ms}_{seq}");
        let cancel = CancellationToken::new();
        let cancel_reason = Arc::new(Mutex::new(None));

        let initial_state = AssessmentState::Running {
            task_id: task_id.clone(),
            stage: "probing_nat".into(),
            progress_pct: 15,
            live_point: None,
        };
        let _ = self.inner.state_tx.send(initial_state);
        info(LOG_MOD, format!("started NAT assessment task_id={task_id}"));

        let inner = Arc::clone(&self.inner);
        let task_id_clone = task_id.clone();
        let cancel_clone = cancel.clone();
        let reason_clone = Arc::clone(&cancel_reason);

        let handle = tokio::spawn(async move {
            run_nat_task(
                inner,
                task_id_clone,
                created_unix_ms,
                input,
                cancel_clone,
                reason_clone,
            )
            .await;
        });

        *active_guard = Some(ActiveTask {
            task_id: task_id.clone(),
            cancel,
            cancel_reason,
            handle,
        });

        Ok(task_id)
    }

    pub async fn start_p2p_assessment(&self, input: P2pTaskInput) -> Result<String, ProbeError> {
        let mut active_guard = self.inner.active.lock().map_err(|_| {
            error(LOG_MOD, "orchestrator active mutex poisoned");
            ProbeError::Internal("orchestrator lock poisoned".into())
        })?;

        if active_guard.is_some() {
            warn(
                LOG_MOD,
                "rejected start_p2p_assessment: task_already_running",
            );
            return Err(ProbeError::InvalidInput(TASK_ALREADY_RUNNING_MSG.into()));
        }

        validate_p2p_input(&input)?;

        let created_unix_ms = now_unix_ms();
        let seq = TASK_SEQ.fetch_add(1, Ordering::Relaxed);
        let task_id = format!("p2p_{created_unix_ms}_{seq}");
        let cancel = CancellationToken::new();
        let cancel_reason = Arc::new(Mutex::new(None));

        let initial_state = AssessmentState::Running {
            task_id: task_id.clone(),
            stage: "preparing_candidates".into(),
            progress_pct: 10,
            live_point: None,
        };
        let _ = self.inner.state_tx.send(initial_state);
        info(
            LOG_MOD,
            format!(
                "started P2P assessment task_id={task_id} room={} peer_label={}",
                input.room_code, input.peer_label
            ),
        );

        let inner = Arc::clone(&self.inner);
        let task_id_clone = task_id.clone();
        let cancel_clone = cancel.clone();
        let reason_clone = Arc::clone(&cancel_reason);

        let handle = tokio::spawn(async move {
            run_p2p_task(
                inner,
                task_id_clone,
                created_unix_ms,
                input,
                cancel_clone,
                reason_clone,
            )
            .await;
        });

        *active_guard = Some(ActiveTask {
            task_id: task_id.clone(),
            cancel,
            cancel_reason,
            handle,
        });

        Ok(task_id)
    }

    pub async fn cancel_current(&self, reason: Option<String>) -> Result<(), ProbeError> {
        let active = {
            let mut guard = self.inner.active.lock().map_err(|_| {
                error(
                    LOG_MOD,
                    "orchestrator active mutex poisoned in cancel_current",
                );
                ProbeError::Internal("orchestrator lock poisoned".into())
            })?;
            guard.take()
        };

        let Some(mut active) = active else {
            info(LOG_MOD, "cancel_current called while no task is active");
            return Ok(());
        };

        info(
            LOG_MOD,
            format!(
                "cancelling active task_id={} reason={:?}",
                active.task_id, reason
            ),
        );

        if let Ok(mut guard) = active.cancel_reason.lock() {
            *guard = reason.clone();
        }
        active.cancel.cancel();

        let wait_res = tokio::time::timeout(
            Duration::from_millis(CANCEL_WAIT_MS),
            &mut active.handle,
        )
        .await;

        match wait_res {
            Ok(Ok(())) => {
                info(
                    LOG_MOD,
                    format!("task_id={} stopped cleanly after cancel", active.task_id),
                );
            }
            Ok(Err(join_err)) => {
                warn(
                    LOG_MOD,
                    format!("task_id={} join error on cancel: {join_err}", active.task_id),
                );
                let _ = self.inner.state_tx.send(AssessmentState::Cancelled { reason });
            }
            Err(_) => {
                warn(
                    LOG_MOD,
                    format!(
                        "task_id={} did not finish within {CANCEL_WAIT_MS}ms; aborting handle",
                        active.task_id
                    ),
                );
                active.handle.abort();
                let _ = active.handle.await;
                let _ = self.inner.state_tx.send(AssessmentState::Cancelled { reason });
            }
        }

        Ok(())
    }

    pub fn list_history(&self) -> Result<Vec<ReportSummary>, ProbeError> {
        self.inner.store.list_reports()
    }

    pub fn get_report(&self, id: &str) -> Result<AssessmentReport, ProbeError> {
        self.inner.store.load_report(id)
    }

    pub fn delete_report(&self, id: &str) -> Result<(), ProbeError> {
        self.inner.store.delete_report(id)
    }
}

async fn run_nat_task(
    inner: Arc<OrchestratorInner>,
    task_id: String,
    created_unix_ms: u64,
    input: NatTaskInput,
    cancel: CancellationToken,
    cancel_reason: Arc<Mutex<Option<String>>>,
) {
    let probe_res = nat_prober::run_nat_probe(&input, cancel.clone()).await;

    let final_state = match probe_res {
        Err(ProbeError::Cancelled) => {
            let reason = cancel_reason.lock().ok().and_then(|g| g.clone());
            info(
                LOG_MOD,
                format!("task_id={task_id} cancelled during probing_nat"),
            );
            AssessmentState::Cancelled { reason }
        }
        Err(err) => {
            warn(
                LOG_MOD,
                format!("task_id={task_id} failed during probing_nat: {err}"),
            );
            AssessmentState::Failed(err)
        }
        Ok(nat_report) if nat_report.status == NatStatus::Unreachable => {
            let reason = if nat_report.notes.is_empty() {
                "all probe nodes unreachable".to_string()
            } else {
                nat_report.notes.join("; ")
            };
            warn(
                LOG_MOD,
                format!("task_id={task_id} unreachable -> Failed(server_unreachable): {reason}"),
            );
            AssessmentState::Failed(ProbeError::ServerUnreachable(reason))
        }
        Ok(nat_report) => {
            if cancel.is_cancelled() {
                let reason = cancel_reason.lock().ok().and_then(|g| g.clone());
                info(
                    LOG_MOD,
                    format!("task_id={task_id} cancelled before saving_report"),
                );
                AssessmentState::Cancelled { reason }
            } else {
                let _ = inner.state_tx.send(AssessmentState::Running {
                    task_id: task_id.clone(),
                    stage: "saving_report".into(),
                    progress_pct: 90,
                    live_point: None,
                });

                let mut report = AssessmentReport {
                    report_id: task_id.clone(),
                    created_unix_ms,
                    nat_report: Some(nat_report),
                    p2p_report: None,
                    storage_warning: None,
                };

                if let Err(err) = inner.store.save_report(&report) {
                    warn(
                        LOG_MOD,
                        format!(
                            "task_id={task_id} saving_report failed; preserving report with storage_warning: {err}"
                        ),
                    );
                    report.storage_warning = Some(err.to_string());
                }

                info(
                    LOG_MOD,
                    format!("task_id={task_id} completed successfully"),
                );
                AssessmentState::Completed(report)
            }
        }
    };

    // Clear the active slot BEFORE broadcasting terminal state so a subscriber
    // reacting immediately to Completed/Cancelled/Failed can start a new task.
    if let Ok(mut guard) = inner.active.lock() {
        if guard
            .as_ref()
            .is_some_and(|active| active.task_id == task_id)
        {
            *guard = None;
        }
    }

    let _ = inner.state_tx.send(final_state);
}

fn validate_p2p_input(input: &P2pTaskInput) -> Result<(), ProbeError> {
    validate_room_code(&input.room_code)?;
    if input.peer_label.trim().is_empty() {
        return Err(ProbeError::InvalidInput(
            "peer_label must not be empty".into(),
        ));
    }
    if input.signaling_ws_url.trim().is_empty() {
        return Err(ProbeError::InvalidInput(
            "signaling_ws_url must not be empty".into(),
        ));
    }
    if !(MIN_PUNCH_TIMEOUT_MS..=MAX_PUNCH_TIMEOUT_MS).contains(&input.punch_timeout_ms) {
        return Err(ProbeError::InvalidInput(format!(
            "punch_timeout_ms {} out of range {MIN_PUNCH_TIMEOUT_MS}..={MAX_PUNCH_TIMEOUT_MS}",
            input.punch_timeout_ms
        )));
    }
    if !(MIN_WINDOW_SECS..=MAX_WINDOW_SECS).contains(&input.window_secs) {
        return Err(ProbeError::InvalidInput(format!(
            "window_secs {} out of range {MIN_WINDOW_SECS}..={MAX_WINDOW_SECS}",
            input.window_secs
        )));
    }
    if !(MIN_PACKET_RATE_HZ..=MAX_PACKET_RATE_HZ).contains(&input.packet_rate_hz) {
        return Err(ProbeError::InvalidInput(format!(
            "packet_rate_hz {} out of range {MIN_PACKET_RATE_HZ}..={MAX_PACKET_RATE_HZ}",
            input.packet_rate_hz
        )));
    }
    Ok(())
}

async fn run_p2p_task(
    inner: Arc<OrchestratorInner>,
    task_id: String,
    created_unix_ms: u64,
    input: P2pTaskInput,
    cancel: CancellationToken,
    cancel_reason: Arc<Mutex<Option<String>>>,
) {
    let final_state = execute_p2p_lifecycle(
        &inner,
        &task_id,
        created_unix_ms,
        &input,
        &cancel,
        &cancel_reason,
    )
    .await;

    if let Ok(mut guard) = inner.active.lock() {
        if guard
            .as_ref()
            .is_some_and(|active| active.task_id == task_id)
        {
            *guard = None;
        }
    }

    let _ = inner.state_tx.send(final_state);
}

async fn execute_p2p_lifecycle(
    inner: &Arc<OrchestratorInner>,
    task_id: &str,
    created_unix_ms: u64,
    input: &P2pTaskInput,
    cancel: &CancellationToken,
    cancel_reason: &Arc<Mutex<Option<String>>>,
) -> AssessmentState {
    // Stage 1: preparing_candidates
    let (session_handle, local_candidates) = match p2p_engine::prepare_session(
        input.echo_udp_v4.as_deref(),
        input.echo_udp_v6.as_deref(),
        &input.public_stun_servers,
        input.token.as_deref(),
        input.candidate_timeout_ms,
        cancel.clone(),
    )
    .await
    {
        Ok(res) => res,
        Err(ProbeError::Cancelled) => {
            let reason = cancel_reason.lock().ok().and_then(|g| g.clone());
            info(
                LOG_MOD,
                format!("task_id={task_id} cancelled during preparing_candidates"),
            );
            return AssessmentState::Cancelled { reason };
        }
        Err(err) => {
            warn(
                LOG_MOD,
                format!("task_id={task_id} failed during preparing_candidates: {err}"),
            );
            return AssessmentState::Failed(err);
        }
    };

    if cancel.is_cancelled() {
        p2p_engine::abort_session(session_handle);
        let reason = cancel_reason.lock().ok().and_then(|g| g.clone());
        return AssessmentState::Cancelled { reason };
    }

    // Stage 2: waiting_for_peer (+ 15s UDP keepalive)
    let _ = inner.state_tx.send(AssessmentState::Running {
        task_id: task_id.to_string(),
        stage: "waiting_for_peer".into(),
        progress_pct: 25,
        live_point: None,
    });

    let pairing_res = {
        let pair_fut = signaling_client::pair_in_room(
            &input.signaling_ws_url,
            &input.room_code,
            input.token.as_deref(),
            &input.peer_label,
            local_candidates,
            input.pair_timeout_secs,
            cancel.clone(),
        );
        tokio::pin!(pair_fut);

        let mut keepalive_tick =
            tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
        keepalive_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        keepalive_tick.tick().await;

        loop {
            tokio::select! {
                res = &mut pair_fut => break res,
                _ = keepalive_tick.tick() => {
                    if let Err(err) = p2p_engine::keepalive_session(
                        &session_handle,
                        input.echo_udp_v4.as_deref(),
                        input.echo_udp_v6.as_deref(),
                        &input.public_stun_servers,
                        input.token.as_deref(),
                    ).await {
                        warn(
                            LOG_MOD,
                            format!("task_id={task_id} waiting keepalive warning: {err}"),
                        );
                    }
                }
            }
        }
    };

    let (pairing, signal_guard) = match pairing_res {
        Ok(res) => res,
        Err(ProbeError::Cancelled) => {
            p2p_engine::abort_session(session_handle);
            let reason = cancel_reason.lock().ok().and_then(|g| g.clone());
            info(
                LOG_MOD,
                format!("task_id={task_id} cancelled during waiting_for_peer"),
            );
            return AssessmentState::Cancelled { reason };
        }
        Err(err) => {
            p2p_engine::abort_session(session_handle);
            warn(
                LOG_MOD,
                format!("task_id={task_id} failed during waiting_for_peer: {err}"),
            );
            return AssessmentState::Failed(err);
        }
    };

    // Stage 3 & 4: punching + sampling
    let _ = inner.state_tx.send(AssessmentState::Running {
        task_id: task_id.to_string(),
        stage: "punching".into(),
        progress_pct: 45,
        live_point: None,
    });

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<P2pEvent>();
    let probe_cancel = cancel.child_token();
    let total_packets = input.window_secs.saturating_mul(input.packet_rate_hz).max(1);

    let probe_fut = p2p_engine::execute_probe(
        session_handle,
        &pairing.session_id,
        &pairing.peer_candidates,
        pairing.start_delay_ms,
        input.punch_timeout_ms,
        input.window_secs,
        input.packet_rate_hz,
        input.strategy,
        move |ev| {
            let _ = event_tx.send(ev);
        },
        probe_cancel.clone(),
    );
    tokio::pin!(probe_fut);

    let mut guard_opt = Some(signal_guard);
    let mut punch_confirmed = false;
    let mut peer_left_during_punch = false;
    let mut local_cancelled = false;
    let mut event_rx_closed = false;

    let probe_res = loop {
        tokio::select! {
            ev_opt = event_rx.recv(), if !event_rx_closed => {
                match ev_opt {
                    Some(ev) => {
                        handle_p2p_event(
                            inner,
                            task_id,
                            total_packets,
                            ev,
                            &mut punch_confirmed,
                            &mut guard_opt,
                        ).await;
                    }
                    None => {
                        event_rx_closed = true;
                    }
                }
            }
            left = async {
                match guard_opt.as_mut() {
                    Some(g) => g.wait_peer_left().await,
                    None => std::future::pending::<bool>().await,
                }
            }, if guard_opt.is_some() && !punch_confirmed && !local_cancelled => {
                if let Some(g) = guard_opt.take() {
                    g.close().await;
                }
                if left {
                    warn(
                        LOG_MOD,
                        format!("task_id={task_id} received peer_left during punching"),
                    );
                    peer_left_during_punch = true;
                    probe_cancel.cancel();
                }
            }
            _ = cancel.cancelled(), if !local_cancelled => {
                local_cancelled = true;
                info(
                    LOG_MOD,
                    format!("task_id={task_id} local cancel triggered during P2P probe"),
                );
                if let Some(g) = guard_opt.take() {
                    g.send_leave_and_close().await;
                }
                probe_cancel.cancel();
            }
            res = &mut probe_fut => break res,
        }
    };

    while let Ok(ev) = event_rx.try_recv() {
        handle_p2p_event(
            inner,
            task_id,
            total_packets,
            ev,
            &mut punch_confirmed,
            &mut guard_opt,
        )
        .await;
    }

    if let Some(g) = guard_opt.take() {
        if local_cancelled || (cancel.is_cancelled() && !peer_left_during_punch) {
            g.send_leave_and_close().await;
        } else {
            g.close().await;
        }
    }

    match probe_res {
        Err(ProbeError::Cancelled) => {
            if peer_left_during_punch {
                AssessmentState::Failed(ProbeError::PeerLeft)
            } else {
                let reason = cancel_reason.lock().ok().and_then(|g| g.clone());
                AssessmentState::Cancelled { reason }
            }
        }
        Err(err) => {
            warn(
                LOG_MOD,
                format!("task_id={task_id} failed during execute_probe: {err}"),
            );
            AssessmentState::Failed(err)
        }
        Ok(mut p2p_report) => {
            if peer_left_during_punch && !p2p_report.punched {
                return AssessmentState::Failed(ProbeError::PeerLeft);
            }
            if cancel.is_cancelled() {
                let reason = cancel_reason.lock().ok().and_then(|g| g.clone());
                return AssessmentState::Cancelled { reason };
            }

            p2p_report.peer_label = Some(pairing.peer_label);

            // Stage 5: saving_report
            let _ = inner.state_tx.send(AssessmentState::Running {
                task_id: task_id.to_string(),
                stage: "saving_report".into(),
                progress_pct: 95,
                live_point: None,
            });

            let mut report = AssessmentReport {
                report_id: task_id.to_string(),
                created_unix_ms,
                nat_report: None,
                p2p_report: Some(p2p_report),
                storage_warning: None,
            };

            if let Err(err) = inner.store.save_report(&report) {
                warn(
                    LOG_MOD,
                    format!(
                        "task_id={task_id} saving_report failed; preserving report with storage_warning: {err}"
                    ),
                );
                report.storage_warning = Some(err.to_string());
            }

            info(
                LOG_MOD,
                format!("task_id={task_id} P2P assessment completed"),
            );
            AssessmentState::Completed(report)
        }
    }
}

async fn handle_p2p_event(
    inner: &Arc<OrchestratorInner>,
    task_id: &str,
    total_packets: u32,
    ev: P2pEvent,
    punch_confirmed: &mut bool,
    guard_opt: &mut Option<signaling_client::SignalGuard>,
) {
    match ev {
        P2pEvent::PunchSucceeded { .. } => {
            *punch_confirmed = true;
            if let Some(g) = guard_opt.take() {
                g.close().await;
            }
            let _ = inner.state_tx.send(AssessmentState::Running {
                task_id: task_id.to_string(),
                stage: "sampling".into(),
                progress_pct: 55,
                live_point: None,
            });
        }
        P2pEvent::LatencyPoint(pt) => {
            let pct = 55u8.saturating_add(((pt.seq.min(total_packets) * 35) / total_packets) as u8);
            let _ = inner.state_tx.send(AssessmentState::Running {
                task_id: task_id.to_string(),
                stage: "sampling".into(),
                progress_pct: pct,
                live_point: Some(pt),
            });
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{NatFiltering, NatMapping, NatReport};
    use std::fs;
    use std::net::SocketAddr;
    use std::time::Instant;
    use tokio::net::UdpSocket;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let seq = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "neteval_c1_{label}_{}_{seq}",
                std::process::id()
            ));
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    async fn wait_for_terminal(
        rx: &mut watch::Receiver<AssessmentState>,
    ) -> AssessmentState {
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

    async fn spawn_mock_stun(mapped: SocketAddr) -> (SocketAddr, CancellationToken) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                tokio::select! {
                    _ = c.cancelled() => break,
                    res = sock.recv_from(&mut buf) => {
                        if let Ok((len, src)) = res {
                            if let Some(txn) = nat_prober::parse_stun_binding_request(&buf[..len]) {
                                let resp = nat_prober::build_stun_binding_response(txn, mapped);
                                let _ = sock.send_to(&resp, src).await;
                            }
                        }
                    }
                }
            }
        });
        (addr, cancel)
    }

    #[tokio::test]
    async fn single_task_mutex_and_restart_after_completion() {
        let dir = TestDir::new("mutex");
        let (stun1, c1) = spawn_mock_stun("203.0.113.10:40000".parse().unwrap()).await;
        let (stun2, c2) = spawn_mock_stun("203.0.113.10:40000".parse().unwrap()).await;

        let orch = Orchestrator::new(&dir.path);
        let mut rx = orch.subscribe();

        let input = NatTaskInput {
            public_stun_servers: vec![stun1.to_string(), stun2.to_string()],
            timeout_ms: 800,
            ..Default::default()
        };

        let task_id = orch.start_nat_assessment(input.clone()).await.unwrap();
        let dup = orch.start_nat_assessment(input.clone()).await;
        assert_eq!(
            dup,
            Err(ProbeError::InvalidInput(TASK_ALREADY_RUNNING_MSG.into()))
        );

        let term = wait_for_terminal(&mut rx).await;
        match term {
            AssessmentState::Completed(report) => {
                assert_eq!(report.report_id, task_id);
                assert!(report.storage_warning.is_none());
                let nat = report.nat_report.unwrap();
                assert_eq!(nat.status, NatStatus::PublicStunOnly);
                assert_eq!(nat.mapping, NatMapping::EndpointIndependent);
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        let list = orch.list_history().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].report_id, task_id);
        assert_eq!(orch.get_report(&task_id).unwrap().report_id, task_id);

        // Must allow starting another task immediately after Completed
        let task_id2 = orch.start_nat_assessment(input).await.unwrap();
        assert_ne!(task_id, task_id2);
        let term2 = wait_for_terminal(&mut rx).await;
        assert!(matches!(term2, AssessmentState::Completed(_)));

        c1.cancel();
        c2.cancel();
    }

    #[tokio::test]
    async fn unreachable_transitions_to_failed_server_unreachable() {
        let dir = TestDir::new("unreachable");
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();

        let orch = Orchestrator::new(&dir.path);
        let mut rx = orch.subscribe();

        let input = NatTaskInput {
            public_stun_servers: vec![silent_addr.to_string()],
            timeout_ms: 200,
            ..Default::default()
        };

        orch.start_nat_assessment(input).await.unwrap();
        let term = wait_for_terminal(&mut rx).await;
        assert!(matches!(term, AssessmentState::Failed(ProbeError::ServerUnreachable(_))));
        assert!(orch.list_history().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancel_completes_under_500ms_and_sets_reason() {
        let dir = TestDir::new("cancel");
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();

        let orch = Orchestrator::new(&dir.path);
        let mut rx = orch.subscribe();

        let input = NatTaskInput {
            public_stun_servers: vec![silent_addr.to_string()],
            timeout_ms: 5000,
            ..Default::default()
        };

        orch.start_nat_assessment(input).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let started = Instant::now();
        orch.cancel_current(Some("user_abort".into())).await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel took {elapsed:?}"
        );

        let term = wait_for_terminal(&mut rx).await;
        assert_eq!(
            term,
            AssessmentState::Cancelled {
                reason: Some("user_abort".into())
            }
        );
    }

    #[tokio::test]
    async fn storage_failure_attaches_warning_and_still_completes() {
        let dir = TestDir::new("storage_warn");
        // Create a regular file at the data_dir path so create_dir_all fails on save_report.
        fs::write(&dir.path, b"not-a-directory").unwrap();

        let (stun1, c1) = spawn_mock_stun("198.51.100.20:30000".parse().unwrap()).await;
        let (stun2, c2) = spawn_mock_stun("198.51.100.20:30000".parse().unwrap()).await;

        let orch = Orchestrator::new(&dir.path);
        let mut rx = orch.subscribe();

        let input = NatTaskInput {
            public_stun_servers: vec![stun1.to_string(), stun2.to_string()],
            timeout_ms: 800,
            ..Default::default()
        };

        orch.start_nat_assessment(input).await.unwrap();
        let term = wait_for_terminal(&mut rx).await;
        match term {
            AssessmentState::Completed(report) => {
                assert!(report.nat_report.is_some());
                assert!(report.storage_warning.is_some());
                let nat: NatReport = report.nat_report.unwrap();
                assert_eq!(nat.filtering, NatFiltering::NotTestedPublicStun);
            }
            other => panic!("expected Completed with storage_warning, got {other:?}"),
        }

        c1.cancel();
        c2.cancel();
    }
}
