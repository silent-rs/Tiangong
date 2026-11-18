/**
 * CentralSystemNote 行为（第十七轮 Z3① + 用户实测四修复）：
 * - 长回报默认折叠（预览 + 展开全文按钮）；展开限高滚动、可收起
 * - 历史兜底：无 render 标记的 [后台工作完成] 注入也走系统条
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { UserMessageGroup } from '@/components/message/UserMessageGroup';
import type { MessageGroup } from '@/components/message/types';

// 历史兜底限定中枢会话（十八轮 P3）——mock store 的 activeSessionId
let mockActiveSessionId = 'central-hub';
vi.mock('@/store/useStore', () => ({
  useStore: (selector: (state: { activeSessionId: string }) => unknown) =>
    selector({ get activeSessionId() { return mockActiveSessionId; } }),
}));

const realNow = Date.now;

function makeGroup(text: string, render?: unknown): MessageGroup {
  return {
    key: 'g1',
    role: 'user',
    created_at: new Date(realNow()).toISOString(),
    messages: [
      {
        id: 'm1',
        role: 'user',
        created_at: new Date(realNow()).toISOString(),
        content: [{ type: 'text', text }],
        meta: render ? { render } : undefined,
        attachments: [],
      } as never,
    ],
  } as never;
}

describe('CentralSystemNote 折叠与历史兜底', () => {
  let container: HTMLDivElement;
  let root: Root;
  beforeEach(() => {
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });
  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  function renderGroup(group: MessageGroup) {
    act(() => {
      root.render(
        <UserMessageGroup
          group={group}
          runStatus="idle"
          nonEditableIds={new Set<string>()}
          editingMessageId={null}
          editingContent=""
          editingAttachments={[]}
          editingTextareaRef={{ current: null }}
          onStartEdit={() => {}}
          onConfirmEdit={() => {}}
          onCancelEdit={() => {}}
          onSetEditingContent={() => {}}
          onSetEditingAttachments={() => {}}
          onAttachFiles={() => {}}
          onEditPaste={() => {}}
        />,
      );
    });
  }

  it('长回报默认折叠，可展开收起（render 标记）', async () => {
    const long = '[后台工作完成] work-1\n' + '回报正文'.repeat(600);
    renderGroup(makeGroup(long, {
      plugin: 'central-host',
      view: 'system-note',
      data: { kind: 'report' },
    }));
    const note = container.querySelector('[data-testid="central-system-note"]');
    expect(note).not.toBeNull();
    const toggle = container.querySelector('[data-testid="central-system-note-toggle"]');
    expect(toggle).not.toBeNull('长文本必须提供展开按钮');
    expect(note!.textContent!.length).toBeLessThan(long.length, '默认收起（预览）');
    await act(async () => {
      toggle!.click();
    });
    expect(note!.textContent!.length).toBeGreaterThan(2000, '展开显示全文');
    await act(async () => {
      toggle!.click();
    });
    expect(note!.textContent!.length).toBeLessThan(long.length, '可再收起');
  });

  it('征询（kind=decision）默认展开——用户必须读到完整问题才能答复', async () => {
    const long = '[后台工作中断] work-w\n【需要用户决策】是否采用方案 A？\n' + '问题描述'.repeat(600);
    renderGroup(makeGroup(long, {
      plugin: 'central-host',
      view: 'system-note',
      data: { kind: 'decision' },
    }));
    const note = container.querySelector('[data-testid="central-system-note"]');
    expect(note).not.toBeNull();
    expect(note!.textContent).toContain('征询', 'kind 标签');
    // 默认展开（修复前与 report 一样折叠 480 字——征询「无法查看完整」）
    expect(note!.textContent!.length).toBeGreaterThan(2000, '默认显示全文');
    const toggle = container.querySelector('[data-testid="central-system-note-toggle"]');
    await act(async () => {
      toggle!.click();
    });
    expect(note!.textContent!.length).toBeLessThan(long.length, '可手动收起');
  });

  it('历史兜底：kind=report 但标记独占行 → 同样默认展开（旧注入消息立即生效）', () => {
    const legacy = '[后台工作中断] work-old\n【需要用户决策】\n选 A 还是 B？\n' + '细节描述'.repeat(600);
    renderGroup(makeGroup(legacy, {
      plugin: 'central-host',
      view: 'system-note',
      data: { kind: 'report' },
    }));
    const note = container.querySelector('[data-testid="central-system-note"]');
    expect(note!.textContent!.length).toBeGreaterThan(2000, '旧征询消息默认全文');
  });

  it('历史兜底：无标记的 [后台工作完成] 全文也按系统条折叠渲染', () => {
    const legacy = '[后台工作完成] work-old\n' + '历史回报'.repeat(800);
    renderGroup(makeGroup(legacy));
    const note = container.querySelector('[data-testid="central-system-note"]');
    expect(note).not.toBeNull('历史注入不得以用户气泡平铺');
    expect(note!.textContent!.length).toBeLessThan(legacy.length);
  });


  it('负例：普通会话手打同前缀不受影响（用户气泡，非系统条）', () => {
    mockActiveSessionId = 'normal-session-1';
    try {
      renderGroup(makeGroup('[后台工作完成] 用户手打的文本'));
      expect(
        container.querySelector('[data-testid="central-system-note"]'),
      ).toBeNull('普通会话不注入系统条（前缀兜底限中枢会话）');
    } finally {
      mockActiveSessionId = 'central-hub';
    }
  });

  it('普通用户消息不受影响（右对齐气泡，非系统条）', () => {
    renderGroup(makeGroup('这是一条普通用户消息'));
    expect(container.querySelector('[data-testid="central-system-note"]')).toBeNull();
  });
});
