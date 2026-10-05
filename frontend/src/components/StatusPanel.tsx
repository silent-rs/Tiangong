import { useState, useRef, useEffect, KeyboardEvent } from 'react';
import { useStore } from '@/store/useStore';
import { api } from '@/api/tauri';
import { isRemoteHost } from '@/api/host';
import { startWindowDrag } from '@/lib/windowDrag';
import { Sun, Moon, Monitor, PanelLeft, SquarePen, Grid3x3, ArrowUpCircle, Search, Puzzle } from 'lucide-react';
import { useTheme } from '@/hooks/useTheme';
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
        {!sidebarOpen && (
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
        <Separator orientation="vertical" className="mr-2 h-4" />
        <Breadcrumb>
          <BreadcrumbList>
            <BreadcrumbItem>
              {(isNewConversation || activeSession) && (
                isNewConversation ? (
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
        {!remote && <SearchButton />}
        {!remote && (
          <button
            data-no-drag
            onClick={() => setPendingSettingsTab('plugin-manager')}
            className="text-muted-foreground transition-colors hover:text-foreground"
            title="插件管理"
          >
            <Puzzle className="w-4 h-4" />
          </button>
        )}
        {onToggleExtension && !remote && (
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
