/**
 * 中枢结构化卡片条（R5 消息卡片，方案 §3.6/§6.2）：
 * - 授权卡片（事件驱动）：派发被 Workspace 授权拒绝时（宿主
 *   `central-approval-required`），渲染目录与任务指纹，用户真实操作签发
 *   票据（centralApproveWorkspace）——「授权」与「授权并记住」两种；
 *   已授权卡片提示助手可重派。模型不能自证同意（§5.6）。
 * - 决定卡片（状态驱动）：等待用户决策的工作（work status 的
 *   waiting_user），提示回中枢对话答复；状态变化自动消失。
 *
 * 数据源均为宿主事实（UI 是投影，§5.3）；挂载于中枢输入区上方。
 */
import { useCallback, useEffect, useState } from 'react';
import { api } from '@/api/tauri';
import { listen } from '@tauri-apps/api/event';
import { ShieldCheck, HelpCircle, Loader2, CheckCircle2 } from 'lucide-react';

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

/** 授权卡片：一次待授权事项（目录 + 任务指纹）。 */
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
        className="flex items-start gap-2.5 rounded-xl border border-emerald-500/30 bg-emerald-500/[0.06] p-3 text-xs"
      >
        <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-emerald-600 dark:text-emerald-400" />
        <div className="min-w-0">
          <p className="font-medium text-emerald-700 dark:text-emerald-300">已授权</p>
          <p className="mt-0.5 break-all leading-relaxed text-muted-foreground">
            {approval.cwd}｜任务「{approval.task}」——助手重新派发即可继续。
          </p>
        </div>
      </div>
    );
  }

  return (
    <div
      data-testid="central-approval-card"
      className="rounded-xl border border-amber-500/30 bg-amber-500/[0.05] p-3 text-xs"
    >
      <div className="flex items-start gap-2.5">
        <ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-amber-600 dark:text-amber-400" />
        <div className="min-w-0 flex-1">
          <p className="font-medium text-amber-700 dark:text-amber-300">需要你授权工作目录</p>
          <p className="mt-1 break-all leading-relaxed text-muted-foreground">
            助手要在以下目录执行后台工作，请确认任务与影响：
          </p>
          <p className="mt-1 break-all font-mono text-[11px] text-foreground/80">{approval.cwd}</p>
          <p className="mt-1 leading-relaxed text-muted-foreground">任务：{approval.task}</p>
          {error ? (
            <p className="mt-1 text-red-600 dark:text-red-400" role="alert">
              授权失败：{error}
            </p>
          ) : null}
          <div className="mt-2.5 flex flex-wrap gap-2">
            <button
              type="button"
              disabled={submitting}
              onClick={() => void approve(false)}
              className="inline-flex items-center gap-1.5 rounded-lg bg-primary px-3 py-1.5 font-medium text-primary-foreground transition-opacity disabled:opacity-60"
            >
              {submitting ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : null}
              授权本次
            </button>
            <button
              type="button"
              disabled={submitting}
              onClick={() => void approve(true)}
              className="rounded-lg border border-border/70 bg-muted/40 px-3 py-1.5 font-medium transition-colors hover:bg-muted disabled:opacity-60"
            >
              授权并记住此目录
            </button>
          </div>
          <p className="mt-1.5 text-[10px] leading-relaxed text-muted-foreground">
            授权仅对上述任务生效（一次性票据）；「记住」后该目录后续派发不再询问。
          </p>
        </div>
      </div>
    </div>
  );
}

/** 决定卡片：等待用户决策的工作（答复经中枢对话，§3.6）。 */
function DecisionCard({ work }: { work: WaitingWork }) {
  return (
    <div
      data-testid="central-decision-card"
      className="flex items-start gap-2.5 rounded-xl border border-amber-500/25 bg-amber-500/[0.04] p-3 text-xs"
    >
      <HelpCircle className="mt-0.5 h-4 w-4 shrink-0 text-amber-600 dark:text-amber-400" />
      <div className="min-w-0">
        <p className="font-medium text-amber-700 dark:text-amber-300">有工作在等你的决定</p>
        <p className="mt-0.5 truncate leading-relaxed text-muted-foreground" title={work.task}>
          {work.task}
        </p>
        <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground">
          在下方输入框直接答复即可（恢复的是同一个执行，上下文连续）。
        </p>
      </div>
    </div>
  );
}

/** 卡片条：授权（事件驱动）+ 等待决定（状态驱动）。 */
export function CentralCardsBar() {
  const [approvals, setApprovals] = useState<PendingApproval[]>([]);
  const [waitingWorks, setWaitingWorks] = useState<WaitingWork[]>([]);

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
          current.some(
            (item) => item.cwd === payload.cwd && item.task === payload.task,
          )
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

  const handleApproved = useCallback((workId: string) => {
    setApprovals((current) =>
      current.map((item) =>
        item.work_id === workId ? { ...item, status: 'approved' as const } : item,
      ),
    );
  }, []);

  if (approvals.length === 0 && waitingWorks.length === 0) return null;

  return (
    <div data-testid="central-cards-bar" className="space-y-2 border-t border-border/60 px-4 py-2">
      {approvals.map((approval) => (
        <ApprovalCard key={`${approval.work_id}-${approval.cwd}`} approval={approval} onApproved={handleApproved} />
      ))}
      {waitingWorks.map((work) => (
        <DecisionCard key={work.work_id} work={work} />
      ))}
    </div>
  );
}
