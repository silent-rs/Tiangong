import { describe, expect, it } from 'vitest';
import { blankViewportRecovery, shouldCompensateItemResize, type ScrollAdjustInput } from '@/components/message/virtualScrollAdjust';

const base: ScrollAdjustInput = {
  isLast: false,
  isFirstMeasure: false,
  domTop: null,
  prevSize: 500,
  itemStart: 0,
  scrollOffset: 0,
  scrollingBackward: false,
};

describe('虚拟列表尺寸变化滚动补偿判定', () => {
  it('末组（轮次结束并回列表、收缩过程）从不补偿，避免把消息拉出视口', () => {
    // 旧缓存尺寸推算会误判为整体在视口之上
    expect(shouldCompensateItemResize({ ...base, isLast: true, itemStart: 0, prevSize: 300, scrollOffset: 2000 })).toBe(false);
    expect(shouldCompensateItemResize({ ...base, isLast: true, isFirstMeasure: true, domTop: -800 })).toBe(false);
  });

  it('DOM 显示条目跨越视口顶部时不补偿（变化发生在锚点下方）', () => {
    expect(shouldCompensateItemResize({ ...base, domTop: -200, prevSize: 500 })).toBe(false);
  });

  it('DOM 显示条目整体在视口之上时补偿，保持阅读位置', () => {
    expect(shouldCompensateItemResize({ ...base, domTop: -600, prevSize: 500 })).toBe(true);
  });

  it('向上翻动时不补偿已测量条目，避免连锁跳动', () => {
    expect(shouldCompensateItemResize({ ...base, domTop: -600, prevSize: 500, scrollingBackward: true })).toBe(false);
  });

  it('首次测量：顶部在视口之上即补偿估算误差，在视口内不补偿', () => {
    expect(shouldCompensateItemResize({ ...base, isFirstMeasure: true, domTop: -10 })).toBe(true);
    expect(shouldCompensateItemResize({ ...base, isFirstMeasure: true, domTop: 40 })).toBe(false);
  });

  it('取不到 DOM 位置时退回布局偏移判定', () => {
    expect(shouldCompensateItemResize({ ...base, itemStart: 0, prevSize: 500, scrollOffset: 600 })).toBe(true);
    expect(shouldCompensateItemResize({ ...base, itemStart: 0, prevSize: 500, scrollOffset: 300 })).toBe(false);
    expect(shouldCompensateItemResize({ ...base, isFirstMeasure: true, itemStart: 100, scrollOffset: 300 })).toBe(true);
  });
});

describe('空白视口恢复判定', () => {
  const viewport = { top: 0, bottom: 600 };

  it('有条目与视口相交时不处理', () => {
    expect(blankViewportRecovery(viewport, [{ index: 3, top: -100, bottom: 50 }], 5)).toEqual({ kind: 'none' });
    expect(blankViewportRecovery(viewport, [{ index: 3, top: 590, bottom: 900 }], 5)).toEqual({ kind: 'none' });
  });

  it('条目全部在视口之上（滚过内容末尾，页面空白）时回到末尾', () => {
    expect(blankViewportRecovery(viewport, [
      { index: 3, top: -2400, bottom: -1800 },
      { index: 4, top: -1800, bottom: -700 },
    ], 5)).toEqual({ kind: 'end' });
  });

  it('条目全部在视口之下时对齐到最靠上的已渲染条目', () => {
    expect(blankViewportRecovery(viewport, [
      { index: 5, top: 1200, bottom: 1500 },
      { index: 4, top: 700, bottom: 1200 },
    ], 8)).toEqual({ kind: 'index', index: 4 });
  });

  it('列表非空却没有渲染任何条目时回到末尾；空列表不处理', () => {
    expect(blankViewportRecovery(viewport, [], 6)).toEqual({ kind: 'end' });
    expect(blankViewportRecovery(viewport, [], 0)).toEqual({ kind: 'none' });
  });
  it('末条目底部远高于视口底部（尾部大片空白）且可向上滚动时回到末尾', () => {
    // 截图场景：回复底部操作栏之下是半屏空白
    expect(blankViewportRecovery(viewport, [{ index: 4, top: -900, bottom: 300 }], 5, true)).toEqual({ kind: 'end' });
    // 已在顶部（内容不足一屏）不处理
    expect(blankViewportRecovery(viewport, [{ index: 4, top: 0, bottom: 300 }], 5, false)).toEqual({ kind: 'none' });
    // 末条目下方仅底部内边距的少量留白不处理
    expect(blankViewportRecovery(viewport, [{ index: 4, top: -900, bottom: 560 }], 5, true)).toEqual({ kind: 'none' });
    // 非末条目不判定尾部空白
    expect(blankViewportRecovery(viewport, [{ index: 3, top: -900, bottom: 300 }], 5, true)).toEqual({ kind: 'none' });
  });
});
