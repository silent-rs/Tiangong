/**
 * 中枢「后台工作」浮动状态（设计稿 §4.4 轻量后台状态）。
 *
 * 右上角玻璃拟态徽章；hover 展开分组详情面板（按「执行中 / 待汇报」
 * 多行分类展示，无点击弹窗），鼠标离开后短暂延迟收起。数据源为后端
 * 状态登记（get_central_work_status），经 `central-work-updated` 事件
 * 驱动刷新，对话状态变化时兜底拉取。
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '@/api/tauri';
import { listen } from '@/api/host';
import { useStore } from '@/store/useStore';
import { Loader2, Inbox, History, ChevronRight, AlertTriangle, CheckCircle2 } from 'lucide-react';
import { restoreFromCentralConversation } from '@/lib/centralConversation';

interface CentralWorkStatus {
  work_id: string;
  session_id: string;
  task: string;
  model: string | null;
  dispatched_at: string;
  state: 'running' | 'report_queued' | 'interrupted' | 'completed';
  reused_session: boolean;
}

const PANEL_CLOSE_DELAY_MS = 250;

function WorkItemRow({
  item,
  onOpen,
}: {
  item: CentralWorkStatus;
  onOpen?: (sessionId: string) => void;
}) {
  const running = item.state === 'running';
  const interrupted = item.state === 'interrupted';
  const completedState = item.state === 'completed';
  return (
    <div
      onClick={() => onOpen?.(item.session_id)}
      role={onOpen ? 'button' : undefined}
      tabIndex={onOpen ? 0 : undefined}
      onKeyDown={(event) => {
        if (onOpen && (event.key === 'Enter' || event.key === ' ')) {
          event.preventDefault();
          onOpen(item.session_id);
        }
      }}
      className={`group/row cursor-pointer rounded-xl border p-3 text-left transition-colors ${
        running
          ? 'border-primary/25 bg-primary/[0.04] hover:border-primary/40'
          : interrupted
            ? 'border-border/60 bg-muted/30 opacity-80'
            : 'border-amber-500/25 bg-amber-500/[0.05] hover:border-amber-500/45'
      }`}
    >
      <div className="flex items-center justify-between gap-2">
        <span
          className={`inline-flex items-center gap-1.5 text-xs font-medium ${
            running
              ? 'text-primary'
              : interrupted
                ? 'text-muted-foreground'
                : completedState
                  ? 'text-emerald-600 dark:text-emerald-400'
                  : 'text-amber-600 dark:text-amber-400'
          }`}
        >
          {running ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              执行中
            </>
          ) : interrupted ? (
            <>
              <AlertTriangle className="h-3.5 w-3.5" />
              已中断
            </>
          ) : completedState ? (
            <>
              <CheckCircle2 className="h-3.5 w-3.5" />
              已完成
            </>
          ) : (
            <>
              <Inbox className="h-3.5 w-3.5" />
              待汇报
            </>
          )}
        </span>
        <span className="text-[10px] tabular-nums text-muted-foreground/80">
          {item.dispatched_at}
        </span>
      </div>
      <p className="mt-1.5 line-clamp-2 text-[13px] leading-snug text-foreground/90">
        {item.task}
      </p>
      <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px] text-muted-foreground">
        <span className="rounded bg-muted px-1.5 py-0.5 font-medium">
          {item.model ?? '默认模型'}
        </span>
        {item.reused_session ? (
          <span className="rounded bg-emerald-500/10 px-1.5 py-0.5 text-emerald-600 dark:text-emerald-400">
            复用会话
          </span>
        ) : null}
        <span className="font-mono text-muted-foreground/70">
          {item.session_id.slice(0, 8)}…/{item.work_id.slice(5, 13)}
        </span>
      </div>
    </div>
  );
}

export function CentralWorkStatusBar() {
  const [items, setItems] = useState<CentralWorkStatus[]>([]);
  const [open, setOpen] = useState(false);
  const closeTimer = useRef<number | null>(null);
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

  useEffect(() => {
    refresh();
  }, [runStatus, messagesLength, refresh]);

  // 无任务时收起面板（否则下次任务出现时面板带着旧 open 状态自动展开）
  useEffect(() => {
    if (items.length === 0) setOpen(false);
  }, [items.length]);

  const scheduleClose = useCallback(() => {
    if (closeTimer.current !== null) window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(() => setOpen(false), PANEL_CLOSE_DELAY_MS);
  }, []);
  const cancelClose = useCallback(() => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
  }, []);

  if (items.length === 0) return null;

  const running = items.filter((item) => item.state === 'running');
  const queued = items.filter((item) => item.state === 'report_queued');
  const interrupted = items.filter((item) => item.state === 'interrupted');
  const completed = items.filter((item) => item.state === 'completed');

  // 点击工作项：退出中枢并在普通模式打开该工作会话（§4.4 可复核）
  const openInNormalMode = useCallback(
    async (sessionId: string) => {
      const store = useStore.getState();
      if (store.centralMode) {
        store.setCentralMode(false);
        // 等 restore 完成再切目标会话（显式调用幂等，保证顺序）
        await restoreFromCentralConversation().catch(() => undefined);
      }
      await useStore.getState().switchSession(sessionId).catch(() => undefined);
    },
    [],
  );

  return (
    <div
      className="absolute right-4 top-3 z-20"
      onMouseEnter={() => {
        cancelClose();
        setOpen(true);
      }}
      onMouseLeave={scheduleClose}
    >
      <button
        type="button"
        aria-label={`后台工作：${running.length} 个执行中，${queued.length} 条待汇报`}
        aria-expanded={open}
        // hover 之外的键盘/点击可达路径（无障碍 + 触屏）
        onClick={() => setOpen((value) => !value)}
        onFocus={() => {
          cancelClose();
          setOpen(true);
        }}
        onBlur={scheduleClose}
        className={`inline-flex items-center gap-2.5 rounded-full border px-3.5 py-1.5 text-xs shadow-lg shadow-black/5 backdrop-blur-md transition-all ${
          running.length > 0
            ? 'border-primary/30 bg-primary/[0.08] text-foreground hover:border-primary/50'
            : 'border-amber-500/30 bg-amber-500/[0.08] text-foreground hover:border-amber-500/50'
        }`}
      >
        <span className="relative flex h-2 w-2">
          {running.length > 0 ? (
            <>
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary opacity-50" />
              <span className="relative inline-flex h-2 w-2 rounded-full bg-primary" />
            </>
          ) : (
            <span className="relative inline-flex h-2 w-2 rounded-full bg-amber-500" />
          )}
        </span>
        <span className="tabular-nums font-medium">
          {running.length > 0
          ? `${running.length} 个后台任务`
          : interrupted.length > 0
            ? `${interrupted.length} 个任务已中断`
            : '后台任务已结束'}
        </span>
        {queued.length > 0 ? (
          <span className="rounded-full bg-amber-500/15 px-1.5 py-px font-medium tabular-nums text-amber-600 dark:text-amber-400">
            {queued.length} 待汇报
          </span>
        ) : null}
        <ChevronRight
          className={`h-3.5 w-3.5 text-muted-foreground transition-transform ${open ? 'rotate-90' : ''}`}
        />
      </button>

      {open ? (
        <div className="absolute right-0 top-[calc(100%+8px)] w-[360px] rounded-2xl border border-border/70 bg-background/95 p-3 shadow-xl shadow-black/10 backdrop-blur-xl">
          {running.length > 0 ? (
            <section className="mb-3">
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <Loader2 className="h-3.5 w-3.5 animate-spin text-primary" />
                  执行中
                </h3>
                <span className="rounded-full bg-primary/10 px-2 py-0.5 text-[10px] font-medium tabular-nums text-primary">
                  {running.length}
                </span>
              </header>
              <div className="space-y-2">
                {running.map((item) => (
                  <WorkItemRow key={item.work_id} item={item} onOpen={openInNormalMode} />
                ))}
              </div>
            </section>
          ) : null}

          {queued.length > 0 ? (
            <section>
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <History className="h-3.5 w-3.5 text-amber-500" />
                  待汇报
                </h3>
                <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-[10px] font-medium tabular-nums text-amber-600 dark:text-amber-400">
                  {queued.length}
                </span>
              </header>
              <div className="space-y-2">
                {queued.map((item) => (
                  <WorkItemRow key={item.work_id} item={item} onOpen={openInNormalMode} />
                ))}
              </div>
            </section>
          ) : null}

          {interrupted.length > 0 ? (
            <section className="mt-3">
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <AlertTriangle className="h-3.5 w-3.5 text-muted-foreground" />
                  已中断（应用重启）
                </h3>
                <span className="rounded-full bg-muted px-2 py-0.5 text-[10px] font-medium tabular-nums text-muted-foreground">
                  {interrupted.length}
                </span>
              </header>
              <p className="px-1 pb-2 text-[11px] leading-relaxed text-muted-foreground">
                进程重启导致执行中断，结果未产出；可让中枢重派（复用原会话延续上下文）。
              </p>
              <div className="space-y-2 opacity-75">
                {interrupted.map((item) => (
                  <WorkItemRow
                    key={item.work_id}
                    item={{ ...item, state: 'interrupted' }}
                    onOpen={openInNormalMode}
                  />
                ))}
              </div>
            </section>
          ) : null}

          {completed.length > 0 ? (
            <section className="mt-3">
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <CheckCircle2 className="h-3.5 w-3.5 text-emerald-500" />
                  已完成
                </h3>
                <span className="rounded-full bg-emerald-500/10 px-2 py-0.5 text-[10px] font-medium tabular-nums text-emerald-600 dark:text-emerald-400">
                  {completed.length}
                </span>
              </header>
              <div className="space-y-2 opacity-80">
                {completed.map((item) => (
                  <WorkItemRow key={item.work_id} item={item} onOpen={openInNormalMode} />
                ))}
              </div>
            </section>
          ) : null}

          {running.length === 0 && queued.length === 0 && interrupted.length === 0 && completed.length === 0 ? (
            <p className="px-1 py-2 text-xs text-muted-foreground">暂无后台工作</p>
          ) : null}
          <p className="mt-2 px-1 text-[10px] text-muted-foreground/70">点击工作项可在普通模式打开对应会话</p>
        </div>
      ) : null}
    </div>
  );
}
