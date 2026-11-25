<script setup lang="ts">
import { computed } from 'vue';
import {
  appStore,
  applyPreset,
  isRunning,
  runNatTest,
  savePrefs,
  stopCurrentTask,
} from '../stores/appState';
import {
  formatNatFiltering,
  formatNatMapping,
  formatNatStatus,
  formatStageLabel,
} from '../utils/format';

const runningInfo = computed(() =>
  typeof appStore.assessmentState === 'object' && 'running' in appStore.assessmentState
    ? appStore.assessmentState.running
    : null,
);

const runningThis = computed(() => isRunning());

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
</script>

<template>
  <div class="nat-page">
    <div class="nat-controls">
      <div class="mode-switch">
        <button
          type="button"
          :class="{ 'btn-primary': !appStore.natForm.useSelfHostedVps }"
          :disabled="runningThis"
          @click="setMode(false)"
        >
          零 VPS 公共 STUN 模式
        </button>
        <button
          type="button"
          :class="{ 'btn-primary': appStore.natForm.useSelfHostedVps }"
          :disabled="runningThis"
          @click="setMode(true)"
        >
          自建 S2 VPS 增强模式
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
          v-model="appStore.natForm.primary_v4_p1"
          type="text"
          class="mono"
          :disabled="runningThis"
          placeholder="主端口 p1 (host:34780)"
          @change="savePrefs"
        />
        <input
          v-model="appStore.natForm.primary_v4_p2"
          type="text"
          class="mono"
          :disabled="runningThis"
          placeholder="主端口 p2 (host:34781)"
          @change="savePrefs"
        />
        <input
          v-model="appStore.natForm.token"
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

      <ul v-if="appStore.latestNatReport.notes.length > 0" class="notes-list">
        <li v-for="(n, idx) in appStore.latestNatReport.notes" :key="idx">{{ n }}</li>
      </ul>
    </div>

    <div v-else class="nat-empty">
      {{
        runningThis && appStore.activeKind === 'nat'
          ? '正在执行本机 NAT 体检...'
          : '暂无本机 NAT 体检结果'
      }}
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
  gap: 4px;
}

.nat-empty {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 32px;
  text-align: center;
  color: var(--text-muted);
}
</style>
