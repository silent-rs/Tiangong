import { describe, expect, it } from 'vitest';
import { shouldCompensateItemResize, type ScrollAdjustInput } from '@/components/message/virtualScrollAdjust';

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
