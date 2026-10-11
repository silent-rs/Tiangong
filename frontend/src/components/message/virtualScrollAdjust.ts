/**
 * 虚拟列表条目尺寸变化时是否补偿滚动位置（作为 TanStack Virtual 的
 * `shouldAdjustScrollPositionOnItemSizeChange`）。
 *
 * 库的默认判定按「缓存尺寸 + 库内记录的滚动偏移」推算条目是否在视口之上，
 * 在本列表里会误判：
 * - 流式轮次渲染在虚拟列表之外，结束时整组重新进入列表。此前执行阶段
 *   （工具调用期间流式 id 为空）该组已在列表里测量过，缓存的是当时的
 *   旧尺寸。重新进入时按旧尺寸推算会把「跨越视口」的末组误判成「整体位于
 *   视口之上」，按 新尺寸-旧尺寸 平移 scrollTop；目标越界时库还会记下
 *   被截断的目标，等之后内容变高（收缩/展开过程、下一轮回复）再补滚过去，
 *   把消息拉出视口。
 * - 库内滚动偏移在同一提交内尚未同步浏览器的截断结果，同样会放大误差。
 *
 * 判定改为以 DOM 实际位置为准：
 * - 末组从不补偿：它的顶部位置不随自身尺寸变化，变化只影响它自身下方，
 *   视口内已显示的内容不会因此移动（缩短时由浏览器截断到底部）。
 * - 其余条目按 DOM 中条目顶部相对视口顶部的位置判断：首次测量时顶部在
 *   视口之上即补偿（估算误差只影响其后的条目）；再次测量时仅当变化前
 *   整个条目都在视口之上才补偿，且向上翻动时不补偿，避免连锁跳动。
 * - 取不到 DOM 位置时退回库的默认判定。
 */
export interface ScrollAdjustInput {
  /** 是否为列表最后一个条目。 */
  isLast: boolean;
  /** 该条目此前是否从未测量过（无缓存尺寸）。 */
  isFirstMeasure: boolean;
  /** 条目顶部相对视口顶部的 DOM 偏移（px，负值表示在视口之上）；取不到为 null。 */
  domTop: number | null;
  /** 变化前的尺寸（缓存或估算）。 */
  prevSize: number;
  /** 条目在虚拟布局中的起点（DOM 不可用时的回退依据）。 */
  itemStart: number;
  /** 库内记录的滚动偏移（DOM 不可用时的回退依据）。 */
  scrollOffset: number;
  /** 当前是否在向上翻动。 */
  scrollingBackward: boolean;
}

export function shouldCompensateItemResize(input: ScrollAdjustInput): boolean {
  if (input.isLast) return false;
  if (input.domTop !== null) {
    if (input.isFirstMeasure) return input.domTop < 0;
    return input.domTop + input.prevSize <= 0 && !input.scrollingBackward;
  }
  if (input.isFirstMeasure) return input.itemStart < input.scrollOffset;
  return input.itemStart + input.prevSize <= input.scrollOffset && !input.scrollingBackward;
}

/** 视口内无任何条目（空白）时的恢复定位方向。 */
export type BlankViewportRecovery =
  | { kind: 'none' }
  | { kind: 'end' }
  | { kind: 'index'; index: number };

/**
 * 判定视口是否落入空白区并给出恢复定位。
 *
 * 轮次结束时流式区并回虚拟列表、过程收缩，总高与条目位置在数帧内剧烈
 * 变化；若视口最终停在没有任何条目的区域（页面空白），按已渲染条目相对
 * 视口的位置恢复：全部在视口之上（滚过了内容末尾）或列表非空却没有任何
 * 已渲染条目 → 回到末尾；全部在视口之下 → 对齐到最靠上的已渲染条目。
 * 有条目与视口相交（含列表为空）时不处理。
 *
 * @param viewport 视口相对自身的可见区间（top/bottom，px）
 * @param items 已渲染条目的下标与相对视口顶部的区间
 * @param count 列表条目总数
 */
export function blankViewportRecovery(
  viewport: { top: number; bottom: number },
  items: { index: number; top: number; bottom: number }[],
  count: number,
): BlankViewportRecovery {
  if (count === 0) return { kind: 'none' };
  if (items.length === 0) return { kind: 'end' };
  if (items.some((item) => item.bottom > viewport.top && item.top < viewport.bottom)) {
    return { kind: 'none' };
  }
  if (items.every((item) => item.top >= viewport.bottom)) {
    const first = items.reduce((a, b) => (a.index <= b.index ? a : b));
    return { kind: 'index', index: first.index };
  }
  return { kind: 'end' };
}
