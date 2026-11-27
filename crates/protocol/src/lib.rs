use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_UDP_PACKET_BYTES: usize = 256;
pub const MAX_S2_ALT_REPLY_BYTES: usize = 128;
pub const TASK_ALREADY_RUNNING_MSG: &str = "task_already_running";

pub const MAX_ROOMS: usize = 64;
pub const ROOM_WAIT_TTL_SECS: u64 = 120;
pub const PUNCH_ROOM_GRACE_MS: u64 = 2000;
pub const KEEPALIVE_INTERVAL_SECS: u64 = 15;
pub const DEFAULT_PUNCH_START_DELAY_MS: u32 = 300;
pub const DEFAULT_PUNCH_TIMEOUT_MS: u32 = 6000;
pub const MIN_PUNCH_TIMEOUT_MS: u32 = 3000;
pub const MAX_PUNCH_TIMEOUT_MS: u32 = 15000;
pub const DEFAULT_WINDOW_SECS: u32 = 20;
pub const MIN_WINDOW_SECS: u32 = 5;
pub const MAX_WINDOW_SECS: u32 = 60;
pub const DEFAULT_PACKET_RATE_HZ: u32 = 10;
pub const MIN_PACKET_RATE_HZ: u32 = 1;
pub const MAX_PACKET_RATE_HZ: u32 = 20;
pub const DEFAULT_PROBE_PORT_P1: u16 = 34778;
pub const DEFAULT_PROBE_PORT_P2: u16 = 34779;
pub const DEFAULT_SIGNALING_WS_PORT: u16 = 34780;

pub const DEFAULT_PUBLIC_STUN_SERVERS: &[&str] = &[
    "stun.qq.com:3478",
    "stun.miwifi.com:3478",
    "stun.cloudflare.com:3478",
];

fn default_nat_timeout_ms() -> u64 {
    2500
}

fn default_candidate_timeout_ms() -> u64 {
    2500
}

fn default_pair_timeout_secs() -> u64 {
    ROOM_WAIT_TTL_SECS
}

fn default_punch_timeout_ms() -> u32 {
    DEFAULT_PUNCH_TIMEOUT_MS
}

fn default_window_secs() -> u32 {
    DEFAULT_WINDOW_SECS
}

fn default_packet_rate_hz() -> u32 {
    DEFAULT_PACKET_RATE_HZ
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeError {
    InvalidInput(String),
    Unauthorized,
    ServerUnreachable(String),
    RoomNotFound,
    RoomFull,
    PeerTimeout,
    PeerLeft,
    PunchFailed(String),
    Cancelled,
    Internal(String),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(msg) => write!(f, "invalid_input: {msg}"),
            Self::Unauthorized => write!(f, "unauthorized"),
            Self::ServerUnreachable(msg) => write!(f, "server_unreachable: {msg}"),
            Self::RoomNotFound => write!(f, "room_not_found"),
            Self::RoomFull => write!(f, "room_full"),
            Self::PeerTimeout => write!(f, "peer_timeout"),
            Self::PeerLeft => write!(f, "peer_left"),
            Self::PunchFailed(msg) => write!(f, "punch_failed: {msg}"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Internal(msg) => write!(f, "internal: {msg}"),
        }
    }
}

impl std::error::Error for ProbeError {}

pub fn validate_room_code(code: &str) -> Result<(), ProbeError> {
    let len = code.len();
    if !(4..=16).contains(&len) || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ProbeError::InvalidInput(format!(
            "invalid room_code {code:?}: must be 4..=16 ASCII alphanumeric characters"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    HostLocal,
    HostGlobalV6,
    ServerReflexiveV4,
    ServerReflexiveV6,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub addr: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyPoint {
    pub seq: u32,
    pub elapsed_ms: u32,
    #[serde(default)]
    pub rtt_ms: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P2pVerdict {
    Excellent,
    Good,
    Unstable,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P2pLatencyTier {
    UltraLow,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P2pStabilityGrade {
    RockSolid,
    Good,
    MildJitter,
    SevereJitter,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoMode {
    #[default]
    Normal,
    ReplyFromAltPort,
    ReplyFromHelperIp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrEchoReq {
    pub nonce: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default)]
    pub mode: EchoMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeIpHint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub nonce: u64,
    pub target_client: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrEchoResp {
    pub nonce: u64,
    pub observed_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responder_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum S2WireMessage {
    AddrEchoReq(AddrEchoReq),
    ChangeIpHint(ChangeIpHint),
    AddrEchoResp(AddrEchoResp),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRoomMsg {
    pub room_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub peer_label: String,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientSignalMsg {
    JoinRoom(JoinRoomMsg),
    LeaveRoom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingForPeerMsg {
    pub room_code: String,
    pub ttl_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartPunchMsg {
    pub session_id: String,
    pub peer_label: String,
    pub peer_candidates: Vec<Candidate>,
    pub start_delay_ms: u32,
    pub punch_duration_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalErrorMsg {
    pub error: ProbeError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerSignalMsg {
    WaitingForPeer(WaitingForPeerMsg),
    StartPunch(StartPunchMsg),
    PeerLeft,
    Error(SignalErrorMsg),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PunchStrategy {
    #[default]
    StandardUdpV6First,
    Ipv4Only,
    Ipv6Only,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P2pMsgKind {
    Punch,
    PunchAck,
    Ping,
    Pong,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct P2pDirectMsg {
    pub session_id: String,
    pub kind: P2pMsgKind,
    pub seq: u32,
    pub sent_offset_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpFamily {
    Ipv4,
    Ipv6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatStatus {
    CompleteFull,
    CompleteSingleVps,
    PublicStunOnly,
    Unreachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatMapping {
    EndpointIndependent,
    AddressOrPortDependentSymmetric,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatFiltering {
    FullConeNat1,
    AddressRestrictedNat2,
    PortRestrictedNat3,
    ConeNat1OrNat2,
    NotTestedPublicStun,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatReport {
    pub status: NatStatus,
    #[serde(default)]
    pub public_ipv4: Option<String>,
    #[serde(default)]
    pub public_ipv6: Option<String>,
    #[serde(default)]
    pub ipv6_reachable: bool,
    pub mapping: NatMapping,
    pub filtering: NatFiltering,
    #[serde(default)]
    pub estimated_difficulty: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatTaskInput {
    #[serde(default)]
    pub public_stun_servers: Vec<String>,
    #[serde(default)]
    pub primary_v4_p1: Option<String>,
    #[serde(default)]
    pub primary_v4_p2: Option<String>,
    #[serde(default)]
    pub primary_v6: Option<String>,
    #[serde(default)]
    pub helper_v4_p1: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default = "default_nat_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub require_vps: bool,
}

impl Default for NatTaskInput {
    fn default() -> Self {
        Self {
            public_stun_servers: DEFAULT_PUBLIC_STUN_SERVERS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            primary_v4_p1: None,
            primary_v4_p2: None,
            primary_v6: None,
            helper_v4_p1: None,
            token: None,
            timeout_ms: default_nat_timeout_ms(),
            require_vps: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct P2pTaskInput {
    pub signaling_ws_url: String,
    pub room_code: String,
    #[serde(default)]
    pub token: Option<String>,
    pub peer_label: String,
    #[serde(default)]
    pub echo_udp_v4: Option<String>,
    #[serde(default)]
    pub echo_udp_v6: Option<String>,
    #[serde(default)]
    pub public_stun_servers: Vec<String>,
    #[serde(default = "default_candidate_timeout_ms")]
    pub candidate_timeout_ms: u64,
    #[serde(default = "default_pair_timeout_secs")]
    pub pair_timeout_secs: u64,
    #[serde(default = "default_punch_timeout_ms")]
    pub punch_timeout_ms: u32,
    #[serde(default = "default_window_secs")]
    pub window_secs: u32,
    #[serde(default = "default_packet_rate_hz")]
    pub packet_rate_hz: u32,
    #[serde(default)]
    pub strategy: PunchStrategy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct P2pReport {
    pub punched: bool,
    #[serde(default)]
    pub peer_label: Option<String>,
    #[serde(default)]
    pub active_peer_addr: Option<String>,
    #[serde(default)]
    pub ip_family: Option<IpFamily>,
    #[serde(default)]
    pub punch_elapsed_ms: Option<u32>,
    pub sent: u32,
    pub received: u32,
    pub loss_ratio: f32,
    #[serde(default)]
    pub avg_rtt_ms: Option<f32>,
    #[serde(default)]
    pub p50_rtt_ms: Option<f32>,
    #[serde(default)]
    pub p95_rtt_ms: Option<f32>,
    #[serde(default)]
    pub jitter_ms: Option<f32>,
    #[serde(default)]
    pub iqr_ms: Option<f32>,
    #[serde(default)]
    pub tail_bloat_ms: Option<f32>,
    #[serde(default)]
    pub spike_ratio: Option<f32>,
    #[serde(default)]
    pub latency_tier: Option<P2pLatencyTier>,
    #[serde(default)]
    pub stability_grade: Option<P2pStabilityGrade>,
    #[serde(default)]
    pub series: Vec<LatencyPoint>,
    pub verdict: P2pVerdict,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub interrupted_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssessmentReport {
    pub report_id: String,
    pub created_unix_ms: u64,
    #[serde(default)]
    pub nat_report: Option<NatReport>,
    #[serde(default)]
    pub p2p_report: Option<P2pReport>,
    #[serde(default)]
    pub storage_warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSummary {
    pub report_id: String,
    pub created_unix_ms: u64,
    pub has_nat: bool,
    #[serde(default)]
    pub nat_status: Option<NatStatus>,
    #[serde(default)]
    pub has_p2p: bool,
    #[serde(default)]
    pub p2p_verdict: Option<P2pVerdict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LivePeerInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_family: Option<IpFamily>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_peer_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub punch_elapsed_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum AssessmentState {
    Idle,
    Running {
        task_id: String,
        stage: String,
        progress_pct: u8,
        #[serde(default)]
        live_point: Option<LatencyPoint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        live_peer: Option<LivePeerInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_message: Option<String>,
    },
    Completed(AssessmentReport),
    Cancelled {
        #[serde(default)]
        reason: Option<String>,
    },
    Failed(ProbeError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s2_wire_messages_roundtrip_and_fit_limits() {
        let req = S2WireMessage::AddrEchoReq(AddrEchoReq {
            nonce: u64::MAX,
            token: Some("secret-token".into()),
            mode: EchoMode::ReplyFromAltPort,
        });
        let req_bytes = serde_json::to_vec(&req).unwrap();
        assert!(req_bytes.len() <= MAX_UDP_PACKET_BYTES);
        assert_eq!(serde_json::from_slice::<S2WireMessage>(&req_bytes).unwrap(), req);

        let resp = S2WireMessage::AddrEchoResp(AddrEchoResp {
            nonce: u64::MAX,
            observed_addr: "203.0.113.255:65535".into(),
            responder_id: Some("vps-a".into()),
        });
        let resp_bytes = serde_json::to_vec(&resp).unwrap();
        assert!(
            resp_bytes.len() <= MAX_S2_ALT_REPLY_BYTES,
            "resp len {} > {}",
            resp_bytes.len(),
            MAX_S2_ALT_REPLY_BYTES
        );
        assert_eq!(serde_json::from_slice::<S2WireMessage>(&resp_bytes).unwrap(), resp);
    }

    #[test]
    fn forward_compatible_unknown_fields_are_ignored() {
        let raw_v1_0 = r#"{
            "report_id": "nat_1",
            "created_unix_ms": 123456,
            "nat_report": {
                "status": "complete_single_vps",
                "public_ipv4": "198.51.100.1",
                "public_ipv6": null,
                "ipv6_reachable": false,
                "mapping": "endpoint_independent",
                "filtering": "cone_nat1_or_nat2",
                "estimated_difficulty": "easy",
                "notes": [],
                "future_field": "ignored"
            },
            "link_reports": []
        }"#;
        let parsed: AssessmentReport = serde_json::from_str(raw_v1_0).unwrap();
        assert_eq!(parsed.report_id, "nat_1");
        assert!(parsed.p2p_report.is_none());
        assert_eq!(
            parsed.nat_report.unwrap().status,
            NatStatus::CompleteSingleVps
        );

        let raw_summary = r#"{
            "report_id": "nat_1",
            "created_unix_ms": 123456,
            "has_nat": true,
            "nat_status": "public_stun_only"
        }"#;
        let parsed_summary: ReportSummary = serde_json::from_str(raw_summary).unwrap();
        assert!(!parsed_summary.has_p2p);
        assert_eq!(parsed_summary.p2p_verdict, None);
    }

    #[test]
    fn signaling_and_p2p_wire_messages_roundtrip_and_fit_limits() {
        let join = ClientSignalMsg::JoinRoom(JoinRoomMsg {
            room_code: "Abc12345".into(),
            token: Some("tok".into()),
            peer_label: "alice".into(),
            candidates: vec![Candidate {
                kind: CandidateKind::HostGlobalV6,
                addr: "[2001:db8::1]:50000".into(),
            }],
        });
        let join_json = serde_json::to_string(&join).unwrap();
        assert_eq!(serde_json::from_str::<ClientSignalMsg>(&join_json).unwrap(), join);

        let start = ServerSignalMsg::StartPunch(StartPunchMsg {
            session_id: "p2p_Abc12345_1".into(),
            peer_label: "bob".into(),
            peer_candidates: vec![Candidate {
                kind: CandidateKind::ServerReflexiveV4,
                addr: "198.51.100.2:41234".into(),
            }],
            start_delay_ms: DEFAULT_PUNCH_START_DELAY_MS,
            punch_duration_ms: DEFAULT_PUNCH_TIMEOUT_MS,
        });
        let start_json = serde_json::to_string(&start).unwrap();
        assert_eq!(serde_json::from_str::<ServerSignalMsg>(&start_json).unwrap(), start);

        let err_msg = ServerSignalMsg::Error(SignalErrorMsg {
            error: ProbeError::RoomFull,
        });
        let err_json = serde_json::to_string(&err_msg).unwrap();
        assert_eq!(serde_json::from_str::<ServerSignalMsg>(&err_json).unwrap(), err_msg);

        let direct = P2pDirectMsg {
            session_id: "p2p_1234567890abcdef_99999".into(),
            kind: P2pMsgKind::PunchAck,
            seq: u32::MAX,
            sent_offset_ms: 60000,
        };
        let direct_bytes = serde_json::to_vec(&direct).unwrap();
        assert!(
            direct_bytes.len() <= MAX_UDP_PACKET_BYTES,
            "direct len {} > {}",
            direct_bytes.len(),
            MAX_UDP_PACKET_BYTES
        );
        assert_eq!(
            serde_json::from_slice::<P2pDirectMsg>(&direct_bytes).unwrap(),
            direct
        );
    }

    #[test]
    fn room_code_validation_enforces_4_to_16_alnum() {
        assert!(validate_room_code("abc1").is_ok());
        assert!(validate_room_code("1234567890ABCDEF").is_ok());
        assert!(validate_room_code("abc").is_err());
        assert!(validate_room_code("1234567890ABCDEFG").is_err());
        assert!(validate_room_code("ab-12").is_err());
        assert!(validate_room_code("ab 12").is_err());
    }

    #[test]
    fn assessment_state_running_backward_compatible_without_live_peer() {
        let old_json = r#"{"running":{"task_id":"p2p_1","stage":"punching","progress_pct":45}}"#;
        let parsed: AssessmentState = serde_json::from_str(old_json).unwrap();
        assert_eq!(
            parsed,
            AssessmentState::Running {
                task_id: "p2p_1".into(),
                stage: "punching".into(),
                progress_pct: 45,
                live_point: None,
                live_peer: None,
                status_message: None,
            }
        );

        let with_peer = AssessmentState::Running {
            task_id: "p2p_1".into(),
            stage: "sampling".into(),
            progress_pct: 60,
            live_point: Some(LatencyPoint {
                seq: 1,
                elapsed_ms: 100,
                rtt_ms: Some(12.5),
            }),
            live_peer: Some(LivePeerInfo {
                room_code: Some("room1".into()),
                peer_label: Some("bob".into()),
                ip_family: Some(IpFamily::Ipv6),
                active_peer_addr: Some("[2001:db8::2]:50000".into()),
                punch_elapsed_ms: Some(85),
            }),
            status_message: Some("connected".into()),
        };
        let ser = serde_json::to_string(&with_peer).unwrap();
        assert_eq!(serde_json::from_str::<AssessmentState>(&ser).unwrap(), with_peer);
    }
}
