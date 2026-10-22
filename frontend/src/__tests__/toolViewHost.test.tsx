import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { SlotContributionEntry } from '@/api/tauri';

const mocks = vi.hoisted(() => {
  const entry = (
    plugin_id: string,
    contribution_id: string,
    tools?: string[],
    render: 'append' | 'replace' = 'replace',
  ): SlotContributionEntry => ({
    plugin_id,
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
    render,
    ...(tools ? { tools } : {}),
  });
  return {
    entry,
    listSlotContributions: vi.fn(() => Promise.resolve([
      entry('fs', 'file-view', ['fs__file_read']),
      entry('fs', 'diff-view', ['fs__file_write:action=replace']),
      entry('other', 'attachment', ['fs__file_read'], 'append'),
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
    message?: { id: string; role: string; render?: { view: string; data?: unknown } };
  }) => (
    <div
      data-testid="plugin-sandbox"
      data-plugin-id={pluginId}
      data-contribution-id={contributionId}
      data-message-id={message?.id}
      data-render={JSON.stringify(message?.render ?? null)}
    />
  ),
}));

const { ToolViewHost, findToolView, findToolTitle } = await import('@/components/message/toolViews/ToolViewHost');
const { resetSlotContributionCache } = await import('@/hooks/useSlotContributions');

describe('工具视图匹配', () => {
  const items = [
    mocks.entry('fs', 'generic', ['fs__file_write']),
    mocks.entry('fs', 'replace', ['fs__file_write:action=replace']),
    mocks.entry('x', 'append-only', ['fs__file_read'], 'append'),
  ];

  it('特指声明优先于通用声明，取值不符时回落通用', () => {
    expect(findToolView(items, 'fs__file_write', { action: 'replace' })).toBe(1);
    expect(findToolView(items, 'fs__file_write', { action: 'write' })).toBe(0);
    expect(findToolView(items, 'fs__file_write', undefined)).toBe(0);
  });

  it('未声明的工具与附加区贡献不接管', () => {
    expect(findToolView(items, 'fs__file_read', {})).toBeNull();
    expect(findToolView(items, 'terminal__run_shell', {})).toBeNull();
  });
});

describe('工具行标题', () => {
  const items = [
    {
      ...mocks.entry('fs', 'tool-view', ['fs__file_read']),
      titles: { fs__file_read: '读取文件', 'fs__file_read:action=list': '列出目录' },
    },
    { ...mocks.entry('x', 'append-only', [], 'append'), titles: { x__t: '不应生效' } },
  ];

  it('特指标题优先于通用标题，未命中回落通用', () => {
    expect(findToolTitle(items, 'fs__file_read', { action: 'list' })).toBe('列出目录');
    expect(findToolTitle(items, 'fs__file_read', { action: 'read' })).toBe('读取文件');
    expect(findToolTitle(items, 'fs__file_read', undefined)).toBe('读取文件');
  });

  it('未声明标题或非 replace 贡献返回 null（宿主显示工具名）', () => {
    expect(findToolTitle(items, 'fs__file_write', {})).toBeNull();
    expect(findToolTitle(items, 'x__t', {})).toBeNull();
  });
});

describe('工具视图挂载', () => {
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

  const data = (tool_name: string, args: unknown, output: string | null = null) => ({
    tool_call_id: 'call-1',
    tool_name,
    arguments: args,
    status: output === null ? ('running' as const) : ('ok' as const),
    output,
    duration_ms: null,
  });

  it('插件声明了视图时挂载插件，调用原始数据经 render.data 交给插件', async () => {
    const payload = data('fs__file_write', { action: 'replace', path: 'a.md' }, 'replacements=1');
    act(() => root.render(
      <ToolViewHost data={payload} fallback={<p data-testid="fallback">文本</p>} />,
    ));
    await flush();
    const sandbox = container.querySelector('[data-testid="plugin-sandbox"]');
    expect(sandbox?.getAttribute('data-contribution-id')).toBe('diff-view');
    expect(sandbox?.getAttribute('data-message-id')).toBe('call-1');
    expect(JSON.parse(sandbox?.getAttribute('data-render') ?? 'null')).toEqual({
      view: 'diff-view',
      data: payload,
    });
    expect(container.querySelector('[data-testid="fallback"]')).toBeNull();
  });

  it('插件未提供视图时以文本默认显示', async () => {
    act(() => root.render(
      <ToolViewHost
        data={data('fs__file_write', { action: 'write' })}
        fallback={<p data-testid="fallback">文本</p>}
      />,
    ));
    await flush();
    expect(container.querySelector('[data-testid="fallback"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="plugin-sandbox"]')).toBeNull();
  });
});
