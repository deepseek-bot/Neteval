import { reactive } from 'vue';
import {
  cancelCurrent,
  deleteReport,
  getCurrentState,
  getReport,
  listHistory,
  parseInvokeError,
  startNatAssessment,
  startP2pAssessment,
  subscribeAssessmentState,
} from '../api/tauri';
import {
  DEFAULT_STUN_SERVERS,
  type AssessmentReport,
  type AssessmentState,
  type LatencyPoint,
  type LivePeerInfo,
  type NatReport,
  type NodePreset,
  type P2pTaskInput,
  type ProbeError,
  type ReportSummary,
} from '../types/protocol';
import {
  formatProbeError,
  generateRoomCode,
  validateRoomCode,
  validateWsUrl,
} from '../utils/format';

const PREFS_KEY = 'neteval_gui_prefs_v1';

interface StoredPrefs {
  presets: NodePreset[];
  selectedPresetId: string;
  p2p: {
    signaling_ws_url: string;
    room_code: string;
    peer_label: string;
    token: string;
    echo_udp_v4: string;
    echo_udp_v6: string;
    stun_text: string;
    candidate_timeout_ms: number;
    pair_timeout_secs: number;
    punch_timeout_ms: number;
    window_secs: number;
    packet_rate_hz: number;
  };
  nat: {
    useSelfHostedVps: boolean;
    primary_v4_p1: string;
    primary_v4_p2: string;
    primary_v6: string;
    helper_v4_p1: string;
    token: string;
    stun_text: string;
    timeout_ms: number;
  };
}

function defaultPrefs(): StoredPrefs {
  return {
    presets: [],
    selectedPresetId: '',
    p2p: {
      signaling_ws_url: '',
      room_code: generateRoomCode(8),
      peer_label: 'player-1',
      token: '',
      echo_udp_v4: '',
      echo_udp_v6: '',
      stun_text: DEFAULT_STUN_SERVERS.join(', '),
      candidate_timeout_ms: 2500,
      pair_timeout_secs: 120,
      punch_timeout_ms: 6000,
      window_secs: 20,
      packet_rate_hz: 10,
    },
    nat: {
      useSelfHostedVps: false,
      primary_v4_p1: '',
      primary_v4_p2: '',
      primary_v6: '',
      helper_v4_p1: '',
      token: '',
      stun_text: DEFAULT_STUN_SERVERS.join(', '),
      timeout_ms: 2500,
    },
  };
}

function loadPrefs(): StoredPrefs {
  const base = defaultPrefs();
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (!raw) return base;
    const parsed = JSON.parse(raw) as Partial<StoredPrefs>;
    return {
      presets: Array.isArray(parsed.presets) ? parsed.presets : base.presets,
      selectedPresetId:
        typeof parsed.selectedPresetId === 'string'
          ? parsed.selectedPresetId
          : base.selectedPresetId,
      p2p: { ...base.p2p, ...(parsed.p2p ?? {}) },
      nat: { ...base.nat, ...(parsed.nat ?? {}) },
    };
  } catch {
    return base;
  }
}

const initialPrefs = loadPrefs();

export const appStore = reactive({
  activeTab: 'p2p' as 'nat' | 'p2p' | 'history',
  leftSidebarOpen: false,
  rightSidebarOpen: false,
  isMobile: typeof window !== 'undefined' ? window.innerWidth < 860 : false,

  assessmentState: 'idle' as AssessmentState,
  activeKind: null as 'nat' | 'p2p' | null,
  liveSeries: [] as LatencyPoint[],
  livePeer: null as LivePeerInfo | null,
  waitingDeadlineMs: null as number | null,
  waitingRemainSecs: null as number | null,

  latestNatReport: null as NatReport | null,
  latestP2pReport: null as AssessmentReport | null,
  modalReport: null as AssessmentReport | null,
  history: [] as ReportSummary[],
  statusNotice: '' as string,
  errorNotice: '' as string,

  presets: initialPrefs.presets,
  selectedPresetId: initialPrefs.selectedPresetId,
  p2pForm: initialPrefs.p2p,
  natForm: initialPrefs.nat,
});

let initialized = false;
let countdownTimer: number | null = null;
let unlistenFn: (() => void) | null = null;

export function savePrefs(): void {
  try {
    const payload: StoredPrefs = {
      presets: appStore.presets,
      selectedPresetId: appStore.selectedPresetId,
      p2p: appStore.p2pForm,
      nat: appStore.natForm,
    };
    localStorage.setItem(PREFS_KEY, JSON.stringify(payload));
  } catch {
    // Ignore storage quota failures
  }
}

function parseStunList(text: string): string[] {
  return text
    .split(/[\n,;]+/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

function clearWaitingTimer(): void {
  if (countdownTimer !== null) {
    window.clearInterval(countdownTimer);
    countdownTimer = null;
  }
  appStore.waitingDeadlineMs = null;
  appStore.waitingRemainSecs = null;
}

function ensureWaitingTimer(pairTimeoutSecs: number): void {
  if (appStore.waitingDeadlineMs === null) {
    appStore.waitingDeadlineMs = Date.now() + pairTimeoutSecs * 1000;
    appStore.waitingRemainSecs = pairTimeoutSecs;
  }
  if (countdownTimer === null) {
    countdownTimer = window.setInterval(() => {
      if (appStore.waitingDeadlineMs === null) return;
      const remain = Math.max(
        0,
        Math.ceil((appStore.waitingDeadlineMs - Date.now()) / 1000),
      );
      appStore.waitingRemainSecs = remain;
    }, 500);
  }
}

function handleStateUpdate(state: AssessmentState): void {
  appStore.assessmentState = state;

  if (state === 'idle') {
    clearWaitingTimer();
    return;
  }

  if ('running' in state) {
    const r = state.running;
    appStore.errorNotice = '';
    appStore.statusNotice = '';
    appStore.activeKind = r.task_id.startsWith('nat_') ? 'nat' : 'p2p';

    if (r.live_peer) {
      appStore.livePeer = r.live_peer;
    }

    if (r.stage === 'waiting_for_peer') {
      ensureWaitingTimer(appStore.p2pForm.pair_timeout_secs);
      if (!appStore.isMobile) {
        appStore.leftSidebarOpen = true;
      }
    } else {
      clearWaitingTimer();
    }

    if (r.stage === 'punching' && !appStore.isMobile) {
      appStore.leftSidebarOpen = true;
    }

    if (r.live_point) {
      const exists = appStore.liveSeries.some((p) => p.seq === r.live_point!.seq);
      if (!exists) {
        appStore.liveSeries.push(r.live_point);
      }
    }
    return;
  }

  clearWaitingTimer();

  if ('completed' in state) {
    const report = state.completed;
    if (report.nat_report) {
      appStore.latestNatReport = report.nat_report;
    }
    if (report.p2p_report) {
      appStore.latestP2pReport = report;
      if (report.p2p_report.series.length > 0) {
        appStore.liveSeries = report.p2p_report.series;
      }
    }
    appStore.modalReport = report;
    void refreshHistory();
    return;
  }

  if ('cancelled' in state) {
    const reason = state.cancelled.reason;
    appStore.statusNotice = reason ? `已停止：${reason}` : '测试已停止';
    return;
  }

  if ('failed' in state) {
    appStore.errorNotice = formatProbeError(state.failed);
  }
}

export function isRunning(): boolean {
  return typeof appStore.assessmentState === 'object' && 'running' in appStore.assessmentState;
}

export async function initStore(): Promise<void> {
  if (initialized) return;
  initialized = true;

  const onResize = () => {
    appStore.isMobile = window.innerWidth < 860;
  };
  window.addEventListener('resize', onResize);

  const isAndroid = /android/i.test(navigator.userAgent);
  if (isAndroid) {
    document.addEventListener('visibilitychange', () => {
      if (document.visibilityState === 'hidden' && isRunning()) {
        void cancelCurrent('android_background');
      }
    });
    window.addEventListener('offline', () => {
      if (isRunning()) {
        void cancelCurrent('network_changed');
      }
    });
  }

  try {
    unlistenFn = await subscribeAssessmentState(handleStateUpdate);
    const current = await getCurrentState();
    handleStateUpdate(current);
    await refreshHistory();
  } catch (err) {
    appStore.errorNotice = formatProbeError(parseInvokeError(err));
  }
}

export function disposeStore(): void {
  clearWaitingTimer();
  if (unlistenFn) {
    unlistenFn();
    unlistenFn = null;
  }
}

export async function refreshHistory(): Promise<void> {
  try {
    appStore.history = await listHistory();
  } catch (err) {
    appStore.errorNotice = formatProbeError(parseInvokeError(err));
  }
}

export async function openReportModal(id: string): Promise<void> {
  try {
    appStore.errorNotice = '';
    appStore.modalReport = await getReport(id);
  } catch (err) {
    appStore.errorNotice = formatProbeError(parseInvokeError(err));
  }
}

export async function removeHistoryItem(id: string): Promise<void> {
  try {
    appStore.errorNotice = '';
    await deleteReport(id);
    if (appStore.modalReport?.report_id === id) {
      appStore.modalReport = null;
    }
    await refreshHistory();
  } catch (err) {
    appStore.errorNotice = formatProbeError(parseInvokeError(err));
  }
}

export function applyPreset(presetId: string): void {
  appStore.selectedPresetId = presetId;
  const preset = appStore.presets.find((p) => p.id === presetId);
  if (!preset) {
    savePrefs();
    return;
  }
  appStore.p2pForm.signaling_ws_url = preset.signaling_ws_url;
  appStore.p2pForm.echo_udp_v4 = preset.echo_udp_v4;
  appStore.p2pForm.echo_udp_v6 = preset.echo_udp_v6;
  appStore.p2pForm.token = preset.token;
  appStore.natForm.primary_v4_p1 = preset.primary_v4_p1;
  appStore.natForm.primary_v4_p2 = preset.primary_v4_p2;
  appStore.natForm.primary_v6 = preset.primary_v6;
  appStore.natForm.helper_v4_p1 = preset.helper_v4_p1;
  appStore.natForm.token = preset.token;
  if (preset.primary_v4_p1.trim()) {
    appStore.natForm.useSelfHostedVps = true;
  }
  savePrefs();
}

export function saveCurrentAsPreset(name: string): string | null {
  const cleanName = name.trim();
  if (!cleanName) {
    return '请输入预设名称';
  }
  const existing = appStore.presets.find((p) => p.name === cleanName);
  const preset: NodePreset = {
    id: existing ? existing.id : `preset_${Date.now()}`,
    name: cleanName,
    signaling_ws_url: appStore.p2pForm.signaling_ws_url.trim(),
    echo_udp_v4: appStore.p2pForm.echo_udp_v4.trim(),
    echo_udp_v6: appStore.p2pForm.echo_udp_v6.trim(),
    primary_v4_p1: appStore.natForm.primary_v4_p1.trim(),
    primary_v4_p2: appStore.natForm.primary_v4_p2.trim(),
    primary_v6: appStore.natForm.primary_v6.trim(),
    helper_v4_p1: appStore.natForm.helper_v4_p1.trim(),
    token: (appStore.p2pForm.token || appStore.natForm.token).trim(),
  };
  if (existing) {
    appStore.presets = appStore.presets.map((p) => (p.id === existing.id ? preset : p));
  } else {
    appStore.presets = [...appStore.presets, preset];
  }
  appStore.selectedPresetId = preset.id;
  savePrefs();
  return null;
}

export function deletePreset(id: string): void {
  appStore.presets = appStore.presets.filter((p) => p.id !== id);
  if (appStore.selectedPresetId === id) {
    appStore.selectedPresetId = '';
  }
  savePrefs();
}

export async function runP2pTest(): Promise<void> {
  appStore.errorNotice = '';
  appStore.statusNotice = '';

  const wsErr = validateWsUrl(appStore.p2pForm.signaling_ws_url);
  if (wsErr) {
    appStore.errorNotice = wsErr;
    return;
  }
  const roomErr = validateRoomCode(appStore.p2pForm.room_code.trim());
  if (roomErr) {
    appStore.errorNotice = roomErr;
    return;
  }
  if (!appStore.p2pForm.peer_label.trim()) {
    appStore.errorNotice = '请填写本机昵称';
    return;
  }

  savePrefs();
  appStore.liveSeries = [];
  appStore.livePeer = null;
  appStore.latestP2pReport = null;

  const input: P2pTaskInput = {
    signaling_ws_url: appStore.p2pForm.signaling_ws_url.trim(),
    room_code: appStore.p2pForm.room_code.trim(),
    token: appStore.p2pForm.token.trim() ? appStore.p2pForm.token.trim() : null,
    peer_label: appStore.p2pForm.peer_label.trim(),
    echo_udp_v4: appStore.p2pForm.echo_udp_v4.trim()
      ? appStore.p2pForm.echo_udp_v4.trim()
      : null,
    echo_udp_v6: appStore.p2pForm.echo_udp_v6.trim()
      ? appStore.p2pForm.echo_udp_v6.trim()
      : null,
    public_stun_servers: parseStunList(appStore.p2pForm.stun_text),
    candidate_timeout_ms: Number(appStore.p2pForm.candidate_timeout_ms) || 2500,
    pair_timeout_secs: Math.min(120, Math.max(1, Number(appStore.p2pForm.pair_timeout_secs) || 120)),
    punch_timeout_ms: Math.min(15000, Math.max(3000, Number(appStore.p2pForm.punch_timeout_ms) || 6000)),
    window_secs: Math.min(60, Math.max(5, Number(appStore.p2pForm.window_secs) || 20)),
    packet_rate_hz: Math.min(20, Math.max(1, Number(appStore.p2pForm.packet_rate_hz) || 10)),
    strategy: 'standard_udp_v6_first',
  };

  try {
    await startP2pAssessment(input);
  } catch (err) {
    const parsed: ProbeError = parseInvokeError(err);
    appStore.errorNotice = formatProbeError(parsed);
  }
}

export async function runNatTest(): Promise<void> {
  appStore.errorNotice = '';
  appStore.statusNotice = '';
  savePrefs();

  const useVps = appStore.natForm.useSelfHostedVps;
  const opt = (s: string) => (s.trim() ? s.trim() : null);

  try {
    await startNatAssessment({
      public_stun_servers: parseStunList(appStore.natForm.stun_text),
      primary_v4_p1: useVps ? opt(appStore.natForm.primary_v4_p1) : null,
      primary_v4_p2: useVps ? opt(appStore.natForm.primary_v4_p2) : null,
      primary_v6: useVps ? opt(appStore.natForm.primary_v6) : null,
      helper_v4_p1: useVps ? opt(appStore.natForm.helper_v4_p1) : null,
      token: useVps ? opt(appStore.natForm.token) : null,
      timeout_ms: Math.max(300, Number(appStore.natForm.timeout_ms) || 2500),
    });
  } catch (err) {
    appStore.errorNotice = formatProbeError(parseInvokeError(err));
  }
}

export async function stopCurrentTask(reason = 'user_cancelled'): Promise<void> {
  try {
    await cancelCurrent(reason);
  } catch (err) {
    appStore.errorNotice = formatProbeError(parseInvokeError(err));
  }
}
