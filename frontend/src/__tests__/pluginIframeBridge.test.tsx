import { describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  bridgeCall: vi.fn(() => Promise.resolve('{}')),
  bridgeSubscribe: vi.fn(() => Promise.resolve()),
  bridgeUnsubscribe: vi.fn(() => Promise.resolve()),
  onBridgeEvent: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock('@/api/tauri', () => ({ api: {
  bridgeCall: mocks.bridgeCall,
  bridgeSubscribe: mocks.bridgeSubscribe,
  bridgeUnsubscribe: mocks.bridgeUnsubscribe,
  onBridgeEvent: mocks.onBridgeEvent,
} }));
vi.mock('@/hooks/useTheme', () => ({ useResolvedTheme: () => 'dark' }));
vi.mock('@/hooks/usePluginMask', () => ({ usePluginMask: () => undefined }));

import { normalizeBridgeMethod } from '@/components/PluginIframe';

describe('iframe 桥接方法透传（审查问题 1 回归）', () => {
  it('完整命名空间方法直接透传，不改写为 plugin.*', () => {
    expect(normalizeBridgeMethod('storage.set')).toBe('storage.set');
    expect(normalizeBridgeMethod('session.getMessages')).toBe('session.getMessages');
    expect(normalizeBridgeMethod('plugin.getConfig')).toBe('plugin.getConfig');
    expect(normalizeBridgeMethod('tool.resolve')).toBe('tool.resolve');
    expect(normalizeBridgeMethod('sidecar.status')).toBe('sidecar.status');
  });

  it('旧协议裸方法名补 plugin. 前缀（v1 设置页兼容）', () => {
    expect(normalizeBridgeMethod('bootstrap')).toBe('plugin.bootstrap');
    expect(normalizeBridgeMethod('get_config')).toBe('plugin.get_config');
  });
});
