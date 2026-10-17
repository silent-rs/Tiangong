/**
 * slash 命令面板的键盘语义（纯函数，便于测试）。
 *
 * slash 面板没有独立搜索框：过滤词就是消息框里的 `/xxx`，焦点一直留在
 * 编辑器里。面板打开时由消息框按键处理调用本函数，决定按键的去向；返回
 * `pass` 表示不消费，交回原有逻辑（输入、发送等）。
 */
export interface SlashKeyInput {
  key: string;
  shiftKey: boolean;
  metaKey: boolean;
  ctrlKey: boolean;
  /** IME 组字中（组字中的 Enter 用来上屏，不能拦截）。 */
  composing: boolean;
}

export type SlashKeyAction =
  | { type: 'pass' }
  | { type: 'move'; index: number }
  | { type: 'select'; index: number }
  | { type: 'close' };

export function slashPanelKeyAction(
  input: SlashKeyInput,
  count: number,
  index: number,
): SlashKeyAction {
  if (input.composing) return { type: 'pass' };
  if (input.key === 'ArrowDown' && count > 0) {
    return { type: 'move', index: (index + 1) % count };
  }
  if (input.key === 'ArrowUp' && count > 0) {
    return { type: 'move', index: (index - 1 + count) % count };
  }
  const plainEnter = input.key === 'Enter' && !input.shiftKey && !input.metaKey && !input.ctrlKey;
  if (plainEnter || (input.key === 'Tab' && count > 0)) {
    // 无匹配：只关闭面板，文本留作普通文字，不发送（与 @ 面板一致）。
    return count > 0
      ? { type: 'select', index: Math.min(Math.max(index, 0), count - 1) }
      : { type: 'close' };
  }
  if (input.key === 'Escape') return { type: 'close' };
  return { type: 'pass' };
}
