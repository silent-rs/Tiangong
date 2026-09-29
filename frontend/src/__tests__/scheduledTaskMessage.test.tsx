import { act, createRef } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { Message } from '@/api/tauri';
import type { MentionEditorHandle } from '@/components/MentionEditor';
import { UserMessageGroup } from '@/components/message/UserMessageGroup';
import type { MessageGroup } from '@/components/message/types';
import { useSearchStore } from '@/store/useSearchStore';
import { findSearchMatches } from '@/utils/search';
import { parseScheduledTaskMessage } from '@/utils/scheduledTaskMessage';

const UNIX_MESSAGE = [
  '[定时任务触发]',
  '任务名称：夜间巡检',
  '任务描述：检查服务状态',
  '',
  '读取监控数据',
  '重启异常实例',
].join('\n');

function message(text: string): Message {
  return {
    id: 'scheduled-message',
    role: 'user',
    content: [{ type: 'text', text }],
    reasoning_content: '',
    phase: 'normal',
    created_at: '2026-07-28 22:00:00',
  };
}

function group(text: string): MessageGroup {
  return {
    key: 'scheduled-message',
    type: 'user',
    messages: [message(text)],
  };
}

describe('定时任务消息解析与搜索', () => {
  it('解析 Unix 换行和多行执行内容，并保留原文位置', () => {
    expect(parseScheduledTaskMessage(UNIX_MESSAGE)).toEqual({
      name: '夜间巡检',
      description: '检查服务状态',
      payload: '读取监控数据\n重启异常实例',
      offsets: {
        name: UNIX_MESSAGE.indexOf('夜间巡检'),
        description: UNIX_MESSAGE.indexOf('检查服务状态'),
        payload: UNIX_MESSAGE.indexOf('读取监控数据'),
      },
    });
  });

  it('解析 Windows 换行并保留多行内容', () => {
    const text = [
      '[定时任务触发]',
      '任务名称：Windows 巡检',
      '任务描述：检查磁盘',
      '',
      '第一行',
      '第二行',
    ].join('\r\n');

    expect(parseScheduledTaskMessage(text)).toEqual({
      name: 'Windows 巡检',
      description: '检查磁盘',
      payload: '第一行\r\n第二行',
      offsets: {
        name: text.indexOf('Windows 巡检'),
        description: text.indexOf('检查磁盘'),
        payload: text.indexOf('第一行'),
      },
    });
  });

  it('允许名称、描述和执行内容为空', () => {
    const text = '[定时任务触发]\n任务名称：\n任务描述：\n\n';

    expect(parseScheduledTaskMessage(text)).toMatchObject({
      name: '',
      description: '',
      payload: '',
    });
  });

  it.each([
    '[定时任务触发] 只是普通文本',
    '[定时任务触发中]\n任务名称：巡检\n任务描述：说明\n\n内容',
    '[定时任务触发]\n名称：巡检\n任务描述：说明\n\n内容',
    '[定时任务触发]\n任务名称：巡检\n任务描述：说明\n内容',
  ])('不把格式相似的普通消息识别为定时任务：%s', (text) => {
    expect(parseScheduledTaskMessage(text)).toBeNull();
  });

  it.each([
    ['夜间巡检', 'name'],
    ['检查服务状态', 'description'],
    ['重启异常实例', 'payload'],
  ] as const)('搜索命中%s时返回原消息中的准确位置', (query, section) => {
    const parsed = parseScheduledTaskMessage(UNIX_MESSAGE)!;
    const sourceOffset = parsed.offsets[section];
    const localOffset = parsed[section].indexOf(query);
    const matches = findSearchMatches(
      [message(UNIX_MESSAGE)],
      query,
      [{ messages: [{ id: 'scheduled-message' }] }],
      'messages',
    );

    expect(matches).toEqual([{
      messageId: 'scheduled-message',
      groupIndex: 0,
      start: sourceOffset + localOffset,
      end: sourceOffset + localOffset + query.length,
    }]);
  });
});

describe('定时任务消息展示与编辑保护', () => {
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
          editingMessageId="scheduled-message"
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

    expect(container.textContent).toContain('定时任务');
    expect(container.textContent).toContain('夜间巡检');
    expect(container.textContent).toContain('检查服务状态');
    expect(container.textContent).toContain('读取监控数据');
    expect(container.textContent).toContain('重启异常实例');
    expect(container.textContent).not.toContain('[定时任务触发]');
    expect(container.querySelector('textarea')).toBeNull();
    expect(container.querySelector('button[title="编辑并重发"]')).toBeNull();
    expect(container.querySelector('button[title="复制"]')).not.toBeNull();
    expect(onStartEdit).not.toHaveBeenCalled();
  });

  it('空执行内容显示明确占位', async () => {
    const text = '[定时任务触发]\n任务名称：空任务\n任务描述：\n\n';

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
      '[定时任务触发]',
      '任务名称：夜间巡检',
      '任务描述：检查服务状态',
      '',
      '请 @dev 处理巡检',
    ].join('\n');
    const query = '处理巡检';

    useSearchStore.setState({
      searchQuery: query,
      currentMessageId: 'scheduled-message',
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
