/**
 * 中枢工作抽屉（R4，方案 §3.9 / §6.1 CentralWorkDrawer）。
 *
 * 从状态条的工作行打开：按需展示单个后台工作的详情与事件时间线，
 * 是复核入口——**不离开中枢模式、不改变中枢对话上下文**（§6.4：
 * 抽屉关闭只影响视图，不影响 Worker）；「打开原始工作会话」才切走
 * （显式动作）。
 *
 * 时间线数据源：宿主事件日志（唯一事实源，central_work_timeline）。
 */
import { useCallback, useEffect, useState } from 'react';
import { api } from '@/api/tauri';
import type { CentralWorkStatus } from '@/features/central/api';
import { ACTIVITY_LABELS, activityDetail, activityOf } from '@/features/central/lib/workActivity';
import {
  X,
  Loader2,
  CheckCircle2,
  XCircle,
  HelpCircle,
  PauseCircle,
  Clock,
  Ban,
  AlertTriangle,
  ExternalLink,
  History,
} from 'lucide-react';

export type CentralWorkDrawerItem = CentralWorkStatus;

interface TimelineEntry {
  at: string;
  kind: string;
  note: string;
}

const STATE_META: Record<string, { label: string; className: string }> = {
  running: { label: '执行中', className: 'text-primary' },
  report_queued: { label: '待汇报', className: 'text-amber-600 dark:text-amber-400' },
  completed: { label: '已完成', className: 'text-emerald-600 dark:text-emerald-400' },
  failed: { label: '失败', className: 'text-red-600 dark:text-red-400' },
  waiting_user: { label: '等待你的决定', className: 'text-amber-600 dark:text-amber-400' },
  paused: { label: '已暂停', className: 'text-muted-foreground' },
  cancelled: { label: '已取消', className: 'text-muted-foreground' },
  queued: { label: '排队中', className: 'text-muted-foreground' },
  interrupted: { label: '已中断', className: 'text-muted-foreground' },
};

function StateIcon({ state }: { state: string }) {
  const cls = 'h-4 w-4';
  switch (state) {
    case 'running':
      return <Loader2 className={`${cls} animate-spin`} />;
    case 'completed':
      return <CheckCircle2 className={cls} />;
    case 'failed':
      return <XCircle className={cls} />;
    case 'waiting_user':
      return <HelpCircle className={cls} />;
    case 'paused':
      return <PauseCircle className={cls} />;
    case 'queued':
      return <Clock className={cls} />;
    case 'cancelled':
      return <Ban className={cls} />;
    default:
      return <AlertTriangle className={cls} />;
  }
}

/** 事件类型 → 人读短名（事件日志的带点类型名）。 */
function kindLabel(kind: string): string {
  const map: Record<string, string> = {
    'work_item.created': '创建',
    'work_item.started': '开始/恢复',
    'work_item.progress': '进展',
    'work_item.phase_completed': '阶段完成',
    'work_item.waiting_user': '等待决定',
    'work_item.waiting_resource': '等待资源',
    'work_item.blocked': '阻塞/暂停',
    'work_item.failed': '失败',
    'work_item.completed': '完成',
    'work_item.cancelled': '取消',
    'model.handover.confirmed': '模型切换',
  };
  return map[kind] ?? kind;
}

export function CentralWorkDrawer({
  work,
  onClose,
  onOpenSession,
}: {
  work: CentralWorkDrawerItem;
  onClose: () => void;
  /** 打开原始工作会话（显式动作：退出中枢并切到该会话）。 */
  onOpenSession: (sessionId: string) => void;
}) {
  const [timeline, setTimeline] = useState<TimelineEntry[] | null>(null);
  const [timelineError, setTimelineError] = useState<string | null>(null);

  const loadTimeline = useCallback(() => {
    let cancelled = false;
    setTimeline(null);
    setTimelineError(null);
    api
      .centralWorkTimeline(work.work_id)
      .then((entries) => {
        if (!cancelled) setTimeline(entries);
      })
      .catch((error: unknown) => {
        if (!cancelled) setTimelineError(String(error));
      });
    return () => {
      cancelled = true;
    };
  }, [work.work_id]);

  useEffect(() => loadTimeline(), [loadTimeline]);

  // Esc 关闭（不改变模式与上下文，§6.4）
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [onClose]);

  const meta = STATE_META[work.state] ?? { label: work.state, className: 'text-muted-foreground' };
  const detail = activityDetail(work);

  return (
    <>
      {/* 视口级遮罩：点击关闭（抽屉不改变模式与上下文，§6.4） */}
      <div
        data-testid="central-work-drawer-overlay"
        className="fixed inset-0 z-40 bg-black/20 backdrop-blur-[1px]"
        onClick={onClose}
        aria-hidden="true"
      />
      <div
        data-testid="central-work-drawer"
        role="dialog"
        aria-label={`工作详情 ${work.work_id}`}
        className="fixed inset-y-0 right-0 z-50 flex w-full max-w-md flex-col border-l border-border/70 bg-background shadow-2xl shadow-black/20"
      >
      {/* 头部：状态 + 关闭 */}
      <div className="flex items-start justify-between gap-3 border-b border-border/60 p-4">
        <div className="min-w-0">
          <div className={`flex items-center gap-2 text-sm font-medium ${meta.className}`}>
            <StateIcon state={work.state} />
            {meta.label}
          </div>
          <p className="mt-1 truncate text-xs text-muted-foreground" title={work.task}>
            {work.task}
          </p>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label="关闭工作详情"
          className="rounded-lg p-1.5 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
        >
          <X className="h-4 w-4" />
        </button>
      </div>

      {/* 详情 */}
      <div className="space-y-2 border-b border-border/60 p-4 text-xs">
        <div className="flex justify-between gap-3">
          <span className="text-muted-foreground">工作</span>
          <span className="truncate font-mono" title={work.work_id}>
            {work.work_id}
          </span>
        </div>
        {work.execution_id ? (
          <div className="flex justify-between gap-3">
            <span className="text-muted-foreground">当前执行</span>
            <span className="truncate font-mono" title={work.execution_id}>
              {work.execution_id}
            </span>
          </div>
        ) : null}
        {work.state === 'running' ? (
          <div className="flex justify-between gap-3">
            <span className="text-muted-foreground">活动</span>
            <span className="truncate" data-testid="drawer-activity">
              {ACTIVITY_LABELS[activityOf(work)]}
              {detail ? ` · ${detail}` : ''}
            </span>
          </div>
        ) : null}
        <div className="flex justify-between gap-3">
          <span className="text-muted-foreground">模型</span>
          <span className="truncate">{work.model ?? '路由默认'}</span>
        </div>
        {work.cwd ? (
          <div className="flex justify-between gap-3">
            <span className="text-muted-foreground">工作目录</span>
            <span className="truncate font-mono" title={work.cwd}>
              {work.cwd}
            </span>
          </div>
        ) : null}
        <div className="flex justify-between gap-3">
          <span className="text-muted-foreground">派发于</span>
          <span>{work.dispatched_at}</span>
        </div>
      </div>

      {/* 等待决定提示（§3.6：答复经中枢对话，不在抽屉内决定） */}
      {work.state === 'waiting_user' ? (
        <div className="border-b border-border/60 bg-amber-500/[0.06] p-3 text-xs leading-relaxed text-amber-700 dark:text-amber-300">
          这个工作在等你的决定。回到中枢对话直接答复即可，恢复的是同一个执行（上下文连续）。
        </div>
      ) : null}

      {/* 事件时间线 */}
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        <div className="mb-3 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
          <History className="h-3.5 w-3.5" />
          事件时间线
        </div>
        {timelineError ? (
          <p className="text-xs text-red-600 dark:text-red-400" role="alert">
            时间线加载失败：{timelineError}
          </p>
        ) : timeline === null ? (
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            加载中…
          </div>
        ) : timeline.length === 0 ? (
          <p className="text-xs text-muted-foreground">暂无事件记录。</p>
        ) : (
          <ol className="space-y-2.5 border-l border-border/70 pl-4" data-testid="central-work-timeline">
            {timeline.map((entry, index) => (
              <li key={`${entry.at}-${index}`} className="relative text-xs">
                <span className="absolute -left-[21px] top-1 h-1.5 w-1.5 rounded-full bg-border" aria-hidden="true" />
                <div className="flex items-baseline justify-between gap-2">
                  <span className="font-medium">{kindLabel(entry.kind)}</span>
                  <span className="shrink-0 text-[10px] text-muted-foreground">{entry.at}</span>
                </div>
                {entry.note ? (
                  <p className="mt-0.5 leading-relaxed text-muted-foreground">{entry.note}</p>
                ) : null}
              </li>
            ))}
          </ol>
        )}
      </div>

      {/* 底部：打开原始会话（显式动作，复核入口） */}
      <div className="border-t border-border/60 p-3">
        <button
          type="button"
          onClick={() => onOpenSession(work.session_id)}
          className="flex w-full items-center justify-center gap-2 rounded-xl border border-border/70 bg-muted/40 px-3 py-2 text-xs font-medium transition-colors hover:bg-muted"
        >
          <ExternalLink className="h-3.5 w-3.5" />
          打开原始工作会话
        </button>
      </div>
      </div>
    </>
  );
}
