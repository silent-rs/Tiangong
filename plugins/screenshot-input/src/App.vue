<script setup lang="ts">
import { ref } from 'vue';
import { createTiangongBridge, type HostBridge } from '@tiangong/plugin-sdk';

const busy = ref(false);
let bridgePromise: Promise<HostBridge> | null = null;

interface CaptureResult {
  cancelled: boolean;
  source?: string;
  original_name?: string;
}

function getBridge() {
  bridgePromise ??= createTiangongBridge();
  return bridgePromise;
}

async function capture() {
  if (busy.value) return;
  busy.value = true;
  try {
    const bridge = await getBridge();
    // v2 无 wasm 桥接：capture 操作经宿主桥 sidecar.* 直达插件 sidecar。
    const raw = await bridge.call('sidecar.capture', '{}');
    const result = JSON.parse(raw) as CaptureResult;
    if (result.cancelled) return;
    if (!result.source?.startsWith('data:image/png;base64,')) {
      throw new Error('截图插件未返回有效的 PNG 图片');
    }
    await bridge.call('session.input.addAttachment', JSON.stringify({
      source: result.source,
      original_name: result.original_name ?? 'screenshot.png',
      mime_type: 'image/png',
    }));
  } catch (error) {
    bridgePromise = null;
    window.alert(error instanceof Error ? error.message : String(error));
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <button
    type="button"
    class="capture-button"
    title="截图"
    aria-label="截图"
    :aria-busy="busy"
    :disabled="busy"
    @click="capture"
  >
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <path d="M14.5 4 16 7h3a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V9a2 2 0 0 1 2-2h3l1.5-3z" />
      <circle cx="12" cy="13" r="3" />
    </svg>
  </button>
</template>

<style scoped>
.capture-button {
  display: inline-flex;
  box-sizing: border-box;
  width: 32px;
  height: 32px;
  align-items: center;
  justify-content: center;
  border: 0;
  border-radius: calc(var(--radius, 0.5rem) - 2px);
  padding: 0;
  background: transparent;
  color: hsl(var(--muted-foreground, 215.4 16.3% 46.9%));
  cursor: pointer;
  transition: background-color 120ms ease, color 120ms ease, opacity 120ms ease;
}

.capture-button:hover:not(:disabled) {
  background: hsl(var(--accent, 210 40% 96.1%));
  color: hsl(var(--accent-foreground, 222.2 47.4% 11.2%));
}

.capture-button:focus-visible {
  outline: none;
  box-shadow: inset 0 0 0 2px hsl(var(--ring, 222.2 47.4% 11.2%));
}

.capture-button:disabled {
  cursor: wait;
  opacity: 0.55;
}

svg {
  width: 16px;
  height: 16px;
  flex: 0 0 auto;
}
</style>
