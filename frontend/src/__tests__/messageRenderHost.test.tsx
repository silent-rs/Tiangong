import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { SlotContributionEntry } from '@/api/tauri';

const mocks = vi.hoisted(() => {
  const entry = (
    contribution_id: string,
    render?: 'append' | 'replace',
  ): SlotContributionEntry => ({
    plugin_id: 'bot',
    contribution_id,
    slot: 'session.message-item',
    title: contribution_id,
    description: '',
    icon: '',
    group: '',
    has_view: true,
    open_mode: 'singleton',
    sandbox: 'shadow',
    source: 'manifest',
    ...(render ? { render } : {}),
  });
  return {
    entry,
    listSlotContributions: vi.fn(() => Promise.resolve([
      entry('attachment'),
      entry('im-message', 'replace'),
    ])),
    pluginOpenEntry: vi.fn(() => Promise.resolve('<div></div>')),
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
    },
  };
});

vi.mock('@/components/PluginSandbox', () => ({
  PluginSandbox: ({ pluginId, contributionId, message }: {
    pluginId: string;
    contributionId: string;
    message?: { id: string; render?: { view: string; data?: unknown } };
  }) => (
    <div
      data-testid="plugin-sandbox"
      data-plugin-id={pluginId}
      data-contribution-id={contributionId}
      data-render={JSON.stringify(message?.render ?? null)}
    />
  ),
}));

const { MessageRenderHost } = await import('@/components/MessageRenderHost');
const { MessagePluginHost } = await import('@/components/MessagePluginHost');
const { resetSlotContributionCache } = await import('@/hooks/useSlotContributions');

const message = { id: 'm1', role: 'user', text: '你好', attachments: [] };

describe('消息渲染声明接管显示', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    resetSlotContributionCache();
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  const flush = async () => {
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
  };

  it('声明指向 replace 贡献时挂载插件并传入渲染数据', async () => {
    act(() => root.render(
      <MessageRenderHost
        render={{ plugin: 'bot', view: 'im-message', data: { platform: 'weixin' } }}
        message={message}
        fallback={<p data-testid="fallback">默认</p>}
      />,
    ));
    await flush();
    const sandbox = container.querySelector('[data-testid="plugin-sandbox"]');
    expect(sandbox?.getAttribute('data-contribution-id')).toBe('im-message');
    expect(JSON.parse(sandbox?.getAttribute('data-render') ?? 'null')).toEqual({
      view: 'im-message',
      data: { platform: 'weixin' },
    });
    expect(container.querySelector('[data-testid="fallback"]')).toBeNull();
  });

  it('插件或视图不存在、或贡献不是 replace 时回退默认显示', async () => {
    for (const render of [
      { plugin: 'missing', view: 'im-message' },
      { plugin: 'bot', view: 'unknown' },
      { plugin: 'bot', view: 'attachment' },
    ]) {
      act(() => root.render(
        <MessageRenderHost
          key={`${render.plugin}:${render.view}`}
          render={render}
          message={message}
          fallback={<p data-testid="fallback">默认</p>}
        />,
      ));
      await flush();
      expect(container.querySelector('[data-testid="fallback"]')).not.toBeNull();
      expect(container.querySelector('[data-testid="plugin-sandbox"]')).toBeNull();
    }
  });

  it('附加区不挂载 replace 贡献', async () => {
    act(() => root.render(<MessagePluginHost slot="session.message-item" message={message} />));
    await flush();
    const ids = [...container.querySelectorAll('[data-testid="plugin-sandbox"]')]
      .map((node) => node.getAttribute('data-contribution-id'));
    expect(ids).toEqual(['attachment']);
  });
});
