<script setup lang="ts">
import { computed } from 'vue';
import type { LinkTargetReport } from '../types/protocol';
import {
  addLinkTarget,
  appStore,
  isRunning,
  removeLinkTarget,
  runLinkTest,
  savePrefs,
  stopCurrentTask,
} from '../stores/appState';
import {
  formatHostPort,
  formatLinkProtocol,
  formatLossPct,
  formatMs,
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
    void runLinkTest();
  }
}

function firstError(target: LinkTargetReport): string | null {
  const hit = target.samples.find((s) => Boolean(s.error));
  return hit?.error ?? null;
}
</script>

<template>
  <div class="link-page" :class="{ mobile: appStore.isMobile }">
    <div class="link-controls">
      <div class="targets-header">
        <span class="section-title">直连目标列表</span>
        <button
          type="button"
          class="btn-sm"
          :disabled="runningThis || appStore.linkForm.targets.length >= 64"
          @click="addLinkTarget"
        >
          + 添加目标
        </button>
      </div>

      <div class="targets-list">
        <div
          v-for="(item, idx) in appStore.linkForm.targets"
          :key="idx"
          class="target-row"
        >
          <input
            v-model="item.label"
            type="text"
            :disabled="runningThis"
            placeholder="名称"
            aria-label="目标名称"
            @change="savePrefs"
          />
          <input
            v-model="item.host_ip"
            type="text"
            class="mono"
            :disabled="runningThis"
            placeholder="IPv4 / IPv6"
            aria-label="目标 IP"
            @change="savePrefs"
          />
          <input
            v-model.number="item.port"
            type="number"
            min="1"
            max="65535"
            class="mono port-input"
            :disabled="runningThis"
            placeholder="端口"
            aria-label="目标端口"
            @change="savePrefs"
          />
          <select
            v-model="item.mode"
            :disabled="runningThis"
            aria-label="探测协议"
            @change="savePrefs"
          >
            <option value="tls_sni">TLS+SNI</option>
            <option value="tcp">TCP</option>
          </select>
          <input
            v-if="item.mode === 'tls_sni'"
            v-model="item.sni"
            type="text"
            class="mono"
            :disabled="runningThis"
            placeholder="SNI 域名"
            aria-label="SNI 域名"
            @change="savePrefs"
          />
          <div v-else class="sni-placeholder mono">--</div>
          <button
            type="button"
            class="btn-sm remove-btn"
            :disabled="runningThis || appStore.linkForm.targets.length <= 1"
            @click="removeLinkTarget(idx)"
          >
            删除
          </button>
        </div>
      </div>

      <div class="params-row">
        <label class="param-field">
          <span>每目标次数 (1~20)</span>
          <input
            v-model.number="appStore.linkForm.attempts_per_target"
            type="number"
            min="1"
            max="20"
            class="mono"
            :disabled="runningThis"
            @change="savePrefs"
          />
        </label>
        <label class="param-field">
          <span>超时 ms (500~15000)</span>
          <input
            v-model.number="appStore.linkForm.timeout_ms"
            type="number"
            min="500"
            max="15000"
            step="100"
            class="mono"
            :disabled="runningThis"
            @change="savePrefs"
          />
        </label>
        <label class="param-field">
          <span>并发数 (1~5)</span>
          <input
            v-model.number="appStore.linkForm.max_concurrency"
            type="number"
            min="1"
            max="5"
            class="mono"
            :disabled="runningThis"
            @change="savePrefs"
          />
        </label>
      </div>

      <div class="link-actions">
        <button
          type="button"
          :class="runningThis ? 'btn-danger' : 'btn-primary'"
          @click="toggleRun"
        >
          {{ runningThis ? '停止' : '开始探测' }}
        </button>
        <span v-if="runningInfo" class="mono progress-txt">
          {{ formatStageLabel(runningInfo.stage) }} ({{ runningInfo.progress_pct }}%)
        </span>
      </div>
    </div>

    <div v-if="appStore.latestLinkReports" class="link-results">
      <div
        v-for="(item, idx) in appStore.latestLinkReports"
        :key="idx"
        class="result-card"
      >
        <div class="result-top">
          <div class="result-title-group">
            <span class="result-label">{{ item.label }}</span>
            <span class="result-endpoint mono">{{ formatHostPort(item.host_ip, item.port) }}</span>
            <span class="tag mono">{{ formatLinkProtocol(item.protocol) }}</span>
          </div>
          <span
            class="status-pill"
            :class="item.reachable ? 'tone-excellent' : 'tone-failed'"
          >
            {{ item.reachable ? '可达' : '不可达' }}
          </span>
        </div>

        <div class="result-metrics">
          <div class="metric-box">
            <span class="k">丢包 / 成功</span>
            <span class="v mono">
              {{ formatLossPct(item.loss_ratio) }} ({{ item.received }}/{{ item.sent }})
            </span>
          </div>
          <div class="metric-box">
            <span class="k">平均 RTT</span>
            <span class="v mono">{{ formatMs(item.avg_rtt_ms) }}</span>
          </div>
          <div class="metric-box">
            <span class="k">最小 RTT</span>
            <span class="v mono">{{ formatMs(item.min_rtt_ms) }}</span>
          </div>
          <div class="metric-box">
            <span class="k">最大 RTT</span>
            <span class="v mono">{{ formatMs(item.max_rtt_ms) }}</span>
          </div>
        </div>

        <div v-if="firstError(item)" class="result-error mono">
          {{ firstError(item) }}
        </div>
      </div>
    </div>

    <div v-else class="link-empty">
      {{
        runningThis && appStore.activeKind === 'link'
          ? '正在执行直连链路探测...'
          : '暂无直连链路探测结果'
      }}
    </div>
  </div>
</template>

<style scoped>
.link-page {
  flex: 1;
  padding: 14px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  overflow-y: auto;
}

.link-controls {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.targets-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
}

.section-title {
  font-weight: 600;
  font-size: 13px;
}

.targets-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.target-row {
  display: grid;
  grid-template-columns: minmax(110px, 1.1fr) minmax(140px, 1.3fr) 84px 104px minmax(130px, 1.2fr) auto;
  gap: 8px;
  align-items: center;
}

.sni-placeholder {
  color: var(--text-muted);
  text-align: center;
  padding: 6px 9px;
  border: 1px dashed var(--border);
  border-radius: 6px;
  background: var(--bg-muted);
}

.params-row {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
  gap: 8px;
}

.param-field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
  color: var(--text-secondary);
}

.link-actions {
  display: flex;
  align-items: center;
  gap: 12px;
}

.progress-txt {
  font-size: 12px;
  color: var(--text-secondary);
}

.link-results {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.result-card {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.result-top {
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
}

.result-title-group {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}

.result-label {
  font-weight: 700;
  font-size: 14px;
}

.result-endpoint {
  color: var(--text-secondary);
  font-size: 12px;
}

.tag {
  font-size: 11px;
  padding: 2px 7px;
  border-radius: 4px;
  background: var(--bg-subtle);
  color: var(--text-secondary);
}

.status-pill {
  font-size: 12px;
  font-weight: 600;
  padding: 2px 8px;
  border-radius: 4px;
}

.result-metrics {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
  gap: 8px;
}

.metric-box {
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

.result-error {
  font-size: 12px;
  color: var(--danger);
  background: var(--danger-soft);
  border-radius: 5px;
  padding: 6px 8px;
  word-break: break-word;
}

.link-empty {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 32px;
  text-align: center;
  color: var(--text-muted);
}

.link-page.mobile {
  padding: 12px;
  gap: 12px;
}

.link-page.mobile .link-controls {
  padding: 14px;
  gap: 12px;
}

.link-page.mobile .target-row {
  grid-template-columns: 1fr;
  gap: 8px;
  padding: 10px;
  background: var(--bg-muted);
  border: 1px solid var(--border);
  border-radius: 6px;
}

.link-page.mobile .sni-placeholder {
  display: none;
}

.link-page.mobile .link-actions {
  flex-direction: column;
  align-items: stretch;
  gap: 8px;
}

.link-page.mobile .link-actions button {
  width: 100%;
  min-height: 46px;
  font-size: 16px;
}

.link-page.mobile .k {
  font-size: 13px;
}

.link-page.mobile .v {
  font-size: 15px;
}
</style>
