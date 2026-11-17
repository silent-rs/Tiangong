/**
 * 当前任务常驻横幅（用户实测结症 2026-11-16：worker 正在处理的任务
 * 没有清晰展示——徽章数字 + hover 浮层太隐蔽）。存在活跃工作
 * （执行中/等你决定/排队/暂停）时在视图顶部常驻显示最新一条的任务
 * 摘要：状态 + 任务文本（一行截断，title 全文）+ 模型；点击展开
 * 后台工作浮层语义由徽章承担，本横幅点击打开对应工作抽屉。
 * 一 worker 一任务（同会话互斥排队，2026-11-16 裁决）——横幅即
 * 「当前正在做什么」的即时答案。
 */
import { useCallback, useEffect, useState } from 'react';
import { Loader2, HelpCircle, Clock, PauseCircle } from 'lucide-react';
import { api } from '@/api/tauri';
import { listen } from '@/api/host';

interface ActiveTask {
  work_id: string;
  session_id: string;
  task: string;
  model: string | null;
  state: string;
}

const ACTIVE_STATES = new Set(['running', 'waiting_user', 'queued', 'paused']);

export function CentralActiveTaskBanner({ onOpen }: { onOpen?: (task: ActiveTask) => void }) {
  const [task, setTask] = useState<ActiveTask | null>(null);

  const refresh = useCallback(() => {
    void api
      .getCentralWorkStatus()
      .then((items) => {
        const active = [...items]
          .filter((item) => ACTIVE_STATES.has(item.state))
          .sort((a, b) => b.dispatched_at.localeCompare(a.dispatched_at));
        setTask(active[0] ?? null);
      })
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    refresh();
    let disposed = false;
    let unsubscribe: (() => void) | undefined;
    void listen('central-work-updated', () => refresh()).then((un) => {
      if (disposed) un();
      else unsubscribe = un;
    });
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, [refresh]);

  if (!task) return null;
  const isRunning = task.state === 'running';
  const isWaiting = task.state === 'waiting_user';
  return (
    <button
      type="button"
      data-testid="central-active-task-banner"
      onClick={() => onOpen?.(task)}
      title={task.task}
      className="flex w-full items-center gap-2 border-b border-border/50 bg-muted/[0.35] px-4 py-1.5 text-left text-xs text-muted-foreground transition-colors hover:bg-muted/60"
    >
      {isRunning ? (
        <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin text-primary" aria-hidden="true" />
      ) : isWaiting ? (
        <HelpCircle className="h-3.5 w-3.5 shrink-0 text-amber-500" aria-hidden="true" />
      ) : task.state === 'paused' ? (
        <PauseCircle className="h-3.5 w-3.5 shrink-0 text-muted-foreground" aria-hidden="true" />
      ) : (
        <Clock className="h-3.5 w-3.5 shrink-0 text-muted-foreground" aria-hidden="true" />
      )}
      <span className="shrink-0 font-medium">
        {isRunning ? '正在执行' : isWaiting ? '等你决定' : task.state === 'paused' ? '已暂停' : '排队中'}
      </span>
      <span className="min-w-0 flex-1 truncate text-foreground/85">{task.task}</span>
      <span className="shrink-0 rounded bg-muted px-1.5 py-0.5 text-[10px]">
        {task.model ?? '默认模型'}
      </span>
    </button>
  );
}
