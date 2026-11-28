import type {
  LinkProtocol,
  NatFiltering,
  NatMapping,
  NatStatus,
  P2pLatencyTier,
  P2pReport,
  P2pStabilityGrade,
  P2pVerdict,
  ProbeError,
  PunchStrategy,
  SignalingProtocol,
} from '../types/protocol';

const STAGE_LABELS: Record<string, string> = {
  probing_nat: '正在探测公网映射与 NAT 过滤行为',
  probing_links: '正在直连探测目标节点',
  preparing_candidates: '正在采集本机网络与公网候选地址',
  connecting_signaling: '正在连接信令节点',
  reconnecting_signaling: '信令服务器不可达，正在自动重试',
  waiting_for_peer: '已成功连接节点并进入房间，等待对端加入',
  punching: '双端同步打洞中',
  sampling: '直连通道已建立，正在固定窗口采样',
  saving_report: '正在保存评估报告',
};

export function formatStageLabel(stage: string): string {
  return STAGE_LABELS[stage] ?? stage;
}

export function formatProbeError(err: ProbeError): string {
  if (typeof err === 'string') {
    switch (err) {
      case 'unauthorized':
        return '访问口令错误或未填写';
      case 'room_not_found':
        return '房间不存在或已过期';
      case 'room_full':
        return '该房间人数已满或服务端达到 64 房间上限';
      case 'peer_timeout':
        return '等待对端加入超时';
      case 'peer_left':
        return '对端已主动退出测试';
      case 'cancelled':
        return '测试已取消';
    }
  }
  if ('invalid_input' in err) {
    if (err.invalid_input === 'task_already_running') {
      return '已有任务正在运行';
    }
    return `参数格式或范围有误：${err.invalid_input}`;
  }
  if ('server_unreachable' in err) {
    return `信令/探测服务器不可达或连接断开：${err.server_unreachable}`;
  }
  if ('punch_failed' in err) {
    return `打洞失败：${err.punch_failed}`;
  }
  if ('internal' in err) {
    return `内部异常：${err.internal}`;
  }
  return '未知错误';
}

export function formatVerdict(verdict: P2pVerdict): {
  label: string;
  tone: 'excellent' | 'good' | 'unstable' | 'failed';
} {
  switch (verdict) {
    case 'excellent':
      return { label: '直连极佳', tone: 'excellent' };
    case 'good':
      return { label: '直连可用', tone: 'good' };
    case 'unstable':
      return { label: '存在明显抖动或丢包', tone: 'unstable' };
    case 'failed':
      return { label: '打洞超时或连接失败', tone: 'failed' };
  }
}

export function formatStabilityGrade(
  grade: P2pStabilityGrade | null | undefined,
  fallbackVerdict?: P2pVerdict,
): {
  label: string;
  tone: 'excellent' | 'good' | 'unstable' | 'failed';
} {
  switch (grade) {
    case 'rock_solid':
      return { label: '极稳', tone: 'excellent' };
    case 'good':
      return { label: '良好', tone: 'good' };
    case 'mild_jitter':
      return { label: '轻度抖动', tone: 'good' };
    case 'severe_jitter':
      return { label: '剧烈抖动', tone: 'unstable' };
    case 'failed':
      return { label: '打洞超时或连接失败', tone: 'failed' };
    default:
      return fallbackVerdict
        ? formatVerdict(fallbackVerdict)
        : { label: '未知', tone: 'unstable' };
  }
}

export function formatLatencyTier(
  tier: P2pLatencyTier | null | undefined,
  avgRttMs?: number | null,
): string | null {
  switch (tier) {
    case 'ultra_low':
      return '极低延迟';
    case 'low':
      return '低延迟';
    case 'medium':
      return '中等延迟';
    case 'high':
      return '跨区高延迟';
    default:
      if (avgRttMs === null || avgRttMs === undefined || Number.isNaN(avgRttMs)) {
        return null;
      }
      if (avgRttMs < 30) return '极低延迟';
      if (avgRttMs < 80) return '低延迟';
      if (avgRttMs <= 150) return '中等延迟';
      return '跨区高延迟';
  }
}

export function formatP2pHeadline(report: P2pReport): {
  label: string;
  tone: 'excellent' | 'good' | 'unstable' | 'failed';
} {
  const stability = formatStabilityGrade(report.stability_grade, report.verdict);
  if (stability.tone === 'failed' || !report.punched) {
    return stability;
  }
  const latency = formatLatencyTier(report.latency_tier, report.avg_rtt_ms);
  return {
    label: latency ? `${stability.label} · ${latency}` : stability.label,
    tone: stability.tone,
  };
}

export function formatNatStatus(status: NatStatus): string {
  switch (status) {
    case 'complete_full':
      return '完整探测（主+辅节点）';
    case 'complete_single_vps':
      return '单 VPS 增强探测';
    case 'public_stun_only':
      return '零 VPS 公共 STUN';
    case 'unreachable':
      return '节点全部不可达';
  }
}

export function formatNatMapping(mapping: NatMapping): string {
  switch (mapping) {
    case 'endpoint_independent':
      return '锥形映射 (Endpoint-Independent)';
    case 'address_or_port_dependent_symmetric':
      return '对称型 NAT4 (Dependent)';
    case 'unknown':
      return '未知';
  }
}

export function formatNatFiltering(filtering: NatFiltering): string {
  switch (filtering) {
    case 'full_cone_nat1':
      return '全锥形 NAT1 (Full Cone)';
    case 'address_restricted_nat2':
      return '受限锥形 NAT2 (Address-Restricted)';
    case 'port_restricted_nat3':
      return '端口受限锥形 NAT3 (Port-Restricted)';
    case 'cone_nat1_or_nat2':
      return '锥形 NAT1 / NAT2';
    case 'not_tested_public_stun':
      return '未测过滤（纯公共 STUN 模式）';
    case 'unknown':
      return '未知';
  }
}

export function formatTime(unixMs: number): string {
  const d = new Date(unixMs);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

export function formatMs(val: number | null | undefined, digits = 1): string {
  if (val === null || val === undefined || Number.isNaN(val)) {
    return '--';
  }
  return `${val.toFixed(digits)} ms`;
}

export function formatLossPct(ratio: number | null | undefined): string {
  if (ratio === null || ratio === undefined || Number.isNaN(ratio)) {
    return '--';
  }
  return `${(ratio * 100).toFixed(1)}%`;
}

export function generateRoomCode(length = 8): string {
  const alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789';
  const arr = new Uint32Array(length);
  crypto.getRandomValues(arr);
  return Array.from(arr, (x) => alphabet[x % alphabet.length]).join('');
}

export function validateRoomCode(code: string): string | null {
  if (code.length < 4 || code.length > 16 || !/^[A-Za-z0-9]+$/.test(code)) {
    return '房间号需为 4～16 位字母或数字';
  }
  return null;
}

export function validateWsUrl(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) {
    return '请填写信令服务器 ws:// 地址';
  }
  if (/^wss:\/\//i.test(trimmed)) {
    return '当前版本仅支持 ws:// 明文信令，暂不支持 wss://';
  }
  if (!/^ws:\/\//i.test(trimmed)) {
    return '信令地址必须以 ws:// 开头';
  }
  if (/[\s#]/.test(trimmed)) {
    return '信令地址不能包含空格或 # 片段';
  }
  return null;
}

const IPV4_REGEX =
  /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;

export function isIpv4Literal(host: string): boolean {
  return IPV4_REGEX.test(host.trim());
}

export function isIpv6Literal(host: string): boolean {
  const trimmed = host.trim();
  if (!trimmed) return false;
  const bare =
    trimmed.startsWith('[') && trimmed.endsWith(']')
      ? trimmed.slice(1, -1)
      : trimmed;
  if (!bare.includes(':') || /[\s/?#@[\]]/.test(bare)) {
    return false;
  }
  try {
    const parsed = new URL(`http://[${bare}]`);
    return parsed.hostname.startsWith('[') && parsed.hostname.endsWith(']');
  } catch {
    return false;
  }
}

export function normalizeHost(raw: string): string {
  const trimmed = raw.trim();
  if (trimmed.startsWith('[') && trimmed.endsWith(']') && isIpv6Literal(trimmed)) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
}

export function formatHostPort(host: string, port: number): string {
  const normalized = normalizeHost(host);
  if (isIpv6Literal(normalized)) {
    return `[${normalized}]:${port}`;
  }
  return `${normalized}:${port}`;
}

export function buildWsUrl(
  protocol: SignalingProtocol,
  host: string,
  port: number,
): string {
  return `${protocol}://${formatHostPort(normalizeHost(host), port)}`;
}

function parseValidPort(portStr: string): number | null {
  if (!/^\d+$/.test(portStr)) return null;
  const n = Number(portStr);
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : null;
}

export function parseServerPaste(
  raw: string,
):
  | { ok: true; protocol?: SignalingProtocol; host: string; ws_port?: number }
  | { ok: false; error: string } {
  const trimmed = raw.trim();
  if (!trimmed) {
    return { ok: true, host: '' };
  }
  if (/^wss:\/\//i.test(trimmed)) {
    return { ok: false, error: '当前版本仅支持 ws:// 明文信令，暂不支持 wss://' };
  }
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(trimmed) && !/^ws:\/\//i.test(trimmed)) {
    return { ok: false, error: '信令地址必须以 ws:// 开头' };
  }
  if (/^ws:\/\//i.test(trimmed)) {
    if (/[\s#]/.test(trimmed)) {
      return { ok: false, error: '信令地址不能包含空格或 # 片段' };
    }
    try {
      const parsed = new URL(trimmed);
      const host = normalizeHost(parsed.hostname);
      if (!host) {
        return { ok: false, error: '信令地址缺少主机名' };
      }
      if (parsed.port) {
        const port = parseValidPort(parsed.port);
        if (port === null) {
          return { ok: false, error: '信令端口需为 1~65535 的整数' };
        }
        return { ok: true, protocol: 'ws', host, ws_port: port };
      }
      return { ok: true, protocol: 'ws', host };
    } catch {
      return { ok: false, error: '信令地址格式有误' };
    }
  }

  const bracketed = /^\[([^\]]+)\](?::(\d+))?$/.exec(trimmed);
  if (bracketed) {
    const host = bracketed[1];
    if (!isIpv6Literal(host)) {
      return { ok: false, error: 'IPv6 地址格式有误' };
    }
    if (bracketed[2] !== undefined) {
      const port = parseValidPort(bracketed[2]);
      if (port === null) {
        return { ok: false, error: '信令端口需为 1~65535 的整数' };
      }
      return { ok: true, host, ws_port: port };
    }
    return { ok: true, host };
  }

  if (isIpv6Literal(trimmed)) {
    return { ok: true, host: normalizeHost(trimmed) };
  }

  const hostPort = /^([^:\s/]+):(\d+)$/.exec(trimmed);
  if (hostPort) {
    const port = parseValidPort(hostPort[2]);
    if (port === null) {
      return { ok: false, error: '信令端口需为 1~65535 的整数' };
    }
    return { ok: true, host: hostPort[1], ws_port: port };
  }

  return { ok: true, host: normalizeHost(trimmed) };
}

export function validateServerConfig(
  protocol: SignalingProtocol,
  host: string,
  wsPort: number,
): string | null {
  const normalized = normalizeHost(host);
  if (!normalized) {
    return '请填写服务器域名或 IP';
  }
  if (/[\s/?#@]/.test(normalized)) {
    return '服务器主机名格式有误';
  }
  if (!Number.isInteger(wsPort) || wsPort < 1 || wsPort > 65535) {
    return '信令端口需为 1~65535 的整数';
  }
  const wsUrl = buildWsUrl(protocol, normalized, wsPort);
  const wsErr = validateWsUrl(wsUrl);
  if (wsErr) {
    return wsErr;
  }
  try {
    const parsed = new URL(wsUrl);
    if (!parsed.hostname) {
      return '服务器主机名格式有误';
    }
  } catch {
    return '服务器主机名或端口格式有误';
  }
  return null;
}

export function formatPunchStrategy(strategy: PunchStrategy): string {
  switch (strategy) {
    case 'standard_udp_v6_first':
      return 'IPv6 优先（默认）';
    case 'ipv4_only':
      return '仅 IPv4';
    case 'ipv6_only':
      return '仅 IPv6';
  }
}

export function formatLinkProtocol(protocol: LinkProtocol): string {
  if (protocol === 'tcp') {
    return 'TCP';
  }
  return `TLS+SNI (${protocol.tls_sni.sni})`;
}

export interface FormattedNatNote {
  kind: 'mappings' | 'text';
  title?: string;
  entries?: { source: string; mapped: string }[];
  text?: string;
}

export function formatNatNoteEntries(notes: string[]): FormattedNatNote[] {
  return notes.map((note) => {
    const trimmed = note.trim();
    const stunMatch = /^Resolved public STUN IPs:\s*(.+)$/i.exec(trimmed);
    if (stunMatch) {
      return {
        kind: 'mappings',
        title: '公共 STUN 节点解析 IP',
        entries: parseKvMappingList(stunMatch[1]),
      };
    }

    const match = /^Observed IPv4 mappings:\s*(.+)$/i.exec(trimmed);
    if (!match) {
      return { kind: 'text', text: note };
    }
    return {
      kind: 'mappings',
      title: '观测到的 IPv4 映射',
      entries: parseKvMappingList(match[1]),
    };
  });
}

function parseKvMappingList(raw: string): { source: string; mapped: string }[] {
  return raw
    .split(/,\s*/)
    .map((item) => item.trim())
    .filter((item) => item.length > 0)
    .map((item) => {
      const eqIdx = item.lastIndexOf('=');
      if (eqIdx === -1) {
        return { source: item, mapped: '' };
      }
      return {
        source: item.slice(0, eqIdx).trim(),
        mapped: item.slice(eqIdx + 1).trim(),
      };
    });
}
