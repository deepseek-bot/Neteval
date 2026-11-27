import { reactive } from 'vue';
import pkg from '../../package.json';
import {
  cancelCurrent,
  deleteReport,
  getAppVersion,
  getCurrentState,
  getReport,
  listHistory,
  openExternalUrl,
  parseInvokeError,
  startNatAssessment,
  startP2pAssessment,
  subscribeAssessmentState,
} from '../api/tauri';
import {
  AUTO_CHECK_INTERVAL_MS,
  STARTUP_COOLDOWN_MS,
  checkLatestRelease,
  normalizeVersion,
  type UpdateInfo,
} from '../utils/updater';
import {
  DEFAULT_PROBE_PORT_P1,
  DEFAULT_PROBE_PORT_P2,
  DEFAULT_SIGNALING_PORT,
  DEFAULT_STUN_SERVERS,
  type AssessmentReport,
  type AssessmentState,
  type LatencyPoint,
  type LivePeerInfo,
  type NatReport,
  type NodePreset,
  type P2pTaskInput,
  type ProbeError,
  type PunchStrategy,
  type ReportSummary,
  type ServerConfig,
} from '../types/protocol';
import {
  buildWsUrl,
  formatHostPort,
  formatProbeError,
  generateRoomCode,
  isIpv4Literal,
  isIpv6Literal,
  normalizeHost,
  parseServerPaste,
  validateRoomCode,
  validateServerConfig,
} from '../utils/format';

const PREFS_KEY = 'neteval_gui_prefs_v1';

interface StoredPrefs {
  presets: NodePreset[];
  selectedPresetId: string;
  server: ServerConfig;
  p2p: {
    room_code: string;
    peer_label: string;
    strategy: PunchStrategy;
    stun_text: string;
    candidate_timeout_ms: number;
    pair_timeout_secs: number;
    punch_timeout_ms: number;
    window_secs: number;
    packet_rate_hz: number;
  };
  nat: {
    useSelfHostedVps: boolean;
    stun_text: string;
    timeout_ms: number;
  };
  update?: {
    autoCheckUpdates: boolean;
    lastUpdateCheckAt: number;
    dismissedVersion: string;
  };
}

function defaultPrefs(): StoredPrefs {
  return {
    presets: [],
    selectedPresetId: '',
    server: {
      protocol: 'ws',
      host: '',
      ws_port: DEFAULT_SIGNALING_PORT,
      probe_p1_port: DEFAULT_PROBE_PORT_P1,
      probe_p2_port: DEFAULT_PROBE_PORT_P2,
      token: '',
      helper_v4_p1: '',
    },
    p2p: {
      room_code: generateRoomCode(8),
      peer_label: 'player-1',
      strategy: 'standard_udp_v6_first',
      stun_text: DEFAULT_STUN_SERVERS.join(', '),
      candidate_timeout_ms: 2500,
      pair_timeout_secs: 120,
      punch_timeout_ms: 6000,
      window_secs: 20,
      packet_rate_hz: 10,
    },
    nat: {
      useSelfHostedVps: false,
      stun_text: DEFAULT_STUN_SERVERS.join(', '),
      timeout_ms: 2500,
    },
    update: {
      autoCheckUpdates: true,
      lastUpdateCheckAt: 0,
      dismissedVersion: '',
    },
  };
}

function validPortOr(val: unknown, fallback: number): number {
  const n = Number(val);
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : fallback;
}

function extractHostPort(raw: unknown): { host: string; port?: number } | null {
  if (typeof raw !== 'string' || !raw.trim()) return null;
  const res = parseServerPaste(raw);
  if (!res.ok || !res.host) return null;
  return { host: res.host, port: res.ws_port };
}

function normalizeStrategy(val: unknown): PunchStrategy {
  if (val === 'ipv4_only' || val === 'ipv6_only' || val === 'standard_udp_v6_first') {
    return val;
  }
  return 'standard_udp_v6_first';
}

function migrateLegacyServer(
  legacyP2p: Record<string, unknown>,
  legacyNat: Record<string, unknown>,
  fallback: ServerConfig,
): ServerConfig {
  const wsInfo = extractHostPort(legacyP2p.signaling_ws_url);
  const p1Info =
    extractHostPort(legacyNat.primary_v4_p1) ??
    extractHostPort(legacyP2p.echo_udp_v4) ??
    extractHostPort(legacyNat.primary_v6) ??
    extractHostPort(legacyP2p.echo_udp_v6);
  const p2Info = extractHostPort(legacyNat.primary_v4_p2);

  const token =
    (typeof legacyP2p.token === 'string' && legacyP2p.token) ||
    (typeof legacyNat.token === 'string' ? legacyNat.token : '');

  return {
    protocol: 'ws',
    host: wsInfo?.host || p1Info?.host || p2Info?.host || fallback.host,
    ws_port: wsInfo?.port ?? fallback.ws_port,
    probe_p1_port: p1Info?.port ?? fallback.probe_p1_port,
    probe_p2_port: p2Info?.port ?? fallback.probe_p2_port,
    token,
    helper_v4_p1:
      typeof legacyNat.helper_v4_p1 === 'string'
        ? legacyNat.helper_v4_p1
        : fallback.helper_v4_p1,
  };
}

function migratePreset(raw: unknown): NodePreset | null {
  if (!raw || typeof raw !== 'object') return null;
  const obj = raw as Record<string, unknown>;
  if (typeof obj.id !== 'string' || typeof obj.name !== 'string') return null;

  if (typeof obj.host === 'string') {
    return {
      id: obj.id,
      name: obj.name,
      protocol: 'ws',
      host: obj.host,
      ws_port: validPortOr(obj.ws_port, DEFAULT_SIGNALING_PORT),
      probe_p1_port: validPortOr(obj.probe_p1_port, DEFAULT_PROBE_PORT_P1),
      probe_p2_port: validPortOr(obj.probe_p2_port, DEFAULT_PROBE_PORT_P2),
      token: typeof obj.token === 'string' ? obj.token : '',
      helper_v4_p1: typeof obj.helper_v4_p1 === 'string' ? obj.helper_v4_p1 : '',
    };
  }

  const wsInfo = extractHostPort(obj.signaling_ws_url);
  const p1Info =
    extractHostPort(obj.primary_v4_p1) ??
    extractHostPort(obj.echo_udp_v4) ??
    extractHostPort(obj.primary_v6) ??
    extractHostPort(obj.echo_udp_v6);
  const p2Info = extractHostPort(obj.primary_v4_p2);

  return {
    id: obj.id,
    name: obj.name,
    protocol: 'ws',
    host: wsInfo?.host || p1Info?.host || p2Info?.host || '',
    ws_port: wsInfo?.port ?? DEFAULT_SIGNALING_PORT,
    probe_p1_port: p1Info?.port ?? DEFAULT_PROBE_PORT_P1,
    probe_p2_port: p2Info?.port ?? DEFAULT_PROBE_PORT_P2,
    token: typeof obj.token === 'string' ? obj.token : '',
    helper_v4_p1: typeof obj.helper_v4_p1 === 'string' ? obj.helper_v4_p1 : '',
  };
}

function loadPrefs(): StoredPrefs {
  const base = defaultPrefs();
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (!raw) return base;
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    const rawP2p =
      parsed.p2p && typeof parsed.p2p === 'object'
        ? (parsed.p2p as Record<string, unknown>)
        : {};
    const rawNat =
      parsed.nat && typeof parsed.nat === 'object'
        ? (parsed.nat as Record<string, unknown>)
        : {};
    const rawUpdate =
      parsed.update && typeof parsed.update === 'object'
        ? (parsed.update as Record<string, unknown>)
        : {};
    const rawServer =
      parsed.server && typeof parsed.server === 'object'
        ? (parsed.server as Record<string, unknown>)
        : null;

    const server: ServerConfig = rawServer
      ? {
          protocol: 'ws',
          host: typeof rawServer.host === 'string' ? rawServer.host : base.server.host,
          ws_port: validPortOr(rawServer.ws_port, base.server.ws_port),
          probe_p1_port: validPortOr(rawServer.probe_p1_port, base.server.probe_p1_port),
          probe_p2_port: validPortOr(rawServer.probe_p2_port, base.server.probe_p2_port),
          token: typeof rawServer.token === 'string' ? rawServer.token : base.server.token,
          helper_v4_p1:
            typeof rawServer.helper_v4_p1 === 'string'
              ? rawServer.helper_v4_p1
              : base.server.helper_v4_p1,
        }
      : migrateLegacyServer(rawP2p, rawNat, base.server);

    const presets = Array.isArray(parsed.presets)
      ? parsed.presets
          .map(migratePreset)
          .filter((p): p is NodePreset => p !== null)
      : base.presets;

    return {
      presets,
      selectedPresetId:
        typeof parsed.selectedPresetId === 'string'
          ? parsed.selectedPresetId
          : base.selectedPresetId,
      server,
      p2p: {
        room_code:
          typeof rawP2p.room_code === 'string' && rawP2p.room_code
            ? rawP2p.room_code
            : base.p2p.room_code,
        peer_label:
          typeof rawP2p.peer_label === 'string' && rawP2p.peer_label
            ? rawP2p.peer_label
            : base.p2p.peer_label,
        strategy: normalizeStrategy(rawP2p.strategy),
        stun_text:
          typeof rawP2p.stun_text === 'string' ? rawP2p.stun_text : base.p2p.stun_text,
        candidate_timeout_ms:
          Number(rawP2p.candidate_timeout_ms) || base.p2p.candidate_timeout_ms,
        pair_timeout_secs:
          Number(rawP2p.pair_timeout_secs) || base.p2p.pair_timeout_secs,
        punch_timeout_ms:
          Number(rawP2p.punch_timeout_ms) || base.p2p.punch_timeout_ms,
        window_secs: Number(rawP2p.window_secs) || base.p2p.window_secs,
        packet_rate_hz: Number(rawP2p.packet_rate_hz) || base.p2p.packet_rate_hz,
      },
      nat: {
        useSelfHostedVps:
          typeof rawNat.useSelfHostedVps === 'boolean'
            ? rawNat.useSelfHostedVps
            : base.nat.useSelfHostedVps,
        stun_text:
          typeof rawNat.stun_text === 'string' ? rawNat.stun_text : base.nat.stun_text,
        timeout_ms: Number(rawNat.timeout_ms) || base.nat.timeout_ms,
      },
      update: {
        autoCheckUpdates:
          typeof rawUpdate.autoCheckUpdates === 'boolean'
            ? rawUpdate.autoCheckUpdates
            : (base.update?.autoCheckUpdates ?? true),
        lastUpdateCheckAt:
          typeof rawUpdate.lastUpdateCheckAt === 'number' &&
          Number.isFinite(rawUpdate.lastUpdateCheckAt)
            ? rawUpdate.lastUpdateCheckAt
            : 0,
        dismissedVersion:
          typeof rawUpdate.dismissedVersion === 'string' ? rawUpdate.dismissedVersion : '',
      },
    };
  } catch {
    return base;
  }
}

const initialPrefs = loadPrefs();

const isAndroidUa =
  typeof navigator !== 'undefined' && /android/i.test(navigator.userAgent);

function detectIsMobile(): boolean {
  return isAndroidUa || (typeof window !== 'undefined' ? window.innerWidth < 860 : false);
}

function syncMobileRootClass(mobile: boolean): void {
  if (typeof document !== 'undefined') {
    document.documentElement.classList.toggle('is-mobile', mobile);
  }
}

export const appStore = reactive({
  activeTab: 'p2p' as 'nat' | 'p2p' | 'history',
  leftSidebarOpen: false,
  rightSidebarOpen: false,
  isMobile: detectIsMobile(),

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
  natVpsUnreachableModal: null as { host: string; reason: string } | null,

  presets: initialPrefs.presets,
  selectedPresetId: initialPrefs.selectedPresetId,
  serverForm: initialPrefs.server,
  p2pForm: initialPrefs.p2p,
  natForm: initialPrefs.nat,

  appVersion: normalizeVersion(pkg.version),
  autoCheckUpdates: initialPrefs.update?.autoCheckUpdates ?? true,
  lastUpdateCheckAt: initialPrefs.update?.lastUpdateCheckAt ?? 0,
  updateDismissedVersion: initialPrefs.update?.dismissedVersion ?? '',
  updateStatus: 'idle' as 'idle' | 'checking' | 'available' | 'up_to_date' | 'error',
  updateInfo: null as UpdateInfo | null,
  updateError: '' as string,
});

let initialized = false;
let countdownTimer: number | null = null;
let unlistenFn: (() => void) | null = null;
let lastRunningTaskId = '';
let updateStartupTimer: number | null = null;
let updatePollTimer: number | null = null;
let visibilityListener: (() => void) | null = null;
let lastNatUsedVps = false;

function resetForNewTask(taskId: string): void {
  if (!taskId || taskId === lastRunningTaskId) return;
  lastRunningTaskId = taskId;
  if (taskId.startsWith('nat_')) {
    appStore.latestNatReport = null;
  } else if (taskId.startsWith('p2p_')) {
    appStore.liveSeries = [];
    appStore.livePeer = null;
    appStore.latestP2pReport = null;
  }
}

export function savePrefs(): void {
  try {
    const payload: StoredPrefs = {
      presets: appStore.presets,
      selectedPresetId: appStore.selectedPresetId,
      server: appStore.serverForm,
      p2p: appStore.p2pForm,
      nat: appStore.natForm,
      update: {
        autoCheckUpdates: appStore.autoCheckUpdates,
        lastUpdateCheckAt: appStore.lastUpdateCheckAt,
        dismissedVersion: appStore.updateDismissedVersion,
      },
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
    resetForNewTask(r.task_id);
    appStore.activeKind = r.task_id.startsWith('nat_') ? 'nat' : 'p2p';

    if (r.stage === 'reconnecting_signaling') {
      appStore.errorNotice = r.status_message || '自建信令服务器不可达，正在自动重试...';
      appStore.statusNotice = '';
    } else if (r.stage === 'waiting_for_peer') {
      appStore.errorNotice = '';
      appStore.statusNotice =
        r.status_message || '已成功连接信令节点并进入房间，等待对端加入';
    } else if (r.stage === 'punching' && r.status_message) {
      appStore.errorNotice = '';
      appStore.statusNotice = r.status_message;
    } else {
      appStore.errorNotice = '';
      appStore.statusNotice = '';
    }

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
    appStore.errorNotice = '';
    appStore.statusNotice = '';
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
    appStore.statusNotice = '';
    appStore.errorNotice = formatProbeError(state.failed);
    if (
      appStore.activeKind === 'nat' &&
      lastNatUsedVps &&
      typeof state.failed === 'object' &&
      'server_unreachable' in state.failed
    ) {
      appStore.natVpsUnreachableModal = {
        host: normalizeHost(appStore.serverForm.host) || '自建服务器',
        reason: state.failed.server_unreachable,
      };
    }
  }
}

export function isRunning(): boolean {
  return typeof appStore.assessmentState === 'object' && 'running' in appStore.assessmentState;
}

export async function initStore(): Promise<void> {
  if (initialized) return;
  initialized = true;

  const updateMobile = () => {
    const mobile = detectIsMobile();
    appStore.isMobile = mobile;
    syncMobileRootClass(mobile);
  };
  updateMobile();
  window.addEventListener('resize', updateMobile);

  visibilityListener = () => {
    if (isAndroidUa && document.visibilityState === 'hidden' && isRunning()) {
      void cancelCurrent('android_background');
    }
    if (
      document.visibilityState === 'visible' &&
      appStore.autoCheckUpdates &&
      Date.now() - appStore.lastUpdateCheckAt >= AUTO_CHECK_INTERVAL_MS
    ) {
      void checkForUpdates(true);
    }
  };
  document.addEventListener('visibilitychange', visibilityListener);

  if (isAndroidUa) {
    window.addEventListener('offline', () => {
      if (isRunning()) {
        void cancelCurrent('network_changed');
      }
    });
  }

  appStore.appVersion = normalizeVersion(await getAppVersion(pkg.version));

  if (
    appStore.autoCheckUpdates &&
    Date.now() - appStore.lastUpdateCheckAt >= STARTUP_COOLDOWN_MS
  ) {
    updateStartupTimer = window.setTimeout(() => {
      updateStartupTimer = null;
      if (appStore.autoCheckUpdates) {
        void checkForUpdates(true);
      }
    }, 3000);
  }
  updatePollTimer = window.setInterval(() => {
    if (appStore.autoCheckUpdates) {
      void checkForUpdates(true);
    }
  }, AUTO_CHECK_INTERVAL_MS);

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
  if (updateStartupTimer !== null) {
    window.clearTimeout(updateStartupTimer);
    updateStartupTimer = null;
  }
  if (updatePollTimer !== null) {
    window.clearInterval(updatePollTimer);
    updatePollTimer = null;
  }
  if (visibilityListener) {
    document.removeEventListener('visibilitychange', visibilityListener);
    visibilityListener = null;
  }
  if (unlistenFn) {
    unlistenFn();
    unlistenFn = null;
  }
}

export async function checkForUpdates(silent = false): Promise<void> {
  if (appStore.updateStatus === 'checking') return;
  appStore.updateStatus = 'checking';
  appStore.updateError = '';
  try {
    const info = await checkLatestRelease(appStore.appVersion);
    appStore.updateInfo = info;
    appStore.lastUpdateCheckAt = Date.now();
    appStore.updateStatus = info.hasUpdate ? 'available' : 'up_to_date';
    if (!info.hasUpdate) {
      appStore.updateDismissedVersion = '';
    }
    savePrefs();
  } catch (err) {
    const msg = err instanceof Error ? err.message : '检查更新失败';
    appStore.updateStatus = 'error';
    appStore.updateError = msg;
    if (!silent) {
      appStore.errorNotice = msg;
    }
  }
}

export function dismissUpdateBanner(): void {
  if (appStore.updateInfo?.latestVersion) {
    appStore.updateDismissedVersion = appStore.updateInfo.latestVersion;
    savePrefs();
  }
}

export async function openUpdateDownload(): Promise<void> {
  if (!appStore.updateInfo) return;
  await openExternalUrl(appStore.updateInfo.downloadUrl);
}

export async function openUpdateReleasePage(): Promise<void> {
  if (!appStore.updateInfo) return;
  await openExternalUrl(appStore.updateInfo.releaseUrl);
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

export function applyServerPaste(raw: string): boolean {
  const res = parseServerPaste(raw);
  if (!res.ok) {
    appStore.errorNotice = res.error;
    return true;
  }
  if (res.protocol === undefined && res.ws_port === undefined) {
    return false;
  }
  if (res.protocol) {
    appStore.serverForm.protocol = res.protocol;
  }
  appStore.serverForm.host = res.host;
  if (res.ws_port !== undefined) {
    appStore.serverForm.ws_port = res.ws_port;
  }
  appStore.errorNotice = '';
  savePrefs();
  return true;
}

export function applyPreset(presetId: string): void {
  appStore.selectedPresetId = presetId;
  const preset = appStore.presets.find((p) => p.id === presetId);
  if (!preset) {
    savePrefs();
    return;
  }
  appStore.serverForm.protocol = preset.protocol;
  appStore.serverForm.host = preset.host;
  appStore.serverForm.ws_port = preset.ws_port;
  appStore.serverForm.probe_p1_port = preset.probe_p1_port;
  appStore.serverForm.probe_p2_port = preset.probe_p2_port;
  appStore.serverForm.token = preset.token;
  appStore.serverForm.helper_v4_p1 = preset.helper_v4_p1;
  if (preset.host.trim()) {
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
    protocol: appStore.serverForm.protocol,
    host: normalizeHost(appStore.serverForm.host),
    ws_port: Number(appStore.serverForm.ws_port) || DEFAULT_SIGNALING_PORT,
    probe_p1_port: Number(appStore.serverForm.probe_p1_port) || DEFAULT_PROBE_PORT_P1,
    probe_p2_port: Number(appStore.serverForm.probe_p2_port) || DEFAULT_PROBE_PORT_P2,
    token: appStore.serverForm.token.trim(),
    helper_v4_p1: appStore.serverForm.helper_v4_p1.trim(),
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

  const wsPort = Number(appStore.serverForm.ws_port);
  const wsErr = validateServerConfig(
    appStore.serverForm.protocol,
    appStore.serverForm.host,
    wsPort,
  );
  if (wsErr) {
    appStore.errorNotice = wsErr;
    return;
  }

  const p1 = DEFAULT_PROBE_PORT_P1;

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

  const host = normalizeHost(appStore.serverForm.host);
  const p1Endpoint = formatHostPort(host, p1);
  let echo_udp_v4: string | null = p1Endpoint;
  let echo_udp_v6: string | null = p1Endpoint;
  if (isIpv4Literal(host)) {
    echo_udp_v6 = null;
  } else if (isIpv6Literal(host)) {
    echo_udp_v4 = null;
  }

  const token = appStore.serverForm.token.trim();
  const input: P2pTaskInput = {
    signaling_ws_url: buildWsUrl(appStore.serverForm.protocol, host, wsPort),
    room_code: appStore.p2pForm.room_code.trim(),
    token: token ? token : null,
    peer_label: appStore.p2pForm.peer_label.trim(),
    echo_udp_v4,
    echo_udp_v6,
    public_stun_servers: parseStunList(appStore.p2pForm.stun_text),
    candidate_timeout_ms: Number(appStore.p2pForm.candidate_timeout_ms) || 2500,
    pair_timeout_secs: Math.min(120, Math.max(1, Number(appStore.p2pForm.pair_timeout_secs) || 120)),
    punch_timeout_ms: Math.min(15000, Math.max(3000, Number(appStore.p2pForm.punch_timeout_ms) || 6000)),
    window_secs: Math.min(60, Math.max(5, Number(appStore.p2pForm.window_secs) || 20)),
    packet_rate_hz: Math.min(20, Math.max(1, Number(appStore.p2pForm.packet_rate_hz) || 10)),
    strategy: appStore.p2pForm.strategy,
  };

  try {
    const taskId = await startP2pAssessment(input);
    resetForNewTask(taskId);
  } catch (err) {
    const parsed: ProbeError = parseInvokeError(err);
    appStore.errorNotice = formatProbeError(parsed);
  }
}

export function closeNatVpsModal(): void {
  appStore.natVpsUnreachableModal = null;
}

export async function retryNatWithVps(): Promise<void> {
  appStore.natVpsUnreachableModal = null;
  await runNatTest();
}

export async function continueNatWithoutVps(): Promise<void> {
  appStore.natVpsUnreachableModal = null;
  await runNatTest({ skipVps: true });
}

export async function runNatTest(options?: { skipVps?: boolean }): Promise<void> {
  appStore.errorNotice = '';
  appStore.statusNotice = '';
  appStore.natVpsUnreachableModal = null;

  const useVps = !options?.skipVps && appStore.natForm.useSelfHostedVps;
  lastNatUsedVps = useVps;
  const opt = (s: string) => (s.trim() ? s.trim() : null);

  let primary_v4_p1: string | null = null;
  let primary_v4_p2: string | null = null;
  let primary_v6: string | null = null;
  let helper_v4_p1: string | null = null;
  let token: string | null = null;

  if (useVps) {
    const host = normalizeHost(appStore.serverForm.host);
    if (!host) {
      appStore.errorNotice = '请填写自建服务器域名或 IP';
      return;
    }
    const hostErr = validateServerConfig(appStore.serverForm.protocol, host, 1);
    if (hostErr) {
      appStore.errorNotice = hostErr;
      return;
    }

    const p1 = DEFAULT_PROBE_PORT_P1;
    const p2 = DEFAULT_PROBE_PORT_P2;

    const ep1 = formatHostPort(host, p1);
    const ep2 = formatHostPort(host, p2);
    primary_v4_p1 = ep1;
    primary_v4_p2 = ep2;
    primary_v6 = ep1;
    if (isIpv4Literal(host)) {
      primary_v6 = null;
    } else if (isIpv6Literal(host)) {
      primary_v4_p1 = null;
      primary_v4_p2 = null;
    }

    token = opt(appStore.serverForm.token);
  }

  savePrefs();

  try {
    const taskId = await startNatAssessment({
      public_stun_servers: parseStunList(appStore.natForm.stun_text),
      primary_v4_p1,
      primary_v4_p2,
      primary_v6,
      helper_v4_p1,
      token,
      timeout_ms: Math.max(300, Number(appStore.natForm.timeout_ms) || 2500),
      require_vps: useVps,
    });
    resetForNewTask(taskId);
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
