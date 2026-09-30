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

  it('已结束轮次不显示运行行', async () => {
    await render([
      msg('user', '帮我生成图片'),
      msg('assistant', '', { phase: 'react', tool_calls: [call('c1', 'volcengine__generate_image')] }),
    ], false);

    expect(container.textContent).not.toContain('太一大战花仙子');
  });
});
