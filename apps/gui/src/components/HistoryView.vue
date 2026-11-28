<script setup lang="ts">
import { onMounted } from 'vue';
import {
  appStore,
  openReportModal,
  refreshHistory,
  removeHistoryItem,
} from '../stores/appState';
import { formatNatStatus, formatTime, formatVerdict } from '../utils/format';

onMounted(() => {
  void refreshHistory();
});
</script>

<template>
  <div class="history-page" :class="{ mobile: appStore.isMobile }">
    <div class="history-toolbar">
      <span class="count mono">共 {{ appStore.history.length }} 条记录</span>
      <button type="button" class="btn-sm" @click="refreshHistory">刷新</button>
    </div>

    <div v-if="appStore.history.length === 0" class="history-empty">
      暂无历史评估报告
    </div>

    <div v-else class="history-list">
      <div
        v-for="item in appStore.history"
        :key="item.report_id"
        class="history-row"
      >
        <div class="row-info">
          <div class="row-top">
            <span class="row-id mono">{{ item.report_id }}</span>
            <span class="row-time mono">{{ formatTime(item.created_unix_ms) }}</span>
          </div>
          <div class="row-tags">
            <span v-if="item.has_nat" class="tag">
              NAT{{ item.nat_status ? ` · ${formatNatStatus(item.nat_status)}` : '' }}
            </span>
            <span
              v-if="item.has_p2p"
              class="tag"
              :class="item.p2p_verdict ? `tone-${formatVerdict(item.p2p_verdict).tone}` : ''"
            >
              P2P{{ item.p2p_verdict ? ` · ${formatVerdict(item.p2p_verdict).label}` : '' }}
            </span>
            <span
              v-if="item.has_link"
              class="tag"
              :class="
                item.link_target_count
                  ? item.link_reachable_count
                    ? 'tone-excellent'
                    : 'tone-failed'
                  : ''
              "
            >
              链路{{
                item.link_target_count !== undefined && item.link_target_count !== null
                  ? ` · ${item.link_reachable_count ?? 0}/${item.link_target_count} 可达`
                  : ''
              }}
            </span>
          </div>
        </div>

        <div class="row-actions">
          <button type="button" class="btn-sm btn-primary" @click="openReportModal(item.report_id)">
            查看详情
          </button>
          <button type="button" class="btn-sm" @click="removeHistoryItem(item.report_id)">
            删除
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.history-page {
  flex: 1;
  padding: 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  overflow-y: auto;
}

.history-toolbar {
  display: flex;
  justify-content: space-between;
  align-items: center;
}

.count {
  font-size: 12px;
  color: var(--text-secondary);
}

.history-empty {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 32px;
  text-align: center;
  color: var(--text-muted);
}

.history-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.history-row {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 10px 12px;
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  gap: 10px;
}

.row-info {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}

.row-top {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
}

.row-id {
  font-weight: 600;
  font-size: 13px;
}

.row-time {
  font-size: 12px;
  color: var(--text-muted);
}

.row-tags {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.tag {
  font-size: 11px;
  padding: 2px 7px;
  border-radius: 4px;
  background: var(--bg-subtle);
  color: var(--text-secondary);
}

.row-actions {
  display: flex;
  gap: 6px;
}

.history-page.mobile {
  padding: 12px;
  gap: 12px;
}

.history-page.mobile .count {
  font-size: 13px;
}

.history-page.mobile .history-row {
  padding: 12px 14px;
  gap: 12px;
}

.history-page.mobile .row-id {
  font-size: 14px;
}

.history-page.mobile .row-time {
  font-size: 13px;
}

.history-page.mobile .tag {
  font-size: 12px;
  padding: 4px 8px;
}

.history-page.mobile .row-actions {
  width: 100%;
  gap: 8px;
}

.history-page.mobile .row-actions button {
  flex: 1;
}
</style>
