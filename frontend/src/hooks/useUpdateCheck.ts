import { useEffect, useRef } from 'react';
import { useStore } from '@/store/useStore';
import { isBrowserHost } from '@/api/host';
import { api } from '@/api/tauri';
import { useToast } from '@/components/Toast';

const CHECK_INTERVAL_MS = 10 * 60 * 1000; // 10min
const INITIAL_DELAY_MS = 5000; // 5s

export function useUpdateCheck() {
  const setUpdateAvailable = useStore((s) => s.setUpdateAvailable);
  const { showInfo, showSuccess, showError } = useToast();
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    // 远程 H5 没有应用更新能力。
    if (isBrowserHost()) return;
    let cancelled = false;

    const doCheck = async (allowAutoInstall: boolean) => {
      if (cancelled) return;
      try {
        const { check } = await import('@tauri-apps/plugin-updater');
        const update = await check({ timeout: 30000 });
        if (cancelled) {
          await update?.close().catch(() => {});
          return;
        }
        if (!update) return;
        setUpdateAvailable({
          version: update.version,
          body: update.body ?? undefined,
          date: update.date ?? undefined,
        });
        if (!allowAutoInstall) {
          await update.close().catch(() => {});
          return;
        }
        // 仅启动首次检查走自动安装；读取失败按默认开启处理。
        const enabled = useStore.getState().autoUpdate ?? (await api.getAutoUpdate().catch(() => true));
        if (!enabled) {
          await update.close().catch(() => {});
          return;
        }
        showInfo('发现新版本', `正在自动更新到 v${update.version}…`);
        try {
          await update.downloadAndInstall();
          showSuccess('更新已安装', '应用即将自动重启');
          const { relaunch } = await import('@tauri-apps/plugin-process');
          await relaunch();
        } catch (error) {
          console.error('自动更新安装失败:', error);
          showError('自动更新失败', '可到设置页「关于与更新」手动安装');
          await update.close().catch(() => {});
        }
      } catch {
        // 静默失败，不打扰用户
      }
    };

    timerRef.current = setTimeout(() => {
      doCheck(true);
      intervalRef.current = setInterval(() => doCheck(false), CHECK_INTERVAL_MS);
    }, INITIAL_DELAY_MS);

    return () => {
      cancelled = true;
      if (timerRef.current) clearTimeout(timerRef.current);
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [setUpdateAvailable, showInfo, showSuccess, showError]);
}
