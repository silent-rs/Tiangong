import { describe, expect, it } from 'vitest';
import { slashPanelKeyAction, type SlashKeyInput } from '@/utils/slashCommandKeys';

function key(name: string, extra: Partial<SlashKeyInput> = {}): SlashKeyInput {
  return { key: name, shiftKey: false, metaKey: false, ctrlKey: false, composing: false, ...extra };
}

describe('slash 命令面板键盘', () => {
  it('Enter 执行高亮命令，不落到发送分支（回归：/ 被原样发成消息）', () => {
    expect(slashPanelKeyAction(key('Enter'), 2, 0)).toEqual({ type: 'select', index: 0 });
    expect(slashPanelKeyAction(key('Enter'), 2, 1)).toEqual({ type: 'select', index: 1 });
  });

  it('Tab 同样执行高亮命令', () => {
    expect(slashPanelKeyAction(key('Tab'), 2, 1)).toEqual({ type: 'select', index: 1 });
  });

  it('高亮下标越界（候选随过滤变少）时取最后一项', () => {
    expect(slashPanelKeyAction(key('Enter'), 1, 3)).toEqual({ type: 'select', index: 0 });
  });

  it('无匹配时 Enter 只关闭面板、不发送', () => {
    expect(slashPanelKeyAction(key('Enter'), 0, 0)).toEqual({ type: 'close' });
  });

  it('方向键循环导航', () => {
    expect(slashPanelKeyAction(key('ArrowDown'), 2, 1)).toEqual({ type: 'move', index: 0 });
    expect(slashPanelKeyAction(key('ArrowUp'), 2, 0)).toEqual({ type: 'move', index: 1 });
  });

  it('Esc 关闭面板', () => {
    expect(slashPanelKeyAction(key('Escape'), 2, 0)).toEqual({ type: 'close' });
  });

  it('IME 组字、Shift+Enter、Cmd/Ctrl+Enter 与普通字符交回原有逻辑', () => {
    expect(slashPanelKeyAction(key('Enter', { composing: true }), 2, 0)).toEqual({ type: 'pass' });
    expect(slashPanelKeyAction(key('Enter', { shiftKey: true }), 2, 0)).toEqual({ type: 'pass' });
    expect(slashPanelKeyAction(key('Enter', { metaKey: true }), 2, 0)).toEqual({ type: 'pass' });
    expect(slashPanelKeyAction(key('Enter', { ctrlKey: true }), 2, 0)).toEqual({ type: 'pass' });
    expect(slashPanelKeyAction(key('a'), 2, 0)).toEqual({ type: 'pass' });
  });
});
