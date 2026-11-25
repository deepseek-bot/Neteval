<script setup lang="ts">
import { computed, ref } from 'vue';
import type { LatencyPoint } from '../types/protocol';

const props = withDefaults(
  defineProps<{
    series: LatencyPoint[];
    compact?: boolean;
  }>(),
  { compact: false },
);

const W = 800;
const H = 260;
const PAD = { top: 18, right: 16, bottom: 28, left: 50 };
const INNER_W = W - PAD.left - PAD.right;
const INNER_H = H - PAD.top - PAD.bottom;

const svgRef = ref<SVGSVGElement | null>(null);
const hoveredIdx = ref<number | null>(null);

const stats = computed(() => {
  const valid = props.series
    .map((p) => p.rtt_ms)
    .filter((v): v is number => v !== null && !Number.isNaN(v));
  const rawMaxRtt = valid.length > 0 ? Math.max(...valid) : 50;
  const maxRtt = Math.max(20, Math.ceil((rawMaxRtt * 1.15) / 10) * 10);
  const maxElapsed =
    props.series.length > 0
      ? Math.max(1000, props.series[props.series.length - 1].elapsed_ms)
      : 1000;
  return { maxRtt, maxElapsed };
});

function xForPoint(pt: LatencyPoint, idx: number): number {
  if (props.series.length <= 1) return PAD.left;
  const ratio =
    stats.value.maxElapsed > 0
      ? pt.elapsed_ms / stats.value.maxElapsed
      : idx / (props.series.length - 1);
  return PAD.left + Math.min(1, Math.max(0, ratio)) * INNER_W;
}

function yForRtt(rtt: number): number {
  const ratio = Math.min(1, Math.max(0, rtt / stats.value.maxRtt));
  return PAD.top + (1 - ratio) * INNER_H;
}

const segments = computed<string[]>(() => {
  const paths: string[] = [];
  let current: string[] = [];
  props.series.forEach((pt, idx) => {
    if (pt.rtt_ms === null) {
      if (current.length > 0) {
        paths.push(current.join(' '));
        current = [];
      }
      return;
    }
    const x = xForPoint(pt, idx).toFixed(1);
    const y = yForRtt(pt.rtt_ms).toFixed(1);
    current.push(current.length === 0 ? `M${x},${y}` : `L${x},${y}`);
  });
  if (current.length > 0) {
    paths.push(current.join(' '));
  }
  return paths;
});

const singleDots = computed(() => {
  const dots: { x: number; y: number; seq: number }[] = [];
  props.series.forEach((pt, idx) => {
    if (pt.rtt_ms === null) return;
    const prevNull = idx === 0 || props.series[idx - 1].rtt_ms === null;
    const nextNull =
      idx === props.series.length - 1 || props.series[idx + 1].rtt_ms === null;
    if (prevNull && nextNull) {
      dots.push({ x: xForPoint(pt, idx), y: yForRtt(pt.rtt_ms), seq: pt.seq });
    }
  });
  return dots;
});

const lossMarkers = computed(() =>
  props.series
    .map((pt, idx) => ({ pt, idx }))
    .filter(({ pt }) => pt.rtt_ms === null)
    .map(({ pt, idx }) => ({
      seq: pt.seq,
      x: xForPoint(pt, idx),
    })),
);

const yTicks = computed(() => {
  const max = stats.value.maxRtt;
  return [0, Math.round(max / 2), max].map((val) => ({
    val,
    y: yForRtt(val),
  }));
});

const xTicks = computed(() => {
  const maxMs = stats.value.maxElapsed;
  const midMs = Math.round(maxMs / 2);
  return [
    { label: '0s', x: PAD.left },
    { label: `${(midMs / 1000).toFixed(1)}s`, x: PAD.left + INNER_W / 2 },
    { label: `${(maxMs / 1000).toFixed(1)}s`, x: PAD.left + INNER_W },
  ];
});

const activePoint = computed(() => {
  if (hoveredIdx.value === null) return null;
  const pt = props.series[hoveredIdx.value];
  if (!pt) return null;
  const x = xForPoint(pt, hoveredIdx.value);
  const y = pt.rtt_ms !== null ? yForRtt(pt.rtt_ms) : PAD.top + INNER_H;
  return { pt, x, y };
});

function pickNearest(clientX: number): void {
  const el = svgRef.value;
  if (!el || props.series.length === 0) return;
  const rect = el.getBoundingClientRect();
  if (rect.width <= 0) return;
  const svgX = ((clientX - rect.left) / rect.width) * W;
  let bestIdx = 0;
  let bestDist = Infinity;
  props.series.forEach((pt, idx) => {
    const d = Math.abs(xForPoint(pt, idx) - svgX);
    if (d < bestDist) {
      bestDist = d;
      bestIdx = idx;
    }
  });
  hoveredIdx.value = bestIdx;
}

function onPointerMove(e: PointerEvent): void {
  pickNearest(e.clientX);
}

function onPointerLeave(): void {
  hoveredIdx.value = null;
}
</script>

<template>
  <div class="chart-wrap" :class="{ compact }">
    <div class="chart-meta mono">
      <span v-if="activePoint">
        #{{ activePoint.pt.seq }} | t={{ (activePoint.pt.elapsed_ms / 1000).toFixed(2) }}s |
        <strong :class="activePoint.pt.rtt_ms === null ? 'loss-text' : ''">
          {{
            activePoint.pt.rtt_ms === null
              ? '丢包'
              : `${activePoint.pt.rtt_ms.toFixed(2)} ms`
          }}
        </strong>
      </span>
      <span v-else-if="series.length > 0">
        样本 {{ series.length }} 点 | 丢包 {{ lossMarkers.length }}
      </span>
      <span v-else>等待采样数据</span>
    </div>

    <div v-if="series.length === 0" class="chart-empty">
      暂无采样数据
    </div>

    <svg
      v-else
      ref="svgRef"
      :viewBox="`0 0 ${W} ${H}`"
      preserveAspectRatio="none"
      class="chart-svg"
      @pointermove="onPointerMove"
      @pointerdown="onPointerMove"
      @pointerleave="onPointerLeave"
    >
      <g v-for="t in yTicks" :key="`y-${t.val}`">
        <line
          :x1="PAD.left"
          :x2="W - PAD.right"
          :y1="t.y"
          :y2="t.y"
          stroke="#e5e7eb"
          stroke-width="1"
        />
        <text
          :x="PAD.left - 6"
          :y="t.y + 4"
          text-anchor="end"
          class="axis-label mono"
        >
          {{ t.val }}
        </text>
      </g>

      <g v-for="xt in xTicks" :key="`x-${xt.label}`">
        <text
          :x="xt.x"
          :y="H - 6"
          text-anchor="middle"
          class="axis-label mono"
        >
          {{ xt.label }}
        </text>
      </g>

      <path
        v-for="(d, idx) in segments"
        :key="`seg-${idx}`"
        :d="d"
        fill="none"
        stroke="#0284c7"
        stroke-width="2"
        stroke-linejoin="round"
        stroke-linecap="round"
      />

      <circle
        v-for="dot in singleDots"
        :key="`dot-${dot.seq}`"
        :cx="dot.x"
        :cy="dot.y"
        r="2.8"
        fill="#0284c7"
      />

      <line
        v-for="m in lossMarkers"
        :key="`loss-${m.seq}`"
        :x1="m.x"
        :x2="m.x"
        :y1="PAD.top + INNER_H - 12"
        :y2="PAD.top + INNER_H"
        stroke="#dc2626"
        stroke-width="2.5"
      />

      <g v-if="activePoint">
        <line
          :x1="activePoint.x"
          :x2="activePoint.x"
          :y1="PAD.top"
          :y2="PAD.top + INNER_H"
          stroke="#64748b"
          stroke-width="1"
          stroke-dasharray="3 3"
        />
        <circle
          :cx="activePoint.x"
          :cy="activePoint.y"
          r="4.2"
          :fill="activePoint.pt.rtt_ms === null ? '#dc2626' : '#0284c7'"
          stroke="#ffffff"
          stroke-width="1.5"
        />
      </g>
    </svg>
  </div>
</template>

<style scoped>
.chart-wrap {
  display: flex;
  flex-direction: column;
  width: 100%;
  height: 100%;
  min-height: 220px;
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 10px 12px;
}

.chart-wrap.compact {
  min-height: 170px;
  padding: 8px 10px;
}

.chart-meta {
  display: flex;
  justify-content: flex-end;
  font-size: 12px;
  color: var(--text-secondary);
  margin-bottom: 4px;
  min-height: 18px;
}

.loss-text {
  color: var(--danger);
}

.chart-empty {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--text-muted);
  font-size: 13px;
}

.chart-svg {
  width: 100%;
  flex: 1;
  min-height: 160px;
  touch-action: pan-y;
  user-select: none;
}

.axis-label {
  font-size: 11px;
  fill: #6b7280;
}
</style>
