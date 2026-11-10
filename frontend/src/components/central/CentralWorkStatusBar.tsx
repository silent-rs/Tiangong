/**
 * 中枢「后台工作」状态条（设计稿 §4.4 轻量后台状态的最小形态）。
 *
 * 显示在输入区上方：执行中的任务数与待汇报条数；悬停可见明细
 * （任务、模型、会话、时间）。数据源为后端状态登记（命令
 * get_central_work_status），经 `central-work-updated` 事件驱动刷新，
 * 并在对话运行状态变化时兜底拉取（覆盖发送链路消费回报的场景）。
 */
import { useCallback, useEffect, useState } from 'react';
import { api } from '@/api/tauri';
import { listen } from '@/api/host';
import { useStore } from '@/store/useStore';

interface CentralWorkStatus {
  work_id: string;
  session_id: string;
  task: string;
  model: string | null;
  dispatched_at: string;
  state: 'running' | 'report_queued';
  reused_session: boolean;
}

export function CentralWorkStatusBar() {
  const [items, setItems] = useState<CentralWorkStatus[]>([]);
  const runStatus = useStore((s) => s.runStatus);
  const messagesLength = useStore((s) => s.messages.length);

  const refresh = useCallback(() => {
    api
      .getCentralWorkStatus()
      .then((data) => setItems((data ?? []) as CentralWorkStatus[]))
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    refresh();
    let disposed = false;
    let unsubscribe: (() => void) | undefined;
    void listen('central-work-updated', () => {
      if (!disposed) refresh();
    }).then((un) => {
      if (disposed) {
        un();
      } else {
        unsubscribe = un;
      }
    });
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, [refresh]);

  // 对话状态变化（发送消费回报 / 汇总轮结束）时兜底刷新
  useEffect(() => {
    refresh();
  }, [runStatus, messagesLength, refresh]);

  if (items.length === 0) return null;

  const running = items.filter((item) => item.state === 'running');
  const queued = items.filter((item) => item.state === 'report_queued');
  const summary = [
    running.length > 0 ? `${running.length} 个后台任务执行中` : null,
    queued.length > 0 ? `${queued.length} 条结果待汇报` : null,
  ]
    .filter(Boolean)
    .join(' · ');
  const detail = items
    .map((item) => {
      const state = item.state === 'running' ? '执行中' : '待汇报';
      const model = item.model ?? '默认模型';
      const reused = item.reused_session ? '（复用会话）' : '';
      return `[${state}] ${item.work_id}｜${model}${reused}｜${item.dispatched_at}\n${item.task}`;
    })
    .join('\n\n');

  return (
    <div
      className="border-b border-border/60 bg-muted/30 px-4 py-1.5 text-center text-xs text-muted-foreground"
      title={detail}
    >
      <span className="inline-flex items-center gap-1.5">
        <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-primary" />
        {summary}
      </span>
    </div>
  );
}
