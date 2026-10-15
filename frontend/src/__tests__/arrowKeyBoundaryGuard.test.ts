import { describe, expect, it } from 'vitest';

import { shouldPreventArrowKey } from '../lib/arrowKeyBoundaryGuard';

/** 构造一个具备 selectionStart/End 与 value 的伪输入元素。 */
function makeEl(value: string, start: number, end: number = start) {
  return {
    value,
    selectionStart: start,
    selectionEnd: end,
  } as unknown as HTMLInputElement;
}

describe('shouldPreventArrowKey', () => {
  describe('左/右方向键（单行）', () => {
    it('光标在最左按左 → 拦截', () => {
      expect(shouldPreventArrowKey(makeEl('abc', 0), 'ArrowLeft')).toBe(true);
    });
    it('光标在最右按右 → 拦截', () => {
      expect(shouldPreventArrowKey(makeEl('abc', 3), 'ArrowRight')).toBe(true);
    });
    it('光标在中间按左 → 不拦截（应移动）', () => {
      expect(shouldPreventArrowKey(makeEl('abc', 2), 'ArrowLeft')).toBe(false);
    });
    it('光标在中间按右 → 不拦截（应移动）', () => {
      expect(shouldPreventArrowKey(makeEl('abc', 1), 'ArrowRight')).toBe(false);
    });
    it('光标在最左按右 → 不拦截（应移动）', () => {
      expect(shouldPreventArrowKey(makeEl('abc', 0), 'ArrowRight')).toBe(false);
    });
    it('光标在最右按左 → 不拦截（应移动）', () => {
      expect(shouldPreventArrowKey(makeEl('abc', 3), 'ArrowLeft')).toBe(false);
    });
    it('空文本按左/右 → 拦截（已无移动空间）', () => {
      expect(shouldPreventArrowKey(makeEl('', 0), 'ArrowLeft')).toBe(true);
      expect(shouldPreventArrowKey(makeEl('', 0), 'ArrowRight')).toBe(true);
    });
  });

  describe('上/下方向键（多行 / 自动换行）', () => {
    it('光标在文本开头按上 → 拦截', () => {
      expect(shouldPreventArrowKey(makeEl('ab\ncd', 0), 'ArrowUp')).toBe(true);
    });
    it('首行非开头按上 → 不拦截（macOS 会移到开头；软换行也可能在视觉上方）', () => {
      expect(shouldPreventArrowKey(makeEl('ab\ncd', 2), 'ArrowUp')).toBe(false);
      expect(shouldPreventArrowKey(makeEl('ab\ncd', 4), 'ArrowUp')).toBe(false);
    });
    it('光标在文本末尾按下 → 拦截', () => {
      expect(shouldPreventArrowKey(makeEl('ab\ncd', 5), 'ArrowDown')).toBe(true);
    });
    it('末行非末尾按下 → 不拦截', () => {
      expect(shouldPreventArrowKey(makeEl('ab\ncd', 3), 'ArrowDown')).toBe(false);
      expect(shouldPreventArrowKey(makeEl('ab\ncd', 1), 'ArrowDown')).toBe(false);
    });
    it('无换行符的长文本（自动换行）中间按上/下 → 不拦截', () => {
      const long = 'a'.repeat(200);
      expect(shouldPreventArrowKey(makeEl(long, 100), 'ArrowUp')).toBe(false);
      expect(shouldPreventArrowKey(makeEl(long, 100), 'ArrowDown')).toBe(false);
    });
  });

  describe('选区与其他按键', () => {
    it('有选区时方向键一律不拦截（会取消选区/移动）', () => {
      expect(shouldPreventArrowKey(makeEl('abc', 0, 2), 'ArrowLeft')).toBe(false);
      expect(shouldPreventArrowKey(makeEl('abc', 1, 3), 'ArrowRight')).toBe(false);
    });
    it('非方向键不拦截', () => {
      expect(shouldPreventArrowKey(makeEl('abc', 0), 'Enter')).toBe(false);
      expect(shouldPreventArrowKey(makeEl('abc', 3), 'Backspace')).toBe(false);
    });
  });
});
