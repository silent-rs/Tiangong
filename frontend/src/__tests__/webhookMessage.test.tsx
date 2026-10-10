import { act, createRef } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { Message } from '@/api/tauri';
import type { MentionEditorHandle } from '@/components/MentionEditor';
import { UserMessageGroup } from '@/components/message/UserMessageGroup';
import type { MessageGroup } from '@/components/message/types';
import { useSearchStore } from '@/store/useSearchStore';
import { findSearchMatches } from '@/utils/search';
import { parseWebhookMessage } from '@/utils/webhookMessage';

const UNIX_MESSAGE = [
  '[Webhook触发]',
  '任务名称：推送构建',
  '任务描述：GitHub 推送',
  '',
  '拉取最新代码',
  '执行部署',
].join('\n');

function message(text: string): Message {
  return {
    id: 'webhook-message',
    role: 'user',
    content: [{ type: 'text', text }],
    created_at: '2026-07-29 09:00:00',
  };
}

function group(text: string): MessageGroup {
  return {
    key: 'webhook-message',
    type: 'user',
    messages: [message(text)],
  };
}

describe('webhook 消息解析与搜索', () => {
  it('解析 Unix 换行和多行执行内容，并保留原文位置', () => {
    expect(parseWebhookMessage(UNIX_MESSAGE)).toEqual({
      name: '推送构建',
      description: 'GitHub 推送',
      payload: '拉取最新代码\n执行部署',
      offsets: {
        name: UNIX_MESSAGE.indexOf('推送构建'),
        description: UNIX_MESSAGE.indexOf('GitHub 推送'),
        payload: UNIX_MESSAGE.indexOf('拉取最新代码'),
      },
    });
  });

  it('解析 Windows 换行并保留多行内容', () => {
    const text = [
      '[Webhook触发]',
      '任务名称：Windows 构建',
      '任务描述：合并请求',
      '',
      '第一行',
      '第二行',
    ].join('\r\n');

    expect(parseWebhookMessage(text)).toEqual({
      name: 'Windows 构建',
      description: '合并请求',
      payload: '第一行\r\n第二行',
      offsets: {
        name: text.indexOf('Windows 构建'),
        description: text.indexOf('合并请求'),
        payload: text.indexOf('第一行'),
      },
    });
  });

  it('允许名称、描述和执行内容为空', () => {
    const text = '[Webhook触发]\n任务名称：\n任务描述：\n\n';

    expect(parseWebhookMessage(text)).toMatchObject({
      name: '',
      description: '',
      payload: '',
    });
  });

  it.each([
    '[Webhook触发] 只是普通文本',
    '[Webhook触发中]\n任务名称：构建\n任务描述：说明\n\n内容',
    '[Webhook触发]\n名称：构建\n任务描述：说明\n\n内容',
    '[Webhook触发]\n任务名称：构建\n任务描述：说明\n内容',
  ])('不把格式相似的普通消息识别为 webhook：%s', (text) => {
    expect(parseWebhookMessage(text)).toBeNull();
  });

  it('不把定时任务消息识别为 webhook（两者互斥）', () => {
    const text = [
      '[定时任务触发]',
      '任务名称：夜间巡检',
      '任务描述：检查服务',
      '',
      '内容',
    ].join('\n');

    expect(parseWebhookMessage(text)).toBeNull();
  });

  it.each([
    ['推送构建', 'name'],
    ['GitHub 推送', 'description'],
    ['执行部署', 'payload'],
  ] as const)('搜索命中%s时返回原消息中的准确位置', (query, section) => {
    const parsed = parseWebhookMessage(UNIX_MESSAGE)!;
    const sourceOffset = parsed.offsets[section];
    const localOffset = parsed[section].indexOf(query);
    const matches = findSearchMatches(
      [message(UNIX_MESSAGE)],
      query,
      [{ messages: [{ id: 'webhook-message' }] }],
      'messages',
    );

    expect(matches).toEqual([{
      messageId: 'webhook-message',
      groupIndex: 0,
      start: sourceOffset + localOffset,
      end: sourceOffset + localOffset + query.length,
    }]);
  });
});

describe('webhook 消息展示与编辑保护', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    useSearchStore.setState({
      searchQuery: '',
      currentMessageId: null,
      currentMatchStart: null,
      caseSensitive: false,
    });
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    delete (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT;
  });

  it('使用专用内容展示，并在外部要求编辑时仍不进入编辑状态', async () => {
    const onStartEdit = vi.fn();

    await act(async () => {
      root.render(
        <UserMessageGroup
          group={group(UNIX_MESSAGE)}
          runStatus="idle"
          nonEditableIds={new Set()}
          editingMessageId="webhook-message"
          editingContent={UNIX_MESSAGE}
          editingAttachments={[]}
          editingTextareaRef={createRef<MentionEditorHandle>()}
          onStartEdit={onStartEdit}
          onConfirmEdit={vi.fn()}
          onCancelEdit={vi.fn()}
          onSetEditingContent={vi.fn()}
          onSetEditingAttachments={vi.fn()}
          onAttachFiles={vi.fn()}
          onEditPaste={vi.fn()}
        />,
      );
    });

    expect(container.textContent).toContain('Webhook 触发');
    expect(container.textContent).toContain('推送构建');
    expect(container.textContent).toContain('GitHub 推送');
    expect(container.textContent).toContain('拉取最新代码');
    expect(container.textContent).toContain('执行部署');
    expect(container.textContent).not.toContain('[Webhook触发]');
    expect(container.querySelector('textarea')).toBeNull();
    expect(container.querySelector('button[title="编辑并重发"]')).toBeNull();
    expect(container.querySelector('button[title="复制"]')).not.toBeNull();
    expect(onStartEdit).not.toHaveBeenCalled();
  });

  it('空执行内容显示明确占位', async () => {
    const text = '[Webhook触发]\n任务名称：空触发\n任务描述：\n\n';

    await act(async () => {
      root.render(
        <UserMessageGroup
          group={group(text)}
          runStatus="idle"
          nonEditableIds={new Set()}
          editingMessageId={null}
          editingContent=""
          editingAttachments={[]}
          editingTextareaRef={createRef<MentionEditorHandle>()}
          onStartEdit={vi.fn()}
          onConfirmEdit={vi.fn()}
          onCancelEdit={vi.fn()}
          onSetEditingContent={vi.fn()}
          onSetEditingAttachments={vi.fn()}
          onAttachFiles={vi.fn()}
          onEditPaste={vi.fn()}
        />,
      );
    });

    expect(container.textContent).toContain('无执行内容');
  });

  it('执行内容中的提及保持标签展示，并正确高亮其后的搜索结果', async () => {
    const text = [
      '[Webhook触发]',
      '任务名称：推送构建',
      '任务描述：GitHub 推送',
      '',
      '请 @dev 处理构建',
    ].join('\n');
    const query = '处理构建';

    useSearchStore.setState({
      searchQuery: query,
      currentMessageId: 'webhook-message',
      currentMatchStart: text.indexOf(query),
      caseSensitive: false,
    });

    await act(async () => {
      root.render(
        <UserMessageGroup
          group={group(text)}
          runStatus="idle"
          nonEditableIds={new Set()}
          editingMessageId={null}
          editingContent=""
          editingAttachments={[]}
          editingTextareaRef={createRef<MentionEditorHandle>()}
          onStartEdit={vi.fn()}
          onConfirmEdit={vi.fn()}
          onCancelEdit={vi.fn()}
          onSetEditingContent={vi.fn()}
          onSetEditingAttachments={vi.fn()}
          onAttachFiles={vi.fn()}
          onEditPaste={vi.fn()}
        />,
      );
    });

    expect(container.querySelector('[data-mention-token="@dev"]')).not.toBeNull();
    expect(container.querySelector('.search-highlight-current')?.textContent).toBe(query);
  });
});
