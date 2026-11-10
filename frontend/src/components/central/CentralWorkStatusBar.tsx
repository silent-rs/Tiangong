/**
 * 中枢「后台工作」浮动状态（设计稿 §4.4 轻量后台状态）。
 *
 * 右上角浮动徽章：执行中任务数 + 待汇报条数；点击弹出详情面板
 * （Dialog），列出每个后台工作的状态、任务、模型、会话与时间。
 * 数据源为后端状态登记（get_central_work_status），经
 * `central-work-updated` 事件驱动刷新，对话状态变化时兜底拉取。
 */
import { useCallback, useEffect, useState } from 'react';
import { api } from '@/api/tauri';
import { listen } from '@/api/host';
import { useStore } from '@/store/useStore';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Loader2, ScrollText } from 'lucide-react';

interface CentralWorkStatus {
  work_id: string;
  session_id: string;
  task: string;
  model: string | null;
  dispatched_at: string;
  state: 'running' | 'report_queued';
  reused_session: boolean;
}

function StateBadge({ state }: { state: CentralWorkStatus['state'] }) {
  if (state === 'running') {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-primary/10 px-2 py-0.5 text-xs text-primary">
        <Loader2 className="h-3 w-3 animate-spin" />
        执行中
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-1 rounded-full bg-amber-500/10 px-2 py-0.5 text-xs text-amber-600 dark:text-amber-400">
      <ScrollText className="h-3 w-3" />
      待汇报
    </span>
  );
}

export function CentralWorkStatusBar() {
  const [items, setItems] = useState<CentralWorkStatus[]>([]);
  const [open, setOpen] = useState(false);
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

  const running = items.filter((item) => item.state === 'running').length;
  const queued = items.filter((item) => item.state === 'report_queued').length;

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        aria-label={`后台工作：${running} 个执行中${queued > 0 ? `，${queued} 条待汇报` : ''}`}
        className="absolute right-4 top-3 z-20 inline-flex items-center gap-2 rounded-full border border-border/70 bg-background/90 px-3 py-1.5 text-xs text-muted-foreground shadow-sm backdrop-blur transition-colors hover:border-ring hover:text-foreground"
      >
        <span className="relative flex h-2 w-2">
          {running > 0 ? (
            <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary opacity-60" />
          ) : null}
          <span
            className={`relative inline-flex h-2 w-2 rounded-full ${running > 0 ? 'bg-primary' : 'bg-amber-500'}`}
          />
        </span>
        {running > 0 ? `${running} 个后台任务执行中` : '后台任务已结束'}
        {queued > 0 ? (
          <span className="rounded-full bg-amber-500/15 px-1.5 py-0.5 text-amber-600 dark:text-amber-400">
            {queued} 待汇报
          </span>
        ) : null}
      </button>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>后台工作</DialogTitle>
          </DialogHeader>
          <div className="max-h-[60vh] space-y-3 overflow-y-auto pr-1">
            {items.map((item) => (
              <div key={item.work_id} className="rounded-lg border border-border/60 p-3">
                <div className="flex items-center justify-between gap-2">
                  <StateBadge state={item.state} />
                  <span className="font-mono text-[11px] text-muted-foreground">
                    {item.dispatched_at}
                  </span>
                </div>
                <p className="mt-2 line-clamp-2 text-sm text-foreground">{item.task}</p>
                <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
                  <span className="font-mono">{item.work_id}</span>
                  <span>模型 {item.model ?? '默认'}</span>
                  {item.reused_session ? <span>复用会话</span> : null}
                  <span className="font-mono">{item.session_id.slice(0, 10)}…</span>
                </div>
              </div>
            ))}
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}
