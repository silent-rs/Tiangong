import { useEffect, useState } from 'react';
import { useStore } from '@/store/useStore';
import { liveTpsTracker, type LiveTpsSnapshot } from '@/utils/tps';

const EMPTY_SNAPSHOT: LiveTpsSnapshot = { active: false, tps: 0, elapsedMs: null };

/** 实时 TPS 采样 hook：流式期间每 600ms 读取窗口快照，结束归零。
 *  `streaming` 为 false 时直接返回空快照，不启动定时器。 */
export function useLiveTps(sessionId: string | null, streaming: boolean): LiveTpsSnapshot {
  const [snapshot, setSnapshot] = useState<LiveTpsSnapshot>(EMPTY_SNAPSHOT);
  useEffect(() => {
    if (!streaming || !sessionId) {
      setSnapshot(EMPTY_SNAPSHOT);
      return;
    }
    const update = () => {
      const next = liveTpsTracker.snapshot(sessionId);
      // 快照是纯派生值：字段相同时保留旧引用，避免空闲期每 600ms 无谓重渲。
      setSnapshot((prev) => (
        prev.active === next.active && prev.tps === next.tps && prev.elapsedMs === next.elapsedMs
          ? prev
          : next
      ));
    };
    update();
    const timer = window.setInterval(update, 600);
    return () => window.clearInterval(timer);
  }, [sessionId, streaming]);
  return snapshot;
}

/** 当前会话 ID（流式视图订阅口径，与 store.applyStreamEvents 一致）。 */
export function useLiveSessionId(): string | null {
  return useStore((s) => s.activeSessionId || s.newConversationId);
}
