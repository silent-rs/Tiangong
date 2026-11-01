import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { SlotContributionEntry } from '@/api/tauri';

const mocks = vi.hoisted(() => {
  const bridgeHandlers = new Set<(event: {
    plugin_id: string;
    channel: string;
    payload: string;
  }) => void>();
  const defaultContribution = (): SlotContributionEntry => ({
    plugin_id: 'interaction',
    contribution_id: 'interaction',
    slot: 'session.interaction',
    title: '交互处理器',
    description: '',
    icon: '',
    group: '',
    has_view: true,
    open_mode: 'singleton',
    sandbox: 'shadow',
    source: 'manifest',
  });
  let contribution: SlotContributionEntry | null = defaultContribution();
  return {
    bridgeHandlers,
    getContribution: () => contribution,
    setContribution: (next: SlotContributionEntry | null) => { contribution = next; },
    resetContribution: () => { contribution = defaultContribution(); },
    listSlotContributions: vi.fn(() => Promise.resolve(contribution ? [contribution] : [])),
    pluginOpenEntry: vi.fn((pluginId: string) => Promise.resolve(`<div>${pluginId}</div>`)),
    bridgeSubscribe: vi.fn(() => Promise.resolve()),
    bridgeUnsubscribe: vi.fn(() => Promise.resolve()),
    onBridgeEvent: vi.fn((handler: (event: {
      plugin_id: string;
      channel: string;
      payload: string;
    }) => void) => {
      bridgeHandlers.add(handler);
      return Promise.resolve(() => bridgeHandlers.delete(handler));
    }),
  };
});

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return {
    ...actual,
    api: {
      ...actual.api,
      listSlotContributions: mocks.listSlotContributions,
      pluginOpenEntry: mocks.pluginOpenEntry,
      bridgeSubscribe: mocks.bridgeSubscribe,
      bridgeUnsubscribe: mocks.bridgeUnsubscribe,
      onBridgeEvent: mocks.onBridgeEvent,
    },
  };
});

vi.mock('@/components/PluginSandbox', () => ({
  PluginSandbox: ({ pluginId, sandbox, sessionId }: {
    pluginId: string;
    sandbox: string;
    sessionId?: string | null;
  }) => (
    <div
      data-testid="plugin-sandbox"
      data-plugin-id={pluginId}
      data-sandbox={sandbox}
      data-session-id={sessionId ?? ''}
    />
  ),
}));

const { InteractionPluginHost } = await import('@/components/InteractionPluginHost');
const { useStore } = await import('@/store/useStore');
const initialState = useStore.getInitialState();

let container: HTMLDivElement | null = null;
let root: Root | null = null;

async function flush() {
  await act(async () => {
    for (let index = 0; index < 10; index += 1) await Promise.resolve();
  });
}

async function renderHost(onVisibilityChange = vi.fn()) {
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root!.render(
      <InteractionPluginHost inputHeight={188} onVisibilityChange={onVisibilityChange} />,
    );
  });
  await flush();
  return onVisibilityChange;
}

function emit(channel: string, payload: object, pluginId = 'interaction') {
  for (const handler of mocks.bridgeHandlers) {
    handler({ plugin_id: pluginId, channel, payload: JSON.stringify(payload) });
  }
}

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  useStore.setState({
    ...initialState,
    activeSessionId: 'session-a',
    newConversationId: null,
  }, true);
  mocks.resetContribution();
});

afterEach(async () => {
  if (root) await act(async () => root!.unmount());
  root = null;
  container?.remove();
  container = null;
  mocks.bridgeHandlers.clear();
  vi.clearAllMocks();
  delete (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT;
});

describe('InteractionPluginHost', () => {
  it('动态加载 Shadow 处理器，并按当前会话显示独立自适应弹层', async () => {
    const onVisibilityChange = await renderHost();
    const dialog = container!.querySelector<HTMLElement>('[role="dialog"]')!;
    const sandbox = container!.querySelector<HTMLElement>('[data-testid="plugin-sandbox"]')!;

    expect(sandbox.dataset.sandbox).toBe('shadow');
    expect(sandbox.dataset.sessionId).toBe('session-a');
    expect(dialog.getAttribute('aria-hidden')).toBe('true');

    await act(async () => {
      emit('tool.requested', { invocation_id: 'invocation-a', session_id: 'session-a' });
    });

    expect(dialog.getAttribute('aria-hidden')).toBe('false');
    expect(dialog.style.minHeight).toBe('188px');
    expect(dialog.style.maxHeight).toContain('520px');
    expect(onVisibilityChange).toHaveBeenLastCalledWith(true);

    await act(async () => {
      useStore.setState({ activeSessionId: 'session-b' });
    });
    expect(dialog.getAttribute('aria-hidden')).toBe('true');
    expect(onVisibilityChange).toHaveBeenLastCalledWith(false);
  });

  it('插件变更事件会热加载新的第三方 Shadow 处理器', async () => {
    await renderHost();
    mocks.setContribution({
      ...mocks.getContribution()!,
      plugin_id: 'third-party-handler',
      contribution_id: 'third-party-entry',
    });

    await act(async () => {
      window.dispatchEvent(new Event('tiangong:plugin-changed'));
    });
    await flush();

    const sandbox = container!.querySelector<HTMLElement>('[data-testid="plugin-sandbox"]')!;
    expect(sandbox.dataset.pluginId).toBe('third-party-handler');
    expect(sandbox.dataset.sandbox).toBe('shadow');
    expect(mocks.pluginOpenEntry).toHaveBeenLastCalledWith(
      'third-party-handler',
      'third-party-entry',
    );
  });

  it('无关插件变化不会重建当前处理器或重置事件订阅', async () => {
    await renderHost();
    expect(mocks.bridgeSubscribe).toHaveBeenCalledTimes(2);

    await act(async () => {
      window.dispatchEvent(new Event('tiangong:plugin-changed'));
    });
    await flush();

    expect(mocks.bridgeSubscribe).toHaveBeenCalledTimes(2);
  });

  it('显示期间禁用处理器会卸载弹层并解除输入锁定', async () => {
    const onVisibilityChange = await renderHost();
    await act(async () => {
      emit('tool.requested', { invocation_id: 'invocation-a', session_id: 'session-a' });
    });
    expect(onVisibilityChange).toHaveBeenLastCalledWith(true);

    mocks.setContribution(null);
    await act(async () => {
      window.dispatchEvent(new Event('tiangong:plugin-changed'));
    });
    await flush();

    expect(container!.querySelector('[role="dialog"]')).toBeNull();
    expect(onVisibilityChange).toHaveBeenLastCalledWith(false);
  });
});
