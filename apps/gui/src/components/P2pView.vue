<script setup lang="ts">
import { computed, ref } from 'vue';
import {
  appStore,
  applyPreset,
  applyServerPaste,
  isRunning,
  runP2pTest,
  savePrefs,
  stopCurrentTask,
} from '../stores/appState';
import { SIGNALING_PROTOCOLS } from '../types/protocol';
import {
  formatLossPct,
  formatMs,
  formatP2pHeadline,
  formatStageLabel,
  generateRoomCode,
  validateRoomCode,
  validateServerConfig,
} from '../utils/format';
import LatencyChart from './LatencyChart.vue';

const showToken = ref(false);
const copiedRoom = ref(false);

const P2P_STAGES = [
  { key: 'preparing_candidates', label: '采集候选' },
  { key: 'connecting_signaling', label: '连接节点' },
  { key: 'waiting_for_peer', label: '等待对端' },
  { key: 'punching', label: '同步打洞' },
  { key: 'sampling', label: '窗口采样' },
  { key: 'saving_report', label: '保存报告' },
];

const runningInfo = computed(() =>
  typeof appStore.assessmentState === 'object' && 'running' in appStore.assessmentState
    ? appStore.assessmentState.running
    : null,
);

const runningThis = computed(() => isRunning());

const currentStageIdx = computed(() => {
  if (!runningInfo.value) return -1;
  const normalizedStage =
    runningInfo.value.stage === 'reconnecting_signaling'
      ? 'connecting_signaling'
      : runningInfo.value.stage;
  return P2P_STAGES.findIndex((s) => s.key === normalizedStage);
});

const mobilePeerExpanded = computed(() => {
  if (!appStore.isMobile || !runningInfo.value) return false;
  return ['waiting_for_peer', 'punching', 'sampling', 'saving_report'].includes(
    runningInfo.value.stage,
  );
});

const liveSummary = computed(() => {
  const pts = appStore.liveSeries;
  if (pts.length === 0) {
    return { latest: null as number | null, avg: null as number | null, loss: null as number | null };
  }
  const latest = pts[pts.length - 1].rtt_ms;
  const ok = pts.map((p) => p.rtt_ms).filter((v): v is number => v !== null);
  const avg = ok.length > 0 ? ok.reduce((a, b) => a + b, 0) / ok.length : null;
  const loss = (pts.length - ok.length) / pts.length;
  return { latest, avg, loss };
});

const wsValidationError = computed(() => {
  if (!appStore.serverForm.host.trim()) return null;
  return validateServerConfig(
    appStore.serverForm.protocol,
    appStore.serverForm.host,
    Number(appStore.serverForm.ws_port),
  );
});

function onHostPaste(e: ClipboardEvent): void {
  const text = e.clipboardData?.getData('text') ?? '';
  if (text && applyServerPaste(text)) {
    e.preventDefault();
  }
}

const roomValidationError = computed(() => {
  const code = appStore.p2pForm.room_code.trim();
  if (!code) return null;
  return validateRoomCode(code);
});

const latestVerdict = computed(() => {
  const p2p = appStore.latestP2pReport?.p2p_report;
  return p2p ? formatP2pHeadline(p2p) : null;
});

function onRandomRoom(): void {
  appStore.p2pForm.room_code = generateRoomCode(8);
  savePrefs();
}

async function onCopyRoom(): Promise<void> {
  const code = appStore.p2pForm.room_code.trim();
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code);
    copiedRoom.value = true;
    window.setTimeout(() => {
      copiedRoom.value = false;
    }, 1500);
  } catch {
    // Clipboard unavailable in some environments
  }
}

function onToggleAction(): void {
  if (runningThis.value) {
    void stopCurrentTask('user_cancelled');
  } else {
    void runP2pTest();
  }
}
</script>

<template>
  <div class="p2p-workspace" :class="{ mobile: appStore.isMobile }">
    <div
      v-if="appStore.isMobile"
      class="mobile-peer-bar"
      :class="{ expanded: mobilePeerExpanded }"
    >
      <template v-if="mobilePeerExpanded && appStore.livePeer">
        <div class="mp-row mono">
          <span>房间：{{ appStore.livePeer.room_code || '--' }}</span>
          <span>对端：{{ appStore.livePeer.peer_label || '等待中' }}</span>
        </div>
        <div class="mp-row mono">
          <span>
            协议：{{
              appStore.livePeer.ip_family === 'ipv6'
                ? 'IPv6'
                : appStore.livePeer.ip_family === 'ipv4'
                  ? 'IPv4'
                  : '--'
            }}
          </span>
          <span>地址：{{ appStore.livePeer.active_peer_addr || '--' }}</span>
          <span>耗时：{{ formatMs(appStore.livePeer.punch_elapsed_ms, 0) }}</span>
        </div>
      </template>
      <template v-else>
        <div class="mp-thin mono">
          对端：{{ appStore.livePeer?.peer_label || '--' }} |
          {{ appStore.livePeer?.active_peer_addr || '未连接' }}
        </div>
      </template>
    </div>

    <div class="control-bar">
      <div class="ctrl-group ctrl-ws">
        <select
          v-if="appStore.presets.length > 0"
          class="preset-select"
          :disabled="runningThis"
          :value="appStore.selectedPresetId"
          @change="applyPreset(($event.target as HTMLSelectElement).value)"
        >
          <option value="">节点预设</option>
          <option v-for="p in appStore.presets" :key="p.id" :value="p.id">
            {{ p.name }}
          </option>
        </select>
        <div class="server-endpoint">
          <select
            v-model="appStore.serverForm.protocol"
            class="mono proto-select"
            :disabled="runningThis"
            aria-label="信令协议"
            @change="savePrefs"
          >
            <option v-for="proto in SIGNALING_PROTOCOLS" :key="proto" :value="proto">
              {{ proto }}://
            </option>
          </select>
          <input
            v-model="appStore.serverForm.host"
            type="text"
            class="mono host-input"
            :disabled="runningThis"
            placeholder="域名或 IP"
            @paste="onHostPaste"
            @change="savePrefs"
          />
          <span class="port-sep mono">:</span>
          <input
            v-model.number="appStore.serverForm.ws_port"
            type="number"
            min="1"
            max="65535"
            class="mono port-input"
            :disabled="runningThis"
            aria-label="信令端口"
            @change="savePrefs"
          />
        </div>
      </div>

      <div class="ctrl-group ctrl-room">
        <input
          v-model="appStore.p2pForm.room_code"
          type="text"
          class="mono room-input"
          maxlength="16"
          :disabled="runningThis"
          placeholder="房间号 (4~16位)"
          @change="savePrefs"
        />
        <button type="button" class="btn-sm" :disabled="runningThis" @click="onRandomRoom">
          随机
        </button>
        <button type="button" class="btn-sm" @click="onCopyRoom">
          {{ copiedRoom ? '已复制' : '复制' }}
        </button>
      </div>

      <div class="ctrl-group ctrl-peer">
        <input
          v-model="appStore.p2pForm.peer_label"
          type="text"
          class="label-input"
          :disabled="runningThis"
          placeholder="本机昵称"
          @change="savePrefs"
        />
        <div class="token-wrap">
          <input
            v-model="appStore.serverForm.token"
            :type="showToken ? 'text' : 'password'"
            class="token-input"
            :disabled="runningThis"
            placeholder="口令(可选)"
            @change="savePrefs"
          />
          <button
            type="button"
            class="btn-sm"
            @click="showToken = !showToken"
          >
            {{ showToken ? '隐藏' : '显示' }}
          </button>
        </div>
      </div>

      <button
        type="button"
        class="action-btn"
        :class="runningThis ? 'btn-danger' : 'btn-primary'"
        @click="onToggleAction"
      >
        {{ runningThis ? '停止' : '开始测试' }}
      </button>
    </div>

    <div v-if="wsValidationError || roomValidationError" class="banner-error">
      {{ wsValidationError || roomValidationError }}
    </div>

    <div class="stage-card">
      <div class="stage-steps">
        <div
          v-for="(st, idx) in P2P_STAGES"
          :key="st.key"
          class="step-item"
          :class="{
            active: idx === currentStageIdx,
            done: currentStageIdx > idx,
          }"
        >
          <span class="step-idx mono">{{ idx + 1 }}</span>
          <span class="step-lbl">{{ st.label }}</span>
        </div>
      </div>

      <div class="stage-status">
        <div class="status-left">
          <template v-if="runningInfo">
            <span
              :class="{
                'status-reconnecting': runningInfo.stage === 'reconnecting_signaling',
              }"
            >
              {{ runningInfo.status_message || formatStageLabel(runningInfo.stage) }}
            </span>
            <span
              v-if="runningInfo.stage === 'waiting_for_peer'"
              class="conn-ok-badge"
            >
              节点已连接
            </span>
            <span
              v-if="
                runningInfo.stage === 'waiting_for_peer' &&
                appStore.waitingRemainSecs !== null
              "
              class="mono wait-badge"
            >
              房间 <strong class="mono">{{ appStore.p2pForm.room_code }}</strong> · 剩余
              {{ appStore.waitingRemainSecs }}s
            </span>
          </template>
          <template v-else-if="latestVerdict && appStore.latestP2pReport?.p2p_report">
            <span class="verdict-pill" :class="`tone-${latestVerdict.tone}`">
              {{ latestVerdict.label }}
            </span>
            <button
              type="button"
              class="btn-sm"
              @click="appStore.modalReport = appStore.latestP2pReport"
            >
              查看完整报告
            </button>
          </template>
          <template v-else>
            <span class="muted">就绪</span>
          </template>
        </div>

        <div class="status-rtt mono">
          <span>最新：{{ formatMs(liveSummary.latest) }}</span>
          <span>均值：{{ formatMs(liveSummary.avg) }}</span>
          <span>丢包：{{ formatLossPct(liveSummary.loss) }}</span>
          <span v-if="runningInfo">进度 {{ runningInfo.progress_pct }}%</span>
        </div>
      </div>
    </div>

    <div class="chart-area">
      <LatencyChart :series="appStore.liveSeries" :compact="appStore.isMobile" />
    </div>
  </div>
</template>

<style scoped>
.p2p-workspace {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 12px 14px;
  height: 100%;
  overflow-y: auto;
}

.p2p-workspace.mobile {
  padding: 12px;
  gap: 12px;
}

.p2p-workspace.mobile .mobile-peer-bar {
  padding: 10px 12px;
  font-size: 13px;
}

.p2p-workspace.mobile .control-bar {
  padding: 12px;
  gap: 10px;
}

.p2p-workspace.mobile .ctrl-group {
  flex: 1 1 100%;
  gap: 8px;
}

.p2p-workspace.mobile .preset-select {
  max-width: none;
  width: 100%;
}

.p2p-workspace.mobile .port-input {
  width: 84px;
}

.p2p-workspace.mobile .label-input {
  width: 112px;
}

.p2p-workspace.mobile .action-btn {
  width: 100%;
  min-height: 46px;
  font-size: 16px;
}

.p2p-workspace.mobile .step-item {
  padding: 7px 4px;
  font-size: 12px;
  justify-content: center;
}

.p2p-workspace.mobile .stage-steps {
  grid-template-columns: repeat(3, 1fr);
}

.p2p-workspace.mobile .stage-status {
  font-size: 13px;
}

.mobile-peer-bar {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 6px 10px;
  font-size: 12px;
  color: var(--text-secondary);
}

.mobile-peer-bar.expanded {
  display: flex;
  flex-direction: column;
  gap: 4px;
  background: var(--accent-soft);
  border-color: #bae6fd;
  color: var(--text);
}

.mp-thin {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.mp-row {
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
}

.control-bar {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 8px 10px;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}

.ctrl-group {
  display: flex;
  align-items: center;
  gap: 6px;
}

.ctrl-ws {
  flex: 1.8 1 280px;
  flex-wrap: wrap;
  min-width: 0;
}

.preset-select {
  max-width: 112px;
}

.server-endpoint {
  display: flex;
  align-items: center;
  gap: 4px;
  flex: 1 1 200px;
  min-width: 0;
}

.proto-select {
  width: auto;
  flex-shrink: 0;
}

.host-input {
  flex: 1;
  min-width: 90px;
}

.port-sep {
  color: var(--text-muted);
  flex-shrink: 0;
}

.port-input {
  width: 72px;
  flex-shrink: 0;
}

.ctrl-room {
  flex: 1 1 200px;
}

.room-input {
  flex: 1;
  min-width: 100px;
}

.ctrl-peer {
  flex: 1 1 210px;
}

.label-input {
  width: 96px;
}

.token-wrap {
  display: flex;
  gap: 4px;
  flex: 1;
  min-width: 120px;
}

.token-input {
  flex: 1;
  min-width: 70px;
}

.action-btn {
  min-width: 92px;
}

.stage-card {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 8px 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.stage-steps {
  display: grid;
  grid-template-columns: repeat(6, 1fr);
  gap: 6px;
}

.step-item {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 5px 8px;
  border-radius: 6px;
  background: var(--bg-muted);
  border: 1px solid var(--border);
  font-size: 12px;
  color: var(--text-muted);
  overflow: hidden;
}

.step-item.active {
  background: var(--accent-soft);
  border-color: var(--accent);
  color: var(--accent-hover);
  font-weight: 600;
}

.step-item.done {
  background: var(--ok-soft);
  border-color: #bbf7d0;
  color: var(--ok);
}

.step-idx {
  width: 18px;
  height: 18px;
  border-radius: 50%;
  background: rgba(15, 23, 42, 0.08);
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  flex-shrink: 0;
}

.step-lbl {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.stage-status {
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
  font-size: 12px;
}

.status-left {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}

.status-rtt {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
  color: var(--text-secondary);
}

.wait-badge {
  background: var(--accent-soft);
  color: var(--accent-hover);
  padding: 2px 8px;
  border-radius: 4px;
}

.conn-ok-badge {
  background: var(--ok-soft);
  color: var(--ok);
  border: 1px solid #bbf7d0;
  padding: 2px 8px;
  border-radius: 4px;
  font-weight: 600;
}

.status-reconnecting {
  color: var(--danger);
  font-weight: 600;
}

.verdict-pill {
  padding: 2px 8px;
  border-radius: 4px;
  font-weight: 600;
}

.muted {
  color: var(--text-muted);
}

.chart-area {
  flex: 1;
  min-height: 240px;
  display: flex;
}

.p2p-workspace.mobile .chart-area {
  flex: 0 0 260px;
  min-height: 240px;
}
</style>
