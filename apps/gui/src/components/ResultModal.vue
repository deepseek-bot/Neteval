<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted } from 'vue';
import type { AssessmentReport } from '../types/protocol';
import {
  formatLossPct,
  formatMs,
  formatNatFiltering,
  formatNatMapping,
  formatNatStatus,
  formatTime,
  formatVerdict,
} from '../utils/format';
import LatencyChart from './LatencyChart.vue';

const props = defineProps<{
  report: AssessmentReport | null;
  isMobile: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const verdictInfo = computed(() =>
  props.report?.p2p_report ? formatVerdict(props.report.p2p_report.verdict) : null,
);

function onKeyDown(e: KeyboardEvent): void {
  if (e.key === 'Escape' && props.report) {
    emit('close');
  }
}

onMounted(() => window.addEventListener('keydown', onKeyDown));
onBeforeUnmount(() => window.removeEventListener('keydown', onKeyDown));
</script>

<template>
  <div
    v-if="report"
    class="modal-backdrop"
    :class="{ mobile: isMobile }"
    @click.self="emit('close')"
  >
    <div class="modal-panel" :class="{ mobile: isMobile }">
      <div class="modal-header">
        <div>
          <span class="modal-title mono">{{ report.report_id }}</span>
          <span class="modal-time mono">{{ formatTime(report.created_unix_ms) }}</span>
        </div>
        <button class="btn-sm" @click="emit('close')">关闭</button>
      </div>

      <div v-if="report.storage_warning" class="banner-warn">
        报告本地落盘警告：{{ report.storage_warning }}
      </div>

      <template v-if="report.p2p_report">
        <div
          v-if="verdictInfo"
          class="verdict-card"
          :class="`tone-${verdictInfo.tone}`"
        >
          <div class="verdict-main">
            <strong>{{ verdictInfo.label }}</strong>
            <span class="mono">对端：{{ report.p2p_report.peer_label || '--' }}</span>
          </div>
          <div
            v-if="report.p2p_report.failure_reason || report.p2p_report.interrupted_reason"
            class="verdict-reason mono"
          >
            {{
              report.p2p_report.failure_reason || report.p2p_report.interrupted_reason
            }}
          </div>
        </div>

        <div class="metric-grid">
          <div class="metric-item">
            <span class="metric-label">平均 RTT</span>
            <span class="metric-value mono">{{ formatMs(report.p2p_report.avg_rtt_ms) }}</span>
          </div>
          <div class="metric-item">
            <span class="metric-label">P95 RTT</span>
            <span class="metric-value mono">{{ formatMs(report.p2p_report.p95_rtt_ms) }}</span>
          </div>
          <div class="metric-item">
            <span class="metric-label">抖动</span>
            <span class="metric-value mono">{{ formatMs(report.p2p_report.jitter_ms) }}</span>
          </div>
          <div class="metric-item">
            <span class="metric-label">丢包率 / 收发</span>
            <span class="metric-value mono">
              {{ formatLossPct(report.p2p_report.loss_ratio) }} ({{
                report.p2p_report.received
              }}/{{ report.p2p_report.sent }})
            </span>
          </div>
        </div>

        <div class="detail-row mono">
          <span>
            协议栈：{{
              report.p2p_report.ip_family === 'ipv6'
                ? 'IPv6'
                : report.p2p_report.ip_family === 'ipv4'
                  ? 'IPv4'
                  : '--'
            }}
          </span>
          <span>对端地址：{{ report.p2p_report.active_peer_addr || '--' }}</span>
          <span>打洞耗时：{{ formatMs(report.p2p_report.punch_elapsed_ms, 0) }}</span>
        </div>

        <div class="modal-chart">
          <LatencyChart :series="report.p2p_report.series" :compact="isMobile" />
        </div>
      </template>

      <template v-if="report.nat_report">
        <div class="nat-grid">
          <div class="metric-item">
            <span class="metric-label">探测模式</span>
            <span class="metric-value">{{ formatNatStatus(report.nat_report.status) }}</span>
          </div>
          <div class="metric-item">
            <span class="metric-label">穿透难度</span>
            <span class="metric-value">{{ report.nat_report.estimated_difficulty || '--' }}</span>
          </div>
          <div class="metric-item">
            <span class="metric-label">映射行为 (Mapping)</span>
            <span class="metric-value">{{ formatNatMapping(report.nat_report.mapping) }}</span>
          </div>
          <div class="metric-item">
            <span class="metric-label">过滤行为 (Filtering)</span>
            <span class="metric-value">{{ formatNatFiltering(report.nat_report.filtering) }}</span>
          </div>
          <div class="metric-item">
            <span class="metric-label">公网 IPv4</span>
            <span class="metric-value mono">{{ report.nat_report.public_ipv4 || '--' }}</span>
          </div>
          <div class="metric-item">
            <span class="metric-label">公网 IPv6</span>
            <span class="metric-value mono">
              {{
                report.nat_report.ipv6_reachable
                  ? report.nat_report.public_ipv6 || '可达'
                  : '不可达'
              }}
            </span>
          </div>
        </div>

        <ul v-if="report.nat_report.notes.length > 0" class="nat-notes">
          <li v-for="(note, idx) in report.nat_report.notes" :key="idx">{{ note }}</li>
        </ul>
      </template>
    </div>
  </div>
</template>

<style scoped>
.modal-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(15, 23, 42, 0.48);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 60;
  padding: 20px;
}

.modal-backdrop.mobile {
  align-items: flex-end;
  padding: 0;
}

.modal-panel {
  width: 85vw;
  max-width: 1040px;
  height: 85vh;
  max-height: 760px;
  background: var(--bg-surface);
  border-radius: 10px;
  border: 1px solid var(--border);
  padding: 16px 18px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  overflow-y: auto;
}

.modal-panel.mobile {
  width: 100%;
  height: 90vh;
  max-height: 90vh;
  border-radius: 14px 14px 0 0;
  padding: 14px;
}

.modal-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}

.modal-title {
  font-weight: 700;
  font-size: 14px;
  margin-right: 10px;
}

.modal-time {
  color: var(--text-muted);
  font-size: 12px;
}

.verdict-card {
  border-radius: 8px;
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.verdict-main {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
  font-size: 14px;
}

.verdict-reason {
  font-size: 12px;
}

.metric-grid,
.nat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(175px, 1fr));
  gap: 8px;
}

.metric-item {
  border: 1px solid var(--border);
  background: var(--bg-muted);
  border-radius: 6px;
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 3px;
}

.metric-label {
  font-size: 11px;
  color: var(--text-muted);
}

.metric-value {
  font-size: 14px;
  font-weight: 600;
}

.detail-row {
  display: flex;
  flex-wrap: wrap;
  gap: 14px;
  font-size: 12px;
  color: var(--text-secondary);
  padding: 6px 10px;
  background: var(--bg-muted);
  border: 1px solid var(--border);
  border-radius: 6px;
}

.modal-chart {
  flex: 1;
  min-height: 240px;
  display: flex;
}

.nat-notes {
  margin: 0;
  padding-left: 18px;
  color: var(--text-secondary);
  font-size: 12px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
</style>
