import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { AgentTurn } from '@/components/message/AgentTurn';
import type { MessageItem } from '@/components/message/types';

/**
 * 执行中的工具调用（tool_calls 已发出、结果未到达）必须在活跃轮次中可见，
 * 包括本轮第一个调用——此时轮次里还没有任何工具结果可供挂靠。
 */

let seq = 0;
const msg = (role: string, content: string, extra: Partial<MessageItem> = {}): MessageItem => ({
  id: `m${++seq}`,
  role,
  content,
  created_at: new Date(2026, 0, 1, 0, 0, seq).toISOString(),
  ...extra,
}) as MessageItem;

const call = (id: string, name: string, prompt = '太一大战花仙子') => ({ id, name, arguments: { prompt, path: prompt } });

describe('执行中的工具调用', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    seq = 0;
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    delete (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT;
  });

  const render = async (messages: MessageItem[], isActive = true) => {
    await act(async () => {
      root.render(
        <AgentTurn
          messages={messages}
          streamingMessageId={null}
          streamingContent=""
          streamingReasoningContent=""
          isActive={isActive}
        />,
      );
    });
  };

  it('本轮首个调用尚无结果时显示运行行', async () => {
    await render([
      msg('user', '帮我生成图片'),
      msg('assistant', '', { phase: 'react', tool_calls: [call('c1', 'volcengine__generate_image')] }),
    ]);

    expect(container.textContent).toContain('太一大战花仙子');
    expect(container.textContent).not.toContain('工具调用 0 次');
  });

  it('工具组之后出现思考时运行行排在思考之后', async () => {
    await render([
      msg('user', '帮我生成图片'),
      msg('assistant', '', { phase: 'react', tool_calls: [call('c1', 'read_file', 'notes.md')] }),
      msg('tool', '文件内容', { tool_call_id: 'c1', tool_name: 'read_file' }),
      msg('assistant', '', {
        phase: 'react',
        reasoning_content: '接下来生成图片',
        tool_calls: [call('c2', 'volcengine__generate_image')],
      }),
    ]);

    const text = container.textContent ?? '';
    const runningAt = text.indexOf('太一大战花仙子');
    const thinkingAt = text.indexOf('思考');
    expect(thinkingAt).toBeGreaterThan(-1);
    expect(runningAt).toBeGreaterThan(thinkingAt);
  });

  it('执行中的调用可以展开查看参数，没有结果区', async () => {
    await render([
      msg('user', '帮我生成图片'),
      msg('assistant', '', { phase: 'react', tool_calls: [call('c1', 'volcengine__generate_image', '八神太一')] }),
    ]);

    const row = container.querySelector('button.tool-run-row') as HTMLButtonElement | null;
    expect(row).not.toBeNull();
    expect(container.textContent).not.toContain('"prompt"');
    await act(async () => row!.click());
    expect(container.textContent).toContain('"prompt"');
    expect(container.textContent).not.toContain('OUT');
  });

  const parallelCalls = () => [
    call('p1', 'volcengine__generate_image', '并行甲'),
    call('p2', 'volcengine__generate_image', '并行乙'),
    call('p3', 'volcengine__generate_image', '并行丙'),
  ];
  const runningRows = () => Array.from(container.querySelectorAll('button.tool-run-row'))
    .map((row) => row.textContent ?? '');

  it('同一批并行调用全部未完成时逐个显示运行行', async () => {
    await render([
      msg('user', '并行生成三张图'),
      msg('assistant', '', { phase: 'react', tool_calls: parallelCalls() }),
    ]);

    const rows = runningRows();
    expect(rows).toHaveLength(3);
    expect(rows[0]).toContain('并行甲');
    expect(rows[1]).toContain('并行乙');
    expect(rows[2]).toContain('并行丙');
  });

  it('并行调用部分完成时已完成的不再显示为运行行，未完成的排在其后', async () => {
    await render([
      msg('user', '并行生成三张图'),
      msg('assistant', '', { phase: 'react', tool_calls: parallelCalls() }),
      msg('tool', '第二张完成', { tool_call_id: 'p2', tool_name: 'volcengine__generate_image' }),
    ]);

    const rows = runningRows();
    expect(rows).toHaveLength(2);
    expect(rows[0]).toContain('并行甲');
    expect(rows[1]).toContain('并行丙');
    const text = container.textContent ?? '';
    expect(text).toContain('工具调用 1 次');
    // 已完成行（摘要为参数「并行乙」）只出现一次，且在运行行之前。
    expect(text.split('并行乙')).toHaveLength(2);
    expect(text.indexOf('并行乙')).toBeLessThan(text.indexOf('并行甲'));
  });

  it('并行调用全部完成后不再有运行行', async () => {
    await render([
      msg('user', '并行生成三张图'),
      msg('assistant', '', { phase: 'react', tool_calls: parallelCalls() }),
      msg('tool', '一', { tool_call_id: 'p1', tool_name: 'volcengine__generate_image' }),
      msg('tool', '三', { tool_call_id: 'p3', tool_name: 'volcengine__generate_image' }),
      msg('tool', '二', { tool_call_id: 'p2', tool_name: 'volcengine__generate_image' }),
    ]);

    expect(runningRows()).toHaveLength(0);
    expect(container.textContent).toContain('工具调用 3 次');
  });

  it('并行调用的运行行各自独立展开', async () => {
    await render([
      msg('user', '并行生成三张图'),
      msg('assistant', '', { phase: 'react', tool_calls: parallelCalls() }),
    ]);

    const rows = container.querySelectorAll('button.tool-run-row');
    await act(async () => (rows[1] as HTMLButtonElement).click());
    const text = container.textContent ?? '';
    expect(text).toContain('"prompt": "并行乙"');
    expect(text).not.toContain('"prompt": "并行甲"');
    expect(text).not.toContain('"prompt": "并行丙"');
  });

  it('已结束轮次不显示运行行', async () => {
    await render([
      msg('user', '帮我生成图片'),
      msg('assistant', '', { phase: 'react', tool_calls: [call('c1', 'volcengine__generate_image')] }),
    ], false);

    expect(container.textContent).not.toContain('太一大战花仙子');
  });
});
