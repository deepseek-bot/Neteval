<script setup lang="ts">
import { onBeforeUnmount, onMounted } from 'vue';
import AdvancedSettingsPanel from './components/AdvancedSettingsPanel.vue';
import HistoryView from './components/HistoryView.vue';
import NatView from './components/NatView.vue';
import P2pView from './components/P2pView.vue';
import PeerSidebar from './components/PeerSidebar.vue';
import ResultModal from './components/ResultModal.vue';
import { appStore, disposeStore, initStore } from './stores/appState';

onMounted(() => {
  void initStore();
});

onBeforeUnmount(() => {
  disposeStore();
});
</script>

<template>
  <div class="app-shell" :class="{ mobile: appStore.isMobile }">
    <header class="top-bar">
      <div class="top-left">
        <span class="app-name">NetEval</span>
        <nav v-if="!appStore.isMobile" class="desktop-tabs">
          <button
            type="button"
            class="tab-btn"
            :class="{ active: appStore.activeTab === 'nat' }"
            @click="appStore.activeTab = 'nat'"
          >
            NAT 体检
          </button>
          <button
            type="button"
            class="tab-btn"
            :class="{ active: appStore.activeTab === 'p2p' }"
            @click="appStore.activeTab = 'p2p'"
          >
            P2P 打洞
          </button>
          <button
            type="button"
            class="tab-btn"
            :class="{ active: appStore.activeTab === 'history' }"
            @click="appStore.activeTab = 'history'"
          >
            历史记录
          </button>
        </nav>
      </div>

      <div class="top-right">
        <button
          v-if="!appStore.isMobile && appStore.activeTab === 'p2p'"
          type="button"
          class="btn-sm"
          :class="{ 'btn-active': appStore.leftSidebarOpen }"
          @click="appStore.leftSidebarOpen = !appStore.leftSidebarOpen"
        >
          对端面板
        </button>
        <button
          type="button"
          class="btn-sm"
          :class="{ 'btn-active': appStore.rightSidebarOpen }"
          @click="appStore.rightSidebarOpen = !appStore.rightSidebarOpen"
        >
          ⚙ 高级设置
        </button>
      </div>
    </header>

    <div v-if="appStore.errorNotice" class="notice-strip">
      <div class="banner-error">{{ appStore.errorNotice }}</div>
    </div>
    <div v-else-if="appStore.statusNotice" class="notice-strip">
      <div class="banner-info">{{ appStore.statusNotice }}</div>
    </div>

    <main class="main-body">
      <PeerSidebar
        v-if="!appStore.isMobile && appStore.activeTab === 'p2p' && appStore.leftSidebarOpen"
      />

      <NatView v-if="appStore.activeTab === 'nat'" />
      <P2pView v-else-if="appStore.activeTab === 'p2p'" />
      <HistoryView v-else />

      <AdvancedSettingsPanel
        v-if="!appStore.isMobile && appStore.rightSidebarOpen"
        @close="appStore.rightSidebarOpen = false"
      />
    </main>

    <nav v-if="appStore.isMobile" class="bottom-nav">
      <button
        type="button"
        class="bnav-btn"
        :class="{ active: appStore.activeTab === 'nat' }"
        @click="appStore.activeTab = 'nat'"
      >
        NAT
      </button>
      <button
        type="button"
        class="bnav-btn"
        :class="{ active: appStore.activeTab === 'p2p' }"
        @click="appStore.activeTab = 'p2p'"
      >
        P2P
      </button>
      <button
        type="button"
        class="bnav-btn"
        :class="{ active: appStore.activeTab === 'history' }"
        @click="appStore.activeTab = 'history'"
      >
        历史
      </button>
    </nav>

    <div
      v-if="appStore.isMobile && appStore.rightSidebarOpen"
      class="drawer-backdrop"
      @click.self="appStore.rightSidebarOpen = false"
    >
      <AdvancedSettingsPanel
        as-drawer
        @close="appStore.rightSidebarOpen = false"
      />
    </div>

    <ResultModal
      :report="appStore.modalReport"
      :is-mobile="appStore.isMobile"
      @close="appStore.modalReport = null"
    />
  </div>
</template>

<style scoped>
.app-shell {
  display: flex;
  flex-direction: column;
  height: 100%;
  width: 100%;
  overflow: hidden;
}

.top-bar {
  height: 46px;
  flex-shrink: 0;
  background: var(--bg-surface);
  border-bottom: 1px solid var(--border);
  padding: 0 12px;
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
}

.app-shell.mobile .top-bar {
  padding: env(safe-area-inset-top, 0px) 14px 0;
  height: calc(56px + env(safe-area-inset-top, 0px));
}

.app-shell.mobile .app-name {
  font-size: 17px;
}

.top-left,
.top-right {
  display: flex;
  align-items: center;
  gap: 8px;
}

.app-name {
  font-weight: 700;
  font-size: 14px;
  margin-right: 6px;
}

.desktop-tabs {
  display: flex;
  gap: 4px;
}

.tab-btn {
  border: 1px solid transparent;
  background: transparent;
  color: var(--text-secondary);
  padding: 5px 12px;
  font-weight: 500;
}

.tab-btn.active {
  background: var(--accent-soft);
  border-color: #bae6fd;
  color: var(--accent-hover);
  font-weight: 600;
}

.btn-active {
  background: var(--accent-soft);
  border-color: var(--accent);
  color: var(--accent-hover);
}

.notice-strip {
  padding: 6px 12px 0;
  flex-shrink: 0;
}

.main-body {
  flex: 1;
  min-height: 0;
  display: flex;
  overflow: hidden;
}

.bottom-nav {
  flex-shrink: 0;
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  background: var(--bg-surface);
  border-top: 1px solid var(--border);
  padding-bottom: env(safe-area-inset-bottom, 0px);
}

.bnav-btn {
  border: none;
  border-radius: 0;
  background: transparent;
  min-height: 56px;
  padding: 14px 8px;
  font-size: 16px;
  font-weight: 600;
  color: var(--text-secondary);
}

.bnav-btn.active {
  color: var(--accent);
  box-shadow: inset 0 3px 0 var(--accent);
}

.drawer-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(15, 23, 42, 0.42);
  display: flex;
  justify-content: flex-end;
  z-index: 50;
}
</style>
