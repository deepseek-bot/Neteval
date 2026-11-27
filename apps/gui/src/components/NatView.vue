<script setup lang="ts">
import { computed } from 'vue';
import {
  appStore,
  applyPreset,
  applyServerPaste,
  closeNatVpsModal,
  continueNatWithoutVps,
  isRunning,
  retryNatWithVps,
  runNatTest,
  savePrefs,
  stopCurrentTask,
} from '../stores/appState';
import {
  formatNatFiltering,
  formatNatMapping,
  formatNatNoteEntries,
  formatNatStatus,
  formatStageLabel,
} from '../utils/format';

const runningInfo = computed(() =>
  typeof appStore.assessmentState === 'object' && 'running' in appStore.assessmentState
    ? appStore.assessmentState.running
    : null,
);

const runningThis = computed(() => isRunning());

const formattedNotes = computed(() =>
  appStore.latestNatReport ? formatNatNoteEntries(appStore.latestNatReport.notes) : [],
);

function toggleRun(): void {
  if (runningThis.value) {
    void stopCurrentTask('user_cancelled');
  } else {
    void runNatTest();
  }
}

function setMode(useVps: boolean): void {
  appStore.natForm.useSelfHostedVps = useVps;
  savePrefs();
}

function onHostPaste(e: ClipboardEvent): void {
  const text = e.clipboardData?.getData('text') ?? '';
  if (text && applyServerPaste(text)) {
    e.preventDefault();
  }
}
</script>

<template>
  <div class="nat-page" :class="{ mobile: appStore.isMobile }">
    <div class="nat-controls">
      <div class="mode-switch">
        <button
          type="button"
          :class="{ 'btn-primary': !appStore.natForm.useSelfHostedVps }"
          :disabled="runningThis"
          @click="setMode(false)"
        >
          公共 STUN 初步判定
        </button>
        <button
          type="button"
          :class="{ 'btn-primary': appStore.natForm.useSelfHostedVps }"
          :disabled="runningThis"
          @click="setMode(true)"
        >
          自建服务器精细判定
        </button>
      </div>

      <div v-if="appStore.natForm.useSelfHostedVps" class="vps-inputs">
        <select
          v-if="appStore.presets.length > 0"
          :value="appStore.selectedPresetId"
          :disabled="runningThis"
          @change="applyPreset(($event.target as HTMLSelectElement).value)"
        >
          <option value="">节点预设</option>
          <option v-for="p in appStore.presets" :key="p.id" :value="p.id">
            {{ p.name }}
          </option>
        </select>
        <input
          v-model="appStore.serverForm.host"
          type="text"
          class="mono"
          :disabled="runningThis"
          placeholder="服务器域名或 IP"
          @paste="onHostPaste"
          @change="savePrefs"
        />
        <input
          v-model="appStore.serverForm.token"
          type="password"
          :disabled="runningThis"
          placeholder="口令(可选)"
          @change="savePrefs"
        />
      </div>

      <div class="nat-actions">
        <button
          type="button"
          :class="runningThis ? 'btn-danger' : 'btn-primary'"
          @click="toggleRun"
        >
          {{ runningThis ? '停止' : '开始体检' }}
        </button>
        <span v-if="runningInfo" class="mono progress-txt">
          {{ formatStageLabel(runningInfo.stage) }} ({{ runningInfo.progress_pct }}%)
        </span>
      </div>
    </div>

    <div v-if="appStore.latestNatReport" class="nat-result">
      <div class="nat-grid">
        <div class="kv-card">
          <span class="k">探测模式</span>
          <span class="v">{{ formatNatStatus(appStore.latestNatReport.status) }}</span>
        </div>
        <div class="kv-card">
          <span class="k">穿透难度</span>
          <span class="v">{{ appStore.latestNatReport.estimated_difficulty || '--' }}</span>
        </div>
        <div class="kv-card">
          <span class="k">映射行为</span>
          <span class="v">{{ formatNatMapping(appStore.latestNatReport.mapping) }}</span>
        </div>
        <div class="kv-card">
          <span class="k">过滤行为</span>
          <span class="v">{{ formatNatFiltering(appStore.latestNatReport.filtering) }}</span>
        </div>
        <div class="kv-card">
          <span class="k">公网 IPv4</span>
          <span class="v mono">{{ appStore.latestNatReport.public_ipv4 || '--' }}</span>
        </div>
        <div class="kv-card">
          <span class="k">IPv6 状态</span>
          <span class="v mono">
            {{
              appStore.latestNatReport.ipv6_reachable
                ? appStore.latestNatReport.public_ipv6 || '可达'
                : '不可达'
            }}
          </span>
        </div>
      </div>

      <ul v-if="formattedNotes.length > 0" class="notes-list">
        <li v-for="(item, idx) in formattedNotes" :key="idx">
          <template v-if="item.kind === 'mappings'">
            <div class="mapping-title">{{ item.title }}</div>
            <div class="mapping-rows">
              <div
                v-for="(entry, eIdx) in item.entries"
                :key="eIdx"
                class="mapping-row mono"
              >
                <span class="mapping-src">{{ entry.source }}</span>
                <span v-if="entry.mapped" class="mapping-val">{{ entry.mapped }}</span>
              </div>
            </div>
          </template>
          <span v-else class="note-text">{{ item.text }}</span>
        </li>
      </ul>
    </div>

    <div v-else class="nat-empty">
      {{
        runningThis && appStore.activeKind === 'nat'
          ? '正在执行本机 NAT 体检...'
          : '暂无本机 NAT 体检结果'
      }}
    </div>

    <div
      v-if="appStore.natVpsUnreachableModal"
      class="vps-modal-backdrop"
      @click.self="closeNatVpsModal"
    >
      <div class="vps-modal-card" role="dialog" aria-modal="true">
        <div class="vps-modal-title">自建服务器不可达</div>
        <div class="vps-modal-body">
          <div class="vps-modal-host mono">{{ appStore.natVpsUnreachableModal.host }}</div>
          <div class="vps-modal-reason">{{ appStore.natVpsUnreachableModal.reason }}</div>
        </div>
        <div class="vps-modal-actions">
          <button type="button" class="btn-primary" @click="retryNatWithVps">
            重试自建服务器
          </button>
          <button type="button" @click="continueNatWithoutVps">
            无 VPS 继续测试
          </button>
          <button type="button" @click="closeNatVpsModal">
            取消
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.nat-page {
  flex: 1;
  padding: 14px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  overflow-y: auto;
}

.nat-controls {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.mode-switch {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}

.vps-inputs {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: 8px;
}

.nat-actions {
  display: flex;
  align-items: center;
  gap: 12px;
}

.progress-txt {
  font-size: 12px;
  color: var(--text-secondary);
}

.nat-result {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.nat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(210px, 1fr));
  gap: 8px;
}

.kv-card {
  background: var(--bg-muted);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 3px;
}

.k {
  font-size: 11px;
  color: var(--text-muted);
}

.v {
  font-size: 13px;
  font-weight: 600;
}

.notes-list {
  margin: 0;
  padding-left: 18px;
  color: var(--text-secondary);
  font-size: 12px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.note-text {
  word-break: break-word;
}

.mapping-title {
  margin-bottom: 4px;
  color: var(--text);
  font-weight: 600;
}

.mapping-rows {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.mapping-row {
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  gap: 6px 12px;
  padding: 5px 8px;
  background: var(--bg-muted);
  border: 1px solid var(--border);
  border-radius: 5px;
  font-size: 12px;
}

.mapping-src {
  color: var(--text-secondary);
  word-break: break-all;
}

.mapping-val {
  color: var(--text);
  font-weight: 600;
  word-break: break-all;
}

.nat-empty {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 32px;
  text-align: center;
  color: var(--text-muted);
}

.nat-page.mobile {
  padding: 12px;
  gap: 12px;
}

.nat-page.mobile .nat-controls {
  padding: 14px;
  gap: 12px;
}

.nat-page.mobile .mode-switch {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px;
}

.nat-page.mobile .vps-inputs {
  grid-template-columns: 1fr;
  gap: 10px;
}

.nat-page.mobile .nat-actions {
  flex-direction: column;
  align-items: stretch;
  gap: 8px;
}

.nat-page.mobile .nat-actions button {
  width: 100%;
  min-height: 46px;
  font-size: 16px;
}

.nat-page.mobile .kv-card {
  padding: 10px 12px;
}

.nat-page.mobile .k {
  font-size: 13px;
}

.nat-page.mobile .v {
  font-size: 15px;
}

.nat-page.mobile .notes-list,
.nat-page.mobile .mapping-row {
  font-size: 13px;
}

.vps-modal-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(15, 23, 42, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 16px;
  z-index: 60;
}

.vps-modal-card {
  width: min(440px, 100%);
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 16px;
  box-shadow: 0 18px 40px rgba(15, 23, 42, 0.22);
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.vps-modal-title {
  font-size: 16px;
  font-weight: 700;
  color: var(--danger);
}

.vps-modal-body {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.vps-modal-host {
  font-size: 13px;
  font-weight: 600;
  color: var(--text);
  word-break: break-all;
}

.vps-modal-reason {
  font-size: 13px;
  color: var(--text-secondary);
  line-height: 1.5;
  word-break: break-word;
}

.vps-modal-actions {
  display: flex;
  flex-wrap: wrap;
  justify-content: flex-end;
  gap: 8px;
}
</style>
