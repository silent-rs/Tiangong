import { useEffect, useState } from 'react';
import { Puzzle } from 'lucide-react';
import { DynamicIcon, iconNames, type IconName } from 'lucide-react/dynamic';
import { api } from '@/api/tauri';
import type { AppEntry } from '@/api/tauri';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuTrigger,
} from './ui/context-menu';

/**
 * 拓展区 App 矩阵（矩阵态视图）。
 *
 * 统一 App 目录：完全由已安装插件的 extension.tab 贡献驱动（浏览器/
 * 终端等官方能力同样以插件形态注册），数据源 listExtensionApps，插件
 * 状态变化时广播刷新。图标右上角绿点 = 该 App 存在已打开实例（含工具
 * 静默建立的标签）；左键有则聚焦无则新建；右键菜单按实例模式区分，
 * 多实例 App 提供"新建实例"入口。
 */
export interface ExtensionMatrixProps {
  /** 当前会话存在已打开实例的 plugin App 键（`plugin_id:contribution_id`）。 */
  runningPluginApps?: string[];
  /** 打开 App：默认聚焦已有实例（无则按 open_mode 新建），newInstance 强制新建。 */
  onOpenPluginApp?: (app: AppEntry, opts?: { newInstance?: boolean }) => void;
}

const ICON_NAMES: ReadonlySet<string> = new Set(iconNames);

/** App 图标：插件贡献声明的宿主图标名（lucide 名称）通用渲染，未识别用拼图。 */
function AppNamedIcon({ icon }: { icon: string }) {
  const className = 'h-6 w-6 text-muted-foreground group-hover:text-foreground';
  if (!ICON_NAMES.has(icon)) return <Puzzle className={className} />;
  return <DynamicIcon name={icon as IconName} className={className} fallback={() => <Puzzle className={className} />} />;
}

/** icon 是否为自定义资源路径（如 "icons/app.png"；含 / 或 . 视为资源）。 */
function isIconResource(icon: string): boolean {
  return icon.includes('/') || icon.includes('.');
}

/** 自定义图标 object URL 缓存（按插件:贡献；组件生命周期内复用）。 */
const iconUrlCache = new Map<string, Promise<string | null>>();

function loadIconUrl(app: AppEntry): Promise<string | null> {
  const key = `${app.plugin_id}:${app.contribution_id}`;
  if (!iconUrlCache.has(key)) {
    iconUrlCache.set(
      key,
      api
        .pluginReadIcon(app.plugin_id, app.contribution_id)
        .then((resource) =>
          URL.createObjectURL(new Blob([new Uint8Array(resource.data)], { type: resource.mime })),
        )
        .catch((error) => {
          console.warn(`插件 ${app.plugin_id} 图标加载失败：`, error);
          return null;
        }),
    );
  }
  return iconUrlCache.get(key)!;
}

/** 自定义图标图元：img 渲染（img 中的 SVG 不执行脚本）；失败回落拼图。 */
function AppIconImage({ app }: { app: AppEntry }) {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    void loadIconUrl(app).then((value) => {
      if (active) setUrl(value);
    });
    return () => {
      active = false;
    };
  }, [app]);
  if (!url) return <Puzzle className="h-6 w-6 text-muted-foreground group-hover:text-foreground" />;
  return <img src={url} alt={app.title} className="h-7 w-7 object-contain" draggable={false} />;
}

export function ExtensionMatrix({
  runningPluginApps = [],
  onOpenPluginApp,
}: ExtensionMatrixProps) {
  const [apps, setApps] = useState<AppEntry[]>([]);

  // 挂载时拉取；插件安装/卸载/启停后广播刷新（App 目录随插件上下线）。
  useEffect(() => {
    let active = true;
    const load = () => {
      api.listExtensionApps()
        .then((entries) => {
          if (active) setApps(entries);
        })
        .catch(() => {
          if (active) setApps([]);
        });
    };
    load();
    const unlisten = api.onPluginsChanged(() => load());
    return () => {
      active = false;
      void unlisten.then((fn) => fn());
    };
  }, []);

  // 列数随容器宽度自适应：每列最小 80px 不够则换列，避免压缩时固定列数
  // 导致卡片溢出轨道相互重叠（lg 视口断点与容器宽度脱钩，不适用）。
  return (
    <div className="grid min-h-0 flex-1 auto-rows-min grid-cols-[repeat(auto-fill,minmax(80px,1fr))] place-content-start gap-x-1.5 gap-y-2 overflow-y-auto p-3">
      {apps.map((app) => {
        const appKey = `${app.plugin_id}:${app.contribution_id}`;
        const running = runningPluginApps.includes(appKey);
        const multi = app.open_mode === 'multi';
        const open = (opts?: { newInstance?: boolean }) => {
          onOpenPluginApp?.(app, opts);
        };
        return (
          <ContextMenu key={appKey}>
            <ContextMenuTrigger asChild>
              <button
                type="button"
                onClick={() => open()}
                className="group flex w-full cursor-default flex-col items-center gap-1"
                title={`${app.title}${running ? '（已打开）' : ''}（${multi ? '多实例' : '单实例'}）—— ${app.description}`}
              >
                <span className="relative flex h-12 w-12 items-center justify-center rounded-lg border bg-muted/50 transition-colors group-hover:border-primary/50 group-hover:bg-accent">
                  {isIconResource(app.icon) ? (
                    <AppIconImage app={app} />
                  ) : (
                    <AppNamedIcon icon={app.icon} />
                  )}
                  {running && (
                    <span
                      className="absolute -right-1 -top-1 h-2.5 w-2.5 rounded-full bg-emerald-500 ring-2 ring-background"
                      title="已打开"
                      aria-label={`${app.title}已打开`}
                    />
                  )}
                </span>
                <span className="max-w-full truncate text-[11px] text-foreground/90">
                  {app.title}
                </span>
              </button>
            </ContextMenuTrigger>
            <ContextMenuContent>
              <ContextMenuItem onClick={() => open()}>
                {running ? `聚焦${app.title}` : `打开${app.title}`}
              </ContextMenuItem>
              {multi && (
                <ContextMenuItem onClick={() => open({ newInstance: true })}>
                  新建{app.title}实例
                </ContextMenuItem>
              )}
            </ContextMenuContent>
          </ContextMenu>
        );
      })}
    </div>
  );
}
