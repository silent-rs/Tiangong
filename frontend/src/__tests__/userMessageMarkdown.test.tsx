import { act, createRef } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { Message } from '@/api/tauri';
import type { MentionEditorHandle } from '@/components/MentionEditor';
import { UserMessageGroup } from '@/components/message/UserMessageGroup';
import type { MessageGroup } from '@/components/message/types';
import { useSearchStore } from '@/store/useSearchStore';

vi.mock('md-editor-rt', () => ({
  MdPreview: ({ modelValue }: { modelValue: string }) => (
    <div data-testid="md-preview">{modelValue}</div>
  ),
}));

function message(text: string): Message {
  return {
    id: 'user-message',
    role: { type: 'user' },
    content: [{ type: 'text', text }],
    created_at: '2026-09-15 12:00:00',
  };
}

function group(text: string): MessageGroup {
  return {
    key: 'user-message',
    type: 'user',
    messages: [message(text)],
  };
}

function renderGroup(root: Root, text: string) {
  return act(async () => {
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
}

describe('用户消息 Markdown 渲染路径', () => {
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

  it('Markdown 文本走预览渲染，原文原样传递', async () => {
    const text = '报告如下：\n\n**结论**：`验证通过`，详见 [说明](https://example.com)';
    await renderGroup(root, text);

    const preview = container.querySelector('[data-testid="md-preview"]');
    expect(preview).not.toBeNull();
    expect(preview!.textContent).toBe(text);
  });

  it('带换行的普通文本同样走预览渲染，换行保留在渲染输入中', async () => {
    const text = '第一行\n第二行\n\n第四行';
    await renderGroup(root, text);

    const preview = container.querySelector('[data-testid="md-preview"]');
    expect(preview).not.toBeNull();
    expect(preview!.textContent).toContain('\n');
  });

  it('包含提及的消息保持标签展示，不走 Markdown 渲染', async () => {
    const text = '请 @dev 处理这个问题';
    await renderGroup(root, text);

    expect(container.querySelector('.mention-chip')).not.toBeNull();
    expect(container.querySelector('[data-testid="md-preview"]')).toBeNull();
    expect(container.textContent).toContain('处理这个问题');
  });

  it('搜索命中时退回纯文本高亮，不走 Markdown 渲染', async () => {
    const text = '**加粗内容**与普通内容';
    await renderGroup(root, text);

    await act(async () => {
      useSearchStore.setState({ searchQuery: '普通内容', currentMessageId: 'user-message', currentMatchStart: 9 });
    });
    await renderGroup(root, text);

    expect(container.querySelector('[data-testid="md-preview"]')).toBeNull();
    expect(container.querySelector('mark')).not.toBeNull();
    expect(container.textContent).toContain('**加粗内容**');
  });

  it('搜索无命中时仍走 Markdown 渲染', async () => {
    const text = '**加粗内容**';
    useSearchStore.setState({ searchQuery: '不存在的词' });
    await renderGroup(root, text);

    const preview = container.querySelector('[data-testid="md-preview"]');
    expect(preview).not.toBeNull();
    expect(preview!.textContent).toBe(text);
  });
});
