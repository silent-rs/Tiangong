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
import { Loader2, Inbox, History, ChevronRight, AlertTriangle, CheckCircle2, XCircle, HelpCircle, PauseCircle, Clock, Ban } from 'lucide-react';
import { restoreFromCentralConversation } from '@/lib/central/centralConversation';
import { CentralWorkDrawer, type CentralWorkDrawerItem } from '@/components/central/CentralWorkDrawer';
import { CentralBadgeIcon, type BadgeState } from '@/components/central/CentralBadgeIcon';

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
  onOpen?: (item: CentralWorkStatus) => void;
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
      onClick={() => onOpen?.(item)}
      role={onOpen ? 'button' : undefined}
      tabIndex={onOpen ? 0 : undefined}
      onKeyDown={(event) => {
        if (onOpen && (event.key === 'Enter' || event.key === ' ')) {
          event.preventDefault();
          onOpen(item);
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
  // 用户实测五：纯 hover 交互（移出即收，250ms 延迟防误触）——
  // 点击固定（pinned）模式已移除；点击工作行打开详情抽屉。
  const [hoverOpen, setHoverOpen] = useState(false);
  const closeTimer = useRef<number | null>(null);
  const rootRef = useRef<HTMLDivElement | null>(null);
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
    }
  }, [items.length]);


  // 卸载清理（review：closeTimer 残留会在组件卸载后触发 setState 警告）
  useEffect(
    () => () => {
      if (closeTimer.current !== null) window.clearTimeout(closeTimer.current);
    },
    [],
  );

  // focus-within（review a11y）：焦点在容器内（键盘 Tab 操作浮层）时
  // 不挂关闭——鼠标移出不应误关正在键盘交互的面板。
  const scheduleClose = useCallback(() => {
    if (rootRef.current?.contains(document.activeElement)) return;
    if (closeTimer.current !== null) window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(() => setHoverOpen(false), PANEL_CLOSE_DELAY_MS);
  }, []);
  const cancelClose = useCallback(() => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
  }, []);

  // R4 工作抽屉：点击工作行打开抽屉复核（不离开中枢、不切上下文，
  // §3.5/§6.4）；「打开原始工作会话」是抽屉内的显式动作。
  // 必须位于早退之前——否则空列表首渲染与有数据渲染的 hooks 数量不一致
  //（React 规则：所有 hooks 需无条件按序调用）。
  const [drawerWork, setDrawerWork] = useState<CentralWorkDrawerItem | null>(null);
  const openDrawer = useCallback((item: CentralWorkStatus) => {
    setHoverOpen(false);
    setDrawerWork(item);
  }, []);
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
  // 用户实测修复：按派发时间降序（最新在前）——运行时 upsert 与投影重建
  // 的数组顺序不一致（append 尾部 vs 倒序遍历头部），统一以时间为准
  const visible = Array.from(aggregated.values()).sort((a, b) =>
    b.dispatched_at.localeCompare(a.dispatched_at),
  );
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
  // 徽章主状态（优先级：等你决定 > 执行中 > 失败 > 中断 > 待汇报/完成 > 其他）
  const badgeState: BadgeState = waitingUser.length > 0
    ? 'waiting_user'
    : running.length > 0
      ? 'running'
      : failed.length > 0
        ? 'failed'
        : interrupted.length > 0
          ? 'interrupted'
          : queued.length + completed.length > 0
            ? 'done'
            : paused.length + cancelled.length + queuedDep.length > 0
              ? 'paused'
              : 'idle';
  return (
    <div
      ref={rootRef}
      className="absolute right-[70px] top-3 z-30"
      onMouseEnter={() => {
        cancelClose();
        setHoverOpen(true);
      }}
      onMouseLeave={scheduleClose}
      onFocusCapture={() => {
        // focus-within（review a11y）：Tab 进入浮层的控件视为仍在
        // 交互区，取消挂起的关闭（不被 250ms 定时误关）。
        cancelClose();
        setHoverOpen(true);
      }}
      onBlurCapture={() => {
        // 延迟到下一拍判定：blur 后焦点可能移入容器内另一控件
        window.setTimeout(scheduleClose, 0);
      }}
    >
      <button
        type="button"
        aria-label={
          totalCount === 0
            ? '后台工作：暂无任务'
            : `后台工作：${running.length} 个执行中，${queued.length} 条待汇报，共 ${totalCount} 项`
        }
        aria-controls={hoverOpen ? 'central-work-popover' : undefined}
        aria-expanded={hoverOpen}
        // 纯 hover/focus 交互（用户实测五）；点击无固定行为——工作详情
        // 经浮层内的工作行打开
        onClick={() => {
          cancelClose();
          setHoverOpen(true);
        }}
        onFocus={() => {
          cancelClose();
          setHoverOpen(true);
        }}
        onBlur={scheduleClose}
        className="group inline-flex items-center gap-2 rounded-full border px-2.5 py-1.5 shadow-lg shadow-black/5 backdrop-blur-md transition-all hover:border-primary/40"
      >
        <CentralBadgeIcon state={badgeState} />
        {totalCount > 0 ? (
          <span className="min-w-4 text-right text-xs font-semibold tabular-nums text-foreground/85">
            {totalCount}
          </span>
        ) : null}
        <ChevronRight
          className={`h-3 w-3 text-muted-foreground/60 transition-transform duration-200 ${hoverOpen ? 'rotate-90' : ''}`}
        />
      </button>
      {hoverOpen ? (
        <div
          id="central-work-popover"
          className="absolute right-0 top-[calc(100%+8px)] w-[360px] rounded-2xl border border-border/70 bg-background/95 p-3 shadow-xl shadow-black/10 backdrop-blur-xl">
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
            onOpen={openDrawer}
          />
        </div>
      ) : null}

      {drawerWork ? (
        <CentralWorkDrawer
          work={drawerWork}
          onClose={() => setDrawerWork(null)}
          onOpenSession={(sessionId) => {
            setDrawerWork(null);
            void openInNormalMode(sessionId);
          }}
        />
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
  onOpen: (item: CentralWorkStatus) => void;
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
