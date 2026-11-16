/**
 * 中枢结构化卡片条（R5 消息卡片，方案 §3.6/§6.2）：
 * - 授权卡片（事件驱动）：派发被 Workspace 授权拒绝时（宿主
 *   `central-approval-required`），渲染紧凑的行内授权条，用户真实操作
 *   签发票据（centralApproveWorkspace）；授权完成后宿主会通知中枢自动
 *   重派，卡片短暂显示「已授权」后自动收起。
 * - 决定卡片（状态驱动）：等待用户决策的工作（work status 的
 *   waiting_user），提示回中枢对话答复；状态变化自动消失。
 *
 * 数据源均为宿主事实（UI 是投影，§5.3）；挂载于中枢输入区上方。
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '@/api/tauri';
import { listen } from '@tauri-apps/api/event';
import { ShieldQuestion, MessageCircleQuestion, Check } from 'lucide-react';

interface ApprovalRequiredEvent {
  cwd: string;
  task: string;
  work_id: string;
}

interface PendingApproval extends ApprovalRequiredEvent {
  status: 'pending' | 'approved';
}

interface WaitingWork {
  work_id: string;
  session_id: string;
  task: string;
  state: string;
}

/** 任务摘要（一行内展示）。 */
function taskPreview(task: string, max = 48): string {
  const chars = Array.from(task);
  return chars.length > max ? `${chars.slice(0, max).join('')}…` : task;
}

/** 授权卡片：紧凑行内条——图标 + 目录/任务 + 右侧动作。 */
function ApprovalCard({
  approval,
  onApproved,
}: {
  approval: PendingApproval;
  onApproved: (workId: string) => void;
}) {
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const approve = useCallback(
    async (persist: boolean) => {
      setSubmitting(true);
      setError(null);
      try {
        await api.centralApproveWorkspace(approval.cwd, approval.task, persist);
        onApproved(approval.work_id);
      } catch (cause) {
        setError(String(cause));
      } finally {
        setSubmitting(false);
      }
    },
    [approval.cwd, approval.task, approval.work_id, onApproved],
  );

  if (approval.status === 'approved') {
    return (
      <div
        data-testid="central-approval-card-approved"
        className="flex items-center gap-2 rounded-lg border border-emerald-500/25 bg-emerald-500/[0.05] px-3 py-1.5 text-xs text-emerald-700 dark:text-emerald-300"
      >
        <Check className="h-3.5 w-3.5 shrink-0" />
        <span className="min-w-0 truncate">已授权 {approval.cwd}，助手正在重新派发…</span>
      </div>
    );
  }

  return (
    <div
      data-testid="central-approval-card"
      className="rounded-lg border border-amber-500/25 bg-amber-500/[0.04] px-3 py-2 text-xs"
    >
      <div className="flex items-center gap-2.5">
        <ShieldQuestion className="h-4 w-4 shrink-0 text-amber-600 dark:text-amber-400" />
        <div className="min-w-0 flex-1">
          <span className="font-medium text-amber-700 dark:text-amber-300">需要授权</span>
          <span className="ml-2 text-muted-foreground" title={approval.task}>
            {taskPreview(approval.task)}
          </span>
          <p className="truncate font-mono text-[10px] leading-4 text-muted-foreground" title={approval.cwd}>
            {approval.cwd}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          <button
            type="button"
            disabled={submitting}
            onClick={() => void approve(false)}
            className="rounded-md bg-primary px-2.5 py-1 text-[11px] font-medium text-primary-foreground transition-opacity disabled:opacity-60"
          >
            授权
          </button>
          <button
            type="button"
            disabled={submitting}
            onClick={() => void approve(true)}
            title="授权并记住此目录（后续派发不再询问）"
            className="rounded-md border border-border/70 bg-muted/30 px-2.5 py-1 text-[11px] font-medium transition-colors hover:bg-muted disabled:opacity-60"
          >
            记住
          </button>
        </div>
      </div>
      {submitting ? (
        <p className="mt-1 pl-6 text-[10px] text-muted-foreground">正在签发授权…</p>
      ) : null}
      {error ? (
        <p className="mt-1 pl-6 text-[10px] text-red-600 dark:text-red-400" role="alert">
          授权失败：{error}
        </p>
      ) : null}
    </div>
  );
}

/** 决定卡片：等待用户决策的工作（答复经中枢对话，§3.6）。 */
function DecisionCard({ work }: { work: WaitingWork }) {
  return (
    <div
      data-testid="central-decision-card"
      className="flex items-center gap-2.5 rounded-lg border border-primary/20 bg-primary/[0.04] px-3 py-2 text-xs"
    >
      <MessageCircleQuestion className="h-4 w-4 shrink-0 text-primary" />
      <span className="min-w-0 flex-1">
        <span className="font-medium">在等你的决定</span>
        <span className="ml-2 text-muted-foreground" title={work.task}>
          {taskPreview(work.task)}
        </span>
      </span>
      <span className="shrink-0 text-[10px] text-muted-foreground">直接在下方答复即可</span>
    </div>
  );
}

/** 卡片条：授权（事件驱动）+ 等待决定（状态驱动）。 */
export function CentralCardsBar() {
  const [approvals, setApprovals] = useState<PendingApproval[]>([]);
  const [waitingWorks, setWaitingWorks] = useState<WaitingWork[]>([]);
  const dismissTimers = useRef(new Map<string, number>());

  // 授权请求：宿主事件驱动（拒绝时到达）
  useEffect(() => {
    let disposed = false;
    let unsubscribe: (() => void) | undefined;
    void listen<ApprovalRequiredEvent>('central-approval-required', (event) => {
      const payload = event.payload;
      if (!payload?.cwd || !payload.task) return;
      setApprovals((current) => {
        // 同目录同任务去重（模型重试拒绝只保留一张卡）
        if (
          current.some((item) => item.cwd === payload.cwd && item.task === payload.task)
        ) {
          return current;
        }
        return [...current, { ...payload, status: 'pending' as const }];
      });
    }).then((un) => {
      if (disposed) un();
      else unsubscribe = un;
    });
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, []);

  // 等待决定：work status 状态驱动（答复后状态变化自动消失）
  const refreshWaiting = useCallback(() => {
    void api
      .getCentralWorkStatus()
      .then((items) => {
        setWaitingWorks(
          items
            .filter((item) => item.state === 'waiting_user')
            .map((item) => ({
              work_id: item.work_id,
              session_id: item.session_id,
              task: item.task,
              state: item.state,
            })),
        );
      })
      .catch(() => undefined);
  }, []);
  useEffect(() => {
    refreshWaiting();
    let disposed = false;
    let unsubscribe: (() => void) | undefined;
    void listen('central-work-updated', () => refreshWaiting()).then((un) => {
      if (disposed) un();
      else unsubscribe = un;
    });
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, [refreshWaiting]);

  // 已授权卡片短暂展示「正在重新派发」后自动收起（用户实测：长期
  // 占据页面；授权完成的事实已由宿主注入中枢，卡片无需常驻）
  useEffect(() => {
    for (const approval of approvals) {
      const key = `${approval.cwd}::${approval.task}`;
      if (approval.status !== 'approved' || dismissTimers.current.has(key)) {
        continue;
      }
      const timer = window.setTimeout(() => {
        dismissTimers.current.delete(key);
        setApprovals((current) =>
          current.filter(
            (item) => !(item.cwd === approval.cwd && item.task === approval.task),
          ),
        );
      }, 3500);
      dismissTimers.current.set(key, timer);
    }
  }, [approvals]);
  useEffect(() => {
    const timers = dismissTimers.current;
    return () => {
      timers.forEach((timer) => window.clearTimeout(timer));
      timers.clear();
    };
  }, []);

  // Z7 修复：按（目录 + 任务指纹）定位卡片——request_workspace_approval
  // 发起的卡片 work_id 为空串，按 id 匹配会同时标记/消失同批全部空 id 卡
  const handleApproved = useCallback((workId: string) => {
    setApprovals((current) => {
      const target = current.find((item) => item.work_id === workId);
      if (!target) return current;
      return current.map((item) =>
        item.cwd === target.cwd && item.task === target.task
          ? { ...item, status: 'approved' as const }
          : item,
      );
    });
  }, []);

  if (approvals.length === 0 && waitingWorks.length === 0) return null;

  return (
    <div data-testid="central-cards-bar" className="space-y-1.5 border-t border-border/60 px-4 py-2">
      {approvals.map((approval) => (
        <ApprovalCard
          key={`${approval.work_id}-${approval.cwd}`}
          approval={approval}
          onApproved={handleApproved}
        />
      ))}
      {waitingWorks.map((work) => (
        <DecisionCard key={work.work_id} work={work} />
      ))}
    </div>
  );
}
