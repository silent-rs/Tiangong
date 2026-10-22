import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { listToolIcons, pluginReadToolIcon } = vi.hoisted(() => ({
  listToolIcons: vi.fn(),
  pluginReadToolIcon: vi.fn(),
}));

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return {
    ...actual,
    api: { ...actual.api, listToolIcons, pluginReadToolIcon },
  };
});

import { ToolIcon, iconForArgs, lookupToolIcon, resetToolIconCache } from '@/components/message/ToolIcon';

/**
 * 工具行图标：插件运行时的查询表优先（插件声明 > runtime 内置表），
 * 查不到、表读取失败或图标无法渲染时使用默认图标「工具交叉」。
 */
describe('工具行图标', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    resetToolIconCache();
    listToolIcons.mockReset();
    pluginReadToolIcon.mockReset();
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    delete (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT;
  });

  const render = async (toolName: string, args?: unknown) => {
    await act(async () => {
      root.render(<ToolIcon toolName={toolName} args={args} className="w-3 h-3" />);
    });
    // 等待查表与动态图标加载。
    for (let i = 0; i < 5; i++) {
      await act(async () => { await new Promise((r) => setTimeout(r, 10)); });
    }
  };

  const iconClass = () => container.querySelector('svg')?.getAttribute('data-tool-icon') ?? '';

  it('按对外名优先、原名其次查表', () => {
    const table = {
      generate_image: { icon: 'image' },
      volcengine__generate_image: { icon: 'flame', plugin_id: 'volcengine' },
    };
    expect(lookupToolIcon(table, 'volcengine__generate_image')?.icon).toBe('flame');
    expect(lookupToolIcon(table, 'other__generate_image')?.icon).toBe('image');
    expect(lookupToolIcon(table, 'unknown_tool')).toBeNull();
    expect(lookupToolIcon(table, '')).toBeNull();
  });

  it('使用插件提供的图标名', async () => {
    listToolIcons.mockResolvedValue({ run_shell: { icon: 'square-terminal' } });
    await render('run_shell');
    expect(iconClass()).toBe('square-terminal');
  });

  it('查不到时使用默认图标（工具交叉）', async () => {
    listToolIcons.mockResolvedValue({ run_shell: { icon: 'square-terminal' } });
    await render('unknown_tool');
    expect(iconClass()).toBe('tools-crossed');
  });

  it('图标表读取失败时使用默认图标', async () => {
    listToolIcons.mockRejectedValue(new Error('no host'));
    await render('run_shell');
    expect(iconClass()).toBe('tools-crossed');
  });

  it('未知图标名使用默认图标', async () => {
    listToolIcons.mockResolvedValue({ demo: { icon: 'not-a-real-icon-name' } });
    await render('demo');
    expect(iconClass()).toBe('tools-crossed');
  });

  it('插件资源图标渲染为图片，加载失败时回落默认图标', async () => {
    const createObjectURL = vi.fn(() => 'blob:tool-icon');
    (URL as unknown as { createObjectURL: typeof createObjectURL }).createObjectURL = createObjectURL;
    listToolIcons.mockResolvedValue({
      speak: { icon: 'icons/speak.svg', plugin_id: 'volcengine' },
      broken: { icon: 'icons/broken.svg', plugin_id: 'volcengine' },
    });
    pluginReadToolIcon.mockImplementation(async (_pluginId: string, icon: string) => {
      if (icon === 'icons/broken.svg') throw new Error('missing');
      return { data: [60, 115, 118, 103, 47, 62], mime: 'image/svg+xml' };
    });

    await render('speak');
    expect(pluginReadToolIcon).toHaveBeenCalledWith('volcengine', 'icons/speak.svg');
    expect(container.querySelector('img')?.getAttribute('src')).toBe('blob:tool-icon');

    await render('broken');
    expect(container.querySelector('img')).toBeNull();
    expect(iconClass()).toBe('tools-crossed');
  });

  it('按调用参数选特指图标，未命中或参数未到时用通用图标', () => {
    const entry = {
      icon: 'file-text',
      variants: { action: { list: 'folder-tree', image: 'image' } },
    };
    expect(iconForArgs(entry, { action: 'list' })).toBe('folder-tree');
    expect(iconForArgs(entry, { action: 'image' })).toBe('image');
    expect(iconForArgs(entry, { action: 'read' })).toBe('file-text');
    expect(iconForArgs(entry, undefined)).toBe('file-text');
    expect(iconForArgs({ icon: 'clock' }, { action: 'list' })).toBe('clock');
  });

  it('工具行按参数渲染特指图标', async () => {
    listToolIcons.mockResolvedValue({
      fs__file_read: { icon: 'file-text', plugin_id: 'fs', variants: { action: { list: 'folder-tree' } } },
    });
    await render('fs__file_read', { action: 'list' });
    expect(iconClass()).toBe('folder-tree');
    await render('fs__file_read', { action: 'read' });
    expect(iconClass()).toBe('file-text');
  });

  it('只有特指图标时未命中使用默认图标', async () => {
    listToolIcons.mockResolvedValue({ speak: { icon: '', variants: { voice: { a: 'mic' } } } });
    await render('speak', { voice: 'b' });
    expect(iconClass()).toBe('tools-crossed');
  });
});
