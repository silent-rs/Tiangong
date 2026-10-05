import type { MouseEvent } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { isBrowserHost } from '@/api/host';

/**
 * header 按下拖动窗口的统一入口。
 * - 仅左键触发；
 * - 窗口未聚焦时首次点击只激活窗口不拖动：激活期间主线程事件风暴会让拖动指令
 *   拿不到真实按下事件，tao 兜底用屏幕坐标合成按下事件导致窗口闪移到屏幕角落，
 *   并伴随 AppKit 告警 "Window move completed without beginning"；
 * - INPUT、BUTTON 与 data-no-drag 元素不触发拖动。
 */
export function startWindowDrag(e: MouseEvent) {
  if (e.button !== 0) return;
  if (isBrowserHost()) return;
  if (!document.hasFocus()) return;
  const target = e.target as HTMLElement;
  if (target.tagName === 'INPUT' || target.tagName === 'BUTTON') return;
  if (target.closest('[data-no-drag]')) return;
  void getCurrentWindow().startDragging();
}
