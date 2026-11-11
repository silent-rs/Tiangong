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
import { Loader2, Inbox, History, ChevronRight, ChevronDown, AlertTriangle, CheckCircle2, XCircle, HelpCircle, PauseCircle, Clock, Ban } from 'lucide-react';
import { restoreFromCentralConversation } from '@/lib/central/centralConversation';

interface CentralWorkStatus {
  work_id: string;
  session_id: string;
  task: string;
  model: string | null;
  dispatched_at: string;
  state: 'running' | 'report_queued' | 'interrupted' | 'completed' | 'failed' | 'waiting_user' | 'paused' | 'cancelled' | 'queued';
  reused_session: boolean;
  last_activity?: string | null;
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
  const failedState = item.state === 'failed';
  const waitingUserState = item.state === 'waiting_user';
  const pausedState = item.state === 'paused';
  const cancelledState = item.state === 'cancelled';
  const queuedState = item.state === 'queued';
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
            : completedState
              ? 'border-emerald-500/25 bg-emerald-500/[0.04] hover:border-emerald-500/40'
              : failedState
                ? 'border-red-500/30 bg-red-500/[0.05] hover:border-red-500/50'
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
                  : failedState
                    ? 'text-red-600 dark:text-red-400'
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
          ) : failedState ? (
            <>
              <XCircle className="h-3.5 w-3.5" />
              失败
            </>
          ) : waitingUserState ? (
            <>
              <HelpCircle className="h-3.5 w-3.5" />
              等用户
            </>
          ) : pausedState ? (
            <>
              <PauseCircle className="h-3.5 w-3.5" />
              已暂停
            </>
          ) : cancelledState ? (
            <>
              <Ban className="h-3.5 w-3.5" />
              已取消
            </>
          ) : queuedState ? (
            <>
              <Clock className="h-3.5 w-3.5" />
              排队
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
      {running && item.last_activity ? (
        <p className="mt-1 truncate text-[11px] text-muted-foreground/80">
          {item.last_activity}
        </p>
      ) : null}
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
  // M6 状态分离：hover 浮层（轻量预览，移出即收）与 pinned 抽屉
  // （点击固定的工作视图，ESC/关闭按钮收起）互不干扰——固定后 hover
  // 移出不再误关面板。
  const [hoverOpen, setHoverOpen] = useState(false);
  const [pinned, setPinned] = useState(false);
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

  // 无任务时收起浮层与抽屉（否则下次任务出现时带着旧展开状态）
  useEffect(() => {
    if (items.length === 0) {
      setHoverOpen(false);
      setPinned(false);
    }
  }, [items.length]);

  // 抽屉打开时 ESC 收起（键盘可达）
  useEffect(() => {
    if (!pinned) return undefined;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setPinned(false);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [pinned]);

  const scheduleClose = useCallback(() => {
    if (closeTimer.current !== null) window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(() => setHoverOpen(false), PANEL_CLOSE_DELAY_MS);
  }, []);
  const cancelClose = useCallback(() => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
  }, []);

  // 点击工作项：退出中枢并在普通模式打开该工作会话（§4.4 可复核）。
  // 必须位于早退之前——否则空列表首渲染与有数据渲染的 hooks 数量不一致
  //（React 规则：所有 hooks 需无条件按序调用）。
  const openInNormalMode = useCallback(
    async (sessionId: string) => {
      const store = useStore.getState();
      if (store.centralMode) {
        store.setCentralMode(false);
        // 等 restore 完成再切目标会话（显式调用幂等，保证顺序）；
        // 失败记录控制台（review：静默吞错掩盖跳转问题）
        await restoreFromCentralConversation().catch((error: unknown) => {
          console.error('[CentralWorkStatusBar] 恢复普通会话失败', error);
        });
      }
      await useStore.getState().switchSession(sessionId).catch((error: unknown) => {
        console.error('[CentralWorkStatusBar] 切换工作会话失败', sessionId, error);
      });
    },
    [],
  );

  // 徽章常驻（用户设计裁决 2026-11-11）：无任务也显示（空态弱化样式），
  // 不因列表清空而消失——用户始终能找到后台工作入口。
  // 会话聚合兜底：后端登记按会话 upsert（一屏一条）；此处对同会话多条
  // 脏数据保留最新一条（数组尾部视为最新），徽章永不显示重复会话。
  const aggregated = new Map<string, CentralWorkStatus>();
  for (const item of items) {
    aggregated.set(item.session_id, item);
  }
  const visible = Array.from(aggregated.values());
  const running = visible.filter((item) => item.state === 'running');
  const queued = visible.filter((item) => item.state === 'report_queued');
  const interrupted = visible.filter((item) => item.state === 'interrupted');
  const completed = visible.filter((item) => item.state === 'completed');
  const failed = visible.filter((item) => item.state === 'failed');
  const waitingUser = visible.filter((item) => item.state === 'waiting_user');
  const paused = visible.filter((item) => item.state === 'paused');
  const cancelled = visible.filter((item) => item.state === 'cancelled');
  const queuedDep = visible.filter((item) => item.state === 'queued');

  const totalCount = visible.length;
  return (
    <div
      className="absolute right-4 top-3 z-20"
      onMouseEnter={() => {
        if (pinned) return;
        cancelClose();
        setHoverOpen(true);
      }}
      onMouseLeave={scheduleClose}
    >
      <button
        type="button"
        aria-label={
          totalCount === 0
            ? '后台工作：暂无任务'
            : `后台工作：${running.length} 个执行中，${queued.length} 条待汇报，共 ${totalCount} 项`
        }
        aria-expanded={pinned || hoverOpen}
        // hover 之外的键盘/点击可达路径（无障碍 + 触屏）；点击固定为抽屉
        onClick={() => {
          setPinned((value) => !value);
          setHoverOpen(false);
        }}
        onFocus={() => {
          if (pinned) return;
          cancelClose();
          setHoverOpen(true);
        }}
        onBlur={scheduleClose}
        className={`inline-flex items-center gap-2.5 rounded-full border px-3.5 py-1.5 text-xs shadow-lg shadow-black/5 backdrop-blur-md transition-all ${
          running.length > 0
            ? 'border-primary/30 bg-primary/[0.08] text-foreground hover:border-primary/50'
            : totalCount > 0
              ? 'border-amber-500/30 bg-amber-500/[0.08] text-foreground hover:border-amber-500/50'
              : 'border-border/60 bg-background/80 text-muted-foreground hover:border-border hover:text-foreground'
        }`}
      >
        <span className="relative flex h-2 w-2">
          {running.length > 0 ? (
            <>
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary opacity-50" />
              <span className="relative inline-flex h-2 w-2 rounded-full bg-primary" />
            </>
          ) : totalCount > 0 ? (
            <span className="relative inline-flex h-2 w-2 rounded-full bg-amber-500" />
          ) : (
            <span className="relative inline-flex h-2 w-2 rounded-full bg-muted-foreground/40" />
          )}
        </span>
        <span className="tabular-nums font-medium">
          {running.length > 0
          ? `${running.length} 个后台任务`
          : interrupted.length > 0
            ? `${interrupted.length} 个任务已中断`
            : totalCount > 0
              ? '后台任务已结束'
              : '无后台任务'}
        </span>
        {queued.length > 0 ? (
          <span className="rounded-full bg-amber-500/15 px-1.5 py-px font-medium tabular-nums text-amber-600 dark:text-amber-400">
            {queued.length} 待汇报
          </span>
        ) : null}
        <ChevronRight
          className={`h-3.5 w-3.5 text-muted-foreground transition-transform ${pinned ? 'rotate-90' : ''}`}
        />
      </button>

      {hoverOpen && !pinned ? (
        <div className="absolute right-0 top-[calc(100%+8px)] w-[360px] rounded-2xl border border-border/70 bg-background/95 p-3 shadow-xl shadow-black/10 backdrop-blur-xl">
          <WorkGroups
            running={running}
            failed={failed}
            waitingUser={waitingUser}
            paused={paused}
            cancelled={cancelled}
            queuedDep={queuedDep}
            queued={queued}
            interrupted={interrupted}
            completed={completed}
            onOpen={openInNormalMode}
          />
        </div>
      ) : null}

      {pinned ? (
        <aside
          role="dialog"
          aria-label="后台工作视图"
          className="absolute right-0 top-[calc(100%+8px)] flex max-h-[min(70vh,560px)] w-[420px] flex-col rounded-2xl border border-border/70 bg-background/98 p-4 shadow-2xl shadow-black/15 backdrop-blur-xl"
        >
          <header className="mb-3 flex items-center justify-between border-b border-border/60 pb-2">
            <h2 className="text-sm font-semibold text-foreground/90">后台工作</h2>
            <button
              type="button"
              aria-label="收起后台工作视图"
              onClick={() => setPinned(false)}
              className="rounded-lg p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              <ChevronDown className="h-4 w-4" />
            </button>
          </header>
          <div className="min-h-0 flex-1 overflow-y-auto pr-1">
            <WorkGroups
              running={running}
              failed={failed}
              waitingUser={waitingUser}
              paused={paused}
              cancelled={cancelled}
              queuedDep={queuedDep}
              queued={queued}
              interrupted={interrupted}
              completed={completed}
              onOpen={openInNormalMode}
            />
          </div>
        </aside>
      ) : null}
    </div>
  );
}

/// 分组渲染（浮层与抽屉共用）。
function WorkGroups({
  running,
  failed,
  waitingUser,
  paused,
  cancelled,
  queuedDep,
  queued,
  interrupted,
  completed,
  onOpen,
}: {
  running: CentralWorkStatus[];
  failed: CentralWorkStatus[];
  waitingUser: CentralWorkStatus[];
  paused: CentralWorkStatus[];
  cancelled: CentralWorkStatus[];
  queuedDep: CentralWorkStatus[];
  queued: CentralWorkStatus[];
  interrupted: CentralWorkStatus[];
  completed: CentralWorkStatus[];
  onOpen: (sessionId: string) => void;
}) {
  return (
    <div>
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
                  <WorkItemRow key={item.work_id} item={item} onOpen={onOpen} />
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
                  <WorkItemRow key={item.work_id} item={item} onOpen={onOpen} />
                ))}
              </div>
            </section>
          ) : null}

          {waitingUser.length > 0 ? (
            <section className="mt-3">
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <HelpCircle className="h-3.5 w-3.5 text-amber-500" />
                  等待用户
                </h3>
                <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-[10px] font-medium tabular-nums text-amber-600 dark:text-amber-400">
                  {waitingUser.length}
                </span>
              </header>
              <p className="px-1 pb-2 text-[11px] leading-relaxed text-muted-foreground">
                工作提出的问题已转述到中枢对话，请在对话中答复后自动继续。
              </p>
              <div className="space-y-2">
                {waitingUser.map((item) => (
                  <WorkItemRow key={item.work_id} item={item} onOpen={onOpen} />
                ))}
              </div>
            </section>
          ) : null}

          {paused.length > 0 ? (
            <section className="mt-3">
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <PauseCircle className="h-3.5 w-3.5 text-sky-500" />
                  已暂停
                </h3>
                <span className="rounded-full bg-sky-500/10 px-2 py-0.5 text-[10px] font-medium tabular-nums text-sky-600 dark:text-sky-400">
                  {paused.length}
                </span>
              </header>
              <p className="px-1 pb-2 text-[11px] leading-relaxed text-muted-foreground">
                上下文保留在会话中，可让中枢恢复继续执行。
              </p>
              <div className="space-y-2">
                {paused.map((item) => (
                  <WorkItemRow key={item.work_id} item={item} onOpen={onOpen} />
                ))}
              </div>
            </section>
          ) : null}

          {queuedDep.length > 0 ? (
            <section>
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <Clock className="h-3.5 w-3.5 text-muted-foreground" />
                  排队中
                </h3>
                <span className="rounded-full bg-muted px-2 py-0.5 text-[10px] font-medium tabular-nums text-muted-foreground">
                  {queuedDep.length}
                </span>
              </header>
              <div className="space-y-2 opacity-75">
                {queuedDep.map((item) => (
                  <WorkItemRow key={item.work_id} item={item} onOpen={onOpen} />
                ))}
              </div>
            </section>
          ) : null}

          {failed.length > 0 ? (
            <section className="mt-3">
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <XCircle className="h-3.5 w-3.5 text-red-500" />
                  失败
                </h3>
                <span className="rounded-full bg-red-500/10 px-2 py-0.5 text-[10px] font-medium tabular-nums text-red-600 dark:text-red-400">
                  {failed.length}
                </span>
              </header>
              <div className="space-y-2">
                {failed.map((item) => (
                  <WorkItemRow key={item.work_id} item={item} onOpen={onOpen} />
                ))}
              </div>
            </section>
          ) : null}

          {cancelled.length > 0 ? (
            <section className="mt-3">
              <header className="mb-2 flex items-center justify-between px-1">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold tracking-wide text-foreground/80">
                  <Ban className="h-3.5 w-3.5 text-muted-foreground" />
                  已取消
                </h3>
                <span className="rounded-full bg-muted px-2 py-0.5 text-[10px] font-medium tabular-nums text-muted-foreground">
                  {cancelled.length}
                </span>
              </header>
              <div className="space-y-2 opacity-60">
                {cancelled.map((item) => (
                  <WorkItemRow key={item.work_id} item={item} onOpen={onOpen} />
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
                    onOpen={onOpen}
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
                  <WorkItemRow key={item.work_id} item={item} onOpen={onOpen} />
                ))}
              </div>
            </section>
          ) : null}

          {running.length === 0 && queued.length === 0 && interrupted.length === 0 && completed.length === 0 && failed.length === 0 && waitingUser.length === 0 && paused.length === 0 && cancelled.length === 0 && queuedDep.length === 0 ? (
            <p role="status" className="px-1 py-2 text-xs text-muted-foreground">暂无后台工作</p>
          ) : null}
          <p className="mt-2 px-1 text-[10px] text-muted-foreground/70">点击工作项可在普通模式打开对应会话</p>
    </div>
  );
}
