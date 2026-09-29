import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { AgentTurn } from '@/components/message/AgentTurn';
import type { MessageItem } from '@/components/message/types';

/**
 * 通知（Notice）在消息列表的可见性与时序。
 *
 * Notice 承载用户需要看见的状态变化（上下文已压缩、已切换模型等）：
 * 1. 折叠过程时不得被一起藏起来；
 * 2. 必须保持与前后步骤的时序关系，不能被挪到过程区之外统一堆放。
 */

let seq = 0;
const msg = (role: string, content: string, extra: Partial<MessageItem> = {}): MessageItem => ({
  id: `m${++seq}`,
  role,
  content,
  created_at: new Date(2026, 0, 1, 0, 0, seq).toISOString(),
  ...extra,
}) as MessageItem;

describe('通知在消息列表中的可见性', () => {
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

  const render = async (messages: MessageItem[], isActive: boolean) => {
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
    await act(async () => { await new Promise((r) => setTimeout(r, 0)); });
  };

  /** 含两段过程、中间夹一条通知的典型轮次。 */
  const turnWithNotice = (): MessageItem[] => [
    msg('user', '用户问题'),
    msg('tool', '工具结果一', { tool_call_id: 'c1' }),
    msg('notice', '[上下文管理] 上下文接近上限，已自动压缩'),
    msg('tool', '工具结果二', { tool_call_id: 'c2' }),
    msg('assistant', '最终回复'),
  ];

  it('过程折叠后通知仍然可见', async () => {
    // isActive=false 触发「已完成轮次默认折叠过程」。
    await render(turnWithNotice(), false);

    expect(container.textContent).toContain('展开过程');
    expect(container.textContent).toContain('已自动压缩');
  });

  it('展开过程时通知按时序位于两段过程之间', async () => {
    await render(turnWithNotice(), true);

    const text = container.textContent ?? '';
    const noticeAt = text.indexOf('已自动压缩');
    const replyAt = text.indexOf('最终回复');
    expect(noticeAt).toBeGreaterThanOrEqual(0);
    // 通知在过程区内，位于最终回复之前——未被挪到轮次末尾统一堆放。
    expect(noticeAt).toBeLessThan(replyAt);
  });

  it('模型切换通知同样不随过程折叠', async () => {
    await render([
      msg('user', '用户问题'),
      msg('tool', '工具结果', { tool_call_id: 'c1' }),
      msg('notice', '[上下文管理] 已切换模型：gpt-6-astra'),
      msg('assistant', '最终回复'),
    ], false);

    expect(container.textContent).toContain('已切换模型');
  });

  /**
   * 回归：通知原先被统一渲染在总结回复**之前**（summaryFrags 固定挂在
   * 轮次末尾），导致发生在回复之后的压缩/切换被错误地显示在回复上方。
   */
  it('通知发生在回复之后时显示在回复之后', async () => {
    const messages = [
      msg('user', '用户问题'),
      msg('tool', '工具结果', { tool_call_id: 'c1' }),
      msg('assistant', '这是回复正文'),
      // 压缩与切换发生在回复产出之后（切换模型前整理上下文的真实时序）。
      msg('notice', '[上下文管理] 上下文接近上限，已自动压缩'),
      msg('notice', '[上下文管理] 已切换模型：glm-5.3'),
    ];

    for (const isActive of [true, false]) {
      await render(messages, isActive);
      const text = container.textContent ?? '';
      const replyAt = text.indexOf('这是回复正文');
      const compressedAt = text.indexOf('已自动压缩');
      const switchedAt = text.indexOf('已切换模型');
      expect(replyAt).toBeGreaterThanOrEqual(0);
      expect(compressedAt).toBeGreaterThan(replyAt);
      expect(switchedAt).toBeGreaterThan(compressedAt);
    }
  });

  it('通知发生在回复之前时显示在回复之前', async () => {
    await render([
      msg('user', '用户问题'),
      msg('notice', '[上下文管理] 上下文接近上限，已自动压缩'),
      msg('assistant', '这是回复正文'),
    ], false);

    const text = container.textContent ?? '';
    expect(text.indexOf('已自动压缩')).toBeLessThan(text.indexOf('这是回复正文'));
  });
});
