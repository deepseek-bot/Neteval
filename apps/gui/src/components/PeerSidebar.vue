<script setup lang="ts">
import { computed } from 'vue';
import { appStore, openReportModal } from '../stores/appState';
import { formatMs, formatTime, formatVerdict } from '../utils/format';

const recentP2pHistory = computed(() =>
  appStore.history.filter((h) => h.has_p2p).slice(0, 8),
);
</script>

<template>
  <aside class="peer-sidebar">
    <section class="side-section">
      <div class="side-title">当前对端信息</div>
      <div class="kv-list">
        <div class="kv-row">
          <span class="k">房间号</span>
          <span class="v mono">{{ appStore.livePeer?.room_code || '--' }}</span>
        </div>
        <div class="kv-row">
          <span class="k">对端昵称</span>
          <span class="v mono">{{ appStore.livePeer?.peer_label || '--' }}</span>
        </div>
        <div class="kv-row">
          <span class="k">协议栈</span>
          <span class="v mono">
            {{
              appStore.livePeer?.ip_family === 'ipv6'
                ? 'IPv6'
                : appStore.livePeer?.ip_family === 'ipv4'
                  ? 'IPv4'
                  : '--'
            }}
          </span>
        </div>
        <div class="kv-row">
          <span class="k">活跃地址</span>
          <span class="v mono addr">{{ appStore.livePeer?.active_peer_addr || '--' }}</span>
        </div>
        <div class="kv-row">
          <span class="k">打洞耗时</span>
          <span class="v mono">{{ formatMs(appStore.livePeer?.punch_elapsed_ms, 0) }}</span>
        </div>
      </div>
    </section>

    <section class="side-section history-section">
      <div class="side-title">近期 P2P 记录</div>
      <div v-if="recentP2pHistory.length === 0" class="empty-hint">
        暂无 P2P 历史记录
      </div>
      <div v-else class="recent-list">
        <button
          v-for="item in recentP2pHistory"
          :key="item.report_id"
          class="recent-item"
          @click="openReportModal(item.report_id)"
        >
          <div class="recent-top">
            <span class="mono">{{ formatTime(item.created_unix_ms).slice(5) }}</span>
            <span
              v-if="item.p2p_verdict"
              class="verdict-tag"
              :class="`tone-${formatVerdict(item.p2p_verdict).tone}`"
            >
              {{ item.p2p_verdict }}
            </span>
          </div>
          <div class="recent-id mono">{{ item.report_id }}</div>
        </button>
      </div>
    </section>
  </aside>
</template>

<style scoped>
.peer-sidebar {
  width: 240px;
  flex-shrink: 0;
  border-right: 1px solid var(--border);
  background: var(--bg-surface);
  display: flex;
  flex-direction: column;
  overflow-y: auto;
}

.side-section {
  padding: 12px;
  border-bottom: 1px solid var(--border);
}

.history-section {
  border-bottom: none;
  flex: 1;
}

.side-title {
  font-weight: 600;
  font-size: 12px;
  color: var(--text-secondary);
  margin-bottom: 8px;
}

.kv-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.kv-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
  font-size: 12px;
}

.k {
  color: var(--text-muted);
  flex-shrink: 0;
}

.v {
  color: var(--text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.empty-hint {
  font-size: 12px;
  color: var(--text-muted);
}

.recent-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.recent-item {
  width: 100%;
  text-align: left;
  padding: 7px 8px;
  background: var(--bg-muted);
  border: 1px solid var(--border);
  border-radius: 6px;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.recent-top {
  display: flex;
  justify-content: space-between;
  align-items: center;
  font-size: 11px;
}

.recent-id {
  font-size: 11px;
  color: var(--text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
}

.verdict-tag {
  padding: 1px 5px;
  border-radius: 4px;
  font-size: 10px;
  font-weight: 600;
}
</style>
