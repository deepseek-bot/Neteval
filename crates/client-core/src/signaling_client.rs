use crate::logger;
use futures_util::{SinkExt, StreamExt};
use protocol::{
    validate_room_code, Candidate, ClientSignalMsg, JoinRoomMsg, ProbeError, ServerSignalMsg,
    SignalErrorMsg,
};
use std::fmt;
use std::io::ErrorKind;
use std::net::Ipv6Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const MODULE: &str = "c2";
const MAX_WS_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

const HANDSHAKE_MAX_TIMEOUT: Duration = Duration::from_secs(15);
const LEAVE_CLOSE_TIMEOUT: Duration = Duration::from_millis(120);
const GUARD_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
const GUARD_STOP_TIMEOUT: Duration = Duration::from_millis(150);
const PONG_WRITE_TIMEOUT: Duration = Duration::from_millis(80);

type ClientWsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingResult {
    pub session_id: String,
    pub peer_label: String,
    pub peer_candidates: Vec<Candidate>,
    pub start_delay_ms: u32,
    pub punch_duration_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerLeftState {
    Active,
    PeerLeft,
    StreamEnded,
}

enum GuardCmd {
    LeaveAndClose(oneshot::Sender<()>),
    CloseOnly(oneshot::Sender<()>),
}

pub struct SignalGuard {
    cmd_tx: Option<oneshot::Sender<GuardCmd>>,
    peer_left_rx: watch::Receiver<PeerLeftState>,
    task: Option<JoinHandle<()>>,
    task_exited: Arc<AtomicBool>,
}

impl fmt::Debug for SignalGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalGuard").finish_non_exhaustive()
    }
}

impl SignalGuard {
    /// Waits in a cancel-safe manner until the server sends `PeerLeft` (`true`),
    /// or returns `false` if the signaling stream terminates without `PeerLeft`.
    pub async fn wait_peer_left(&mut self) -> bool {
        loop {
            let state = *self.peer_left_rx.borrow_and_update();
            match state {
                PeerLeftState::PeerLeft => return true,
                PeerLeftState::StreamEnded => return false,
                PeerLeftState::Active => {}
            }
            if self.peer_left_rx.changed().await.is_err() {
                return matches!(*self.peer_left_rx.borrow(), PeerLeftState::PeerLeft);
            }
        }
    }

    pub async fn send_leave_and_close(self) {
        self.shutdown_guard(true).await;
    }

    pub async fn close(self) {
        self.shutdown_guard(false).await;
    }

    async fn shutdown_guard(mut self, send_leave: bool) {
        if let Some(cmd_tx) = self.cmd_tx.take() {
            let (ack_tx, ack_rx) = oneshot::channel();
            let cmd = if send_leave {
                GuardCmd::LeaveAndClose(ack_tx)
            } else {
                GuardCmd::CloseOnly(ack_tx)
            };
            if cmd_tx.send(cmd).is_err() {
                logger::warn(
                    MODULE,
                    "signaling reader task already exited before guard shutdown",
                );
            } else {
                let wait_res = tokio::time::timeout(GUARD_STOP_TIMEOUT, async {
                    let _ = ack_rx.await;
                    if let Some(ref mut handle) = self.task {
                        let _ = handle.await;
                    }
                })
                .await;
                if wait_res.is_err() {
                    logger::warn(
                        MODULE,
                        "signal guard shutdown timed out; aborting background reader task",
                    );
                    if let Some(handle) = self.task.take() {
                        handle.abort();
                        let _ = handle.await;
                    }
                } else {
                    self.task = None;
                }
                return;
            }
        }

        let timed_out = if let Some(ref mut handle) = self.task {
            tokio::time::timeout(GUARD_STOP_TIMEOUT, handle)
                .await
                .is_err()
        } else {
            false
        };
        if timed_out {
            logger::warn(
                MODULE,
                "signal guard reader join timed out; aborting task",
            );
            if let Some(handle) = self.task.take() {
                handle.abort();
                let _ = handle.await;
            }
        } else {
            self.task = None;
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        self.cmd_tx.take();
        if let Some(handle) = self.task.take() {
            if !self.task_exited.load(Ordering::Acquire) {
                logger::info(
                    MODULE,
                    "dropping active SignalGuard; aborting background reader task",
                );
                handle.abort();
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WsUrl {
    host: String,
    port: u16,
    path: String,
    host_header: String,
}

impl WsUrl {
    fn parse(raw: &str) -> Result<Self, ProbeError> {
        if raw.is_empty() || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ProbeError::InvalidInput(
                "WebSocket URL must be non-empty and contain no whitespace or control characters"
                    .into(),
            ));
        }
        if raw.contains('#') {
            return Err(ProbeError::InvalidInput(
                "WebSocket URL must not contain '#' fragment".into(),
            ));
        }
        if raw
            .get(..6)
            .map(|prefix| prefix.eq_ignore_ascii_case("wss://"))
            == Some(true)
        {
            return Err(ProbeError::InvalidInput(
                "wss:// is not supported in this build without a TLS feature; use ws://".into(),
            ));
        }
        if raw
            .get(..5)
            .map(|prefix| prefix.eq_ignore_ascii_case("ws://"))
            != Some(true)
        {
            return Err(ProbeError::InvalidInput(
                "WebSocket URL must start with ws://".into(),
            ));
        }

        let rest = &raw[5..];
        let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..auth_end];
        let tail = &rest[auth_end..];
        let path = if tail.is_empty() {
            "/".to_string()
        } else if tail.starts_with('/') {
            tail.to_string()
        } else {
            format!("/{tail}")
        };

        if authority.is_empty() {
            return Err(ProbeError::InvalidInput(
                "WebSocket URL is missing host".into(),
            ));
        }
        if authority.contains('@') {
            return Err(ProbeError::InvalidInput(
                "WebSocket URL must not contain userinfo '@'".into(),
            ));
        }

        let (host, port, is_ipv6) = if let Some(after_open) = authority.strip_prefix('[') {
            let Some(close_idx) = after_open.find(']') else {
                return Err(ProbeError::InvalidInput(
                    "unclosed '[' in IPv6 WebSocket URL".into(),
                ));
            };
            let host_inner = &after_open[..close_idx];
            if host_inner.parse::<Ipv6Addr>().is_err() {
                return Err(ProbeError::InvalidInput(format!(
                    "invalid IPv6 literal [{host_inner}] in WebSocket URL"
                )));
            }
            let suffix = &after_open[close_idx + 1..];
            let port = parse_optional_port(suffix)?;
            (host_inner.to_string(), port, true)
        } else {
            if authority.contains('[') || authority.contains(']') {
                return Err(ProbeError::InvalidInput(
                    "unexpected bracket in WebSocket URL host".into(),
                ));
            }
            if let Some((h, p)) = authority.split_once(':') {
                if p.contains(':') {
                    return Err(ProbeError::InvalidInput(
                        "IPv6 hosts in ws:// URLs must be enclosed in brackets, e.g. ws://[::1]:8080"
                            .into(),
                    ));
                }
                validate_host_token(h)?;
                let port = parse_port_str(p)?;
                (h.to_string(), port, false)
            } else {
                validate_host_token(authority)?;
                (authority.to_string(), 80, false)
            }
        };

        let host_header = match (is_ipv6, port) {
            (true, 80) => format!("[{host}]"),
            (true, p) => format!("[{host}]:{p}"),
            (false, 80) => host.clone(),
            (false, p) => format!("{host}:{p}"),
        };

        Ok(Self {
            host,
            port,
            path,
            host_header,
        })
    }
}

fn validate_host_token(host: &str) -> Result<(), ProbeError> {
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
    {
        return Err(ProbeError::InvalidInput(format!(
            "invalid host '{host}' in WebSocket URL"
        )));
    }
    Ok(())
}

fn parse_optional_port(suffix: &str) -> Result<u16, ProbeError> {
    if suffix.is_empty() {
        return Ok(80);
    }
    if let Some(port_str) = suffix.strip_prefix(':') {
        return parse_port_str(port_str);
    }
    Err(ProbeError::InvalidInput(format!(
        "unexpected trailing characters '{suffix}' after IPv6 host"
    )))
}

fn parse_port_str(port_str: &str) -> Result<u16, ProbeError> {
    if port_str.is_empty() || !port_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ProbeError::InvalidInput(format!(
            "invalid port '{port_str}' in WebSocket URL"
        )));
    }
    let port: u16 = port_str.parse().map_err(|_| {
        ProbeError::InvalidInput(format!(
            "port '{port_str}' out of range 1..=65535 in WebSocket URL"
        ))
    })?;
    if port == 0 {
        return Err(ProbeError::InvalidInput(
            "port 0 is not valid in WebSocket URL".into(),
        ));
    }
    Ok(port)
}

pub async fn pair_in_room(
    ws_url: &str,
    room_code: &str,
    token: Option<&str>,
    peer_label: &str,
    local_candidates: Vec<Candidate>,
    timeout_secs: u64,
    cancel: CancellationToken,
) -> Result<(PairingResult, SignalGuard), ProbeError> {
    if let Err(e) = validate_room_code(room_code) {
        logger::warn(MODULE, format!("invalid room_code rejected: {e}"));
        return Err(e);
    }
    if peer_label.trim().is_empty() {
        let err = ProbeError::InvalidInput("peer_label must not be empty".into());
        logger::warn(MODULE, format!("invalid peer_label rejected: {err}"));
        return Err(err);
    }
    let parsed_url = match WsUrl::parse(ws_url) {
        Ok(u) => u,
        Err(e) => {
            logger::warn(MODULE, format!("invalid signaling ws_url '{ws_url}': {e}"));
            return Err(e);
        }
    };

    if cancel.is_cancelled() {
        logger::info(MODULE, "pair_in_room cancelled before connecting");
        return Err(ProbeError::Cancelled);
    }

    let now = Instant::now();
    let overall_deadline = now + Duration::from_secs(timeout_secs);
    let handshake_deadline = overall_deadline.min(now + HANDSHAKE_MAX_TIMEOUT);

    logger::info(
        MODULE,
        format!(
            "connecting signaling ws://{}:{}{} room={}",
            parsed_url.host, parsed_url.port, parsed_url.path, room_code
        ),
    );

    let mut ws = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            logger::info(MODULE, "signaling connect/handshake cancelled");
            return Err(ProbeError::Cancelled);
        }
        _ = tokio::time::sleep_until(handshake_deadline) => {
            logger::warn(MODULE, "signaling connect/handshake timed out");
            return Err(ProbeError::ServerUnreachable(
                "signaling TCP/WebSocket handshake timed out".into(),
            ));
        }
        res = connect_async_with_config(ws_url, Some(client_ws_config()), true) => {
            match res {
                Ok((ws, _resp)) => ws,
                Err(WsError::Url(e)) => {
                    logger::warn(MODULE, format!("invalid signaling ws_url '{ws_url}': {e}"));
                    return Err(ProbeError::InvalidInput(format!(
                        "invalid signaling WebSocket URL: {e}"
                    )));
                }
                Err(e) => {
                    logger::warn(MODULE, format!("signaling connect/handshake failed: {e}"));
                    return Err(ProbeError::ServerUnreachable(e.to_string()));
                }
            }
        }
    };

    logger::info(
        MODULE,
        format!(
            "WebSocket handshake completed; sending JoinRoom room={} candidates={}",
            room_code,
            local_candidates.len()
        ),
    );

    let join_msg = ClientSignalMsg::JoinRoom(JoinRoomMsg {
        room_code: room_code.to_string(),
        token: token.map(str::to_string),
        peer_label: peer_label.to_string(),
        candidates: local_candidates,
    });
    let join_json = match serde_json::to_string(&join_msg) {
        Ok(s) => s,
        Err(e) => {
            logger::error(MODULE, format!("failed to serialize JoinRoomMsg: {e}"));
            best_effort_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
            return Err(ProbeError::Internal(format!(
                "failed to serialize JoinRoomMsg: {e}"
            )));
        }
    };

    let deadline_sleep = tokio::time::sleep_until(overall_deadline);
    tokio::pin!(deadline_sleep);
    let cancelled = cancel.cancelled();
    tokio::pin!(cancelled);

    tokio::select! {
        biased;
        _ = &mut cancelled => {
            logger::info(MODULE, "pairing cancelled while sending JoinRoom");
            best_effort_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
            return Err(ProbeError::Cancelled);
        }
        _ = &mut deadline_sleep => {
            logger::warn(MODULE, "pairing timed out while sending JoinRoom");
            best_effort_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
            return Err(ProbeError::ServerUnreachable(
                "timed out sending JoinRoom to signaling server".into(),
            ));
        }
        res = ws.send(Message::text(join_json)) => {
            if let Err(e) = res {
                logger::warn(MODULE, format!("failed to send JoinRoom frame: {e}"));
                return Err(ProbeError::ServerUnreachable(format!(
                    "failed to send JoinRoom: {e}"
                )));
            }
        }
    }

    loop {
        let ev = tokio::select! {
            biased;
            _ = &mut cancelled => {
                logger::info(MODULE, "pairing cancelled while waiting for peer; sending LeaveRoom");
                best_effort_leave_and_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                return Err(ProbeError::Cancelled);
            }
            _ = &mut deadline_sleep => {
                logger::warn(MODULE, "pairing timed out waiting for peer; sending LeaveRoom");
                best_effort_leave_and_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                return Err(ProbeError::PeerTimeout);
            }
            res = ws.next() => res,
        };

        let msg = match ev {
            Some(Ok(msg)) => msg,
            Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) | None => {
                logger::warn(MODULE, "signaling stream ended before start_punch");
                return Err(ProbeError::ServerUnreachable(
                    "signaling server disconnected before start_punch".into(),
                ));
            }
            Some(Err(e)) => {
                logger::warn(MODULE, format!("signaling read error before start_punch: {e}"));
                best_effort_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                return Err(ProbeError::ServerUnreachable(format!(
                    "signaling WebSocket error: {e}"
                )));
            }
        };

        match msg {
            Message::Ping(_) => {
                tokio::select! {
                    biased;
                    _ = &mut cancelled => {
                        logger::info(MODULE, "pairing cancelled while sending pong; sending LeaveRoom");
                        best_effort_leave_and_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                        return Err(ProbeError::Cancelled);
                    }
                    _ = &mut deadline_sleep => {
                        logger::warn(MODULE, "pairing timed out while sending pong; sending LeaveRoom");
                        best_effort_leave_and_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                        return Err(ProbeError::PeerTimeout);
                    }
                    res = flush_bounded(&mut ws, PONG_WRITE_TIMEOUT) => {
                        if let Err(e) = res {
                            logger::warn(MODULE, format!("failed to send Pong during pairing: {e}"));
                            return Err(ProbeError::ServerUnreachable(format!(
                                "signaling Pong write failed: {e}"
                            )));
                        }
                    }
                }
            }
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Binary(_) => {
                logger::warn(MODULE, "signaling server sent unsupported binary frame");
                best_effort_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                return Err(ProbeError::ServerUnreachable(
                    "signaling server sent unsupported binary frame".into(),
                ));
            }
            Message::Close(frame) => {
                let code = frame.as_ref().map(|f| f.code);
                logger::warn(
                    MODULE,
                    format!("signaling server sent Close({code:?}) before start_punch"),
                );
                let _ = tokio::time::timeout(LEAVE_CLOSE_TIMEOUT, ws.flush()).await;
                return Err(ProbeError::ServerUnreachable(
                    "signaling server closed connection before start_punch".into(),
                ));
            }
            Message::Text(text) => {
                let signal_msg: ServerSignalMsg = match serde_json::from_str(text.as_str()) {
                    Ok(m) => m,
                    Err(e) => {
                        logger::warn(
                            MODULE,
                            format!("invalid ServerSignalMsg JSON from server: {e}"),
                        );
                        best_effort_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                        return Err(ProbeError::ServerUnreachable(format!(
                            "invalid signaling payload: {e}"
                        )));
                    }
                };

                match signal_msg {
                    ServerSignalMsg::WaitingForPeer(waiting) => {
                        logger::info(
                            MODULE,
                            format!(
                                "waiting for peer in room={} ttl_secs={}",
                                waiting.room_code, waiting.ttl_secs
                            ),
                        );
                    }
                    ServerSignalMsg::StartPunch(start) => {
                        logger::info(
                            MODULE,
                            format!(
                                "received StartPunch session_id={} peer_label={} candidates={} start_delay_ms={} punch_duration_ms={}",
                                start.session_id,
                                start.peer_label,
                                start.peer_candidates.len(),
                                start.start_delay_ms,
                                start.punch_duration_ms
                            ),
                        );
                        let guard = spawn_signal_guard(ws);
                        let result = PairingResult {
                            session_id: start.session_id,
                            peer_label: start.peer_label,
                            peer_candidates: start.peer_candidates,
                            start_delay_ms: start.start_delay_ms,
                            punch_duration_ms: start.punch_duration_ms,
                        };
                        return Ok((result, guard));
                    }
                    ServerSignalMsg::Error(SignalErrorMsg { error }) => {
                        logger::warn(MODULE, format!("signaling server returned error: {error}"));
                        best_effort_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                        return Err(error);
                    }
                    ServerSignalMsg::PeerLeft => {
                        logger::warn(MODULE, "received PeerLeft before StartPunch");
                        best_effort_close(&mut ws, LEAVE_CLOSE_TIMEOUT).await;
                        return Err(ProbeError::PeerLeft);
                    }
                }
            }
        }
    }
}

fn spawn_signal_guard(ws: ClientWsStream) -> SignalGuard {
    let (cmd_tx, cmd_rx) = oneshot::channel();
    let (peer_left_tx, peer_left_rx) = watch::channel(PeerLeftState::Active);
    let task_exited = Arc::new(AtomicBool::new(false));
    let task_exited_clone = Arc::clone(&task_exited);

    let handle = tokio::spawn(async move {
        struct ExitFlag(Arc<AtomicBool>);
        impl Drop for ExitFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _exit_guard = ExitFlag(task_exited_clone);
        run_guard_loop(ws, cmd_rx, peer_left_tx).await;
    });

    SignalGuard {
        cmd_tx: Some(cmd_tx),
        peer_left_rx,
        task: Some(handle),
        task_exited,
    }
}

async fn run_guard_loop(
    mut ws: ClientWsStream,
    mut cmd_rx: oneshot::Receiver<GuardCmd>,
    peer_left_tx: watch::Sender<PeerLeftState>,
) {
    loop {
        tokio::select! {
            biased;
            cmd_res = &mut cmd_rx => {
                match cmd_res {
                    Ok(GuardCmd::LeaveAndClose(ack_tx)) => {
                        best_effort_leave_and_close(&mut ws, GUARD_WRITE_TIMEOUT).await;
                        let _ = ack_tx.send(());
                    }
                    Ok(GuardCmd::CloseOnly(ack_tx)) => {
                        best_effort_close(&mut ws, GUARD_WRITE_TIMEOUT).await;
                        let _ = ack_tx.send(());
                    }
                    Err(_) => {
                        logger::info(MODULE, "SignalGuard command sender dropped; stopping reader");
                    }
                }
                break;
            }
            ev_res = ws.next() => {
                match ev_res {
                    Some(Ok(Message::Ping(_))) => {
                        if let Err(e) = flush_bounded(&mut ws, PONG_WRITE_TIMEOUT).await {
                            logger::warn(MODULE, format!("SignalGuard failed to reply Pong: {e}"));
                            mark_stream_ended_if_active(&peer_left_tx);
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerSignalMsg>(text.as_str()) {
                            Ok(ServerSignalMsg::PeerLeft) => {
                                logger::info(MODULE, "SignalGuard received PeerLeft from server");
                                if *peer_left_tx.borrow() == PeerLeftState::Active {
                                    let _ = peer_left_tx.send(PeerLeftState::PeerLeft);
                                }
                            }
                            Ok(ServerSignalMsg::Error(SignalErrorMsg { error })) => {
                                logger::warn(
                                    MODULE,
                                    format!("SignalGuard received server error after StartPunch: {error}"),
                                );
                            }
                            Ok(other) => {
                                logger::warn(
                                    MODULE,
                                    format!("SignalGuard ignored unexpected message after StartPunch: {other:?}"),
                                );
                            }
                            Err(e) => {
                                logger::warn(
                                    MODULE,
                                    format!("SignalGuard failed to parse server JSON message: {e}"),
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        logger::warn(MODULE, "SignalGuard received unsupported binary frame");
                        best_effort_close(&mut ws, PONG_WRITE_TIMEOUT).await;
                        mark_stream_ended_if_active(&peer_left_tx);
                        break;
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|f| f.code);
                        logger::info(MODULE, format!("SignalGuard received Close({code:?})"));
                        let _ = tokio::time::timeout(PONG_WRITE_TIMEOUT, ws.flush()).await;
                        mark_stream_ended_if_active(&peer_left_tx);
                        break;
                    }
                    None | Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                        logger::info(MODULE, "SignalGuard signaling stream reached EOF");
                        mark_stream_ended_if_active(&peer_left_tx);
                        break;
                    }
                    Some(Err(e)) => {
                        logger::warn(MODULE, format!("SignalGuard signaling read error: {e}"));
                        mark_stream_ended_if_active(&peer_left_tx);
                        break;
                    }
                }
            }
        }
    }
}

fn mark_stream_ended_if_active(peer_left_tx: &watch::Sender<PeerLeftState>) {
    if *peer_left_tx.borrow() == PeerLeftState::Active {
        let _ = peer_left_tx.send(PeerLeftState::StreamEnded);
    }
}

fn client_ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_WS_MESSAGE_BYTES))
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

async fn flush_bounded(ws: &mut ClientWsStream, timeout_dur: Duration) -> Result<(), String> {
    match tokio::time::timeout(timeout_dur, ws.flush()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("WebSocket flush timed out".to_string()),
    }
}

async fn drain_close_handshake(ws: &mut ClientWsStream) {
    while let Some(res) = ws.next().await {
        match res {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

async fn best_effort_leave_and_close(ws: &mut ClientWsStream, timeout_dur: Duration) {
    let leave_json = match serde_json::to_string(&ClientSignalMsg::LeaveRoom) {
        Ok(s) => s,
        Err(e) => {
            logger::warn(MODULE, format!("failed to serialize LeaveRoom: {e}"));
            best_effort_close(ws, timeout_dur).await;
            return;
        }
    };

    let res = tokio::time::timeout(timeout_dur, async {
        ws.send(Message::text(leave_json)).await?;
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: Utf8Bytes::default(),
        };
        ws.close(Some(frame)).await?;
        let _ = ws.get_mut().shutdown().await;
        drain_close_handshake(ws).await;
        Ok::<(), WsError>(())
    })
    .await;

    match res {
        Ok(Ok(())) => logger::info(MODULE, "sent LeaveRoom and WebSocket Close frame"),
        Ok(Err(e)) if is_benign_close_error(&e) => {
            logger::info(MODULE, "sent LeaveRoom; server closed signaling stream")
        }
        Ok(Err(e)) => logger::warn(MODULE, format!("best-effort LeaveRoom/Close error: {e}")),
        Err(_) => logger::warn(MODULE, "best-effort LeaveRoom/Close timed out"),
    }
}

async fn best_effort_close(ws: &mut ClientWsStream, timeout_dur: Duration) {
    let frame = CloseFrame {
        code: CloseCode::Normal,
        reason: Utf8Bytes::default(),
    };
    let res = tokio::time::timeout(timeout_dur, async {
        ws.close(Some(frame)).await?;
        let _ = ws.get_mut().shutdown().await;
        drain_close_handshake(ws).await;
        Ok::<(), WsError>(())
    })
    .await;

    match res {
        Ok(Ok(())) => logger::info(MODULE, "sent WebSocket Close frame"),
        Ok(Err(e)) if is_benign_close_error(&e) => {}
        Ok(Err(e)) => logger::warn(MODULE, format!("best-effort Close error: {e}")),
        Err(_) => logger::warn(MODULE, "best-effort Close timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{CandidateKind, StartPunchMsg, WaitingForPeerMsg};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    async fn send_server_msg(ws: &mut WebSocketStream<TcpStream>, msg: &ServerSignalMsg) {
        let json = serde_json::to_string(msg).unwrap();
        ws.send(Message::text(json)).await.unwrap();
    }

    async fn recv_client_msg(ws: &mut WebSocketStream<TcpStream>) -> ClientSignalMsg {
        loop {
            match ws.next().await.expect("unexpected EOF").unwrap() {
                Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                Message::Ping(_) => ws.flush().await.unwrap(),
                Message::Pong(_) => {}
                other => panic!("expected ClientSignalMsg text frame, got {other:?}"),
            }
        }
    }

    async fn expect_client_close(ws: &mut WebSocketStream<TcpStream>) {
        let ev = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for client close");
        assert!(
            matches!(ev, Some(Ok(Message::Close(_))) | None | Some(Err(WsError::ConnectionClosed))),
            "expected Close from client, got {ev:?}"
        );
    }

    fn sample_candidate(addr: &str) -> Candidate {
        Candidate {
            kind: CandidateKind::HostLocal,
            addr: addr.to_string(),
        }
    }

    #[test]
    fn url_parsing_valid_and_invalid() {
        assert_eq!(
            WsUrl::parse("ws://127.0.0.1:9000/ws").unwrap(),
            WsUrl {
                host: "127.0.0.1".into(),
                port: 9000,
                path: "/ws".into(),
                host_header: "127.0.0.1:9000".into(),
            }
        );
        assert_eq!(
            WsUrl::parse("ws://127.0.0.1").unwrap(),
            WsUrl {
                host: "127.0.0.1".into(),
                port: 80,
                path: "/".into(),
                host_header: "127.0.0.1".into(),
            }
        );
        assert_eq!(
            WsUrl::parse("ws://signal.example.com/a/b?token=1").unwrap(),
            WsUrl {
                host: "signal.example.com".into(),
                port: 80,
                path: "/a/b?token=1".into(),
                host_header: "signal.example.com".into(),
            }
        );
        assert_eq!(
            WsUrl::parse("ws://[::1]:8080/signal").unwrap(),
            WsUrl {
                host: "::1".into(),
                port: 8080,
                path: "/signal".into(),
                host_header: "[::1]:8080".into(),
            }
        );
        assert_eq!(
            WsUrl::parse("ws://[::1]/path").unwrap(),
            WsUrl {
                host: "::1".into(),
                port: 80,
                path: "/path".into(),
                host_header: "[::1]".into(),
            }
        );

        let invalid_urls = [
            "",
            "wss://127.0.0.1:8443/ws",
            "WSS://example.com/ws",
            "http://127.0.0.1:8080/ws",
            "ws://",
            "ws://:8080/ws",
            "ws://127.0.0.1:0/ws",
            "ws://127.0.0.1:70000/ws",
            "ws://127.0.0.1:abc/ws",
            "ws://::1:8080/ws",
            "ws://[::1/ws",
            "ws://[not-ipv6]/ws",
            "ws://user@127.0.0.1/ws",
            "ws://127.0.0.1/ws#frag",
            "ws://127.0.0.1/bad space",
        ];
        for url in invalid_urls {
            assert!(
                matches!(WsUrl::parse(url), Err(ProbeError::InvalidInput(_))),
                "expected InvalidInput for {url:?}"
            );
        }
    }

    #[tokio::test]
    async fn input_validation_rejects_bad_room_and_label() {
        let cancel = CancellationToken::new();
        assert!(matches!(
            pair_in_room(
                "ws://127.0.0.1:8080/ws",
                "ab",
                None,
                "alice",
                vec![],
                5,
                cancel.clone(),
            )
            .await,
            Err(ProbeError::InvalidInput(_))
        ));
        assert!(matches!(
            pair_in_room(
                "ws://127.0.0.1:8080/ws",
                "room1",
                None,
                "   ",
                vec![],
                5,
                cancel.clone(),
            )
            .await,
            Err(ProbeError::InvalidInput(_))
        ));
        assert!(matches!(
            pair_in_room(
                "wss://127.0.0.1:8080/ws",
                "room1",
                None,
                "alice",
                vec![],
                5,
                cancel,
            )
            .await,
            Err(ProbeError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn pairing_flow_waiting_start_punch_and_ping_pong() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (guard_ping_done_tx, guard_ping_done_rx) = oneshot::channel::<()>();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            let join = recv_client_msg(&mut ws).await;
            assert_eq!(
                join,
                ClientSignalMsg::JoinRoom(JoinRoomMsg {
                    room_code: "Room42".into(),
                    token: Some("tok-1".into()),
                    peer_label: "alice".into(),
                    candidates: vec![sample_candidate("192.168.1.10:5000")],
                })
            );

            // Ping during pairing must receive Pong
            ws.send(Message::Ping(b"pair".to_vec().into())).await.unwrap();
            let pong = ws.next().await.unwrap().unwrap();
            assert!(matches!(pong, Message::Pong(ref p) if p.as_ref() == b"pair"));

            send_server_msg(
                &mut ws,
                &ServerSignalMsg::WaitingForPeer(WaitingForPeerMsg {
                    room_code: "Room42".into(),
                    ttl_secs: 120,
                }),
            )
            .await;

            send_server_msg(
                &mut ws,
                &ServerSignalMsg::StartPunch(StartPunchMsg {
                    session_id: "sess_1".into(),
                    peer_label: "bob".into(),
                    peer_candidates: vec![sample_candidate("198.51.100.2:6000")],
                    start_delay_ms: 300,
                    punch_duration_ms: 6000,
                }),
            )
            .await;

            // Verify SignalGuard still replies Pong to Ping after StartPunch
            ws.send(Message::Ping(b"guard".to_vec().into())).await.unwrap();
            let guard_pong = ws.next().await.unwrap().unwrap();
            assert!(matches!(guard_pong, Message::Pong(ref p) if p.as_ref() == b"guard"));
            let _ = guard_ping_done_tx.send(());

            // Close from client without LeaveRoom
            expect_client_close(&mut ws).await;
        });

        let url = format!("ws://{addr}/ws");
        let (pairing, guard) = pair_in_room(
            &url,
            "Room42",
            Some("tok-1"),
            "alice",
            vec![sample_candidate("192.168.1.10:5000")],
            5,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            pairing,
            PairingResult {
                session_id: "sess_1".into(),
                peer_label: "bob".into(),
                peer_candidates: vec![sample_candidate("198.51.100.2:6000")],
                start_delay_ms: 300,
                punch_duration_ms: 6000,
            }
        );

        guard_ping_done_rx.await.unwrap();
        guard.close().await;
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn error_room_full_propagates_and_closes_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = recv_client_msg(&mut ws).await;

            send_server_msg(
                &mut ws,
                &ServerSignalMsg::Error(SignalErrorMsg {
                    error: ProbeError::RoomFull,
                }),
            )
            .await;

            expect_client_close(&mut ws).await;
        });

        let url = format!("ws://{addr}/ws");
        let res = pair_in_room(
            &url,
            "Room42",
            None,
            "alice",
            vec![],
            5,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(res.unwrap_err(), ProbeError::RoomFull);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn cancel_before_start_punch_sends_leave_room_and_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = recv_client_msg(&mut ws).await;

            send_server_msg(
                &mut ws,
                &ServerSignalMsg::WaitingForPeer(WaitingForPeerMsg {
                    room_code: "Room42".into(),
                    ttl_secs: 60,
                }),
            )
            .await;

            cancel_trigger.cancel();

            assert_eq!(recv_client_msg(&mut ws).await, ClientSignalMsg::LeaveRoom);
            expect_client_close(&mut ws).await;
        });

        let url = format!("ws://{addr}/ws");
        let res = pair_in_room(&url, "Room42", None, "alice", vec![], 5, cancel).await;
        assert_eq!(res.unwrap_err(), ProbeError::Cancelled);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn timeout_before_start_punch_sends_leave_room_and_returns_peer_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = recv_client_msg(&mut ws).await;

            send_server_msg(
                &mut ws,
                &ServerSignalMsg::WaitingForPeer(WaitingForPeerMsg {
                    room_code: "Room42".into(),
                    ttl_secs: 1,
                }),
            )
            .await;

            assert_eq!(recv_client_msg(&mut ws).await, ClientSignalMsg::LeaveRoom);
            expect_client_close(&mut ws).await;
        });

        let url = format!("ws://{addr}/ws");
        let res = pair_in_room(
            &url,
            "Room42",
            None,
            "alice",
            vec![],
            1,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(res.unwrap_err(), ProbeError::PeerTimeout);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn signal_guard_wait_peer_left_and_send_leave_and_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (peer_left_gate_tx, peer_left_gate_rx) = oneshot::channel::<()>();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = recv_client_msg(&mut ws).await;

            send_server_msg(
                &mut ws,
                &ServerSignalMsg::StartPunch(StartPunchMsg {
                    session_id: "sess_2".into(),
                    peer_label: "bob".into(),
                    peer_candidates: vec![],
                    start_delay_ms: 300,
                    punch_duration_ms: 6000,
                }),
            )
            .await;

            let _ = peer_left_gate_rx.await;
            send_server_msg(&mut ws, &ServerSignalMsg::PeerLeft).await;

            assert_eq!(recv_client_msg(&mut ws).await, ClientSignalMsg::LeaveRoom);
            expect_client_close(&mut ws).await;
        });

        let url = format!("ws://{addr}/ws");
        let (_, mut guard) = pair_in_room(
            &url,
            "Room42",
            None,
            "alice",
            vec![],
            5,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        // Cancel wait_peer_left once in select! to confirm cancel-safety
        let fired = tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(15)) => true,
            _ = guard.wait_peer_left() => false,
        };
        assert!(fired);

        let _ = peer_left_gate_tx.send(());
        assert!(guard.wait_peer_left().await);
        assert!(guard.wait_peer_left().await);

        guard.send_leave_and_close().await;
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn signal_guard_wait_peer_left_returns_false_on_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = recv_client_msg(&mut ws).await;

            send_server_msg(
                &mut ws,
                &ServerSignalMsg::StartPunch(StartPunchMsg {
                    session_id: "sess_3".into(),
                    peer_label: "bob".into(),
                    peer_candidates: vec![],
                    start_delay_ms: 300,
                    punch_duration_ms: 6000,
                }),
            )
            .await;

            let frame = CloseFrame {
                code: CloseCode::Normal,
                reason: Utf8Bytes::default(),
            };
            ws.close(Some(frame)).await.unwrap();
            expect_client_close(&mut ws).await;
        });

        let url = format!("ws://{addr}/ws");
        let (_, mut guard) = pair_in_room(
            &url,
            "Room42",
            None,
            "alice",
            vec![],
            5,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(!guard.wait_peer_left().await);
        guard.close().await;
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn server_unreachable_on_refused_port_and_non_101_handshake() {
        let closed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = closed_listener.local_addr().unwrap();
        drop(closed_listener);

        let err = pair_in_room(
            &format!("ws://{dead_addr}/ws"),
            "Room42",
            None,
            "alice",
            vec![],
            2,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProbeError::ServerUnreachable(_)));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf).await;
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
        });

        let err = pair_in_room(
            &format!("ws://{addr}/ws"),
            "Room42",
            None,
            "alice",
            vec![],
            2,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProbeError::ServerUnreachable(_)));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn signal_guard_drop_aborts_task_and_closes_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = recv_client_msg(&mut ws).await;

            send_server_msg(
                &mut ws,
                &ServerSignalMsg::StartPunch(StartPunchMsg {
                    session_id: "sess_drop".into(),
                    peer_label: "bob".into(),
                    peer_candidates: vec![],
                    start_delay_ms: 300,
                    punch_duration_ms: 6000,
                }),
            )
            .await;

            let eof_res = tokio::time::timeout(Duration::from_secs(2), ws.next())
                .await
                .expect("socket should close promptly on SignalGuard drop");
            assert!(matches!(eof_res, None | Some(Err(_))));
        });

        let url = format!("ws://{addr}/ws");
        let (_, guard) = pair_in_room(
            &url,
            "Room42",
            None,
            "alice",
            vec![],
            5,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        drop(guard);
        server_task.await.unwrap();
    }
}
