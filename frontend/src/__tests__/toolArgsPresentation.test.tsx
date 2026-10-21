import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { toolArgsPresentation } = vi.hoisted(() => ({ toolArgsPresentation: vi.fn() }));

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return { ...actual, api: { ...actual.api, toolArgsPresentation } };
});

const { useToolArgsPresentation, resetToolArgsPresentationCache, EMPTY_ARGS_PRESENTATION } =
  await import('@/components/message/toolArgsPresentation');

function Probe() {
  const hints = useToolArgsPresentation();
  return <span data-testid="hints">{JSON.stringify(hints)}</span>;
}

describe('参数摘要提示由运行时提供', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    resetToolArgsPresentationCache();
    toolArgsPresentation.mockReset();
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  const read = async () => {
    await act(async () => root.render(<Probe />));
    for (let i = 0; i < 3; i++) await act(async () => { await Promise.resolve(); });
    return JSON.parse(container.querySelector('[data-testid="hints"]')?.textContent ?? 'null');
  };

  it('读取运行时提示', async () => {
    const value = { lead_args: ['action'], skip_args: ['cwd'], hidden_value_prefixes: [] };
    toolArgsPresentation.mockResolvedValue(value);
    expect(await read()).toEqual(value);
    expect(toolArgsPresentation).toHaveBeenCalledTimes(1);
  });

  it('读取失败时使用空提示（通用摘要）', async () => {
    toolArgsPresentation.mockRejectedValue(new Error('no host'));
    expect(await read()).toEqual(EMPTY_ARGS_PRESENTATION);
  });
});
