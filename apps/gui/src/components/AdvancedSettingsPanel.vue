<script setup lang="ts">
import { ref } from 'vue';
import {
  appStore,
  applyPreset,
  deletePreset,
  saveCurrentAsPreset,
  savePrefs,
} from '../stores/appState';
import { validateWsUrl } from '../utils/format';

defineProps<{
  asDrawer?: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const presetName = ref('');
const presetMsg = ref('');

function onSavePreset(): void {
  const err = saveCurrentAsPreset(presetName.value);
  presetMsg.value = err ?? '';
  if (!err) {
    presetName.value = '';
  }
}

function reuseNatPrimaryV4(): void {
  if (appStore.natForm.primary_v4_p1.trim()) {
    appStore.p2pForm.echo_udp_v4 = appStore.natForm.primary_v4_p1.trim();
    savePrefs();
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
        <div class="sec-title">P2P 信令与回显</div>

        <label class="field">
          <span>信令地址 (ws://)</span>
          <input
            v-model="appStore.p2pForm.signaling_ws_url"
            type="text"
            class="mono"
            placeholder="ws://1.2.3.4:34781"
            @change="savePrefs"
          />
          <span
            v-if="
              appStore.p2pForm.signaling_ws_url &&
              validateWsUrl(appStore.p2pForm.signaling_ws_url)
            "
            class="field-err"
          >
            {{ validateWsUrl(appStore.p2pForm.signaling_ws_url) }}
          </span>
        </label>

        <label class="field">
          <div class="label-row">
            <span>S2 IPv4 回显 (`echo_udp_v4`)</span>
            <button
              v-if="appStore.natForm.primary_v4_p1.trim()"
              type="button"
              class="btn-link"
              @click="reuseNatPrimaryV4"
            >
              复用 NAT 主端口
            </button>
          </div>
          <input
            v-model="appStore.p2pForm.echo_udp_v4"
            type="text"
            class="mono"
            placeholder="host:34780 (可选)"
            @change="savePrefs"
          />
        </label>

        <label class="field">
          <span>S2 IPv6 回显 (`echo_udp_v6`)</span>
          <input
            v-model="appStore.p2pForm.echo_udp_v6"
            type="text"
            class="mono"
            placeholder="[::]:34780 (可选)"
            @change="savePrefs"
          />
        </label>

        <label class="field">
          <span>公共 STUN 列表 (逗号分隔)</span>
          <textarea
            v-model="appStore.p2pForm.stun_text"
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

        <div class="kv-readonly">
          <span>打洞策略</span>
          <span class="mono">IPv6 优先回退 IPv4</span>
        </div>
      </section>

      <section class="adv-section">
        <div class="sec-title">NAT 体检高级参数</div>
        <label class="field">
          <span>自建主节点 p1 (`primary_v4_p1`)</span>
          <input
            v-model="appStore.natForm.primary_v4_p1"
            type="text"
            class="mono"
            placeholder="host:34780"
            @change="savePrefs"
          />
        </label>
        <label class="field">
          <span>自建主节点 p2 (`primary_v4_p2`)</span>
          <input
            v-model="appStore.natForm.primary_v4_p2"
            type="text"
            class="mono"
            placeholder="host:34781"
            @change="savePrefs"
          />
        </label>
        <label class="field">
          <span>自建 IPv6 (`primary_v6`)</span>
          <input
            v-model="appStore.natForm.primary_v6"
            type="text"
            class="mono"
            placeholder="[host]:34780 (可选)"
            @change="savePrefs"
          />
        </label>
        <label class="field">
          <span>自建辅节点 (`helper_v4_p1`)</span>
          <input
            v-model="appStore.natForm.helper_v4_p1"
            type="text"
            class="mono"
            placeholder="helper_ip:34780 (可选)"
            @change="savePrefs"
          />
        </label>
        <label class="field">
          <span>VPS 口令 (`token`)</span>
          <input
            v-model="appStore.natForm.token"
            type="password"
            placeholder="可选"
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
        <label class="field">
          <span>单步超时 (`timeout_ms`)</span>
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
  width: min(88vw, 340px);
  box-shadow: -8px 0 24px rgba(15, 23, 42, 0.16);
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

.btn-link {
  border: none;
  background: transparent;
  color: var(--accent);
  padding: 0;
  font-size: 11px;
}

.field-err {
  color: var(--danger);
  font-size: 11px;
}

.kv-readonly {
  display: flex;
  justify-content: space-between;
  font-size: 12px;
  color: var(--text-secondary);
  padding: 6px 8px;
  background: var(--bg-muted);
  border-radius: 6px;
  border: 1px solid var(--border);
}
</style>
