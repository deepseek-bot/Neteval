use crate::log_sink;
use futures_util::{SinkExt, StreamExt};
use protocol::{
    validate_room_code, ClientSignalMsg, ProbeError, ServerSignalMsg, SignalErrorMsg,
    StartPunchMsg, WaitingForPeerMsg, DEFAULT_PUNCH_START_DELAY_MS, DEFAULT_PUNCH_TIMEOUT_MS,
    MAX_ROOMS, PUNCH_ROOM_GRACE_MS, ROOM_WAIT_TTL_SECS,
};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{accept_async_with_config, WebSocketStream};
use tokio_util::sync::CancellationToken;

const LOG_MODULE: &str = "s1";
const DEFAULT_SIGNALING_WS_PORT: u16 = 34780;
const MAX_WS_MESSAGE_BYTES: usize = 64 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const JOIN_MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_WAIT_MS: u64 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalingServerConfig {
    pub bind_ws: SocketAddr,
    pub bind_ws_v6: Option<SocketAddr>,
    pub bind_ws_v6_best_effort: bool,
    pub access_token: Option<String>,
    pub wait_ttl: Duration,
    pub start_delay_ms: u32,
    pub punch_duration_ms: u32,
}

impl Default for SignalingServerConfig {
    fn default() -> Self {
        Self {
            bind_ws: SocketAddr::from(([0, 0, 0, 0], DEFAULT_SIGNALING_WS_PORT)),
            bind_ws_v6: None,
            bind_ws_v6_best_effort: false,
            access_token: None,
            wait_ttl: Duration::from_secs(ROOM_WAIT_TTL_SECS),
            start_delay_ms: DEFAULT_PUNCH_START_DELAY_MS,
            punch_duration_ms: DEFAULT_PUNCH_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalingServerBound {
    pub ws_addr: SocketAddr,
    pub ws_v6_addr: Option<SocketAddr>,
}

struct ServerState {
    config: SignalingServerConfig,
    rooms: Mutex<HashMap<String, RoomEntry>>,
    next_id: AtomicU64,
}

impl ServerState {
    fn next_unique_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

struct WaitingRoom {
    peer1_id: u64,
    peer1_label: String,
    peer1_candidates: Vec<protocol::Candidate>,
    peer1_tx: mpsc::UnboundedSender<OutboundSignal>,
    expires_at: Instant,
}

struct PairedRoom {
    session_id: String,
    peer1_id: u64,
    peer1_tx: Option<mpsc::UnboundedSender<OutboundSignal>>,
    peer2_id: u64,
    peer2_tx: Option<mpsc::UnboundedSender<OutboundSignal>>,
    expires_at: Instant,
}

enum RoomEntry {
    Waiting(WaitingRoom),
    Paired(PairedRoom),
}

impl RoomEntry {
    fn expires_at(&self) -> Instant {
        match self {
            Self::Waiting(w) => w.expires_at,
            Self::Paired(p) => p.expires_at,
        }
    }
}

#[derive(Debug)]
enum OutboundSignal {
    StartPunch {
        msg: StartPunchMsg,
        expires_at: Instant,
    },
    TimeoutClose,
    PeerLeftClose,
    GraceExpiredClose,
}

enum JoinOutcome {
    Waiting {
        room_code: String,
        expires_at: Instant,
        sig_rx: mpsc::UnboundedReceiver<OutboundSignal>,
    },
    Paired {
        room_code: String,
        session_id: String,
        expires_at: Instant,
        sig_rx: mpsc::UnboundedReceiver<OutboundSignal>,
    },
    Rejected,
}

enum PeerPhase {
    Waiting,
    Paired { session_id: String },
}

fn bind_tcp_listener(
    role_label: &str,
    addr: SocketAddr,
) -> std::io::Result<(TcpListener, SocketAddr)> {
    let std_listener: std::net::TcpListener = (|| {
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        #[cfg(unix)]
        socket.set_reuse_address(true)?;
        if addr.is_ipv6() {
            socket.set_only_v6(true)?;
        }
        socket.set_nonblocking(true)?;
        socket.bind(&SockAddr::from(addr))?;
        socket.listen(1024)?;
        Ok::<std::net::TcpListener, std::io::Error>(socket.into())
    })()
    .map_err(|err| {
        log_sink::error(
            LOG_MODULE,
            format!("failed to bind {role_label} socket on {addr}: {err}"),
        );
        err
    })?;

    let listener = TcpListener::from_std(std_listener).map_err(|err| {
        log_sink::error(
            LOG_MODULE,
            format!("failed to register {role_label} socket on {addr}: {err}"),
        );
        err
    })?;

    let local = listener.local_addr().map_err(|err| {
        log_sink::error(
            LOG_MODULE,
            format!("failed to read local_addr for {role_label} socket: {err}"),
        );
        err
    })?;

    log_sink::info(LOG_MODULE, format!("bound {role_label} socket on {local}"));
    Ok((listener, local))
}

pub async fn start_signaling_server(
    config: SignalingServerConfig,
    cancel: CancellationToken,
) -> std::io::Result<(SignalingServerBound, JoinHandle<()>)> {
    let (listener_v4, ws_addr) = bind_tcp_listener("ws", config.bind_ws)?;

    let (listener_v6, ws_v6_addr) = match config.bind_ws_v6 {
        Some(v6_addr) => match bind_tcp_listener("ws-v6", v6_addr) {
            Ok((listener, local)) => (Some(listener), Some(local)),
            Err(err) if config.bind_ws_v6_best_effort => {
                log_sink::warn(
                    LOG_MODULE,
                    format!(
                        "optional IPv6 ws socket {v6_addr} unavailable ({err}); continuing IPv4-only"
                    ),
                );
                (None, None)
            }
            Err(err) => return Err(err),
        },
        None => (None, None),
    };

    log_sink::info(
        LOG_MODULE,
        format!(
            "signaling server started ws={ws_addr} ws_v6={ws_v6_addr:?} token_configured={} wait_ttl_ms={} start_delay_ms={} punch_duration_ms={}",
            config.access_token.is_some(),
            config.wait_ttl.as_millis(),
            config.start_delay_ms,
            config.punch_duration_ms
        ),
    );

    let state = Arc::new(ServerState {
        config,
        rooms: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    });

    let bound = SignalingServerBound {
        ws_addr,
        ws_v6_addr,
    };
    let handle = tokio::spawn(run_accept_loop(listener_v4, listener_v6, state, cancel));
    Ok((bound, handle))
}

pub async fn run_signaling_server(
    config: SignalingServerConfig,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let (_bound, handle) = start_signaling_server(config, cancel).await?;
    handle.await.map_err(|err| {
        log_sink::error(
            LOG_MODULE,
            format!("signaling server coordinator task failed: {err}"),
        );
        std::io::Error::other(format!("signaling server coordinator task failed: {err}"))
    })
}

async fn handle_accept_result(
    accept_res: std::io::Result<(TcpStream, SocketAddr)>,
    join_set: &mut JoinSet<()>,
    state: &Arc<ServerState>,
    cancel: &CancellationToken,
) -> bool {
    match accept_res {
        Ok((stream, peer_addr)) => {
            let st = Arc::clone(state);
            let tok = cancel.clone();
            join_set.spawn(async move {
                handle_connection(stream, peer_addr, st, tok).await;
            });
            true
        }
        Err(err) => {
            log_sink::warn(LOG_MODULE, format!("ws accept error: {err}"));
            tokio::select! {
                biased;
                _ = cancel.cancelled() => false,
                _ = tokio::time::sleep(Duration::from_millis(10)) => true,
            }
        }
    }
}

async fn run_accept_loop(
    listener_v4: TcpListener,
    listener_v6: Option<TcpListener>,
    state: Arc<ServerState>,
    cancel: CancellationToken,
) {
    let mut join_set = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                break;
            }
            Some(join_res) = join_set.join_next(), if !join_set.is_empty() => {
                if let Err(err) = join_res {
                    if !err.is_cancelled() {
                        log_sink::error(LOG_MODULE, format!("client connection task panicked: {err}"));
                    }
                }
            }
            accept_res = listener_v4.accept() => {
                if !handle_accept_result(accept_res, &mut join_set, &state, &cancel).await {
                    break;
                }
            }
            accept_res = async {
                match listener_v6.as_ref() {
                    Some(l) => l.accept().await,
                    None => std::future::pending().await,
                }
            } => {
                if !handle_accept_result(accept_res, &mut join_set, &state, &cancel).await {
                    break;
                }
            }
        }
    }

    let deadline = Instant::now() + Duration::from_millis(SHUTDOWN_WAIT_MS);
    while !join_set.is_empty() {
        tokio::select! {
            biased;
            res = join_set.join_next() => {
                if let Some(Err(err)) = res {
                    if !err.is_cancelled() {
                        log_sink::error(LOG_MODULE, format!("client connection task panicked on shutdown: {err}"));
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                log_sink::warn(
                    LOG_MODULE,
                    format!(
                        "{} connection tasks did not stop within {SHUTDOWN_WAIT_MS}ms, aborting",
                        join_set.len()
                    ),
                );
                join_set.abort_all();
                while let Some(res) = join_set.join_next().await {
                    if let Err(err) = res {
                        if !err.is_cancelled() {
                            log_sink::error(LOG_MODULE, format!("aborted task error: {err}"));
                        }
                    }
                }
                break;
            }
        }
    }

    log_sink::info(LOG_MODULE, "signaling server stopped");
}

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    state: Arc<ServerState>,
    cancel: CancellationToken,
) {
    if let Err(err) = stream.set_nodelay(true) {
        log_sink::warn(
            LOG_MODULE,
            format!("failed to set TCP_NODELAY for {peer_addr}: {err}"),
        );
    }

    let mut ws = tokio::select! {
        biased;
        _ = cancel.cancelled() => return,
        res = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            accept_async_with_config(stream, Some(server_ws_config())),
        ) => {
            match res {
                Ok(Ok(ws)) => ws,
                Ok(Err(err)) => {
                    log_sink::warn(LOG_MODULE, format!("rejected ws handshake from {peer_addr}: {err}"));
                    return;
                }
                Err(_) => {
                    log_sink::warn(LOG_MODULE, format!("ws handshake timeout from {peer_addr}"));
                    return;
                }
            }
        }
    };

    let first_text = match await_first_text_message(peer_addr, &mut ws, &cancel).await {
        Some(text) => text,
        None => return,
    };

    let join = match serde_json::from_str::<ClientSignalMsg>(&first_text) {
        Ok(ClientSignalMsg::JoinRoom(join)) => join,
        Ok(ClientSignalMsg::LeaveRoom) => {
            log_sink::warn(
                LOG_MODULE,
                format!("peer {peer_addr} sent leave_room before join_room"),
            );
            send_signal_error_and_close(
                &mut ws,
                ProbeError::InvalidInput("first message must be join_room".to_string()),
                peer_addr,
            )
            .await;
            return;
        }
        Err(err) => {
            log_sink::warn(
                LOG_MODULE,
                format!("invalid initial signal json from {peer_addr}: {err}"),
            );
            send_signal_error_and_close(
                &mut ws,
                ProbeError::InvalidInput(format!("invalid client signal message: {err}")),
                peer_addr,
            )
            .await;
            return;
        }
    };

    let my_id = state.next_unique_id();
    let outcome = process_join_room(my_id, peer_addr, join, &state, &mut ws).await;

    let (room_code, mut phase, mut deadline, mut sig_rx) = match outcome {
        JoinOutcome::Waiting {
            room_code,
            expires_at,
            sig_rx,
        } => (room_code, PeerPhase::Waiting, Some(expires_at), sig_rx),
        JoinOutcome::Paired {
            room_code,
            session_id,
            expires_at,
            sig_rx,
        } => (
            room_code,
            PeerPhase::Paired { session_id },
            Some(expires_at),
            sig_rx,
        ),
        JoinOutcome::Rejected => return,
    };

    run_peer_session_loop(
        peer_addr,
        my_id,
        &room_code,
        &mut phase,
        &mut deadline,
        &mut sig_rx,
        &mut ws,
        &state,
        &cancel,
    )
    .await;
}

async fn await_first_text_message(
    peer_addr: SocketAddr,
    ws: &mut WebSocketStream<TcpStream>,
    cancel: &CancellationToken,
) -> Option<String> {
    let deadline = Instant::now() + JOIN_MESSAGE_TIMEOUT;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                send_ws_close(ws, CloseCode::Away, peer_addr).await;
                return None;
            }
            _ = tokio::time::sleep_until(deadline) => {
                log_sink::warn(LOG_MODULE, format!("peer {peer_addr} timed out waiting for join_room"));
                send_signal_error_and_close(
                    ws,
                    ProbeError::InvalidInput("timed out waiting for join_room".to_string()),
                    peer_addr,
                )
                .await;
                return None;
            }
            ev = ws.next() => {
                match ev {
                    Some(Ok(Message::Ping(_))) => {
                        match tokio::time::timeout(WRITE_TIMEOUT, ws.flush()).await {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => {
                                log_sink::warn(LOG_MODULE, format!("failed to send pong to {peer_addr}: {err}"));
                                return None;
                            }
                            Err(_) => {
                                log_sink::warn(LOG_MODULE, format!("pong flush timed out for {peer_addr}"));
                                return None;
                            }
                        }
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Text(text))) => return Some(text.to_string()),
                    Some(Ok(Message::Binary(_))) => {
                        log_sink::warn(LOG_MODULE, format!("peer {peer_addr} sent unsupported binary frame"));
                        send_ws_close(ws, CloseCode::Unsupported, peer_addr).await;
                        return None;
                    }
                    Some(Ok(Message::Close(_))) => {
                        let _ = tokio::time::timeout(WRITE_TIMEOUT, ws.flush()).await;
                        return None;
                    }
                    None | Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => return None,
                    Some(Err(err)) => {
                        if let Some(code) = close_code_for_ws_error(&err) {
                            log_sink::warn(LOG_MODULE, format!("ws protocol error from {peer_addr}: {err}"));
                            send_ws_close(ws, code, peer_addr).await;
                        } else {
                            log_sink::warn(LOG_MODULE, format!("ws read error from {peer_addr}: {err}"));
                        }
                        return None;
                    }
                }
            }
        }
    }
}

async fn process_join_room(
    my_id: u64,
    peer_addr: SocketAddr,
    join: protocol::JoinRoomMsg,
    state: &Arc<ServerState>,
    ws: &mut WebSocketStream<TcpStream>,
) -> JoinOutcome {
    if let Some(expected_token) = state.config.access_token.as_deref() {
        if join.token.as_deref() != Some(expected_token) {
            log_sink::warn(
                LOG_MODULE,
                format!("unauthorized join_room from {peer_addr} for room {:?}", join.room_code),
            );
            send_signal_error_and_close(ws, ProbeError::Unauthorized, peer_addr).await;
            return JoinOutcome::Rejected;
        }
    }

    if let Err(err) = validate_room_code(&join.room_code) {
        log_sink::warn(
            LOG_MODULE,
            format!("invalid room_code from {peer_addr}: {err}"),
        );
        send_signal_error_and_close(ws, err, peer_addr).await;
        return JoinOutcome::Rejected;
    }

    if join.peer_label.trim().is_empty() {
        log_sink::warn(
            LOG_MODULE,
            format!("empty peer_label from {peer_addr} for room {}", join.room_code),
        );
        send_signal_error_and_close(
            ws,
            ProbeError::InvalidInput("peer_label must not be empty".to_string()),
            peer_addr,
        )
        .await;
        return JoinOutcome::Rejected;
    }

    let room_code = join.room_code.clone();
    let now = Instant::now();

    enum LockedAction {
        BecameWaiting {
            expires_at: Instant,
            sig_rx: mpsc::UnboundedReceiver<OutboundSignal>,
        },
        PairedWithPeer1 {
            session_id: String,
            expires_at: Instant,
            start_for_peer2: StartPunchMsg,
            sig_rx: mpsc::UnboundedReceiver<OutboundSignal>,
        },
        RoomFull,
    }

    let action = {
        let mut rooms = state.rooms.lock().await;
        evict_expired_rooms(&mut rooms, now);

        match rooms.remove(&room_code) {
            None => {
                if rooms.len() >= MAX_ROOMS {
                    log_sink::warn(
                        LOG_MODULE,
                        format!(
                            "rejected join_room from {peer_addr} for room {room_code}: room capacity {MAX_ROOMS} reached"
                        ),
                    );
                    LockedAction::RoomFull
                } else {
                    let (sig_tx, sig_rx) = mpsc::unbounded_channel();
                    let expires_at = now + state.config.wait_ttl;
                    rooms.insert(
                        room_code.clone(),
                        RoomEntry::Waiting(WaitingRoom {
                            peer1_id: my_id,
                            peer1_label: join.peer_label,
                            peer1_candidates: join.candidates,
                            peer1_tx: sig_tx,
                            expires_at,
                        }),
                    );
                    log_sink::info(
                        LOG_MODULE,
                        format!("peer {peer_addr} created waiting room {room_code} (id={my_id})"),
                    );
                    LockedAction::BecameWaiting { expires_at, sig_rx }
                }
            }
            Some(RoomEntry::Paired(paired)) => {
                rooms.insert(room_code.clone(), RoomEntry::Paired(paired));
                log_sink::warn(
                    LOG_MODULE,
                    format!("rejected 3rd peer {peer_addr} for paired room {room_code}: room_full"),
                );
                LockedAction::RoomFull
            }
            Some(RoomEntry::Waiting(waiting)) => {
                let session_id = format!("p2p_{room_code}_{}", state.next_unique_id());
                let paired_expires_at = now
                    + Duration::from_millis(
                        state.config.punch_duration_ms as u64 + PUNCH_ROOM_GRACE_MS,
                    );
                let start_for_peer1 = StartPunchMsg {
                    session_id: session_id.clone(),
                    peer_label: join.peer_label.clone(),
                    peer_candidates: join.candidates.clone(),
                    start_delay_ms: state.config.start_delay_ms,
                    punch_duration_ms: state.config.punch_duration_ms,
                };
                let start_for_peer2 = StartPunchMsg {
                    session_id: session_id.clone(),
                    peer_label: waiting.peer1_label.clone(),
                    peer_candidates: waiting.peer1_candidates.clone(),
                    start_delay_ms: state.config.start_delay_ms,
                    punch_duration_ms: state.config.punch_duration_ms,
                };

                if waiting
                    .peer1_tx
                    .send(OutboundSignal::StartPunch {
                        msg: start_for_peer1,
                        expires_at: paired_expires_at,
                    })
                    .is_ok()
                {
                    let (sig_tx, sig_rx) = mpsc::unbounded_channel();
                    rooms.insert(
                        room_code.clone(),
                        RoomEntry::Paired(PairedRoom {
                            session_id: session_id.clone(),
                            peer1_id: waiting.peer1_id,
                            peer1_tx: Some(waiting.peer1_tx),
                            peer2_id: my_id,
                            peer2_tx: Some(sig_tx),
                            expires_at: paired_expires_at,
                        }),
                    );
                    log_sink::info(
                        LOG_MODULE,
                        format!(
                            "paired room {room_code} session={session_id} peer1_id={} peer2_id={my_id}",
                            waiting.peer1_id
                        ),
                    );
                    LockedAction::PairedWithPeer1 {
                        session_id,
                        expires_at: paired_expires_at,
                        start_for_peer2,
                        sig_rx,
                    }
                } else {
                    log_sink::warn(
                        LOG_MODULE,
                        format!(
                            "previous waiting peer in room {room_code} already disconnected; peer {peer_addr} (id={my_id}) taking waiting slot"
                        ),
                    );
                    let (sig_tx, sig_rx) = mpsc::unbounded_channel();
                    let expires_at = now + state.config.wait_ttl;
                    rooms.insert(
                        room_code.clone(),
                        RoomEntry::Waiting(WaitingRoom {
                            peer1_id: my_id,
                            peer1_label: join.peer_label,
                            peer1_candidates: join.candidates,
                            peer1_tx: sig_tx,
                            expires_at,
                        }),
                    );
                    LockedAction::BecameWaiting { expires_at, sig_rx }
                }
            }
        }
    };

    match action {
        LockedAction::RoomFull => {
            send_signal_error_and_close(ws, ProbeError::RoomFull, peer_addr).await;
            JoinOutcome::Rejected
        }
        LockedAction::BecameWaiting { expires_at, sig_rx } => {
            let ttl_secs = u32::try_from(state.config.wait_ttl.as_secs()).unwrap_or(u32::MAX);
            let msg = ServerSignalMsg::WaitingForPeer(WaitingForPeerMsg {
                room_code: room_code.clone(),
                ttl_secs,
            });
            if let Err(err) = send_server_msg(ws, &msg).await {
                log_sink::warn(
                    LOG_MODULE,
                    format!("failed to send waiting_for_peer to {peer_addr}: {err}"),
                );
                cleanup_on_disconnect(state, Some(&room_code), my_id).await;
                return JoinOutcome::Rejected;
            }
            JoinOutcome::Waiting {
                room_code,
                expires_at,
                sig_rx,
            }
        }
        LockedAction::PairedWithPeer1 {
            session_id,
            expires_at,
            start_for_peer2,
            sig_rx,
        } => {
            let msg = ServerSignalMsg::StartPunch(start_for_peer2);
            if let Err(err) = send_server_msg(ws, &msg).await {
                log_sink::warn(
                    LOG_MODULE,
                    format!("failed to send start_punch to peer2 {peer_addr}: {err}"),
                );
                cleanup_on_disconnect(state, Some(&room_code), my_id).await;
                return JoinOutcome::Rejected;
            }
            JoinOutcome::Paired {
                room_code,
                session_id,
                expires_at,
                sig_rx,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_peer_session_loop(
    peer_addr: SocketAddr,
    my_id: u64,
    room_code: &str,
    phase: &mut PeerPhase,
    deadline: &mut Option<Instant>,
    sig_rx: &mut mpsc::UnboundedReceiver<OutboundSignal>,
    ws: &mut WebSocketStream<TcpStream>,
    state: &Arc<ServerState>,
    cancel: &CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                cleanup_on_disconnect(state, Some(room_code), my_id).await;
                send_ws_close(ws, CloseCode::Away, peer_addr).await;
                break;
            }
            _ = sleep_optional(*deadline) => {
                match phase {
                    PeerPhase::Waiting => {
                        let timed_out = {
                            let mut rooms = state.rooms.lock().await;
                            match rooms.get(room_code) {
                                Some(RoomEntry::Waiting(w)) if w.peer1_id == my_id => {
                                    rooms.remove(room_code);
                                    true
                                }
                                _ => false,
                            }
                        };
                        if timed_out {
                            log_sink::info(
                                LOG_MODULE,
                                format!("waiting room {room_code} timed out (peer {peer_addr}, id={my_id})"),
                            );
                            send_signal_error_and_close(ws, ProbeError::PeerTimeout, peer_addr).await;
                            break;
                        }
                        *deadline = None;
                    }
                    PeerPhase::Paired { session_id } => {
                        {
                            let mut rooms = state.rooms.lock().await;
                            let now = Instant::now();
                            if let Some(RoomEntry::Paired(p)) = rooms.get(room_code) {
                                if p.session_id == *session_id && now >= p.expires_at {
                                    rooms.remove(room_code);
                                    log_sink::info(
                                        LOG_MODULE,
                                        format!("paired room {room_code} session={session_id} keepalive expired"),
                                    );
                                }
                            }
                        }
                        send_ws_close(ws, CloseCode::Normal, peer_addr).await;
                        break;
                    }
                }
            }
            sig = sig_rx.recv() => {
                match sig {
                    Some(OutboundSignal::StartPunch { msg, expires_at }) => {
                        let session_id = msg.session_id.clone();
                        if let Err(err) = send_server_msg(ws, &ServerSignalMsg::StartPunch(msg)).await {
                            log_sink::warn(
                                LOG_MODULE,
                                format!("failed to send start_punch to peer1 {peer_addr}: {err}"),
                            );
                            cleanup_on_disconnect(state, Some(room_code), my_id).await;
                            break;
                        }
                        *phase = PeerPhase::Paired { session_id };
                        *deadline = Some(expires_at);
                    }
                    Some(OutboundSignal::TimeoutClose) => {
                        log_sink::info(
                            LOG_MODULE,
                            format!("waiting room {room_code} expired before pairing (peer {peer_addr}, id={my_id})"),
                        );
                        send_signal_error_and_close(ws, ProbeError::PeerTimeout, peer_addr).await;
                        break;
                    }
                    Some(OutboundSignal::PeerLeftClose) => {
                        log_sink::info(
                            LOG_MODULE,
                            format!("forwarding peer_left to {peer_addr} in room {room_code}"),
                        );
                        if let Err(err) = send_server_msg(ws, &ServerSignalMsg::PeerLeft).await {
                            log_sink::warn(
                                LOG_MODULE,
                                format!("failed to forward peer_left to {peer_addr}: {err}"),
                            );
                        } else {
                            send_ws_close(ws, CloseCode::Normal, peer_addr).await;
                        }
                        break;
                    }
                    Some(OutboundSignal::GraceExpiredClose) => {
                        send_ws_close(ws, CloseCode::Normal, peer_addr).await;
                        break;
                    }
                    None => {
                        break;
                    }
                }
            }
            ws_ev = ws.next() => {
                match ws_ev {
                    Some(Ok(Message::Ping(_))) => {
                        match tokio::time::timeout(WRITE_TIMEOUT, ws.flush()).await {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => {
                                log_sink::warn(LOG_MODULE, format!("failed to send pong to {peer_addr}: {err}"));
                                cleanup_on_disconnect(state, Some(room_code), my_id).await;
                                break;
                            }
                            Err(_) => {
                                log_sink::warn(LOG_MODULE, format!("pong flush timed out for {peer_addr}"));
                                cleanup_on_disconnect(state, Some(room_code), my_id).await;
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientSignalMsg>(text.as_str()) {
                            Ok(ClientSignalMsg::LeaveRoom) => {
                                handle_explicit_leave(state, room_code, my_id, peer_addr).await;
                                send_ws_close(ws, CloseCode::Normal, peer_addr).await;
                                break;
                            }
                            Ok(ClientSignalMsg::JoinRoom(_)) => {
                                log_sink::warn(
                                    LOG_MODULE,
                                    format!("peer {peer_addr} sent unexpected duplicate join_room in {room_code}"),
                                );
                                cleanup_on_disconnect(state, Some(room_code), my_id).await;
                                send_signal_error_and_close(
                                    ws,
                                    ProbeError::InvalidInput("already joined room".to_string()),
                                    peer_addr,
                                )
                                .await;
                                break;
                            }
                            Err(err) => {
                                log_sink::warn(
                                    LOG_MODULE,
                                    format!("invalid signal json from {peer_addr} in {room_code}: {err}"),
                                );
                                cleanup_on_disconnect(state, Some(room_code), my_id).await;
                                send_signal_error_and_close(
                                    ws,
                                    ProbeError::InvalidInput(format!("invalid client signal message: {err}")),
                                    peer_addr,
                                )
                                .await;
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        log_sink::warn(
                            LOG_MODULE,
                            format!("peer {peer_addr} sent unsupported binary frame in {room_code}"),
                        );
                        cleanup_on_disconnect(state, Some(room_code), my_id).await;
                        send_ws_close(ws, CloseCode::Unsupported, peer_addr).await;
                        break;
                    }
                    Some(Ok(Message::Close(_))) => {
                        cleanup_on_disconnect(state, Some(room_code), my_id).await;
                        let _ = tokio::time::timeout(WRITE_TIMEOUT, ws.flush()).await;
                        break;
                    }
                    None | Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                        cleanup_on_disconnect(state, Some(room_code), my_id).await;
                        break;
                    }
                    Some(Err(err)) => {
                        cleanup_on_disconnect(state, Some(room_code), my_id).await;
                        if let Some(code) = close_code_for_ws_error(&err) {
                            log_sink::warn(
                                LOG_MODULE,
                                format!("ws protocol error from {peer_addr} in {room_code}: {err}"),
                            );
                            send_ws_close(ws, code, peer_addr).await;
                        } else {
                            log_sink::warn(
                                LOG_MODULE,
                                format!("ws read error from {peer_addr} in {room_code}: {err}"),
                            );
                        }
                        break;
                    }
                }
            }
        }
    }
}

async fn sleep_optional(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

fn evict_expired_rooms(rooms: &mut HashMap<String, RoomEntry>, now: Instant) {
    let expired_keys: Vec<String> = rooms
        .iter()
        .filter(|(_, entry)| entry.expires_at() <= now)
        .map(|(k, _)| k.clone())
        .collect();

    for key in expired_keys {
        if let Some(entry) = rooms.remove(&key) {
            match entry {
                RoomEntry::Waiting(w) => {
                    log_sink::info(
                        LOG_MODULE,
                        format!("lazily evicted expired waiting room {key} (peer1_id={})", w.peer1_id),
                    );
                    let _ = w.peer1_tx.send(OutboundSignal::TimeoutClose);
                }
                RoomEntry::Paired(p) => {
                    log_sink::info(
                        LOG_MODULE,
                        format!("lazily evicted expired paired room {key} session={}", p.session_id),
                    );
                    if let Some(tx1) = p.peer1_tx {
                        let _ = tx1.send(OutboundSignal::GraceExpiredClose);
                    }
                    if let Some(tx2) = p.peer2_tx {
                        let _ = tx2.send(OutboundSignal::GraceExpiredClose);
                    }
                }
            }
        }
    }
}

async fn cleanup_on_disconnect(state: &Arc<ServerState>, room_code: Option<&str>, my_id: u64) {
    let Some(code) = room_code else {
        return;
    };
    let mut rooms = state.rooms.lock().await;
    match rooms.get_mut(code) {
        Some(RoomEntry::Waiting(w)) if w.peer1_id == my_id => {
            rooms.remove(code);
            log_sink::info(
                LOG_MODULE,
                format!("removed waiting room {code} after peer disconnect (id={my_id})"),
            );
        }
        Some(RoomEntry::Paired(p)) => {
            if p.peer1_id == my_id {
                p.peer1_tx = None;
            } else if p.peer2_id == my_id {
                p.peer2_tx = None;
            }
            if p.peer1_tx.is_none() && p.peer2_tx.is_none() {
                let session_id = p.session_id.clone();
                rooms.remove(code);
                log_sink::info(
                    LOG_MODULE,
                    format!("removed paired room {code} session={session_id} after both peers closed"),
                );
            }
        }
        _ => {}
    }
}

async fn handle_explicit_leave(
    state: &Arc<ServerState>,
    room_code: &str,
    my_id: u64,
    peer_addr: SocketAddr,
) {
    let partner_tx = {
        let mut rooms = state.rooms.lock().await;
        match rooms.get(room_code) {
            Some(RoomEntry::Waiting(w)) if w.peer1_id == my_id => {
                rooms.remove(room_code);
                log_sink::info(
                    LOG_MODULE,
                    format!("peer {peer_addr} left waiting room {room_code} (id={my_id})"),
                );
                None
            }
            Some(RoomEntry::Paired(p)) if p.peer1_id == my_id || p.peer2_id == my_id => {
                let Some(RoomEntry::Paired(paired)) = rooms.remove(room_code) else {
                    return;
                };
                log_sink::info(
                    LOG_MODULE,
                    format!(
                        "peer {peer_addr} sent leave_room in paired room {room_code} session={}",
                        paired.session_id
                    ),
                );
                if paired.peer1_id == my_id {
                    paired.peer2_tx
                } else {
                    paired.peer1_tx
                }
            }
            _ => None,
        }
    };

    if let Some(tx) = partner_tx {
        let _ = tx.send(OutboundSignal::PeerLeftClose);
    }
}

fn server_ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_WS_MESSAGE_BYTES))
}

fn close_code_for_ws_error(err: &WsError) -> Option<CloseCode> {
    match err {
        WsError::Protocol(_) => Some(CloseCode::Protocol),
        WsError::Capacity(_) => Some(CloseCode::Size),
        WsError::Utf8(_) => Some(CloseCode::Invalid),
        _ => None,
    }
}

fn is_benign_close_error(err: &WsError) -> bool {
    match err {
        WsError::ConnectionClosed | WsError::AlreadyClosed => true,
        WsError::Io(io_err) => matches!(
            io_err.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
        ),
        _ => false,
    }
}

async fn send_server_msg(
    ws: &mut WebSocketStream<TcpStream>,
    msg: &ServerSignalMsg,
) -> Result<(), String> {
    let json = serde_json::to_string(msg).map_err(|err| {
        log_sink::error(LOG_MODULE, format!("failed to serialize ServerSignalMsg: {err}"));
        format!("serialize error: {err}")
    })?;
    tokio::time::timeout(WRITE_TIMEOUT, ws.send(Message::text(json)))
        .await
        .map_err(|_| "ws message write timed out".to_string())?
        .map_err(|err| err.to_string())
}

async fn send_ws_close(ws: &mut WebSocketStream<TcpStream>, code: CloseCode, peer_addr: SocketAddr) {
    let frame = CloseFrame {
        code,
        reason: Utf8Bytes::default(),
    };
    match tokio::time::timeout(WRITE_TIMEOUT, ws.close(Some(frame))).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) if is_benign_close_error(&err) => {}
        Ok(Err(err)) => {
            log_sink::warn(
                LOG_MODULE,
                format!("failed to send ws close to {peer_addr}: {err}"),
            );
        }
        Err(_) => {
            log_sink::warn(
                LOG_MODULE,
                format!("timed out sending ws close to {peer_addr}"),
            );
        }
    }
}

async fn send_signal_error_and_close(
    ws: &mut WebSocketStream<TcpStream>,
    error: ProbeError,
    peer_addr: SocketAddr,
) {
    let msg = ServerSignalMsg::Error(SignalErrorMsg { error });
    if let Err(err) = send_server_msg(ws, &msg).await {
        log_sink::warn(
            LOG_MODULE,
            format!("failed to send signal error to {peer_addr}: {err}"),
        );
    }
    send_ws_close(ws, CloseCode::Normal, peer_addr).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Candidate, CandidateKind, JoinRoomMsg};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::{connect_async, MaybeTlsStream};

    struct TestWsClient {
        ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    }

    impl TestWsClient {
        async fn connect(addr: SocketAddr) -> Self {
            let (ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
            Self { ws }
        }

        async fn send_signal(&mut self, msg: &ClientSignalMsg) {
            let json = serde_json::to_string(msg).unwrap();
            self.ws.send(Message::text(json)).await.unwrap();
        }

        async fn send_close(&mut self) {
            let frame = CloseFrame {
                code: CloseCode::Normal,
                reason: Utf8Bytes::default(),
            };
            self.ws.close(Some(frame)).await.unwrap();
        }

        async fn recv_msg(&mut self, timeout_ms: u64) -> Option<Message> {
            match tokio::time::timeout(Duration::from_millis(timeout_ms), self.ws.next()).await {
                Ok(Some(Ok(msg))) => Some(msg),
                _ => None,
            }
        }

        async fn recv_signal(&mut self, timeout_ms: u64) -> Option<ServerSignalMsg> {
            let msg = self.recv_msg(timeout_ms).await?;
            match msg {
                Message::Text(text) => Some(serde_json::from_str(text.as_str()).unwrap()),
                other => panic!("expected text frame, got {other:?}"),
            }
        }

        async fn expect_close(&mut self, timeout_ms: u64) {
            let msg = self.recv_msg(timeout_ms).await;
            assert!(
                matches!(msg, Some(Message::Close(_))),
                "expected Close frame, got {msg:?}"
            );
        }
    }

    fn test_config() -> SignalingServerConfig {
        SignalingServerConfig {
            bind_ws: "127.0.0.1:0".parse().unwrap(),
            bind_ws_v6: None,
            bind_ws_v6_best_effort: false,
            access_token: None,
            wait_ttl: Duration::from_secs(10),
            start_delay_ms: 250,
            punch_duration_ms: 3000,
        }
    }

    fn sample_join(room_code: &str, label: &str, token: Option<&str>) -> ClientSignalMsg {
        ClientSignalMsg::JoinRoom(JoinRoomMsg {
            room_code: room_code.to_string(),
            token: token.map(str::to_string),
            peer_label: label.to_string(),
            candidates: vec![Candidate {
                kind: CandidateKind::ServerReflexiveV4,
                addr: "198.51.100.10:40000".to_string(),
            }],
        })
    }

    #[tokio::test]
    async fn ws_pairing_flow_and_no_false_peer_left_on_ws_close() {
        let cancel = CancellationToken::new();
        let (bound, handle) = start_signaling_server(test_config(), cancel.clone())
            .await
            .unwrap();

        let mut peer1 = TestWsClient::connect(bound.ws_addr).await;
        peer1.send_signal(&sample_join("RoomA1", "alice", None)).await;

        match peer1.recv_signal(500).await.unwrap() {
            ServerSignalMsg::WaitingForPeer(w) => {
                assert_eq!(w.room_code, "RoomA1");
                assert_eq!(w.ttl_secs, 10);
            }
            other => panic!("expected WaitingForPeer, got {other:?}"),
        }

        let mut peer2 = TestWsClient::connect(bound.ws_addr).await;
        peer2.send_signal(&sample_join("RoomA1", "bob", None)).await;

        let s1 = match peer1.recv_signal(500).await.unwrap() {
            ServerSignalMsg::StartPunch(msg) => msg,
            other => panic!("expected StartPunch for peer1, got {other:?}"),
        };
        let s2 = match peer2.recv_signal(500).await.unwrap() {
            ServerSignalMsg::StartPunch(msg) => msg,
            other => panic!("expected StartPunch for peer2, got {other:?}"),
        };

        assert_eq!(s1.session_id, s2.session_id);
        assert_eq!(s1.peer_label, "bob");
        assert_eq!(s2.peer_label, "alice");
        assert_eq!(s1.start_delay_ms, 250);
        assert_eq!(s1.punch_duration_ms, 3000);

        // Peer 1 closes WS without sending LeaveRoom (simulating C2.close() after punch_succeeded):
        // Peer 2 must NOT receive a false PeerLeft.
        peer1.send_close().await;
        peer1.expect_close(300).await;
        assert!(peer2.recv_msg(120).await.is_none());

        let stop_start = Instant::now();
        cancel.cancel();
        handle.await.unwrap();
        assert!(stop_start.elapsed() < Duration::from_millis(150));
    }

    #[tokio::test]
    async fn unauthorized_token_and_invalid_inputs_are_rejected() {
        let cancel = CancellationToken::new();
        let mut cfg = test_config();
        cfg.access_token = Some("top-secret".to_string());
        let (bound, handle) = start_signaling_server(cfg, cancel.clone()).await.unwrap();

        let mut bad_tok = TestWsClient::connect(bound.ws_addr).await;
        bad_tok
            .send_signal(&sample_join("Room01", "alice", Some("wrong")))
            .await;
        match bad_tok.recv_signal(500).await.unwrap() {
            ServerSignalMsg::Error(e) => assert_eq!(e.error, ProbeError::Unauthorized),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
        bad_tok.expect_close(300).await;

        let mut missing_tok = TestWsClient::connect(bound.ws_addr).await;
        missing_tok
            .send_signal(&sample_join("Room01", "alice", None))
            .await;
        match missing_tok.recv_signal(500).await.unwrap() {
            ServerSignalMsg::Error(e) => assert_eq!(e.error, ProbeError::Unauthorized),
            other => panic!("expected Unauthorized, got {other:?}"),
        }

        let mut bad_room = TestWsClient::connect(bound.ws_addr).await;
        bad_room
            .send_signal(&sample_join("ab", "alice", Some("top-secret")))
            .await;
        match bad_room.recv_signal(500).await.unwrap() {
            ServerSignalMsg::Error(e) => assert!(matches!(e.error, ProbeError::InvalidInput(_))),
            other => panic!("expected InvalidInput, got {other:?}"),
        }

        let mut leave_first = TestWsClient::connect(bound.ws_addr).await;
        leave_first.send_signal(&ClientSignalMsg::LeaveRoom).await;
        match leave_first.recv_signal(500).await.unwrap() {
            ServerSignalMsg::Error(e) => assert!(matches!(e.error, ProbeError::InvalidInput(_))),
            other => panic!("expected InvalidInput, got {other:?}"),
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn third_peer_gets_room_full_without_disturbing_paired_peers() {
        let cancel = CancellationToken::new();
        let (bound, handle) = start_signaling_server(test_config(), cancel.clone())
            .await
            .unwrap();

        let mut p1 = TestWsClient::connect(bound.ws_addr).await;
        p1.send_signal(&sample_join("Full12", "p1", None)).await;
        assert!(matches!(
            p1.recv_signal(500).await.unwrap(),
            ServerSignalMsg::WaitingForPeer(_)
        ));

        let mut p2 = TestWsClient::connect(bound.ws_addr).await;
        p2.send_signal(&sample_join("Full12", "p2", None)).await;
        assert!(matches!(
            p1.recv_signal(500).await.unwrap(),
            ServerSignalMsg::StartPunch(_)
        ));
        assert!(matches!(
            p2.recv_signal(500).await.unwrap(),
            ServerSignalMsg::StartPunch(_)
        ));

        let mut p3 = TestWsClient::connect(bound.ws_addr).await;
        p3.send_signal(&sample_join("Full12", "p3", None)).await;
        match p3.recv_signal(500).await.unwrap() {
            ServerSignalMsg::Error(e) => assert_eq!(e.error, ProbeError::RoomFull),
            other => panic!("expected RoomFull, got {other:?}"),
        }
        p3.expect_close(300).await;

        // Existing two peers remain undisturbed until explicitly leaving
        assert!(p1.recv_msg(80).await.is_none());
        assert!(p2.recv_msg(80).await.is_none());

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn waiting_room_emits_peer_timeout_on_short_ttl() {
        let cancel = CancellationToken::new();
        let mut cfg = test_config();
        cfg.wait_ttl = Duration::from_millis(80);
        let (bound, handle) = start_signaling_server(cfg, cancel.clone()).await.unwrap();

        let mut p1 = TestWsClient::connect(bound.ws_addr).await;
        p1.send_signal(&sample_join("Ttl123", "p1", None)).await;
        assert!(matches!(
            p1.recv_signal(500).await.unwrap(),
            ServerSignalMsg::WaitingForPeer(_)
        ));

        match p1.recv_signal(500).await.unwrap() {
            ServerSignalMsg::Error(e) => assert_eq!(e.error, ProbeError::PeerTimeout),
            other => panic!("expected PeerTimeout, got {other:?}"),
        }
        p1.expect_close(300).await;

        // Room can be re-created cleanly after timeout eviction
        let mut p2 = TestWsClient::connect(bound.ws_addr).await;
        p2.send_signal(&sample_join("Ttl123", "p2", None)).await;
        assert!(matches!(
            p2.recv_signal(500).await.unwrap(),
            ServerSignalMsg::WaitingForPeer(_)
        ));

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn leave_room_forwards_peer_left_during_paired_window() {
        let cancel = CancellationToken::new();
        let (bound, handle) = start_signaling_server(test_config(), cancel.clone())
            .await
            .unwrap();

        let mut p1 = TestWsClient::connect(bound.ws_addr).await;
        p1.send_signal(&sample_join("Leave1", "p1", None)).await;
        assert!(matches!(
            p1.recv_signal(500).await.unwrap(),
            ServerSignalMsg::WaitingForPeer(_)
        ));

        let mut p2 = TestWsClient::connect(bound.ws_addr).await;
        p2.send_signal(&sample_join("Leave1", "p2", None)).await;
        assert!(matches!(
            p1.recv_signal(500).await.unwrap(),
            ServerSignalMsg::StartPunch(_)
        ));
        assert!(matches!(
            p2.recv_signal(500).await.unwrap(),
            ServerSignalMsg::StartPunch(_)
        ));

        p1.send_signal(&ClientSignalMsg::LeaveRoom).await;
        p1.expect_close(300).await;

        match p2.recv_signal(500).await.unwrap() {
            ServerSignalMsg::PeerLeft => {}
            other => panic!("expected PeerLeft, got {other:?}"),
        }
        p2.expect_close(300).await;

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn max_rooms_limit_is_enforced() {
        let cancel = CancellationToken::new();
        let (bound, handle) = start_signaling_server(test_config(), cancel.clone())
            .await
            .unwrap();

        let mut clients = Vec::with_capacity(MAX_ROOMS);
        for idx in 0..MAX_ROOMS {
            let mut client = TestWsClient::connect(bound.ws_addr).await;
            let room = format!("Rm{:04}", idx);
            client.send_signal(&sample_join(&room, "host", None)).await;
            assert!(matches!(
                client.recv_signal(500).await.unwrap(),
                ServerSignalMsg::WaitingForPeer(_)
            ));
            clients.push(client);
        }

        let mut overflow = TestWsClient::connect(bound.ws_addr).await;
        overflow
            .send_signal(&sample_join("Rm9999", "overflow", None))
            .await;
        match overflow.recv_signal(500).await.unwrap() {
            ServerSignalMsg::Error(e) => assert_eq!(e.error, ProbeError::RoomFull),
            other => panic!("expected RoomFull on MAX_ROOMS+1, got {other:?}"),
        }

        // Disconnecting one waiting client frees a slot for Rm9999
        drop(clients.pop());
        let mut retry = None;
        for _ in 0..10 {
            let mut c = TestWsClient::connect(bound.ws_addr).await;
            c.send_signal(&sample_join("Rm9999", "retry", None)).await;
            match c.recv_signal(500).await.unwrap() {
                ServerSignalMsg::WaitingForPeer(w) => {
                    assert_eq!(w.room_code, "Rm9999");
                    retry = Some(c);
                    break;
                }
                ServerSignalMsg::Error(e) if e.error == ProbeError::RoomFull => {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                other => panic!("unexpected signal: {other:?}"),
            }
        }
        assert!(retry.is_some(), "expected room slot to free after peer disconnect");

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn ping_pong_and_unmasked_client_frame_rejected() {
        let cancel = CancellationToken::new();
        let (bound, handle) = start_signaling_server(test_config(), cancel.clone())
            .await
            .unwrap();

        let mut client = TestWsClient::connect(bound.ws_addr).await;
        client
            .ws
            .send(Message::Ping(b"hb".to_vec().into()))
            .await
            .unwrap();
        let pong = client.recv_msg(500).await.unwrap();
        assert!(matches!(pong, Message::Pong(ref p) if p.as_ref() == b"hb"));

        client.send_signal(&sample_join("Ping12", "alice", None)).await;
        assert!(matches!(
            client.recv_signal(500).await.unwrap(),
            ServerSignalMsg::WaitingForPeer(_)
        ));

        // Unmasked client frame must be rejected by RFC 6455 with Close(1002)
        let bad_client = TestWsClient::connect(bound.ws_addr).await;
        let MaybeTlsStream::Plain(mut raw_stream) = bad_client.ws.into_inner() else {
            panic!("expected plain TcpStream");
        };
        raw_stream.write_all(&[0x81, 0x02, b'{', b'}']).await.unwrap();
        raw_stream.flush().await.unwrap();
        let mut resp = [0u8; 4];
        tokio::time::timeout(Duration::from_millis(500), raw_stream.read_exact(&mut resp))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resp[0] & 0x0F, 0x8);
        assert_eq!(&resp[2..4], &1002u16.to_be_bytes());

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn bad_http_upgrade_and_oversized_headers_are_rejected() {
        let cancel = CancellationToken::new();
        let (bound, handle) = start_signaling_server(test_config(), cancel.clone())
            .await
            .unwrap();

        // Missing Upgrade: websocket header
        let mut s1 = TcpStream::connect(bound.ws_addr).await.unwrap();
        s1.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nSec-WebSocket-Key: abc\r\n\r\n")
            .await
            .unwrap();
        let mut resp1 = String::new();
        let _ =
            tokio::time::timeout(Duration::from_millis(500), s1.read_to_string(&mut resp1)).await;
        assert!(!resp1.starts_with("HTTP/1.1 101"));

        // Oversized (> 8 KB) handshake header
        let mut s2 = TcpStream::connect(bound.ws_addr).await.unwrap();
        let big_hdr = format!(
            "GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nX-Pad: {}\r\n\r\n",
            "a".repeat(16 * 1024)
        );
        let _ = s2.write_all(big_hdr.as_bytes()).await;
        let mut resp2 = String::new();
        let _ =
            tokio::time::timeout(Duration::from_millis(500), s2.read_to_string(&mut resp2)).await;
        assert!(!resp2.starts_with("HTTP/1.1 101"));

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn ws_v4_and_v6_can_bind_same_port() {
        let Ok(probe) = TcpListener::bind("[::]:0").await else {
            return;
        };
        drop(probe);

        let mut last_err = None;
        for _ in 0..5 {
            let v4_probe = TcpListener::bind("0.0.0.0:0").await.unwrap();
            let port = v4_probe.local_addr().unwrap().port();
            drop(v4_probe);

            let cancel = CancellationToken::new();
            let mut cfg = test_config();
            cfg.bind_ws = SocketAddr::from(([0, 0, 0, 0], port));
            cfg.bind_ws_v6 = Some(format!("[::]:{port}").parse().unwrap());

            match start_signaling_server(cfg, cancel.clone()).await {
                Ok((bound, handle)) => {
                    assert_eq!(bound.ws_addr.port(), port);
                    assert_eq!(bound.ws_v6_addr.unwrap().port(), port);

                    let mut peer_v4 =
                        TestWsClient::connect(SocketAddr::from(([127, 0, 0, 1], port))).await;
                    peer_v4
                        .send_signal(&sample_join("DualV6", "v4-peer", None))
                        .await;
                    assert!(matches!(
                        peer_v4.recv_signal(500).await.unwrap(),
                        ServerSignalMsg::WaitingForPeer(_)
                    ));

                    let mut peer_v6 =
                        TestWsClient::connect(format!("[::1]:{port}").parse().unwrap()).await;
                    peer_v6
                        .send_signal(&sample_join("DualV6", "v6-peer", None))
                        .await;

                    let s1 = match peer_v4.recv_signal(500).await.unwrap() {
                        ServerSignalMsg::StartPunch(msg) => msg,
                        other => panic!("expected StartPunch for v4 peer, got {other:?}"),
                    };
                    let s2 = match peer_v6.recv_signal(500).await.unwrap() {
                        ServerSignalMsg::StartPunch(msg) => msg,
                        other => panic!("expected StartPunch for v6 peer, got {other:?}"),
                    };
                    assert_eq!(s1.session_id, s2.session_id);
                    assert_eq!(s1.peer_label, "v6-peer");
                    assert_eq!(s2.peer_label, "v4-peer");

                    cancel.cancel();
                    handle.await.unwrap();
                    return;
                }
                Err(err) => {
                    last_err = Some(err);
                }
            }
        }
        panic!(
            "failed to bind ws 0.0.0.0 and [::] on the same ephemeral port: {:?}",
            last_err
        );
    }
}
