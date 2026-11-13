/**
 * 中枢模式入口状态与快捷键（设计稿 §4.1 / §16）。
 *
 * 关键约束：中枢模式仅在 Tauri 桌面窗口（desktop 宿主）可用——`request_user`
 * 等中枢依赖仅 Desktop 提供（§5.4），浏览器配置页与远程 H5 一律不提供入口。
 * 即便 store 状态被意外置位，非 desktop 宿主下 `centralActive` 仍为 false，
 * 布局不会切换（双保险）。
 *
 * 模式切换是纯前端视图分支：Session 数据、消息缓存、后台 turn 一律保留，
 * 退出即完整恢复普通模式（§4.1 退出无损、§16 兼容原则）。
 */
import { useCallback, useEffect } from 'react';
import { useStore } from '@/store/useStore';
import { isDesktopHost } from '@/api/host';

/** 中枢模式状态与入口动作。 */
export function useCentralMode() {
  const centralMode = useStore((s) => s.centralMode);
  const setCentralMode = useStore((s) => s.setCentralMode);
  const canUseCentral = isDesktopHost();
  /** 布局判定用：仅 desktop 宿主且模式开启时才真正切换视图。 */
  const centralActive = centralMode && canUseCentral;

  const enterCentral = useCallback(() => {
    if (isDesktopHost()) setCentralMode(true);
  }, [setCentralMode]);

  const exitCentral = useCallback(() => {
    setCentralMode(false);
  }, [setCentralMode]);

  const toggleCentral = useCallback(() => {
    if (isDesktopHost()) {
      setCentralMode(!useStore.getState().centralMode);
    }
  }, [setCentralMode]);

  return { centralMode, centralActive, canUseCentral, enterCentral, exitCentral, toggleCentral };
}

/**
 * 注册中枢模式快捷键 `Cmd/Ctrl + Shift + J`（设计稿 §4.1 候选）。
 *
 * 仅 desktop 宿主注册；只在应用根组件挂载一次，重复挂载由 useEffect
 * 的注册/清理语义自然去重。
 */
export function useCentralModeHotkey() {
  useEffect(() => {
    if (!isDesktopHost()) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.repeat) return;
      if (!(event.metaKey || event.ctrlKey) || !event.shiftKey) return;
      if (event.key.toLowerCase() !== 'j') return;
      event.preventDefault();
      const store = useStore.getState();
      store.setCentralMode(!store.centralMode);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, []);
}
