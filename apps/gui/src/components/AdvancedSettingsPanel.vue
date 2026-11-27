<script setup lang="ts">
import { computed, ref } from 'vue';
import {
  appStore,
  applyPreset,
  applyServerPaste,
  deletePreset,
  saveCurrentAsPreset,
  savePrefs,
} from '../stores/appState';
import { SIGNALING_PROTOCOLS, type PunchStrategy } from '../types/protocol';
import { formatPunchStrategy, validateServerConfig } from '../utils/format';

defineProps<{
  asDrawer?: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const PUNCH_STRATEGIES: PunchStrategy[] = [
  'standard_udp_v6_first',
  'ipv4_only',
  'ipv6_only',
];

const presetName = ref('');
const presetMsg = ref('');

const serverValidationError = computed(() => {
  if (!appStore.serverForm.host.trim()) return null;
  return validateServerConfig(
    appStore.serverForm.protocol,
    appStore.serverForm.host,
    Number(appStore.serverForm.ws_port),
  );
});

function onSavePreset(): void {
  const err = saveCurrentAsPreset(presetName.value);
  presetMsg.value = err ?? '';
  if (!err) {
    presetName.value = '';
  }
}

function onHostPaste(e: ClipboardEvent): void {
  const text = e.clipboardData?.getData('text') ?? '';
  if (text && applyServerPaste(text)) {
    e.preventDefault();
  }
}
</script>

<template>
  <aside class="adv-panel" :class="{ drawer: asDrawer }">
    <div class="adv-header">
      <span class="adv-title">高级设置</span>
      <button class="btn-sm" @click="emit('close')">收起</button>
    </div>

    <div class="adv-body">
      <section class="adv-section">
        <div class="sec-title">节点预设</div>
        <div class="row">
          <select
            :value="appStore.selectedPresetId"
            @change="applyPreset(($event.target as HTMLSelectElement).value)"
          >
            <option value="">自定义配置</option>
            <option v-for="p in appStore.presets" :key="p.id" :value="p.id">
              {{ p.name }}
            </option>
          </select>
          <button
            v-if="appStore.selectedPresetId"
            class="btn-sm"
            @click="deletePreset(appStore.selectedPresetId)"
          >
            删除
          </button>
        </div>
        <div class="row">
          <input
            v-model="presetName"
            type="text"
            placeholder="预设名称"
          />
          <button class="btn-sm" @click="onSavePreset">保存预设</button>
        </div>
        <div v-if="presetMsg" class="field-err">{{ presetMsg }}</div>
      </section>

      <section class="adv-section">
        <div class="sec-title">自建服务器</div>

        <div class="field">
          <span>信令地址与端口</span>
          <div class="endpoint-row">
            <select
              v-model="appStore.serverForm.protocol"
              class="mono proto-select"
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
              placeholder="域名或 IP"
              @paste="onHostPaste"
              @change="savePrefs"
            />
            <input
              v-model.number="appStore.serverForm.ws_port"
              type="number"
              min="1"
              max="65535"
              class="mono port-input"
              aria-label="信令端口"
              @change="savePrefs"
            />
          </div>
          <span v-if="serverValidationError" class="field-err">
            {{ serverValidationError }}
          </span>
        </div>

        <div class="grid-2">
          <label class="field">
            <span>UDP 探测主端口</span>
            <input
              v-model.number="appStore.serverForm.probe_p1_port"
              type="number"
              min="1"
              max="65535"
              class="mono"
              @change="savePrefs"
            />
          </label>
          <label class="field">
            <span>UDP 探测副端口</span>
            <input
              v-model.number="appStore.serverForm.probe_p2_port"
              type="number"
              min="1"
              max="65535"
              class="mono"
              @change="savePrefs"
            />
          </label>
        </div>

        <label class="field">
          <span>访问口令</span>
          <input
            v-model="appStore.serverForm.token"
            type="password"
            placeholder="可选"
            @change="savePrefs"
          />
        </label>

        <details class="helper-details">
          <summary>第二 IP 辅节点（可选）</summary>
          <input
            v-model="appStore.serverForm.helper_v4_p1"
            type="text"
            class="mono"
            placeholder="辅助节点 IP:34778（可选）"
            @change="savePrefs"
          />
        </details>
      </section>

      <section class="adv-section">
        <div class="sec-title">打洞与探测参数</div>

        <label class="field">
          <span>打洞策略</span>
          <select v-model="appStore.p2pForm.strategy" @change="savePrefs">
            <option v-for="s in PUNCH_STRATEGIES" :key="s" :value="s">
              {{ formatPunchStrategy(s) }}
            </option>
          </select>
        </label>

        <label class="field">
          <span>P2P 公共 STUN 列表 (逗号分隔)</span>
          <textarea
            v-model="appStore.p2pForm.stun_text"
            rows="2"
            class="mono"
            @change="savePrefs"
          />
        </label>

        <label class="field">
          <span>NAT 公共 STUN 列表</span>
          <textarea
            v-model="appStore.natForm.stun_text"
            rows="2"
            class="mono"
            @change="savePrefs"
          />
        </label>

        <div class="grid-2">
          <label class="field">
            <span>候选超时 (ms)</span>
            <input
              v-model.number="appStore.p2pForm.candidate_timeout_ms"
              type="number"
              min="500"
              max="10000"
              class="mono"
              @change="savePrefs"
            />
          </label>
          <label class="field">
            <span>等友超时 (1~120s)</span>
            <input
              v-model.number="appStore.p2pForm.pair_timeout_secs"
              type="number"
              min="1"
              max="120"
              class="mono"
              @change="savePrefs"
            />
          </label>
        </div>

        <label class="field">
          <div class="label-row">
            <span>打洞窗口 (3000~15000 ms)</span>
            <span class="mono">{{ appStore.p2pForm.punch_timeout_ms }} ms</span>
          </div>
          <input
            v-model.number="appStore.p2pForm.punch_timeout_ms"
            type="range"
            min="3000"
            max="15000"
            step="500"
            @change="savePrefs"
          />
        </label>

        <label class="field">
          <div class="label-row">
            <span>采样窗口 (5~60 s)</span>
            <span class="mono">{{ appStore.p2pForm.window_secs }} s</span>
          </div>
          <input
            v-model.number="appStore.p2pForm.window_secs"
            type="range"
            min="5"
            max="60"
            step="1"
            @change="savePrefs"
          />
        </label>

        <label class="field">
          <div class="label-row">
            <span>发包频率 (1~20 Hz)</span>
            <span class="mono">{{ appStore.p2pForm.packet_rate_hz }} Hz</span>
          </div>
          <input
            v-model.number="appStore.p2pForm.packet_rate_hz"
            type="range"
            min="1"
            max="20"
            step="1"
            @change="savePrefs"
          />
        </label>

        <label class="field">
          <span>NAT 单步超时 (ms)</span>
          <input
            v-model.number="appStore.natForm.timeout_ms"
            type="number"
            min="300"
            max="10000"
            class="mono"
            @change="savePrefs"
          />
        </label>
      </section>
    </div>
  </aside>
</template>

<style scoped>
.adv-panel {
  width: 320px;
  flex-shrink: 0;
  border-left: 1px solid var(--border);
  background: var(--bg-surface);
  display: flex;
  flex-direction: column;
  height: 100%;
}

.adv-panel.drawer {
  width: min(92vw, 380px);
  box-shadow: -8px 0 24px rgba(15, 23, 42, 0.16);
}

.adv-panel.drawer .adv-header {
  padding: 14px;
}

.adv-panel.drawer .adv-title {
  font-size: 16px;
}

.adv-panel.drawer .adv-body {
  padding: 14px;
  gap: 16px;
}

.adv-panel.drawer .sec-title {
  font-size: 14px;
}

.adv-panel.drawer .field {
  font-size: 14px;
  gap: 6px;
}

.adv-panel.drawer .port-input {
  width: 84px;
}

.adv-header {
  padding: 10px 12px;
  border-bottom: 1px solid var(--border);
  display: flex;
  justify-content: space-between;
  align-items: center;
}

.adv-title {
  font-weight: 700;
  font-size: 13px;
}

.adv-body {
  flex: 1;
  overflow-y: auto;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.adv-section {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding-bottom: 12px;
  border-bottom: 1px solid var(--border);
}

.adv-section:last-child {
  border-bottom: none;
}

.sec-title {
  font-size: 12px;
  font-weight: 700;
  color: var(--text-secondary);
}

.row {
  display: flex;
  gap: 6px;
}

.row > select,
.row > input {
  flex: 1;
}

.grid-2 {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 8px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
  color: var(--text-secondary);
}

.label-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 6px;
}

.endpoint-row {
  display: flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
}

.proto-select {
  width: auto;
  flex-shrink: 0;
}

.host-input {
  flex: 1;
  min-width: 80px;
}

.port-input {
  width: 72px;
  flex-shrink: 0;
}

.helper-details {
  display: flex;
  flex-direction: column;
  gap: 6px;
  font-size: 12px;
  color: var(--text-secondary);
}

.helper-details > summary {
  cursor: pointer;
  user-select: none;
}

.helper-details > input {
  margin-top: 6px;
  width: 100%;
}

.field-err {
  color: var(--danger);
  font-size: 11px;
}
</style>
