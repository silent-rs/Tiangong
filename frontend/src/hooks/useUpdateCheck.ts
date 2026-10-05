import { useEffect, useRef } from 'react';
import { useStore } from '@/store/useStore';
import { isBrowserHost } from '@/api/host';

const CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000; // 24h
const INITIAL_DELAY_MS = 5000; // 5s

export function useUpdateCheck() {
  const setUpdateAvailable = useStore((s) => s.setUpdateAvailable);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    // 远程 H5 没有应用更新能力。
    if (isBrowserHost()) return;
    let cancelled = false;

    const doCheck = async () => {
      if (cancelled) return;
      try {
        const { check } = await import('@tauri-apps/plugin-updater');
        const update = await check({ timeout: 30000 });
        if (cancelled) return;
        if (update) {
          setUpdateAvailable({
            version: update.version,
            body: update.body ?? undefined,
            date: update.date ?? undefined,
          });
        }
        await update?.close().catch(() => {});
      } catch {
        // 静默失败，不打扰用户
      }
    };

    timerRef.current = setTimeout(() => {
      doCheck();
      intervalRef.current = setInterval(doCheck, CHECK_INTERVAL_MS);
    }, INITIAL_DELAY_MS);

    return () => {
      cancelled = true;
      if (timerRef.current) clearTimeout(timerRef.current);
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [setUpdateAvailable]);
}
