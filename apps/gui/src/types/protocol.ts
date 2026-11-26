export type ProbeError =
  | 'unauthorized'
  | 'room_not_found'
  | 'room_full'
  | 'peer_timeout'
  | 'peer_left'
  | 'cancelled'
  | { invalid_input: string }
  | { server_unreachable: string }
  | { punch_failed: string }
  | { internal: string };

export type IpFamily = 'ipv4' | 'ipv6';

export type PunchStrategy = 'standard_udp_v6_first' | 'ipv4_only' | 'ipv6_only';

export type SignalingProtocol = 'ws';

export const SIGNALING_PROTOCOLS: SignalingProtocol[] = ['ws'];

export const DEFAULT_SIGNALING_PORT = 34780;
export const DEFAULT_PROBE_PORT_P1 = 34778;
export const DEFAULT_PROBE_PORT_P2 = 34779;

export interface ServerConfig {
  protocol: SignalingProtocol;
  host: string;
  ws_port: number;
  probe_p1_port: number;
  probe_p2_port: number;
  token: string;
  helper_v4_p1: string;
}

export type P2pVerdict = 'excellent' | 'good' | 'unstable' | 'failed';

export type NatStatus =
  | 'complete_full'
  | 'complete_single_vps'
  | 'public_stun_only'
  | 'unreachable';

export type NatMapping =
  | 'endpoint_independent'
  | 'address_or_port_dependent_symmetric'
  | 'unknown';

export type NatFiltering =
  | 'full_cone_nat1'
  | 'address_restricted_nat2'
  | 'port_restricted_nat3'
  | 'cone_nat1_or_nat2'
  | 'not_tested_public_stun'
  | 'unknown';

export interface LatencyPoint {
  seq: number;
  elapsed_ms: number;
  rtt_ms: number | null;
}

export interface LivePeerInfo {
  room_code?: string | null;
  peer_label?: string | null;
  ip_family?: IpFamily | null;
  active_peer_addr?: string | null;
  punch_elapsed_ms?: number | null;
}

export interface NatReport {
  status: NatStatus;
  public_ipv4: string | null;
  public_ipv6: string | null;
  ipv6_reachable: boolean;
  mapping: NatMapping;
  filtering: NatFiltering;
  estimated_difficulty: string;
  notes: string[];
}

export interface NatTaskInput {
  public_stun_servers: string[];
  primary_v4_p1: string | null;
  primary_v4_p2: string | null;
  primary_v6: string | null;
  helper_v4_p1: string | null;
  token: string | null;
  timeout_ms: number;
}

export interface P2pTaskInput {
  signaling_ws_url: string;
  room_code: string;
  token: string | null;
  peer_label: string;
  echo_udp_v4: string | null;
  echo_udp_v6: string | null;
  public_stun_servers: string[];
  candidate_timeout_ms: number;
  pair_timeout_secs: number;
  punch_timeout_ms: number;
  window_secs: number;
  packet_rate_hz: number;
  strategy: PunchStrategy;
}

export interface P2pReport {
  punched: boolean;
  peer_label: string | null;
  active_peer_addr: string | null;
  ip_family: IpFamily | null;
  punch_elapsed_ms: number | null;
  sent: number;
  received: number;
  loss_ratio: number;
  avg_rtt_ms: number | null;
  p95_rtt_ms: number | null;
  jitter_ms: number | null;
  series: LatencyPoint[];
  verdict: P2pVerdict;
  failure_reason: string | null;
  interrupted_reason: string | null;
}

export interface AssessmentReport {
  report_id: string;
  created_unix_ms: number;
  nat_report: NatReport | null;
  p2p_report: P2pReport | null;
  storage_warning: string | null;
}

export interface ReportSummary {
  report_id: string;
  created_unix_ms: number;
  has_nat: boolean;
  nat_status: NatStatus | null;
  has_p2p: boolean;
  p2p_verdict: P2pVerdict | null;
}

export interface RunningState {
  task_id: string;
  stage: string;
  progress_pct: number;
  live_point: LatencyPoint | null;
  live_peer?: LivePeerInfo | null;
}

export type AssessmentState =
  | 'idle'
  | { running: RunningState }
  | { completed: AssessmentReport }
  | { cancelled: { reason: string | null } }
  | { failed: ProbeError };

export interface NodePreset {
  id: string;
  name: string;
  protocol: SignalingProtocol;
  host: string;
  ws_port: number;
  probe_p1_port: number;
  probe_p2_port: number;
  token: string;
  helper_v4_p1: string;
}

export const DEFAULT_STUN_SERVERS: string[] = [
  'stun.qq.com:3478',
  'stun.miwifi.com:3478',
  'stun.cloudflare.com:3478',
];
