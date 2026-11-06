import { useState, useRef, useEffect, KeyboardEvent } from 'react';
import { useStore } from '@/store/useStore';
import { api } from '@/api/tauri';
import { isRemoteHost } from '@/api/host';
import { startWindowDrag } from '@/lib/windowDrag';
import { Sun, Moon, Monitor, PanelLeft, SquarePen, Grid3x3, ArrowUpCircle, Search, Puzzle, Sparkles } from 'lucide-react';
import { useTheme } from '@/hooks/useTheme';
import { useCentralMode } from '@/hooks/useCentralMode';

/** title 激光扫描进度：0（未开始）→ 1（扫完）。 */
interface TitleScan {
  active: boolean;
  x: number;
  /** true = 普通标题 →「中枢」；false = 反向恢复 */
  toCentral: boolean;
}

/** 进入方向扫描时长与停留（与 CentralTransition 阶段一时长对齐：扫描后维持足 2s）。 */
const TITLE_SCAN_MS = 750;
const TITLE_SCAN_HOLD_MS = 2000 - TITLE_SCAN_MS;
const TITLE_SCAN_REVERSE_MS = 500;
import { useSearchStore } from '@/store/useSearchStore';
import { Separator } from './ui/separator';
import { GlobalStatusPluginHost } from './GlobalStatusPluginHost';
import { useSidebar } from './ui/sidebar';
import { Button } from './ui/button';
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbList,
  BreadcrumbPage,
} from './ui/breadcrumb';

interface StatusPanelProps {
  /** 拓展区按钮高亮：当前会话存在任一已打开的 App tab。 */
  extensionActive?: boolean;
  /** 任一 App 被 agent 使用中（用户未打开拓展区时显示绿点）。 */
  extensionAgentActive?: boolean;
  /** 点击「拓展区」按钮：面板开则收起，关则进入矩阵态或上次 App 态。 */
  onToggleExtension?: () => void;
}

function SearchButton() {
  const searchActive = useSearchStore((s) => s.searchActive);
  return (
    <button
      data-no-drag
      onClick={() => {
        const store = useSearchStore.getState();
        store.searchActive ? store.closeSearch() : store.openSearch();
      }}
      className={`transition-colors ${
        searchActive
          ? 'text-primary'
          : 'text-muted-foreground hover:text-foreground'
      }`}
      title="搜索消息"
    >
      <Search className="w-4 h-4" />
    </button>
  );
}

export function StatusPanel({ extensionActive, extensionAgentActive, onToggleExtension }: StatusPanelProps) {
  const { activeSessionId, isNewConversation, sessions, startNewConversation, updateAvailable, setPendingSettingsTab } = useStore();
  const [isEditing, setIsEditing] = useState(false);
  const [editValue, setEditValue] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  const savingRef = useRef(false);

  const { theme, setTheme } = useTheme();
  const { centralMode, canUseCentral, toggleCentral } = useCentralMode();
  const remote = isRemoteHost();
  const { toggleSidebar, open: sidebarOpen } = useSidebar();
  const activeSession = isNewConversation ? null : sessions.find((s) => s.id === activeSessionId);
  const currentTitle = isNewConversation ? '新对话' : (activeSession?.title || '新对话');



  const cycleTheme = () => {
    const next = theme === 'dark' ? 'light' : theme === 'light' ? 'system' : 'dark';
    setTheme(next);
  };
  const ThemeIcon = theme === 'dark' ? Moon : theme === 'light' ? Sun : Monitor;
  const themeLabel = theme === 'dark' ? '深色模式' : theme === 'light' ? '浅色模式' : '跟随系统';

  useEffect(() => {
    if (isEditing && inputRef.current) {
      inputRef.current.focus();
      inputRef.current.select();
    }
  }, [isEditing]);

  useEffect(() => {
    setIsEditing(false);
  }, [activeSessionId]);

  // 中枢模式下标题固定为「中枢」，退出可能残留的标题编辑态
  useEffect(() => {
    if (centralMode) {
      setIsEditing(false);
      savingRef.current = false;
    }
  }, [centralMode]);

  // title 激光扫描：centralMode 翻转时播放（进入 750ms 扫描 + 维持；退出反向 500ms）
  const [titleScan, setTitleScan] = useState<TitleScan | null>(null);
  const scanRafRef = useRef<number | null>(null);
  const scanHoldRef = useRef<number | null>(null);
  const prevCentralModeRef = useRef(centralMode);
  useEffect(() => {
    if (centralMode === prevCentralModeRef.current) return;
    const toCentral = centralMode;
    prevCentralModeRef.current = centralMode;
    const duration = toCentral ? TITLE_SCAN_MS : TITLE_SCAN_REVERSE_MS;
    const start = performance.now();
    setTitleScan({ active: true, x: 0, toCentral });
    const tick = (now: number) => {
      const t = Math.min(1, (now - start) / duration);
      setTitleScan((s) => (s ? { ...s, x: t } : s));
      if (t < 1) {
        scanRafRef.current = requestAnimationFrame(tick);
      } else {
        const hold = toCentral ? TITLE_SCAN_HOLD_MS : 160;
        scanHoldRef.current = window.setTimeout(() => setTitleScan(null), hold);
      }
    };
    scanRafRef.current = requestAnimationFrame(tick);
    return () => {
      if (scanRafRef.current !== null) cancelAnimationFrame(scanRafRef.current);
      if (scanHoldRef.current !== null) window.clearTimeout(scanHoldRef.current);
    };
  }, [centralMode]);

  const startEditing = () => {
    if (!activeSession) return;
    setEditValue(currentTitle);
    setIsEditing(true);
  };

  const saveTitle = async () => {
    if (savingRef.current) return;
    savingRef.current = true;

    setIsEditing(false);
    const trimmed = editValue.trim();
    const finalTitle = trimmed || '新对话';

    if (finalTitle !== currentTitle) {
      try {
        // 标题更新由 Core 发 title_changed 事件驱动前端刷新，不再整表 loadSessions。
        await api.updateSessionTitle(finalTitle);
      } catch (e) {
        console.error('保存标题失败:', e);
      }
    }

    savingRef.current = false;
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      inputRef.current?.blur();
    } else if (e.key === 'Escape') {
      setIsEditing(false);
      savingRef.current = false;
    }
  };


  const titlePaddingLeft = !remote && navigator.platform.includes('Mac') ? '80px' : '16px';

  return (
      <header
      className="flex h-12 shrink-0 items-center gap-2 border-b pr-4 select-none"
      style={{ paddingLeft: titlePaddingLeft }}
      onMouseDown={startWindowDrag}
    >
      <div className="flex items-center gap-2">
        {/* 中枢模式：隐藏侧栏开关与新对话（单一中枢对话，会话操作不在此进行） */}
        {!centralMode && (
          <Button
            data-no-drag
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            onClick={toggleSidebar}
          >
            <PanelLeft className="h-4 w-4" />
            <span className="sr-only">切换侧边栏</span>
          </Button>
        )}
        {!centralMode && !sidebarOpen && (
          <Button
            data-no-drag
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            onClick={() => startNewConversation()}
            title="新对话"
          >
            <SquarePen className="h-4 w-4" />
          </Button>
        )}
        {!centralMode && <Separator orientation="vertical" className="mr-2 h-4" />}
        <Breadcrumb>
          <BreadcrumbList>
            <BreadcrumbItem>
              {titleScan?.active ? (
                // 激光扫描期：旧标题暗态铺底，新标题随激光从左向右揭示，
                // 激光束（青芯白热）+ 尾迹光带同步推进（进入维持至 2s 再静止）
                <span className="relative inline-flex items-center" data-testid="central-title-scan">
                  <span className="opacity-40">{titleScan.toCentral ? currentTitle : '中枢'}</span>
                  <span
                    aria-hidden
                    className={`absolute inset-0 ${titleScan.toCentral ? 'font-medium text-primary' : ''}`}
                    style={{
                      clipPath: `inset(0 ${(1 - titleScan.x) * 100}% 0 0)`,
                      textShadow: '0 0 12px rgba(34,211,238,.85), 0 0 30px rgba(167,139,250,.45)',
                    }}
                  >
                    {titleScan.toCentral ? '中枢' : currentTitle}
                  </span>
                  {titleScan.x < 1 ? (
                    <>
                      <span
                        className="absolute top-1/2 h-7 w-[2px] -translate-y-1/2 rounded-full"
                        style={{
                          left: `${titleScan.x * 100}%`,
                          background:
                            'linear-gradient(180deg, transparent, #22d3ee 28%, #f0f9ff 50%, #22d3ee 72%, transparent)',
                          boxShadow:
                            '0 0 10px rgba(34,211,238,.95), 0 0 26px rgba(34,211,238,.6), 0 0 44px rgba(167,139,250,.35)',
                        }}
                      />
                      <span
                        className="pointer-events-none absolute inset-y-0"
                        style={{
                          left: 0,
                          width: `${titleScan.x * 100}%`,
                          background:
                            'linear-gradient(90deg, rgba(34,211,238,0), rgba(34,211,238,.13) 70%, rgba(34,211,238,.26))',
                        }}
                      />
                    </>
                  ) : null}
                </span>
              ) : centralMode ? (
                <BreadcrumbPage
                  className="animate-central-title-glow font-medium"
                  data-testid="central-title"
                >
                  中枢
                </BreadcrumbPage>
              ) : (
                (isNewConversation || activeSession) &&
                (isNewConversation ? (
                  <BreadcrumbPage>新对话</BreadcrumbPage>
                ) : isEditing ? (
                  <input
                    ref={inputRef}
                    value={editValue}
                    onChange={(e) => setEditValue(e.target.value)}
                    onBlur={saveTitle}
                    onKeyDown={handleKeyDown}
                    className="text-sm text-foreground bg-transparent border-b border-primary outline-none max-w-[300px] py-0.5"
                  />
                ) : (
                  <BreadcrumbPage
                    data-no-drag
                    className="cursor-pointer hover:text-foreground"
                    onClick={startEditing}
                    title="点击编辑标题"
                  >
                    {currentTitle}
                  </BreadcrumbPage>
                )
              )
              )}
            </BreadcrumbItem>
          </BreadcrumbList>
        </Breadcrumb>
      </div>

      <div className="ml-auto flex items-center gap-3">
        {/* 全局状态项（如自动朗读开关）：由插件经 global.status-item 注入 */}
        <span data-no-drag className="contents">
          <GlobalStatusPluginHost />
        </span>
        {updateAvailable && !remote && (
          <button
            data-no-drag
            onClick={() => setPendingSettingsTab('about')}
            className="flex items-center gap-1 px-2 py-0.5 rounded text-xs font-medium text-white bg-sky-500 hover:bg-sky-400 transition-colors"
            title={`发现新版本 ${updateAvailable.version}，点击查看`}
          >
            <ArrowUpCircle className="w-3.5 h-3.5" />
            <span>v{updateAvailable.version}</span>
          </button>
        )}
        {/* 中枢模式入口：仅 desktop 宿主提供（设计稿 §4.1；浏览器配置页与远程 H5 不提供）。
            进入与退出复用同一按钮（toggle）。 */}
        {canUseCentral && (
          <button
            data-no-drag
            onClick={toggleCentral}
            className={`transition-colors ${
              centralMode
                ? 'text-primary'
                : 'text-muted-foreground hover:text-foreground'
            }`}
            title={centralMode ? '退出中枢模式（Cmd/Ctrl+Shift+J）' : '进入中枢模式（Cmd/Ctrl+Shift+J）'}
            aria-label={centralMode ? '退出中枢模式' : '进入中枢模式'}
          >
            <Sparkles className="w-4 h-4" />
          </button>
        )}
        {/* 中枢模式为单一对话：搜索 / 插件管理 / 拓展区均不在中枢内操作 */}
        {!centralMode && !remote && <SearchButton />}
        {!centralMode && !remote && (
          <button
            data-no-drag
            onClick={() => setPendingSettingsTab('plugin-manager')}
            className="text-muted-foreground transition-colors hover:text-foreground"
            title="插件管理"
          >
            <Puzzle className="w-4 h-4" />
          </button>
        )}
        {onToggleExtension && !remote && !centralMode && (
          <button
            data-no-drag
            onClick={onToggleExtension}
            className={`relative transition-colors ${
              extensionActive
                ? 'text-primary'
                : 'text-muted-foreground hover:text-foreground'
            }`}
            title={extensionActive ? '拓展区已打开' : '打开拓展区'}
          >
            <Grid3x3 className="w-4 h-4" />
            {extensionAgentActive && (
              <span
                className="absolute -top-0.5 -right-0.5 w-2 h-2 rounded-full bg-emerald-500"
                title="Agent 正在使用拓展区应用"
                aria-label="Agent 正在使用拓展区应用"
              />
            )}
          </button>
        )}
        {!remote && (
          <button
            data-no-drag
            onClick={cycleTheme}
            className="text-muted-foreground hover:text-foreground transition-colors"
            title={themeLabel}
          >
            <ThemeIcon className="w-4 h-4" />
          </button>
        )}
      </div>
    </header>
  );
}
