import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  AlertCircle,
  ArrowUpCircle,
  BookOpen,
  CheckCircle2,
  Circle,
  Download,
  FolderInput,
  Loader2,
  RefreshCw,
  RotateCw,
  Search,
  Sparkles,
  Trash2,
  Undo2,
} from 'lucide-react';
import { open } from '@tauri-apps/plugin-dialog';
import { isWebHost } from '@/api/host';
import {
  api,
  type AvailablePlugin,
  type SlotContributionEntry,
  type PluginStatus,
  type TrustedPublisherEntry,
} from '@/api/tauri';
import { isRemotePluginVersionNewer } from '@/lib/pluginVersion';
import { Badge } from './ui/badge';
import { Button } from './ui/button';
import { DefaultPluginOnboarding } from './DefaultPluginOnboarding';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from './ui/dialog';
import { Input } from './ui/input';
import { Separator } from './ui/separator';
import { Switch } from './ui/switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from './ui/tabs';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from './ui/tooltip';
import { useToast } from './Toast';

type Props = {
  onContributionsChanged: (entries: SlotContributionEntry[]) => void;
  initialPlugins: PluginStatus[];
  initialAvailable: AvailablePlugin[];
  initialCatalogError: string | null;
  onRefreshStateChange: (refreshing: boolean) => void;
};

type Operation =
  | 'enable'
  | 'disable'
  | 'import'
  | 'install'
  | 'reload'
  | 'upgrade'
  | 'rollback'
  | 'uninstall';

type ActiveOperation = {
  pluginId: string;
  operation: Operation;
};

const stateLabel: Record<PluginStatus['state'], string> = {
  loaded: '已加载',
  disabled: '已停用',
  degraded: '运行异常',
  error: '加载失败',
  invalid: '无效',
};

const PLUGIN_DEVELOPMENT_DOC_URL =
  'https://github.com/silent-rs/silent-Tiangong/blob/main/docs/plugin-development.md';

export function PluginManagerSettings({
  onContributionsChanged,
  initialPlugins,
  initialAvailable,
  initialCatalogError,
  onRefreshStateChange,
}: Props) {
  const [plugins, setPlugins] = useState<PluginStatus[]>(initialPlugins);
  const [available, setAvailable] = useState<AvailablePlugin[]>(initialAvailable);
  const [loading, setLoading] = useState(false);
  const [catalogError, setCatalogError] = useState<string | null>(initialCatalogError);
  const [activeOperation, setActiveOperation] = useState<ActiveOperation | null>(null);
  const [uninstallTarget, setUninstallTarget] = useState<PluginStatus | null>(null);
  const [keepData, setKeepData] = useState(true);
  const [query, setQuery] = useState('');
  // 安装/升级下载进度，按 pluginId 存百分比（0-100）。
  const [installProgress, setInstallProgress] = useState<Record<string, number>>({});
  // 推荐安装引导的缺失默认插件列表；为 null 时不显示。
  const [recommendMissing, setRecommendMissing] = useState<AvailablePlugin[] | null>(null);
  const { showError, showSuccess } = useToast();

  // 监听后端推送的下载进度事件，更新对应插件的进度。
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void api
      .onPluginInstallProgress(({ plugin_id, downloaded, total }) => {
        if (disposed) return;
        const percent = total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : 0;
        setInstallProgress((prev) => ({ ...prev, [plugin_id]: percent }));
      })
      .then((stop) => {
        if (disposed) {
          stop();
        } else {
          unlisten = stop;
        }
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    setPlugins(initialPlugins);
    setAvailable(initialAvailable);
    setCatalogError(initialCatalogError);
  }, [initialAvailable, initialCatalogError, initialPlugins]);

  const refreshContributions = useCallback(async () => {
    const contributions = await api.listSlotContributions('settings.plugin-page');
    onContributionsChanged(contributions.filter((entry) => entry.has_view));
  }, [onContributionsChanged]);

  const refresh = useCallback(async () => {
    setLoading(true);
    onRefreshStateChange(true);
    try {
      const [installedResult, availableResult] = await Promise.allSettled([
        api.listPlugins(),
        api.listAvailablePlugins(),
      ]);
      if (installedResult.status === 'fulfilled') {
        setPlugins(installedResult.value);
      } else {
        showError('读取失败', String(installedResult.reason));
      }
      if (availableResult.status === 'fulfilled') {
        setAvailable(availableResult.value);
        setCatalogError(null);
      } else {
        setCatalogError(String(availableResult.reason));
      }
    } finally {
      setLoading(false);
      onRefreshStateChange(false);
    }
  }, [onRefreshStateChange, showError]);

  const availableById = useMemo(
    () => new Map(available.map((plugin) => [plugin.id, plugin])),
    [available],
  );

  // 关键字过滤：匹配插件名称或描述，对已安装和可安装列表同时生效。
  const normalizedQuery = query.trim().toLowerCase();
  const matches = (text: string) => text.toLowerCase().includes(normalizedQuery);
  const filteredPlugins = useMemo(
    () =>
      normalizedQuery === ''
        ? plugins
        : plugins.filter((plugin) => matches(plugin.name) || matches(plugin.id)),
    [plugins, normalizedQuery],
  );
  const filteredAvailable = useMemo(() => {
    // 隐藏已安装且无更新的插件（已安装且无更新的不再出现在可安装列表）。
    const installable = available.filter(
      (plugin) => plugin.installed_version === null || plugin.update_available,
    );
    return normalizedQuery === ''
      ? installable
      : installable.filter((plugin) => matches(plugin.name) || matches(plugin.description));
  }, [available, normalizedQuery]);

  const finishOperation = async (
    pluginId: string,
    operation: Operation,
    result: unknown,
  ) => {
    // 操作是插件级的：只更新目标行，不重读或重置其他插件。
    if (operation === 'uninstall') {
      setPlugins((current) => current.filter((plugin) => plugin.id !== pluginId));
    } else if (result && typeof result === 'object' && 'id' in result) {
      const status = result as PluginStatus;
      setPlugins((current) => {
        const exists = current.some((plugin) => plugin.id === status.id);
        return exists
          ? current.map((plugin) => plugin.id === status.id ? status : plugin)
          : [...current, status].sort((left, right) => left.id.localeCompare(right.id));
      });
    }

    // Slot 也只接收目标插件变更，由各宿主增删该插件贡献。
    window.dispatchEvent(new CustomEvent('tiangong:plugin-changed', {
      detail: { pluginId, operation },
    }));

    // 设置页贡献数量少，刷新它不会触碰其他插件运行状态。
    await refreshContributions().catch((error) => {
      showError('插件页面刷新失败', String(error));
    });
  };

  const runOperation = async (
    pluginId: string,
    operation: Operation,
    action: () => Promise<unknown>,
    successTitle: string,
    successMessage?: string,
  ) => {
    setActiveOperation({ pluginId, operation });
    try {
      const result = await action();
      await finishOperation(pluginId, operation, result);
      showSuccess(successTitle, successMessage);
      return true;
    } catch (error) {
      showError(`${operationLabel(operation)}失败`, String(error));
      const current = await api.listPlugins().catch(() => null);
      if (current) setPlugins(current);
      return false;
    } finally {
      setActiveOperation(null);
      setInstallProgress((prev) => {
        if (!(pluginId in prev)) return prev;
        const next = { ...prev };
        delete next[pluginId];
        return next;
      });
    }
  };

  const toggleEnabled = async (plugin: PluginStatus, enabled: boolean) => {
    await runOperation(
      plugin.id,
      enabled ? 'enable' : 'disable',
      () => api.setPluginEnabled(plugin.id, enabled),
      enabled ? '插件已启用' : '插件已停用',
      plugin.name,
    );
  };

  const reload = async (plugin: PluginStatus) => {
    await runOperation(
      plugin.id,
      'reload',
      () => api.reloadPlugin(plugin.id),
      '插件已热加载',
      plugin.name,
    );
  };

  const install = async (plugin: AvailablePlugin) => {
    await runOperation(
      plugin.id,
      'install',
      () => api.installPlugin(plugin.id),
      '插件已安装',
      `${plugin.name} ${plugin.version}`,
    );
  };

  const importLocal = async () => {
    let selected: string | string[] | null;
    try {
      selected = isWebHost()
        ? window.prompt('输入天工所在机器上的插件目录或 .tar.zst 归档的绝对路径')?.trim() || null
        : await open({
          directory: true,
          multiple: false,
          title: '选择本地插件目录',
        });
    } catch (error) {
      showError('选择失败', String(error));
      return;
    }
    if (typeof selected !== 'string') return;

    setActiveOperation({ pluginId: 'local', operation: 'import' });
    try {
      const plugin = await api.importLocalPlugin(selected);
      await finishOperation(plugin.id, 'import', plugin);
      showSuccess('插件已导入', `${plugin.name} ${plugin.manifest_version}`);
    } catch (error) {
      showError('导入失败', String(error));
      await refresh();
    } finally {
      setActiveOperation(null);
    }
  };

  // 打开默认插件推荐引导：拉取全部默认插件（含已安装与未安装），总是弹出对话框。
  const openRecommend = async () => {
    try {
      const list = await api.listAvailablePlugins();
      const defaults = list.filter((plugin) => plugin.is_default && plugin.supported);
      if (defaults.length === 0) {
        showError('无法获取默认插件', '插件目录为空或网络不可达');
        return;
      }
      setRecommendMissing(defaults);
    } catch (error) {
      showError('检测失败', String(error));
    }
  };

  const upgrade = async (plugin: PluginStatus) => {
    const release = availableById.get(plugin.id);
    await runOperation(
      plugin.id,
      'upgrade',
      () => api.upgradePlugin(plugin.id),
      '插件已升级',
      release ? `${plugin.name} ${release.version}` : plugin.name,
    );
  };

  const rollback = async (plugin: PluginStatus) => {
    await runOperation(
      plugin.id,
      'rollback',
      () => api.rollbackPlugin(plugin.id),
      '插件已回滚',
      plugin.name,
    );
  };

  const confirmUninstall = async () => {
    if (!uninstallTarget) return;
    const target = uninstallTarget;
    // 无效插件同样走卸载：后端识别登记表中的无效目录后按同一删除路径处理。
    const succeeded = await runOperation(
      target.id,
      'uninstall',
      () => api.uninstallPlugin(target.id, keepData),
      '插件已卸载',
      target.name,
    );
    if (succeeded) setUninstallTarget(null);
  };

  const isBusy = activeOperation !== null;

  return (
    <TooltipProvider>
      <div className="relative flex h-full min-w-0 flex-col overflow-hidden">
        <div className="flex min-h-16 shrink-0 items-center gap-3 border-b px-4 sm:px-6">
          <div className="min-w-0">
            <h2 className="text-base font-semibold">插件管理</h2>
            <p className="truncate text-xs text-muted-foreground">
              {plugins.length} 个已安装，{available.length} 个可用
            </p>
          </div>
          <div className="ml-auto flex shrink-0 items-center gap-1">
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="ghost" size="icon" className="h-8 w-8" asChild>
                  <a
                    href={PLUGIN_DEVELOPMENT_DOC_URL}
                    target="_blank"
                    rel="noopener noreferrer"
                    aria-label="打开插件开发文档"
                  >
                    <BookOpen />
                  </a>
                </Button>
              </TooltipTrigger>
              <TooltipContent>插件开发文档</TooltipContent>
            </Tooltip>
            <Button
              variant="outline"
              size="sm"
              className="h-8 gap-1.5 px-2 text-xs"
              onClick={() => void openRecommend()}
              disabled={loading || isBusy}
            >
              <Sparkles className="h-3.5 w-3.5" />
              推荐
            </Button>
            <IconAction
              label="导入本地插件"
              onClick={() => void importLocal()}
              disabled={loading || isBusy}
              working={activeOperation?.operation === 'import'}
            >
              <FolderInput />
            </IconAction>
            <IconAction
              label="刷新插件目录和状态"
              onClick={() => void refresh()}
              disabled={loading || isBusy}
            >
              <RefreshCw className={loading ? 'animate-spin' : ''} />
            </IconAction>
          </div>
        </div>

        <Tabs defaultValue="installed" className="flex min-h-0 flex-1 flex-col">
          <div className="flex shrink-0 items-center gap-2 border-b px-4 py-2 sm:px-6">
            <TabsList className="grid h-9 w-full max-w-72 grid-cols-3">
              <TabsTrigger value="installed" className="py-1 text-xs">
                已安装
              </TabsTrigger>
              <TabsTrigger value="available" className="py-1 text-xs">
                可安装
              </TabsTrigger>
              <TabsTrigger value="trust" className="py-1 text-xs">
                信任
              </TabsTrigger>
            </TabsList>
            <div className="relative ml-auto w-full max-w-48">
              <Search className="pointer-events-none absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="搜索插件"
                className="h-9 pl-8 text-xs"
              />
            </div>
          </div>

          <TabsContent value="installed" className="m-0 min-h-0 flex-1 overflow-y-auto px-4 sm:px-6">
            {loading && plugins.length === 0 ? (
              <LoadingState label="正在读取插件状态" />
            ) : plugins.length === 0 ? (
              <EmptyState label="暂无已安装插件" />
            ) : filteredPlugins.length === 0 ? (
              <EmptyState label="没有匹配的插件" />
            ) : (
              filteredPlugins.map((plugin) => (
                <InstalledPluginRow
                  key={plugin.id}
                  plugin={plugin}
                  release={availableById.get(plugin.id)}
                  activeOperation={activeOperation}
                  disabled={isBusy}
                  progress={installProgress[plugin.id] ?? null}
                  onToggle={toggleEnabled}
                  onReload={reload}
                  onUpgrade={upgrade}
                  onRollback={rollback}
                  onUninstall={(target) => {
                    setKeepData(true);
                    setUninstallTarget(target);
                  }}
                />
              ))
            )}
          </TabsContent>

          <TabsContent value="available" className="m-0 min-h-0 flex-1 overflow-y-auto px-4 sm:px-6">
            {loading && available.length === 0 ? (
              <LoadingState label="正在读取 OSS 插件目录" />
            ) : catalogError ? (
              <div className="flex min-h-32 items-start gap-3 py-8 text-sm text-destructive">
                <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
                <span className="break-all">{catalogError}</span>
              </div>
            ) : available.length === 0 ? (
              <EmptyState label="暂无可安装插件" />
            ) : filteredAvailable.length === 0 ? (
              <EmptyState label="没有匹配的插件" />
            ) : (
              filteredAvailable.map((plugin, index) => (
                <AvailablePluginRow
                  key={plugin.id}
                  plugin={plugin}
                  index={index}
                  activeOperation={activeOperation}
                  disabled={isBusy}
                  progress={installProgress[plugin.id] ?? null}
                  onInstall={install}
                />
              ))
            )}
          </TabsContent>

          <TabsContent value="trust" className="m-0 min-h-0 flex-1 overflow-y-auto px-4 sm:px-6">
            <TrustManagerSection />
          </TabsContent>
        </Tabs>
      </div>

      <Dialog open={uninstallTarget !== null} onOpenChange={(open) => !open && setUninstallTarget(null)}>
        <DialogContent className="mx-4 w-[calc(100%-2rem)] max-w-md">
          <DialogHeader>
            <DialogTitle>卸载 {uninstallTarget?.name}</DialogTitle>
            <DialogDescription>插件能力将立即从当前应用中移除。</DialogDescription>
          </DialogHeader>
          <div className="flex items-center justify-between gap-4 rounded-md border p-3">
            <div className="min-w-0">
              <p className="text-sm font-medium">保留插件数据</p>
              <p className="text-xs text-muted-foreground">重新安装后可继续使用现有数据</p>
            </div>
            <Switch
              checked={keepData}
              onCheckedChange={setKeepData}
              disabled={activeOperation?.operation === 'uninstall'}
              aria-label="保留插件数据"
            />
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setUninstallTarget(null)} disabled={isBusy}>
              取消
            </Button>
            <Button variant="destructive" onClick={() => void confirmUninstall()} disabled={isBusy}>
              {activeOperation?.operation === 'uninstall' && <Loader2 className="animate-spin" />}
              卸载
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <DefaultPluginOnboarding
        missing={recommendMissing}
        writeCompletionMarker={false}
        onOpenChange={(open) => {
          if (!open) setRecommendMissing(null);
        }}
        onComplete={() => {
          void refresh();
          void refreshContributions();
        }}
      />
    </TooltipProvider>
  );
}

function InstalledPluginRow({
  plugin,
  release,
  activeOperation,
  disabled,
  progress,
  onToggle,
  onReload,
  onUpgrade,
  onRollback,
  onUninstall,
}: {
  plugin: PluginStatus;
  release?: AvailablePlugin;
  activeOperation: ActiveOperation | null;
  disabled: boolean;
  progress: number | null;
  onToggle: (plugin: PluginStatus, enabled: boolean) => Promise<void>;
  onReload: (plugin: PluginStatus) => Promise<void>;
  onUpgrade: (plugin: PluginStatus) => Promise<void>;
  onRollback: (plugin: PluginStatus) => Promise<void>;
  onUninstall: (plugin: PluginStatus) => void;
}) {
  const working = activeOperation?.pluginId === plugin.id;
  const healthy = plugin.state === 'loaded';
  const invalid = plugin.state === 'invalid';
  const currentVersion = plugin.loaded_version ?? plugin.manifest_version;
  const canUpgrade = Boolean(
    release?.supported && isRemotePluginVersionNewer(currentVersion, release.version),
  );
  const sidecar = getSidecarPresentation(plugin);
  const StatusIcon = healthy ? CheckCircle2 : plugin.state === 'disabled' ? Circle : AlertCircle;

  return (
    <div className="border-b py-2.5 last:border-b-0">
      <div className="flex items-center gap-2.5">
        <StatusIcon
          className={`h-4 w-4 shrink-0 ${
            healthy
              ? 'text-emerald-500'
              : plugin.state === 'disabled'
                ? 'text-muted-foreground'
                : 'text-amber-500'
          }`}
        />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
            <span className="break-words text-sm font-medium text-foreground">{plugin.name}</span>
            <Badge variant={healthy ? 'secondary' : 'outline'} className="px-1.5 py-0 text-[10px]">
              {stateLabel[plugin.state]}
            </Badge>
            {canUpgrade && (
              <Badge className="border-primary/30 bg-primary/10 px-1.5 py-0 text-[10px] text-primary">
                发现新版本
              </Badge>
            )}
            {!invalid && (
              <span className="inline-flex items-center gap-1">
                <span className="text-muted-foreground/70">v{currentVersion}</span>
                {canUpgrade && release && (
                  <span className="font-medium text-primary">→ {release.version}</span>
                )}
              </span>
            )}
            {!invalid && (
              <span className={`inline-flex items-center gap-1 ${sidecar.className}`}>
                <span className={`h-1.5 w-1.5 rounded-full ${sidecar.dotClassName}`} />
                {sidecar.label}
              </span>
            )}
          </div>

          {plugin.description && (
            <p
              className="mt-1 truncate text-xs leading-5 text-muted-foreground"
              title={plugin.description}
            >
              {plugin.description}
            </p>
          )}

          {plugin.last_error && (
            <p className="mt-1 break-words text-xs leading-5 text-destructive">{plugin.last_error}</p>
          )}

          {progress !== null && (
            <div className="mt-1.5 h-1 w-full max-w-64 overflow-hidden rounded-full bg-muted">
              <div
                className="h-full bg-primary transition-[width] duration-150"
                style={{ width: `${progress}%` }}
              />
            </div>
          )}

          <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
            {invalid ? (
              <IconAction
                label="卸载插件"
                className="h-7 w-7"
                onClick={() => onUninstall(plugin)}
                disabled={disabled}
                destructive
              >
                <Trash2 className="h-3.5 w-3.5" />
              </IconAction>
            ) : (
              <>
                {canUpgrade && (
                  <Button
                    size="sm"
                    className="h-7 gap-1 px-2 text-xs"
                    onClick={() => void onUpgrade(plugin)}
                    disabled={disabled}
                  >
                    {working && activeOperation?.operation === 'upgrade' ? (
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    ) : (
                      <ArrowUpCircle className="h-3.5 w-3.5" />
                    )}
                    升级
                  </Button>
                )}
                <IconAction
                  label="重新加载插件"
                  className="h-7 w-7"
                  onClick={() => void onReload(plugin)}
                  disabled={disabled || !plugin.enabled}
                  working={working && activeOperation?.operation === 'reload'}
                >
                  <RotateCw className="h-3.5 w-3.5" />
                </IconAction>
                <IconAction
                  label="回滚到上一版本"
                  className="h-7 w-7"
                  onClick={() => void onRollback(plugin)}
                  disabled={disabled || !plugin.can_rollback}
                  working={working && activeOperation?.operation === 'rollback'}
                >
                  <Undo2 className="h-3.5 w-3.5" />
                </IconAction>
                <IconAction
                  label="卸载插件"
                  className="h-7 w-7"
                  onClick={() => onUninstall(plugin)}
                  disabled={disabled}
                  destructive
                >
                  <Trash2 className="h-3.5 w-3.5" />
                </IconAction>
              </>
            )}
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-2 pl-2">
          {invalid ? null : working && ['enable', 'disable'].includes(activeOperation?.operation ?? '') ? (
            <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
          ) : (
            <Switch
              checked={plugin.enabled}
              onCheckedChange={(enabled) => void onToggle(plugin, enabled)}
              disabled={disabled}
              aria-label={`${plugin.enabled ? '停用' : '启用'} ${plugin.name}`}
            />
          )}
        </div>
      </div>
    </div>
  );
}

function getSidecarPresentation(plugin: PluginStatus) {
  if (!plugin.has_sidecar) {
    return {
      label: '无需后台服务',
      className: 'text-muted-foreground',
      dotClassName: 'bg-muted-foreground/60',
    };
  }
  if (plugin.sidecar_running) {
    return {
      label: '后台服务运行中',
      className: 'text-emerald-500',
      dotClassName: 'bg-emerald-500',
    };
  }
  if (!plugin.enabled || plugin.state === 'disabled') {
    return {
      label: '后台服务已停用',
      className: 'text-muted-foreground',
      dotClassName: 'bg-muted-foreground/60',
    };
  }
  if (plugin.last_error || plugin.state === 'error' || plugin.state === 'degraded') {
    return {
      label: '后台服务异常',
      className: 'text-destructive',
      dotClassName: 'bg-destructive',
    };
  }
  return {
    label: '后台服务按需启动',
    className: 'text-muted-foreground',
    dotClassName: 'bg-amber-500',
  };
}

function AvailablePluginRow({
  plugin,
  index,
  activeOperation,
  disabled,
  progress,
  onInstall,
}: {
  plugin: AvailablePlugin;
  index: number;
  activeOperation: ActiveOperation | null;
  disabled: boolean;
  progress: number | null;
  onInstall: (plugin: AvailablePlugin) => Promise<void>;
}) {
  const installing = activeOperation?.pluginId === plugin.id && activeOperation.operation === 'install';
  return (
    <div>
      {index > 0 && <Separator />}
      <div className="flex items-start gap-3 py-2.5">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span className="break-words text-sm font-medium">{plugin.name}</span>
            <Badge variant="outline" className="px-1.5 py-0 text-[10px]">{plugin.version}</Badge>
            {!plugin.supported && <Badge variant="outline" className="px-1.5 py-0 text-[10px]">当前平台不可用</Badge>}
            {plugin.installed_version && <Badge variant="secondary" className="px-1.5 py-0 text-[10px]">已安装</Badge>}
          </div>
          {plugin.description && (
            <p className="mt-1 break-words text-xs leading-5 text-muted-foreground">
              {plugin.description}
            </p>
          )}
          {progress !== null && (
            <div className="mt-1.5 h-1 w-full overflow-hidden rounded-full bg-muted">
              <div
                className="h-full bg-primary transition-[width] duration-150"
                style={{ width: `${progress}%` }}
              />
            </div>
          )}
        </div>
        <IconAction
          label="安装插件"
          className="h-7 w-7 shrink-0"
          onClick={() => void onInstall(plugin)}
          disabled={disabled || !plugin.supported || plugin.installed_version !== null}
          working={installing}
        >
          <Download className="h-3.5 w-3.5" />
        </IconAction>
      </div>
    </div>
  );
}

function IconAction({
  label,
  working = false,
  destructive = false,
  className,
  children,
  ...props
}: React.ButtonHTMLAttributes<HTMLButtonElement> & {
  label: string;
  working?: boolean;
  destructive?: boolean;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          className={`h-8 w-8 shrink-0 ${destructive ? 'hover:bg-destructive/10 hover:text-destructive' : ''} ${className ?? ''}`}
          aria-label={label}
          {...props}
        >
          {working ? <Loader2 className="animate-spin" /> : children}
        </Button>
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}

function LoadingState({ label }: { label: string }) {
  return (
    <div className="flex h-32 items-center justify-center text-sm text-muted-foreground">
      <Loader2 className="mr-2 h-4 w-4 animate-spin" />
      {label}
    </div>
  );
}

function EmptyState({ label }: { label: string }) {
  return (
    <div className="flex h-32 items-center justify-center text-sm text-muted-foreground">{label}</div>
  );
}

function operationLabel(operation: Operation) {
  const labels: Record<Operation, string> = {
    enable: '启用',
    disable: '停用',
    import: '导入',
    install: '安装',
    reload: '热加载',
    upgrade: '升级',
    rollback: '回滚',
    uninstall: '卸载',
  };
  return labels[operation];
}


type TrustManagerSectionProps = Record<string, never>;

/// 插件信任管理：本机用户密钥指纹 + 第三方发布者公钥（导入 / 指纹 / 移除）。
/// 导入只在此界面手动进行（指纹需与开发者公开渠道人工比对），插件与
/// Agent 均无导入通道。
function TrustManagerSection({}: TrustManagerSectionProps) {
  const { showError, showSuccess } = useToast();
  const [publishers, setPublishers] = useState<TrustedPublisherEntry[]>([]);
  const [userFingerprint, setUserFingerprint] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [publisherId, setPublisherId] = useState('');
  const [publicKeyText, setPublicKeyText] = useState('');
  const [importing, setImporting] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [entries, fingerprint] = await Promise.all([
        api.listTrustedPublishers(),
        api.userKeyFingerprint(),
      ]);
      setPublishers(entries);
      setUserFingerprint(fingerprint);
    } catch (error) {
      showError(`读取信任信息失败：${String(error)}`);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const importPublisher = useCallback(async () => {
    const publisher = publisherId.trim();
    const publicKey = publicKeyText.trim();
    if (!publisher || !publicKey) {
      showError('请填写发布者标识与公钥内容');
      return;
    }
    setImporting(true);
    try {
      const entry = await api.importTrustedPublisher(publisher, publicKey);
      showSuccess(
        `已导入 ${entry.publisher}（指纹 ${entry.fingerprint}）——请与开发者公开渠道的指纹比对确认。`,
      );
      setPublisherId('');
      setPublicKeyText('');
      await refresh();
    } catch (error) {
      showError(`导入失败：${String(error)}`);
    } finally {
      setImporting(false);
    }
  }, [publisherId, publicKeyText, refresh]);

  const removePublisher = useCallback(
    async (publisher: string) => {
      try {
        await api.removeTrustedPublisher(publisher);
        showSuccess(`已移除 ${publisher}（其签名插件下次启动将失效）`);
        await refresh();
      } catch (error) {
        showError(`移除失败：${String(error)}`);
      }
    },
    [refresh],
  );

  const pickKeyFile = useCallback(async () => {
    const picked = isWebHost()
      ? window.prompt('输入公钥文件（.pub / .txt）的绝对路径')?.trim() || null
      : await open({
        multiple: false,
        filters: [{ name: '公钥文件', extensions: ['pub', 'txt'] }],
      });
    if (!picked) return;
    try {
      const content = await api.readPublicKeyFile(picked);
      setPublicKeyText(content.trim());
    } catch (error) {
      showError(`读取公钥文件失败：${String(error)}`);
    }
  }, [showError]);

  return (
    <div className="space-y-6 py-4">
      <section className="space-y-2">
        <h4 className="text-sm font-medium">本机签名密钥</h4>
        <p className="text-xs text-muted-foreground">
          自建插件（插件创作链）安装时自动以此密钥签名，全程免确认；指纹用于
          在你自己的多台设备间核验密钥一致性。
        </p>
        {userFingerprint ? (
          <p className="font-mono text-xs">
            指纹：<span className="text-primary">{userFingerprint}</span>
          </p>
        ) : (
          <p className="text-xs text-muted-foreground">
            尚未生成（首次创作安装插件时自动生成）。
          </p>
        )}
      </section>

      <Separator />

      <section className="space-y-3">
        <h4 className="text-sm font-medium">第三方开发者公钥</h4>
        <p className="text-xs text-muted-foreground">
          导入第三方开发者的公钥后，即可安装该开发者签名的插件（本地导入插件
          包）。导入后请将指纹与开发者公开渠道（GitHub / 官网）公布的指纹比对。
        </p>
        <div className="space-y-2">
          <Input
            value={publisherId}
            onChange={(e) => setPublisherId(e.target.value)}
            placeholder="发布者标识（如 acme-dev，来自插件签名清单）"
            className="h-8 text-xs"
          />
          <textarea
            value={publicKeyText}
            onChange={(e) => setPublicKeyText(e.target.value)}
            placeholder="公钥内容（base64 单行，即开发者公布的 .pub 文件内容）"
            rows={3}
            className="w-full rounded-md border bg-background px-3 py-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
          />
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              className="h-8 text-xs"
              onClick={() => void importPublisher()}
              disabled={importing || loading}
            >
              {importing ? <Loader2 className="mr-1 h-3 w-3 animate-spin" /> : null}
              导入公钥
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="h-8 text-xs"
              onClick={() => void pickKeyFile()}
            >
              <FolderInput className="mr-1 h-3 w-3" />
              选择文件
            </Button>
          </div>
        </div>

        {loading ? (
          <LoadingState label="正在读取信任公钥" />
        ) : publishers.length === 0 ? (
          <EmptyState label="尚未导入第三方公钥" />
        ) : (
          <div className="space-y-2">
            {publishers.map((entry) => (
              <div
                key={entry.publisher}
                className="flex items-center justify-between gap-3 rounded-md border px-3 py-2"
              >
                <div className="min-w-0">
                  <p className="truncate text-xs font-medium">{entry.publisher}</p>
                  <p className="truncate font-mono text-xs text-muted-foreground">
                    指纹 {entry.fingerprint} · 导入于 {entry.imported_at}
                  </p>
                </div>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-8 text-xs text-destructive hover:text-destructive"
                  onClick={() => void removePublisher(entry.publisher)}
                  disabled={loading}
                >
                  <Trash2 className="mr-1 h-3 w-3" />
                  移除
                </Button>
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}
