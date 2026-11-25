import type {
  NatFiltering,
  NatMapping,
  NatStatus,
  P2pVerdict,
  ProbeError,
} from '../types/protocol';

const STAGE_LABELS: Record<string, string> = {
  probing_nat: '正在探测公网映射与 NAT 过滤行为',
  preparing_candidates: '正在采集本机网络与公网候选地址',
  waiting_for_peer: '已进入房间，等待对端加入',
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
