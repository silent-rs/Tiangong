import { selectCurrentIsSending, useStore } from '@/store/useStore';
import { Button } from './ui/button';
import { ScrollArea } from './ui/scroll-area';
import { useSidebar } from './ui/sidebar';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from './ui/context-menu';
import { Plus, Trash2, Folder, FilePlus2, FolderX, ChevronRight, RefreshCw, Search, X } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { SettingsDialog } from './SettingsDialog';
import { isRemoteHost } from '@/api/host';
import { useToast } from './Toast';
import type { Session } from '@/api/tauri';

/** 默认分组 key：与全局 workspace 一致（或 cwd 为空）的会话归入此分组，平铺不显示分组头 */
const DEFAULT_GROUP_KEY = '__default__';
/** 收缩状态下每个分组初始显示的会话数，也是"收起"回落的数量 */
const COLLAPSED_LIMIT = 5;
/** 点击"显示更多"每次追加的会话数 */
const LOAD_MORE_STEP = 5;

interface SessionGroup {
  /** 分组 key：默认分组为 __default__，其余为会话 cwd */
  key: string;
  /** 分组显示名：默认分组为空字符串（不显示头），其余为 cwd 的 basename */
  label: string;
  /** 完整 cwd 路径，用作 title 提示 */
  fullPath: string;
  isDefault: boolean;
  /** 组内会话，已按 updated_at 倒序 */
  sessions: Session[];
}

/** 取路径末段作为分组显示名，兼容 / 与 \ */
function basename(path: string): string {
  if (!path) return '';
  const trimmed = path.replace(/[\\/]+$/, '');
  const parts = trimmed.split(/[\\/]/);
  return parts[parts.length - 1] || trimmed;
}

/** 是否应归入默认分组：cwd 为空或与全局 workspace 一致 */
function isDefaultCwd(cwd: string, workspaceDir: string): boolean {
  return !cwd || cwd === workspaceDir;
}

/**
 * 按 workspace(cwd) 对会话分组。
 * - 默认分组（cwd 与全局 workspace 一致或为空）排最前且不显示分组头。
 * - 其余分组按"组内最新 updated_at"倒序排列。
 * - 每个分组内部按 updated_at 倒序。
 */
function groupSessions(sessions: Session[], workspaceDir: string): SessionGroup[] {
  const map = new Map<string, Session[]>();
  for (const session of sessions) {
    const key = isDefaultCwd(session.cwd, workspaceDir) ? DEFAULT_GROUP_KEY : session.cwd;
    const list = map.get(key);
    if (list) {
      list.push(session);
    } else {
      map.set(key, [session]);
    }
  }

  const groups: SessionGroup[] = [];
  for (const [key, list] of map) {
    list.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
    if (key === DEFAULT_GROUP_KEY) {
      groups.push({ key, label: '', fullPath: workspaceDir, isDefault: true, sessions: list });
    } else {
      groups.push({ key, label: basename(key), fullPath: key, isDefault: false, sessions: list });
    }
  }

  // 默认分组置顶，其余按组内最新 updated_at 倒序
  groups.sort((a, b) => {
    if (a.isDefault !== b.isDefault) return a.isDefault ? -1 : 1;
    if (a.isDefault) return 0; // 两个默认分组不会同时存在，仅占位
    const aLatest = a.sessions[0]?.updated_at ?? '';
    const bLatest = b.sessions[0]?.updated_at ?? '';
    return bLatest.localeCompare(aLatest);
  });

  return groups;
}

export function AppSidebar() {
  const {
    sessions,
    activeSessionId,
    isNewConversation,
    sessionRunStatuses,
    newConversationId,
    startNewConversation,
    switchSession,
    deleteSession,
    deleteSessionsByCwd,
    isLoadingSessions,
    sessionsLoadError,
    loadSessions,
    workspaceDir,
  } = useStore();
  const isSending = useStore(selectCurrentIsSending);

  const { open, isMobile, openMobile, setOpenMobile } = useSidebar();
  const { showWarning, showError } = useToast();
  const [showDeleteConfirm, setShowDeleteConfirm] = useState(false);
  // 待删除的会话 ID（点击删除按钮时记录，确认后按此 ID 删除）。
  const [pendingDeleteSessionId, setPendingDeleteSessionId] = useState<string | null>(null);
  // 待删除的 workspace 分组（cwd + 显示名），null 表示无待删除
  const [pendingDeleteWorkspace, setPendingDeleteWorkspace] = useState<{ cwd: string; label: string; count: number } | null>(null);
  // 每个分组当前显示的会话数：未记录时为 COLLAPSED_LIMIT；"显示更多"每次 +LOAD_MORE_STEP，"收起"回落
  const [visibleCounts, setVisibleCounts] = useState<Record<string, number>>({});
  // 折叠的分组：true 时整组收起不显示任何会话（点击分组头切换），展开后恢复之前的显示数
  const [collapsedGroups, setCollapsedGroups] = useState<Record<string, boolean>>({});

  // 仅展示有消息的会话或当前活跃会话；有过滤词时再按标题与 workspace 路径过滤（不区分大小写）。
  const [filterOpen, setFilterOpen] = useState(false);
  const [filterText, setFilterText] = useState('');
  const filterQuery = filterText.trim().toLowerCase();
  const visibleSessions = useMemo(
    () => sessions.filter((s) => {
      if (!(s.message_count > 0 || s.id === activeSessionId)) return false;
      if (!filterQuery) return true;
      return (s.title || '新对话').toLowerCase().includes(filterQuery)
        || (s.cwd || '').toLowerCase().includes(filterQuery);
    }),
    [sessions, activeSessionId, filterQuery],
  );

  const groups = useMemo(
    () => groupSessions(visibleSessions, workspaceDir),
    [visibleSessions, workspaceDir],
  );
  // 是否存在非默认（workspace 分类）分组；没有时默认分组不收缩、全部平铺
  const hasWorkspaceGroups = groups.some((g) => !g.isDefault);

  // 浮出态（窄窗口/手机）下切换/新建会话后收起侧边栏，避免持续遮挡内容。
  // 新建对话时会话 ID 为 newConversationId（已在新对话中再次新建也会变化）。
  const conversationKey = isNewConversation ? `new:${newConversationId ?? ''}` : activeSessionId;
  useEffect(() => {
    if (isMobile && openMobile) {
      setOpenMobile(false);
    }
    // 仅在会话切换时触发；isMobile/openMobile 不入依赖（否则打开浮层会被立即收起）
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conversationKey]);

  // 点击后立即收起浮层：不等异步的新建/切换完成，也覆盖"点了当前会话"这类不触发切换的情况。
  const closeOverlay = () => {
    if (isMobile) setOpenMobile(false);
  };
  const newConversation = (cwd?: string) => {
    if (isSending) return;
    closeOverlay();
    void startNewConversation(cwd);
  };

  // 刷新会话列表：普通加载（显示加载中并在失败时给出原因），供远程连接异常后手动恢复。
  const [refreshing, setRefreshing] = useState(false);
  const handleRefresh = async () => {
    if (refreshing) return;
    setRefreshing(true);
    try {
      await loadSessions();
    } finally {
      setRefreshing(false);
    }
  };

  // 过滤会话：打开时聚焦输入框，关闭时清空过滤词恢复完整列表。
  const filterInputRef = useRef<HTMLInputElement>(null);
  const closeFilter = () => {
    setFilterOpen(false);
    setFilterText('');
  };
  const toggleFilter = () => {
    if (filterOpen) {
      closeFilter();
      return;
    }
    setFilterOpen(true);
    requestAnimationFrame(() => filterInputRef.current?.focus());
  };

  const showMore = (key: string, total: number) => {
    setVisibleCounts((prev) => {
      const current = prev[key] ?? COLLAPSED_LIMIT;
      return { ...prev, [key]: Math.min(current + LOAD_MORE_STEP, total) };
    });
  };

  const collapseToList = (key: string) => {
    setVisibleCounts((prev) => ({ ...prev, [key]: COLLAPSED_LIMIT }));
  };

  const toggleGroupCollapsed = (key: string) => {
    setCollapsedGroups((prev) => ({ ...prev, [key]: !prev[key] }));
  };

  const handleDeleteSession = async () => {
    if (!pendingDeleteSessionId) return;
    await deleteSession(pendingDeleteSessionId);
    setPendingDeleteSessionId(null);
    setShowDeleteConfirm(false);
  };

  const handleDeleteWorkspace = async () => {
    if (!pendingDeleteWorkspace) return;
    const { cwd, count } = pendingDeleteWorkspace;
    setPendingDeleteWorkspace(null);
    const { failed } = await deleteSessionsByCwd(cwd);
    if (failed < 0) {
      showError('删除失败', '工作空间会话删除遇到错误');
    } else if (failed > 0) {
      showWarning('部分删除失败', `成功 ${count - failed} 个，${failed} 个失败`);
    }
  };

  const renderSessionItem = (session: Session) => {
    const isRunning = !!sessionRunStatuses[session.id];
    const isActive = !isNewConversation && activeSessionId === session.id;
    return (
      <div
        key={session.id}
        className={`w-full px-3 py-1.5 rounded-md text-sm flex items-center gap-2 group transition-colors ${
          isActive
            ? 'bg-sidebar-accent text-sidebar-accent-foreground'
            : 'text-sidebar-foreground/70 hover:bg-sidebar-accent hover:text-sidebar-accent-foreground'
        }`}
      >
        <button
          type="button"
          className="flex min-w-0 flex-1 items-center gap-2 text-left"
          onClick={() => {
            closeOverlay();
            if (session.id !== activeSessionId || isNewConversation) {
              switchSession(session.id);
            }
          }}
        >
          <span className="flex-1 truncate">{session.title || '新对话'}</span>
          {isRunning && (
            <span className="w-2 h-2 rounded-full bg-yellow-500 animate-pulse shrink-0" />
          )}
        </button>
        {!isRunning && (
          <button
            type="button"
            className="opacity-0 group-hover:opacity-100 [@media(hover:none)]:opacity-100 hover:text-destructive transition-opacity"
            onClick={() => {
              setPendingDeleteSessionId(session.id);
              setShowDeleteConfirm(true);
            }}
            aria-label="删除对话"
            title="删除对话"
          >
            <Trash2 className="w-3 h-3" />
          </button>
        )}
      </div>
    );
  };

  const renderGroup = (group: SessionGroup) => {
    const isCollapsed = !filterQuery && collapsedGroups[group.key];
    // 默认分组在无 workspace 分类分组时不收缩，全部平铺
    const allowCollapse = group.isDefault ? hasWorkspaceGroups : true;
    // 渐进显示：未记录时为 COLLAPSED_LIMIT，"显示更多"逐步追加直至全部；
    // 过滤时直接展示全部匹配项，避免命中项藏在「显示更多」后面。
    const visibleCount = filterQuery
      ? group.sessions.length
      : Math.min(visibleCounts[group.key] ?? COLLAPSED_LIMIT, group.sessions.length);
    const allShown = visibleCount >= group.sessions.length;
    const visibleItems = !allowCollapse || allShown
      ? group.sessions
      : group.sessions.slice(0, visibleCount);
    const canCollapse = !filterQuery && allowCollapse && group.sessions.length > COLLAPSED_LIMIT;
    // workspace 下任一会话正在运行时，禁用批量删除。
    const groupHasRunning = group.sessions.some(
      (session) => !!sessionRunStatuses[session.id],
    );

    if (group.isDefault) {
      // 默认分组：无分组头也无折叠交互，直接平铺
      return (
        <div key={group.key} className="space-y-1">
          {visibleItems.map(renderSessionItem)}
          {canCollapse && (
            <button
              className="w-full text-left px-3 py-1.5 rounded-md text-xs text-muted-foreground hover:text-foreground hover:bg-sidebar-accent/50 transition-colors"
              onClick={() =>
                allShown ? collapseToList(group.key) : showMore(group.key, group.sessions.length)
              }
            >
              {allShown ? '收起' : '显示更多'}
            </button>
          )}
        </div>
      );
    }

    // 非默认分组：显示分组头 + 会话列表，整组支持右键菜单
    return (
      <ContextMenu key={group.key}>
        <ContextMenuTrigger asChild>
          <div className="space-y-1 select-none">
            <div className="flex items-center gap-1 group">
              {/* 点击分组头折叠/展开整组；右侧新建按钮独立响应不受影响 */}
              <div
                className="flex-1 flex items-center gap-1.5 px-2 py-1.5 rounded-md text-xs font-medium text-muted-foreground min-w-0 cursor-pointer hover:text-foreground hover:bg-sidebar-accent/50 transition-colors"
                title={group.fullPath}
                onClick={() => toggleGroupCollapsed(group.key)}
              >
                <ChevronRight
                  className={`w-3 h-3 shrink-0 transition-transform ${isCollapsed ? '' : 'rotate-90'}`}
                />
                <Folder className="w-3.5 h-3.5 shrink-0" />
                <span className="truncate">{group.label}</span>
                <span className="text-muted-foreground/70 shrink-0">{group.sessions.length}</span>
              </div>
              <button
                className="p-1 rounded text-muted-foreground opacity-0 group-hover:opacity-100 [@media(hover:none)]:opacity-100 hover:text-foreground hover:bg-sidebar-accent/50 transition-opacity shrink-0"
                onClick={() => newConversation(group.fullPath)}
                disabled={isSending}
                title="在此 workspace 下新建对话"
              >
                <Plus className="w-3.5 h-3.5" />
              </button>
            </div>
            {!isCollapsed && (
              <>
                <div className="space-y-1 pl-1">
                  {visibleItems.map(renderSessionItem)}
                </div>
                {canCollapse && (
                  <button
                    className="w-full text-left px-3 py-1.5 rounded-md text-xs text-muted-foreground hover:text-foreground hover:bg-sidebar-accent/50 transition-colors"
                    onClick={() =>
                      allShown ? collapseToList(group.key) : showMore(group.key, group.sessions.length)
                    }
                  >
                    {allShown ? '收起' : '显示更多'}
                  </button>
                )}
              </>
            )}
          </div>
        </ContextMenuTrigger>
        <ContextMenuContent>
          <ContextMenuItem
            disabled={isSending}
            onSelect={() => newConversation(group.fullPath)}
          >
            <FilePlus2 className="w-4 h-4 mr-2" />
            新对话
          </ContextMenuItem>
          <ContextMenuSeparator />
          <ContextMenuItem
            disabled={isSending || groupHasRunning}
            className="text-destructive focus:text-destructive"
            onSelect={() =>
              setPendingDeleteWorkspace({
                cwd: group.fullPath,
                label: group.label,
                count: group.sessions.length,
              })
            }
          >
            <FolderX className="w-4 h-4 mr-2" />
            删除 workspace（{group.sessions.length} 个对话）
          </ContextMenuItem>
        </ContextMenuContent>
      </ContextMenu>
    );
  };

  const content = (
    <div className="flex flex-col h-full min-h-0 min-w-[var(--sidebar-width,16rem)]">
      {/* 顶部操作：刷新列表 | 新对话 | 过滤会话 */}
      <div className="p-2 shrink-0 space-y-2">
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="icon"
            className="h-9 w-9 shrink-0"
            disabled={isLoadingSessions || refreshing}
            onClick={handleRefresh}
            title="刷新会话列表"
            aria-label="刷新会话列表"
          >
            <RefreshCw className={`w-4 h-4 ${refreshing ? 'animate-spin' : ''}`} />
          </Button>
          <Button
            variant="ghost"
            className="flex-1 min-w-0 justify-start"
            disabled={isSending}
            onClick={() => newConversation()}
          >
            <Plus className="w-4 h-4 mr-2" />
            新对话
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className={`h-9 w-9 shrink-0 ${filterOpen || filterQuery ? 'bg-sidebar-accent text-sidebar-accent-foreground' : ''}`}
            onClick={toggleFilter}
            title="搜索会话"
            aria-label="搜索会话"
            aria-pressed={filterOpen}
          >
            <Search className="w-4 h-4" />
          </Button>
        </div>
        {filterOpen && (
          <div className="relative">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <input
              ref={filterInputRef}
              type="search"
              value={filterText}
              onChange={(event) => setFilterText(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Escape') closeFilter();
              }}
              placeholder="按标题或工作目录过滤"
              aria-label="过滤会话"
              className="h-8 w-full rounded-md border border-sidebar-border bg-background pl-8 pr-8 text-sm outline-none placeholder:text-muted-foreground focus-visible:ring-1 focus-visible:ring-ring"
            />
            {filterText && (
              <button
                type="button"
                onClick={() => {
                  setFilterText('');
                  filterInputRef.current?.focus();
                }}
                className="absolute right-1.5 top-1/2 flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded text-muted-foreground hover:text-foreground"
                title="清空"
                aria-label="清空过滤"
              >
                <X className="h-3.5 w-3.5" />
              </button>
            )}
          </div>
        )}
      </div>

      {/* 会话列表（按 workspace 分组） */}
      <ScrollArea className="flex-1 min-h-0 pl-2 pr-1">
        <div className="space-y-2 py-1 pr-1">
          {isLoadingSessions ? (
            <div className="px-3 py-2 text-sm text-muted-foreground">加载中...</div>
          ) : groups.length === 0 && sessionsLoadError ? (
            <div className="space-y-2 px-3 py-2 text-sm">
              <div className="text-destructive">会话列表加载失败</div>
              <div className="break-all text-xs text-muted-foreground">{sessionsLoadError}</div>
              <Button size="sm" variant="outline" onClick={() => void loadSessions()}>
                重试
              </Button>
            </div>
          ) : groups.length === 0 && filterQuery ? (
            <div className="px-3 py-2 text-sm text-muted-foreground">没有匹配的对话</div>
          ) : groups.length === 0 ? (
            <div className="px-3 py-2 text-sm text-muted-foreground">暂无对话</div>
          ) : (
            groups.map(renderGroup)
          )}
        </div>
      </ScrollArea>

      {/* 底部设置（远程模式不开放设置） */}
      {!isRemoteHost() && (
        <div className="p-2 border-t border-sidebar-border shrink-0">
          <SettingsDialog />
        </div>
      )}
    </div>
  );

  const deleteConfirm = showDeleteConfirm && pendingDeleteSessionId && (
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50">
      <div className="bg-popover border rounded-lg p-4 max-w-sm w-full mx-4">
        <h3 className="font-medium mb-2">删除对话</h3>
        <p className="text-muted-foreground text-sm mb-4">
          {(() => {
            const session = sessions.find((s) => s.id === pendingDeleteSessionId);
            const title = session?.title || '新对话';
            return `确定要删除「${title}」吗？可从设置-数据清理中恢复。`;
          })()}
        </p>
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => setShowDeleteConfirm(false)}>
            取消
          </Button>
          <Button variant="destructive" onClick={handleDeleteSession}>
            删除
          </Button>
        </div>
      </div>
    </div>
  );

  const deleteWorkspaceConfirm = pendingDeleteWorkspace && (
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50">
      <div className="bg-popover border rounded-lg p-4 max-w-sm w-full mx-4">
        <h3 className="font-medium mb-2">删除 workspace</h3>
        <p className="text-muted-foreground text-sm mb-4">
          将删除 workspace「<span className="font-mono text-foreground">{pendingDeleteWorkspace.label}</span>
          」下的全部 {pendingDeleteWorkspace.count} 个对话。此操作无法撤销。
        </p>
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => setPendingDeleteWorkspace(null)}>
            取消
          </Button>
          <Button variant="destructive" onClick={handleDeleteWorkspace}>
            删除全部
          </Button>
        </div>
      </div>
    </div>
  );

  const sidebarBody = (
    <>
      {content}
      {deleteConfirm}
      {deleteWorkspaceConfirm}
    </>
  );

  // 窄窗口（浮层断点内）：浮层展示，起于顶栏（h-12）下方、不遮挡 header；
  // 不挤压内容、不扩窗；点击遮罩收起，切换会话后由上方 effect 自动收起。
  if (isMobile) {
    return (
      <>
        {openMobile && (
          <div
            className="fixed inset-x-0 bottom-0 top-12 z-40 bg-black/50 animate-in fade-in-0 duration-300"
            onClick={() => setOpenMobile(false)}
          />
        )}
        <aside
          className="fixed bottom-0 left-0 top-12 z-40 flex w-[var(--sidebar-width,16rem)] flex-col overflow-hidden border-r bg-sidebar text-sidebar-foreground shadow-lg transition-transform duration-300 ease-in-out"
          style={{ transform: openMobile ? 'translateX(0)' : 'translateX(-100%)' }}
          aria-hidden={!openMobile}
        >
          {sidebarBody}
        </aside>
      </>
    );
  }

  return (
    <aside
      className="shrink-0 min-h-0 border-r bg-sidebar text-sidebar-foreground flex flex-col overflow-hidden transition-[width] duration-200 ease-linear"
      style={{ width: open ? 'var(--sidebar-width, 16rem)' : '0px' }}
    >
      {sidebarBody}
    </aside>
  );
}
