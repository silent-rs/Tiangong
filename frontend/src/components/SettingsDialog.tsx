import { useState, useEffect, useRef, useCallback } from 'react';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from './ui/dialog';
import { Button } from './ui/button';
import { Input } from './ui/input';
import { Label } from './ui/label';
import { RequestHeadersEditor } from './RequestHeadersEditor';
import { Badge } from './ui/badge';
import { Card, CardContent } from './ui/card';
import { Switch } from './ui/switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from './ui/tabs';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from './ui/select';
import { Settings, Eye, EyeOff, Puzzle, Plus, Trash2, Loader2, Github, Globe, Edit2, RefreshCw, Info, FolderOpen, Save, ShieldCheck, X, Bot as BotIcon, Package, Brain, HardDriveDownload } from 'lucide-react';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import type { DownloadEvent, Update } from '@tauri-apps/plugin-updater';
import { api } from '@/api/tauri';
import { startWindowDrag } from '@/lib/windowDrag';
import type { ServerConfig, ModelsConfigView, ProviderConfigView, ModelEntryView, ModelCapabilityInfo, TrashedSession, SandboxPolicyView } from '@/api/tauri';
import { useStore } from '@/store/useStore';
import { useToast } from './Toast';
import { WebhookPanel } from './automation/WebhookPanel';
import { BotPanel } from './bots/BotPanel';
import { PluginIframe } from './PluginIframe';
import { PluginSandbox } from './PluginSandbox';
import { PluginManagerSettings } from './PluginManagerSettings';
import { type SlotContributionEntry } from '../api/tauri';

type SaveStatus = 'idle' | 'saving' | 'saved' | 'error';

function contributionIcon(name: string) {
  const Icon = name === 'brain' ? Brain : Puzzle;
  return <Icon className="w-4 h-4 sm:mr-2" />;
}

export function SettingsDialog() {
  const [open, setOpen] = useState(false);
  const [activeTab, setActiveTab] = useState('agent');
  const [saveStatus, setSaveStatus] = useState<SaveStatus>('idle');
  const [pluginContributions, setPluginContributions] = useState<SlotContributionEntry[]>([]);
  const [pluginStatuses, setPluginStatuses] = useState<import('../api/tauri').PluginStatus[]>([]);
  const [availablePlugins, setAvailablePlugins] = useState<import('../api/tauri').AvailablePlugin[]>([]);
  const [pluginCatalogError, setPluginCatalogError] = useState<string | null>(null);
  const [pluginDataLoaded, setPluginDataLoaded] = useState(false);
  const [, setPluginDataLoading] = useState(false);
  const [pluginRefreshMask, setPluginRefreshMask] = useState(false);
  const pendingSettingsTab = useStore((s) => s.pendingSettingsTab);
  const setPendingSettingsTab = useStore((s) => s.setPendingSettingsTab);

  // 加载 settings.plugin-page 挂载点的贡献（有页面的才显示设置页入口）。
  useEffect(() => {
    if (open && pluginContributions.length === 0) {
      api.listSlotContributions('settings.plugin-page')
        .then((entries) => setPluginContributions(entries.filter((e) => e.has_view)))
        .catch(() => {});
    }
  }, [open, pluginContributions.length]);

  useEffect(() => {
    if (!open || activeTab !== 'plugin-manager') return;

    let cancelled = false;
    // 每次进入插件管理都读取本地运行时状态，确保启停后重新进入显示真实值。
    // 远程目录只在首次进入时后台加载，不能阻塞本地状态展示。
    setPluginDataLoading(true);
    void api.listPlugins()
      .then((installed) => {
        if (!cancelled) setPluginStatuses(installed);
      })
      .finally(() => {
        if (!cancelled) setPluginDataLoading(false);
      });

    if (!pluginDataLoaded) {
      void api.listAvailablePlugins()
        .then((available) => {
          if (cancelled) return;
          setAvailablePlugins(available);
          setPluginCatalogError(null);
        })
        .catch((error) => {
          if (!cancelled) setPluginCatalogError(String(error));
        })
        .finally(() => {
          if (!cancelled) setPluginDataLoaded(true);
        });
    }

    return () => {
      cancelled = true;
    };
  }, [activeTab, open, pluginDataLoaded]);

  // 响应外部触发打开设置页
  useEffect(() => {
    if (pendingSettingsTab) {
      setActiveTab(pendingSettingsTab);
      setOpen(true);
      setPendingSettingsTab(null);
    }
  }, [pendingSettingsTab, setPendingSettingsTab]);

  useEffect(() => {
    if (!open) {
      window.dispatchEvent(new Event('tiangong:restore-browser-panel'));
    }
  }, [open]);

  return (
    <>
      <Button
        variant="ghost"
        className="w-full justify-start"
        onClick={() => setOpen(true)}
      >
        <Settings className="w-4 h-4 mr-2" />
        设置
      </Button>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent showCloseButton={false} className="w-screen max-w-none h-screen max-h-screen overflow-hidden flex flex-col rounded-none border-0 p-0">
          {/* 顶部标题栏 — 可拖动窗口 */}
          <header
            className="flex h-12 shrink-0 items-center border-b pr-4 select-none"
            style={{ paddingLeft: navigator.platform.includes('Mac') ? '80px' : '16px' }}
            onMouseDown={startWindowDrag}
          >
            <span className="text-sm font-medium">设置</span>
            <span className={`ml-auto text-xs flex items-center transition-opacity ${saveStatus === 'idle' ? 'opacity-0' : 'opacity-100'} ${saveStatus === 'error' ? 'text-destructive' : 'text-muted-foreground'}`}>
              {saveStatus === 'saving' && (
                <><Loader2 className="w-3 h-3 mr-1 animate-spin" />保存中...</>
              )}
              {(saveStatus === 'saved' || saveStatus === 'idle') && '已自动保存'}
              {saveStatus === 'error' && '保存失败'}
            </span>
          </header>

          <Tabs value={activeTab} onValueChange={setActiveTab} className="flex min-h-0 min-w-0 flex-1 overflow-hidden">
            <aside className="w-14 sm:w-60 shrink-0 border-r bg-muted/30 flex flex-col">
              <TabsList className="h-auto w-full flex-1 flex-col items-stretch justify-start rounded-none bg-transparent p-2 pt-4">
                <TabsTrigger value="agent" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                  <ShieldCheck className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">智能体</span>
                </TabsTrigger>
                <TabsTrigger value="llm" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                  <Settings className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">模型配置</span>
                </TabsTrigger>
                <TabsTrigger value="server" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                  <Globe className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">Server</span>
                </TabsTrigger>
                <TabsTrigger value="bots" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                  <BotIcon className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">移动端控制</span>
                </TabsTrigger>
                <TabsTrigger value="sandbox" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                  <ShieldCheck className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">沙箱管理</span>
                </TabsTrigger>
                <TabsTrigger value="plugin-manager" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                  <Package className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">插件管理</span>
                </TabsTrigger>
                {pluginContributions.map((entry) => (
                  <TabsTrigger key={`plugin:${entry.plugin_id}:${entry.contribution_id}`} value={`plugin:${entry.plugin_id}`} className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                    {contributionIcon(entry.icon)}
                    <span className="sr-only sm:not-sr-only">{entry.title}</span>
                  </TabsTrigger>
                ))}
                <TabsTrigger value="data-cleanup" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                  <HardDriveDownload className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">数据清理</span>
                </TabsTrigger>
                <TabsTrigger value="about" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                  <Info className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">关于与更新</span>
                </TabsTrigger>
              </TabsList>
              <div className="border-t p-2">
                <Button
                  variant="ghost"
                  className="w-full justify-center px-0 sm:justify-start sm:px-4"
                  onClick={() => setOpen(false)}
                >
                  <X className="w-4 h-4 sm:mr-2" />
                  <span className="sr-only sm:not-sr-only">退出设置</span>
                </Button>
              </div>
            </aside>

            <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
              <TabsContent value="agent" className="m-0 flex-1 min-h-0 overflow-hidden flex flex-col">
                <AgentSettings onSaveStatusChange={setSaveStatus} />
              </TabsContent>
              <TabsContent value="llm" className="m-0 flex-1 min-h-0 overflow-hidden">
                <LLMSettings onSaveStatusChange={setSaveStatus} />
              </TabsContent>
              <TabsContent value="server" className="m-0 flex-1 min-h-0 overflow-y-auto">
                <ServerSettings />
              </TabsContent>
              <TabsContent value="bots" className="m-0 flex-1 min-h-0 overflow-y-auto">
                <BotPanel />
              </TabsContent>
              <TabsContent value="sandbox" className="m-0 flex-1 min-h-0 overflow-y-auto">
                <SandboxSettings onSaveStatusChange={setSaveStatus} />
              </TabsContent>
              <TabsContent value="plugin-manager" className="m-0 flex-1 min-h-0 overflow-hidden">
                <PluginManagerSettings
                  onContributionsChanged={setPluginContributions}
                  initialPlugins={pluginStatuses}
                  initialAvailable={availablePlugins}
                  initialCatalogError={pluginCatalogError}
                  onRefreshStateChange={setPluginRefreshMask}
                />
              </TabsContent>
              <TabsContent value="data-cleanup" className="m-0 flex-1 min-h-0 overflow-y-auto">
                <DataCleanupSettings />
              </TabsContent>
              {pluginContributions.map((entry) => (
                <TabsContent key={`plugin:${entry.plugin_id}:${entry.contribution_id}`} value={`plugin:${entry.plugin_id}`} className="m-0 flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
                  <PluginView contribution={entry} />
                </TabsContent>
              ))}
              <TabsContent value="about" className="m-0 flex-1 min-h-0 overflow-y-auto">
                <AppUpdateSettings />
              </TabsContent>
            </div>
          </Tabs>
          {pluginRefreshMask && (
            <div
              className="fixed inset-0 z-[100] flex items-center justify-center bg-background/80 backdrop-blur-[2px]"
              role="status"
              aria-live="polite"
              aria-label="正在刷新插件状态"
            >
              <div className="flex items-center gap-2 rounded-md border bg-background px-4 py-3 text-sm shadow-xl">
                <Loader2 className="h-4 w-4 animate-spin text-primary" />
                正在刷新插件目录和运行状态…
              </div>
            </div>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

function AgentSettings({ onSaveStatusChange }: { onSaveStatusChange: (status: SaveStatus) => void }) {
  const [defaultTrustMode, setDefaultTrustMode] = useState('full_trust');
  const { showError } = useToast();
  const { workspaceDir, setWorkspaceDir } = useStore();
  const [editWorkspaceDir, setEditWorkspaceDir] = useState(workspaceDir);
  const [isSavingWorkspace, setIsSavingWorkspace] = useState(false);
  const { showSuccess } = useToast();
  useEffect(() => {
    setEditWorkspaceDir(workspaceDir);
  }, [workspaceDir]);

  useEffect(() => {
    api.getDefaultTrustMode()
      .then((mode) => {
        setDefaultTrustMode(mode);
      })
      .catch((error) => {
        console.error('加载 Agent 配置失败:', error);
        showError('加载失败', '无法加载 Agent 配置');
      });
  }, [showError]);

  const handleTrustModeChange = async (mode: string) => {
    setDefaultTrustMode(mode);
    onSaveStatusChange('saving');
    try {
      await api.setDefaultTrustMode(mode);
      onSaveStatusChange('saved');
      setTimeout(() => onSaveStatusChange('idle'), 2000);
    } catch (error) {
      console.error('保存默认审核模式失败:', error);
      onSaveStatusChange('error');
      showError('保存失败', '无法保存默认审核模式');
    }
  };

  const handleSelectDirectory = async () => {
    try {
      const selected = await openDialog({
        directory: true,
        multiple: false,
        defaultPath: editWorkspaceDir || workspaceDir || undefined,
        title: '选择工作区目录',
      });
      if (selected && typeof selected === 'string') {
        setEditWorkspaceDir(selected);
      }
    } catch (error) {
      console.error('选择工作区目录失败:', error);
      showError('选择失败', '无法打开目录选择器');
    }
  };

  const handleSaveWorkspace = async () => {
    const nextWorkspaceDir = editWorkspaceDir.trim();
    if (!nextWorkspaceDir) {
      showError('路径为空', '请选择或输入工作区目录');
      return;
    }
    setIsSavingWorkspace(true);
    try {
      await setWorkspaceDir(nextWorkspaceDir);
      showSuccess('工作区已更新', '未指定对话目录时会默认使用该工作区');
    } catch (error) {
      console.error('保存工作区失败:', error);
      showError('保存失败', error instanceof Error ? error.message : '无法保存工作区目录');
    } finally {
      setIsSavingWorkspace(false);
    }
  };

  return (
    <div className="p-4 flex flex-col flex-1 min-h-0 overflow-hidden">
      {/* 固定区域：审核模式 + 工作区 */}
      <div className="shrink-0 space-y-5 pb-4">
        <div className="space-y-2">
          <Label>新对话默认审核权限</Label>
          <Select value={defaultTrustMode} onValueChange={handleTrustModeChange}>
            <SelectTrigger className="w-56">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="full_trust">完全信任</SelectItem>
              <SelectItem value="supervised">监督审核</SelectItem>
            </SelectContent>
          </Select>
        </div>

        <div className="space-y-2">
          <Label htmlFor="workspacePath">默认工作区目录</Label>
          <div className="flex gap-2">
            <Input
              id="workspacePath"
              value={editWorkspaceDir}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) => setEditWorkspaceDir(e.target.value)}
              placeholder="选择或输入工作区目录"
              disabled={isSavingWorkspace}
            />
            <Button variant="outline" onClick={handleSelectDirectory} disabled={isSavingWorkspace}>
              <FolderOpen className="w-4 h-4 mr-2" />
              选择
            </Button>
          </div>
          <div className="flex items-center justify-between min-h-7">
            <p className="text-xs text-muted-foreground">
              {workspaceDir
                ? <><span className="opacity-60">当前：</span><span className="font-mono">{workspaceDir}</span></>
                : <span className="opacity-60">未设置</span>
              }
            </p>
            {editWorkspaceDir.trim() !== workspaceDir && (
              <Button size="sm" className="h-6 text-xs" onClick={handleSaveWorkspace} disabled={isSavingWorkspace}>
                {isSavingWorkspace
                  ? <><Loader2 className="w-3 h-3 mr-1 animate-spin" />保存中...</>
                  : <><Save className="w-3 h-3 mr-1" />保存</>
                }
              </Button>
            )}
          </div>
        </div>
      </div>

    </div>
  );
}

const EMPTY_SANDBOX_POLICY: SandboxPolicyView = {
  directory_allowlist: [], environment_blocklist: [],
};

function SandboxSettings({ onSaveStatusChange }: { onSaveStatusChange: (status: SaveStatus) => void }) {
  // selector 订阅：preparing 轮询每秒刷新 store 时不触发其余字段订阅者重渲染。
  const sandboxDisabled = useStore((state) => state.sandboxDisabled);
  const loadSandboxDisabled = useStore((state) => state.loadSandboxDisabled);
  const setSandboxDisabled = useStore((state) => state.setSandboxDisabled);
  const sandboxState = useStore((state) => state.sandboxState);
  const loadSandboxState = useStore((state) => state.loadSandboxState);
  const [policy, setPolicy] = useState<SandboxPolicyView>(EMPTY_SANDBOX_POLICY);
  const [directoryInput, setDirectoryInput] = useState('');
  const [environmentInput, setEnvironmentInput] = useState('');
  const [confirmDisable, setConfirmDisable] = useState(false);
  const [checking, setChecking] = useState(false);
  const { showSuccess, showError } = useToast();
  useEffect(() => { if (sandboxDisabled === null) void loadSandboxDisabled(); }, [sandboxDisabled, loadSandboxDisabled]);
  useEffect(() => { if (sandboxState === null) void loadSandboxState(); }, [sandboxState, loadSandboxState]);
  useEffect(() => { void api.getSandboxPolicy().then(setPolicy).catch((e) => showError('加载失败', String(e))); }, [showError]);
  // 状态走全局 store：设置页修复成功后输入区"沙箱无效"指示同步消失。
  const refresh = useCallback(() => { void loadSandboxState(true); }, [loadSandboxState]);
  useEffect(refresh, [refresh]);
  useEffect(() => {
    if (sandboxState?.status !== 'preparing') return;
    const timer = window.setInterval(refresh, 1000);
    return () => window.clearInterval(timer);
  }, [refresh, sandboxState?.status]);
  const save = async (next: SandboxPolicyView) => {
    onSaveStatusChange('saving');
    try { const saved = await api.setSandboxPolicy(next); setPolicy(saved); onSaveStatusChange('saved'); setTimeout(() => onSaveStatusChange('idle'), 1500); }
    catch (e) { onSaveStatusChange('error'); showError('保存失败', String(e)); throw e; }
  };
  const addDirectory = async () => {
    const value = directoryInput.trim(); if (!value) return;
    await save({ ...policy, directory_allowlist: [...policy.directory_allowlist, value] }); setDirectoryInput('');
  };
  const chooseDirectory = async () => {
    const selected = await openDialog({ directory: true, multiple: false, title: '选择允许沙箱读写的目录' });
    if (typeof selected === 'string') await save({ ...policy, directory_allowlist: [...policy.directory_allowlist, selected] });
  };
  const addEnvironment = async () => {
    const value = environmentInput.trim(); if (!value) return;
    await save({ ...policy, environment_blocklist: [...policy.environment_blocklist, value] }); setEnvironmentInput('');
  };
  const toggle = async (disabled: boolean) => { await setSandboxDisabled(disabled); setConfirmDisable(false); showSuccess(disabled ? '沙箱已关闭' : '沙箱已开启', '按需进程将在下次使用时按新设置重建'); };
  const update = async () => { setChecking(true); try { const r = await api.upgradeLauncher(); refresh(); showSuccess('沙箱程序已就绪', `v${r.version}`); } catch (e) { showError('更新失败', String(e)); } finally { setChecking(false); } };
  const sandboxStatus = sandboxState === null
    ? '正在检测…'
    : sandboxState.status === 'ready'
      ? `已就绪${sandboxState.version ? ` v${sandboxState.version}` : ''}`
      : sandboxState.status === 'preparing'
        ? '正在准备…'
        : sandboxState.status === 'failed'
          ? `沙箱无效${sandboxState.failure ? `：${sandboxState.failure}` : ''}`
          : '未安装';
  const List = ({ items, remove }: { items: string[]; remove: (index: number) => void }) => <div className="rounded-lg border divide-y">{items.length === 0 ? <p className="p-4 text-sm text-muted-foreground">列表为空</p> : items.map((item, index) => <div key={`${item}-${index}`} className="flex items-center gap-3 px-3 py-2"><span className="min-w-0 flex-1 truncate font-mono text-xs" title={item}>{item}</span><Button variant="ghost" size="icon" className="h-7 w-7" onClick={() => remove(index)} aria-label={`删除 ${item}`}><Trash2 className="h-3.5 w-3.5" /></Button></div>)}</div>;
  return <div className="mx-auto max-w-3xl space-y-6 p-6">
    <div><h2 className="text-lg font-semibold">沙箱管理</h2></div>
    <Card><CardContent className="space-y-5 pt-6"><div className="flex items-center justify-between"><div><Label>按需进程沙箱</Label><p className="text-xs text-muted-foreground">Terminal、Command、解释器及其他按需进程</p></div><Switch checked={sandboxDisabled !== true} disabled={sandboxDisabled === null} onCheckedChange={(enabled) => enabled ? void toggle(false) : setConfirmDisable(true)} /></div><div className="flex items-center justify-between"><div><Label>沙箱程序</Label><p className="text-xs text-muted-foreground truncate" title={sandboxStatus}>{sandboxStatus}</p></div><Button variant="outline" disabled={checking} onClick={() => void update()}>{checking ? '正在更新…' : '检查并更新'}</Button></div></CardContent></Card>
    <Card><CardContent className="space-y-4 pt-6"><div><Label>目录白名单</Label><p className="text-xs text-muted-foreground">列表中的目录允许沙箱进程读取和写入，不限制用户选择范围。</p></div><List items={policy.directory_allowlist} remove={(index) => void save({ ...policy, directory_allowlist: policy.directory_allowlist.filter((_, i) => i !== index) })} /><div className="flex gap-2"><Input value={directoryInput} onChange={(e) => setDirectoryInput(e.target.value)} placeholder="输入目录路径" onKeyDown={(e) => { if (e.key === 'Enter') void addDirectory(); }} /><Button variant="outline" onClick={() => void chooseDirectory()}><FolderOpen className="mr-1 h-4 w-4" />浏览</Button><Button onClick={() => void addDirectory()}><Plus className="mr-1 h-4 w-4" />添加</Button></div></CardContent></Card>
    <Card><CardContent className="space-y-4 pt-6"><div><Label>环境变量黑名单</Label><p className="text-xs text-muted-foreground">列表中的变量不会传给沙箱进程；名称不区分大小写。</p></div><List items={policy.environment_blocklist} remove={(index) => void save({ ...policy, environment_blocklist: policy.environment_blocklist.filter((_, i) => i !== index) })} /><div className="flex gap-2"><Input value={environmentInput} onChange={(e) => setEnvironmentInput(e.target.value)} placeholder="例如 MY_SECRET_TOKEN" onKeyDown={(e) => { if (e.key === 'Enter') void addEnvironment(); }} /><Button onClick={() => void addEnvironment()}><Plus className="mr-1 h-4 w-4" />添加</Button></div></CardContent></Card>
    <p className="text-xs text-muted-foreground">沙箱管理面仍由宿主强制隔离，目录白名单不能覆盖管理项保护。</p>
    <Dialog open={confirmDisable} onOpenChange={setConfirmDisable}><DialogContent className="max-w-md"><DialogHeader><DialogTitle>关闭按需进程沙箱？</DialogTitle><DialogDescription>关闭后按需进程以完整用户权限运行。</DialogDescription></DialogHeader><DialogFooter><Button variant="outline" onClick={() => setConfirmDisable(false)}>取消</Button><Button variant="destructive" onClick={() => void toggle(true)}>关闭沙箱</Button></DialogFooter></DialogContent></Dialog>
  </div>;
}

type LLMSubTab = 'providers' | 'routing';
function LLMSettings({ onSaveStatusChange }: { onSaveStatusChange: (status: SaveStatus) => void }) {
  const [subTab, setSubTab] = useState<LLMSubTab>('providers');
  const [modelsConfig, setModelsConfig] = useState<ModelsConfigView>({
    providers: {},
    models: {},
    routing: {},
  });
  const [capabilities, setCapabilities] = useState<ModelCapabilityInfo[]>([]);
  const [isLoading, setIsLoading] = useState(false);
  const { showError } = useToast();
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const loadConfig = async () => {
    setIsLoading(true);
    try {
      const [cfg, caps] = await Promise.all([
        api.getModelsConfig(),
        api.getModelCapabilities(),
      ]);
      // Ensure default providers are always present
      const mergedProviders = { ...cfg.providers };
      for (const [name, providerConfig] of Object.entries(DEFAULT_PROVIDERS)) {
        if (!mergedProviders[name]) {
          mergedProviders[name] = { ...providerConfig };
        }
      }
      setModelsConfig({ ...cfg, providers: mergedProviders });
      setCapabilities(caps);
    } catch (error) {
      console.error('加载配置失败:', error);
      showError('加载失败', '无法加载模型配置，请重试');
    } finally {
      setIsLoading(false);
    }
  };

  useEffect(() => {
    loadConfig();
  }, []);

  // 自动保存：配置变更时 debounce 500ms 后保存到后端
  const autoSave = useCallback(async (config: ModelsConfigView) => {
    onSaveStatusChange('saving');
    try {
      await api.setModelsConfig(config);
      onSaveStatusChange('saved');
      setTimeout(() => onSaveStatusChange('idle'), 2000);
    } catch (error) {
      console.error('自动保存失败:', error);
      onSaveStatusChange('error');
      showError('保存失败', '无法保存模型配置');
    }
  }, [showError, onSaveStatusChange]);

  const handleChange = useCallback((newConfig: ModelsConfigView) => {
    setModelsConfig(newConfig);
    // debounce 自动保存
    if (saveTimerRef.current) {
      clearTimeout(saveTimerRef.current);
    }
    saveTimerRef.current = setTimeout(() => {
      autoSave(newConfig);
    }, 500);
  }, [autoSave]);

  // 清理 timer
  useEffect(() => {
    return () => {
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
      }
    };
  }, []);

  if (isLoading) {
    return (
      <div className="flex items-center justify-center py-8">
        <Loader2 className="w-6 h-6 animate-spin text-primary mr-2" />
        <span className="text-sm text-muted-foreground">加载配置中...</span>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full">
      {/* 子标签栏 — 固定不动 */}
      <div className="flex gap-1 shrink-0 p-4 pb-0">
        {(['providers', 'routing'] as const).map((tab) => (
          <button
            key={tab}
            className={`px-3 py-1.5 text-sm rounded-md transition-colors ${
              subTab === tab
                ? 'bg-primary text-primary-foreground'
                : 'bg-muted text-muted-foreground hover:text-foreground'
            }`}
            onClick={() => setSubTab(tab)}
          >
            {tab === 'providers' ? '模型' : '路由'}
          </button>
        ))}
      </div>

      {/* 内容区域 */}
      <div className={`flex-1 min-h-0 ${subTab === 'providers' ? 'overflow-hidden' : 'overflow-y-auto p-4'}`}>
        {subTab === 'providers' && (
          <ProviderModelsView config={modelsConfig} onChange={handleChange} capabilities={capabilities} />
        )}
        {subTab === 'routing' && (
          <RoutingSection config={modelsConfig} onChange={handleChange} />
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 预设供应商
// ---------------------------------------------------------------------------

const DEFAULT_PROVIDERS: Record<string, ProviderConfigView> = {
  'DeepSeek': { base_url: 'https://api.deepseek.com', api_key: '', timeout_ms: 300000, protocol: 'deepseek' },
  '智谱': { base_url: 'https://open.bigmodel.cn/api/paas/v4', api_key: '', timeout_ms: 300000, protocol: 'openai_chatcompletions' },
};

interface UrlPreset {
  label: string;
  url: string;
  protocol: string;
}

const DEFAULT_PROVIDER_URL_PRESETS: Record<string, UrlPreset[]> = {
  '智谱': [
    { label: 'OpenAI 兼容（通用）', url: 'https://open.bigmodel.cn/api/paas/v4', protocol: 'openai_chatcompletions' },
    { label: 'OpenAI 兼容（Coding 套餐）', url: 'https://open.bigmodel.cn/api/coding/paas/v4', protocol: 'openai_chatcompletions' },
    { label: 'Anthropic 兼容（Coding 套餐）', url: 'https://open.bigmodel.cn/api/anthropic', protocol: 'anthropic' },
  ],
};

// 协议对应的默认 URL
const PROTOCOL_DEFAULTS: Record<string, string> = {
  openai: 'https://api.openai.com/v1',
  openai_chatcompletions: 'https://api.openai.com/v1',
  anthropic: 'https://api.anthropic.com',
};

// ---------------------------------------------------------------------------
// DeepSeek 余额查询
// ---------------------------------------------------------------------------

function ProviderBalanceSection({ providerName }: { providerName: string }) {
  const [balance, setBalance] = useState<import('@/api/tauri').ProviderBalance | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchBalance = async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await api.getProviderBalance(providerName);
      setBalance(result);
    } catch (e) {
      setError(e instanceof Error ? e.message : '查询失败');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="mt-3 pt-3 border-t">
      <div className="flex items-center gap-3 flex-wrap">
        <Label className="text-xs">账户余额</Label>
        <Button
          size="sm"
          variant="outline"
          className="h-6 text-xs px-2"
          onClick={fetchBalance}
          disabled={loading}
        >
          {loading ? '查询中...' : '查询余额'}
        </Button>
        {error && <span className="text-xs text-destructive">{error}</span>}
        {balance && (
          <div className="flex items-center gap-3 flex-wrap">
            <span className={`text-xs font-medium ${balance.is_available ? 'text-green-500' : 'text-destructive'}`}>
              {balance.is_available ? '可用' : '不可用'}
            </span>
            {balance.balance_infos.map((info, i) => (
              <span key={i} className="text-xs text-muted-foreground">
                {info.currency === 'CNY' ? '¥' : '$'}{info.total_balance}
                <span className="ml-1.5 opacity-70">充值 {info.topped_up_balance}</span>
                <span className="ml-1.5 opacity-70">赠金 {info.granted_balance}</span>
              </span>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 供应商与模型 — 分栏视图
// ---------------------------------------------------------------------------

// 原生支持图片理解、配置时默认自动勾选多模态能力的模型名前缀（用户可手动取消勾选）
const VISION_DEFAULT_MODEL_PREFIXES = ['deepseek-flash', 'glm-5.3-flash'];

function ProviderModelsView({
  config,
  onChange,
  capabilities,
}: {
  config: ModelsConfigView;
  onChange: (c: ModelsConfigView) => void;
  capabilities: ModelCapabilityInfo[];
}) {
  const providerKeys = Object.keys(config.providers);
  const defaultKeys = Object.keys(DEFAULT_PROVIDERS).filter((k) => providerKeys.includes(k));
  const customKeys = providerKeys.filter((k) => !(k in DEFAULT_PROVIDERS)).sort();
  const sortedProviderKeys = [...defaultKeys, ...customKeys];

  const [selectedProvider, setSelectedProvider] = useState(sortedProviderKeys[0] || '');
  const [showApiKey, setShowApiKey] = useState(false);

  // Add provider modal
  const [showAddProvider, setShowAddProvider] = useState(false);
  const [newProviderKey, setNewProviderKey] = useState('');
  const [newProviderDraft, setNewProviderDraft] = useState<ProviderConfigView>({
    base_url: '', api_key: '', timeout_ms: 300000, protocol: 'openai_chatcompletions',
  });
  const [showNewApiKey, setShowNewApiKey] = useState(false);

  // Model modal
  const [modelModalMode, setModelModalMode] = useState<'add' | 'edit' | null>(null);
  const [editingModelKey, setEditingModelKey] = useState('');
  const [modelDraft, setModelDraft] = useState<ModelEntryView>({
    provider: '', model: '', capabilities: [], options: {}, context_window: undefined,
  });
  const [availableModels, setAvailableModels] = useState<string[]>([]);
  const [isFetchingModels, setIsFetchingModels] = useState(false);
  const [ttsVoices, setTtsVoices] = useState<{ id: string; name: string; gender?: string }[]>([]);
  const [isFetchingVoices, setIsFetchingVoices] = useState(false);
  const { showError } = useToast();

  // Effective selected provider
  const activeProvider = (selectedProvider && config.providers[selectedProvider])
    ? selectedProvider
    : (sortedProviderKeys[0] || '');
  const selectedConfig = config.providers[activeProvider] || null;
  const providerModels = activeProvider
    ? Object.entries(config.models).filter(([, m]) => m.provider === activeProvider).sort(([a], [b]) => a.localeCompare(b))
    : [];

  // ---- Provider handlers ----
  const updateProviderField = (field: keyof ProviderConfigView, value: unknown) => {
    if (!activeProvider) return;
    const next = { ...config };
    next.providers = {
      ...next.providers,
      [activeProvider]: { ...next.providers[activeProvider], [field]: value },
    };
    onChange(next);
  };

  const removeProvider = (key: string) => {
    const next = { ...config };
    const { [key]: _, ...rest } = next.providers;
    next.providers = rest;
    // 清理引用该 provider 的模型
    const newModels = { ...next.models };
    for (const [mk, mv] of Object.entries(newModels)) {
      if (mv.provider === key) delete newModels[mk];
    }
    next.models = newModels;
    // 清理引用该 provider 的路由
    const newRouting = { ...next.routing };
    for (const [slot, entry] of Object.entries(newRouting)) {
      if (entry.provider === key) delete newRouting[slot];
    }
    next.routing = newRouting;
    onChange(next);
    if (activeProvider === key) {
      const remaining = sortedProviderKeys.filter((k) => k !== key);
      setSelectedProvider(remaining[0] || '');
    }
  };

  const addCustomProvider = () => {
    if (!newProviderKey.trim()) return;
    const key = newProviderKey.trim();
    const next = { ...config };
    next.providers = { ...next.providers, [key]: { ...newProviderDraft } };
    onChange(next);
    setSelectedProvider(key);
    setShowAddProvider(false);
    setNewProviderKey('');
    setNewProviderDraft({ base_url: '', api_key: '', timeout_ms: 300000, protocol: 'openai_chatcompletions' });
  };

  // ---- Model handlers ----
  const openAddModel = () => {
    setModelModalMode('add');
    setModelDraft({ provider: activeProvider, model: '', capabilities: [], options: {}, context_window: undefined });
    setAvailableModels([]);
    setTtsVoices([]);
  };

  const openEditModel = (key: string) => {
    setModelModalMode('edit');
    setEditingModelKey(key);
    setModelDraft({ ...config.models[key] });
    setAvailableModels([]);
  };

  const saveModelEdit = () => {
    if (!editingModelKey) return;
    const next = { ...config };
    next.models = { ...next.models, [editingModelKey]: { ...modelDraft } };
    onChange(next);
    setModelModalMode(null);
  };

  const addModel = () => {
    if (!modelDraft.model.trim()) return;
    let key = modelDraft.model.trim();
    if (config.models[key]) key = `${modelDraft.provider}-${key}`;
    const next = { ...config };
    next.models = { ...next.models, [key]: { ...modelDraft } };
    onChange(next);
    setModelModalMode(null);
  };

  const removeModel = (key: string) => {
    const next = { ...config };
    const { [key]: _, ...rest } = next.models;
    next.models = rest;
    // 同时清理路由中引用该模型的条目
    const newRouting = { ...next.routing };
    for (const [slot, entry] of Object.entries(newRouting)) {
      if (entry.provider === config.models[key]?.provider && entry.model === config.models[key]?.model) {
        delete newRouting[slot];
      }
    }
    next.routing = newRouting;
    onChange(next);
  };

  // 模型名变化时，若 context_window 未设且有 chat/multimodal 能力，查映射默认值填入
  useEffect(() => {
    const model = modelDraft.model.trim();
    const hasCtxCapability = modelDraft.capabilities.includes('chat') || modelDraft.capabilities.includes('multimodal');
    if (!model || !hasCtxCapability || modelDraft.context_window !== undefined) return;
    let cancelled = false;
    api.resolveModelContextWindow(model).then((defaultCtx) => {
      if (!cancelled && defaultCtx > 0) {
        setModelDraft((prev) => prev.context_window === undefined ? { ...prev, context_window: defaultCtx } : prev);
      }
    }).catch(() => {});
    return () => { cancelled = true; };
  }, [modelDraft.model, modelDraft.capabilities, modelDraft.context_window]);

  // 模型名变为命中默认视觉前缀时自动补勾多模态。
  // 仅在模型名实际变化时触发：编辑已有模型（首次加载）保留原有选择，
  // 用户手动取消勾选后也不会被重复补回。
  const prevModelNameRef = useRef<string | null>(null);
  useEffect(() => {
    const model = modelDraft.model.trim();
    const prev = prevModelNameRef.current;
    prevModelNameRef.current = model;
    if (prev === null || prev === model) return;
    if (!model || modelDraft.capabilities.includes('multimodal')) return;
    if (!VISION_DEFAULT_MODEL_PREFIXES.some((prefix) => model.startsWith(prefix))) return;
    setModelDraft((prevDraft) =>
      prevDraft.capabilities.includes('multimodal')
        ? prevDraft
        : { ...prevDraft, capabilities: [...prevDraft.capabilities, 'multimodal'] },
    );
  }, [modelDraft.model]);

  const toggleCapability = (cap: string) => {
    if (modelDraft.capabilities.includes(cap)) {
      setModelDraft({ ...modelDraft, capabilities: modelDraft.capabilities.filter((c) => c !== cap) });
    } else {
      setModelDraft({ ...modelDraft, capabilities: [...modelDraft.capabilities, cap] });
    }
  };

  const fetchModelsForProvider = async (providerKey: string) => {
    const provider = config.providers[providerKey];
    if (!provider?.base_url || !provider?.api_key) {
      showError('配置不完整', '请先配置 Base URL 和 API Key');
      return;
    }
    setIsFetchingModels(true);
    try {
      const models = await api.fetchProviderModels(provider.base_url, provider.api_key, provider.timeout_ms, provider.protocol, provider.headers);
      if (models.length === 0) showError('无可用模型', '该供应商未返回任何模型');
      setAvailableModels(models);
    } catch (error) {
      showError('获取失败', `无法获取模型列表：${error}`);
      setAvailableModels([]);
    } finally {
      setIsFetchingModels(false);
    }
  };

  const fetchTtsVoices = async () => {
    setIsFetchingVoices(true);
    try { setTtsVoices(await api.listTtsVoices()); } catch { setTtsVoices([]); } finally { setIsFetchingVoices(false); }
  };

  // DeepSeek 自动获取模型：选中 DeepSeek 且有 api-key 时自动拉取并填充
  useEffect(() => {
    if (activeProvider !== 'DeepSeek' || !selectedConfig?.api_key?.trim()) return;
    if (isFetchingModels) return;

    let cancelled = false;
    const autoFetch = async () => {
      const provider = config.providers[activeProvider];
      if (!provider?.base_url || !provider?.api_key) return;
      setIsFetchingModels(true);
      try {
        const models = await api.fetchProviderModels(provider.base_url, provider.api_key, provider.timeout_ms, provider.protocol, provider.headers);
        if (cancelled || models.length === 0) return;
        const next = { ...config };
        for (const modelId of models) {
          if (!next.models[modelId]) {
            next.models = { ...next.models, [modelId]: { provider: activeProvider, model: modelId, capabilities: ['chat'], options: {} } };
          }
        }
        onChange(next);
      } catch {
        // 静默失败，不影响 UI
      } finally {
        if (!cancelled) setIsFetchingModels(false);
      }
    };
    autoFetch();
    return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeProvider, selectedConfig?.api_key]);

  return (
    <div className="flex h-full">
      {/* 左侧：供应商列表 */}
      <div className="w-56 shrink-0 border-r flex flex-col">
        <div className="px-3 pt-4 pb-2 text-xs text-muted-foreground font-medium">供应商</div>
        <div className="flex-1 overflow-y-auto px-2 pt-1">
          {/* 默认供应商 — Card 风格 */}
          {defaultKeys.map((key) => (
            <button
              key={key}
              className={`w-full text-left px-3 py-2 rounded-md text-sm transition-colors flex items-center justify-between border mb-1.5 ${
                activeProvider === key
                  ? 'bg-primary/10 text-primary font-medium border-primary/30'
                  : 'border-border hover:bg-muted text-muted-foreground hover:text-foreground'
              }`}
              onClick={() => setSelectedProvider(key)}
            >
              <span>{key}</span>
            </button>
          ))}
          {/* 自定义供应商 */}
          {customKeys.length > 0 && defaultKeys.length > 0 && <div className="border-t my-1.5 mx-1" />}
          {customKeys.map((key) => (
            <button
              key={key}
              className={`w-full text-left px-3 py-2 rounded-md text-sm transition-colors ${
                activeProvider === key
                  ? 'bg-primary/10 text-primary font-medium'
                  : 'hover:bg-muted text-muted-foreground hover:text-foreground'
              }`}
              onClick={() => setSelectedProvider(key)}
            >
              {key}
            </button>
          ))}
        </div>
        <div className="border-t p-2">
          <Button size="sm" className="w-full" onClick={() => { setShowAddProvider(true); setNewProviderKey(''); setNewProviderDraft({ base_url: '', api_key: '', timeout_ms: 300000, protocol: 'openai_chatcompletions' }); setShowNewApiKey(false); }}>
            <Plus className="w-3 h-3 mr-1" />自定义供应商
          </Button>
        </div>
      </div>

      {/* 右侧：供应商设置 + 模型列表 */}
      <div className="flex-1 min-w-0 overflow-y-auto p-4">
        {activeProvider && selectedConfig ? (
          <>
            {/* 供应商设置 */}
            <div className="mb-6">
              <div className="flex items-center justify-between mb-3">
                <h4 className="text-sm font-medium">{activeProvider} 设置</h4>
                {!(activeProvider in DEFAULT_PROVIDERS) && (
                <Button variant="ghost" size="sm" className="h-7 text-destructive hover:text-destructive hover:bg-destructive/10" onClick={() => removeProvider(activeProvider)}>
                  <Trash2 className="w-3 h-3 mr-1" />移除供应商
                </Button>
                )}
              </div>
              <div className="space-y-3">
                <div>
                  <Label className="text-xs">API Key</Label>
                  <div className="relative">
                    <Input
                      type={showApiKey ? 'text' : 'password'}
                      value={selectedConfig.api_key}
                      onChange={(e) => updateProviderField('api_key', e.target.value)}
                      className="text-sm h-8 pr-8"
                      placeholder="sk-... 或 ${ENV_VAR}"
                    />
                    <button type="button" onClick={() => setShowApiKey(!showApiKey)} className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground">
                      {showApiKey ? <EyeOff className="w-3.5 h-3.5" /> : <Eye className="w-3.5 h-3.5" />}
                    </button>
                  </div>
                  <p className="text-xs text-muted-foreground mt-1">支持 {'${ENV_VAR}'} 引用环境变量</p>
                </div>
                {activeProvider !== 'DeepSeek' && (
                <div>
                  <Label className="text-xs">Base URL</Label>
                  {(() => {
                    const urlPresets = DEFAULT_PROVIDER_URL_PRESETS[activeProvider];
                    if (urlPresets) {
                      const matchIdx = urlPresets.findIndex((p) => p.url === selectedConfig.base_url);
                      const isCustom = matchIdx === -1;
                      return (
                        <>
                          <Select
                            value={isCustom ? '__custom__' : String(matchIdx)}
                            onValueChange={(v) => {
                              if (v === '__custom__') {
                                updateProviderField('base_url', '');
                              } else {
                                const preset = urlPresets[parseInt(v)];
                                const next = { ...config };
                                next.providers = { ...next.providers, [activeProvider]: { ...selectedConfig, base_url: preset.url, protocol: preset.protocol } };
                                onChange(next);
                              }
                            }}
                          >
                            <SelectTrigger className="h-8 text-sm"><SelectValue /></SelectTrigger>
                            <SelectContent>
                              {urlPresets.map((p, i) => (
                                <SelectItem key={i} value={String(i)}>{p.label}</SelectItem>
                              ))}
                              <SelectItem value="__custom__">自定义 URL</SelectItem>
                            </SelectContent>
                          </Select>
                          {!isCustom && (
                            <div className="text-xs text-muted-foreground mt-1.5 font-mono break-all">{selectedConfig.base_url}</div>
                          )}
                          {isCustom && (
                            <Input
                              value={selectedConfig.base_url}
                              onChange={(e) => updateProviderField('base_url', e.target.value)}
                              className="text-sm h-8 mt-2"
                              placeholder="https://..."
                            />
                          )}
                        </>
                      );
                    }
                    return (
                      <Input
                        value={selectedConfig.base_url}
                        onChange={(e) => updateProviderField('base_url', e.target.value)}
                        className="text-sm h-8"
                        placeholder="https://api.openai.com/v1"
                      />
                    );
                  })()}
                </div>
                )}
                <div className="flex gap-3">
                  {activeProvider !== 'DeepSeek' && (
                  <div className="flex-1">
                    <Label className="text-xs">协议</Label>
                    <Select
                      value={selectedConfig.protocol || 'openai_chatcompletions'}
                      onValueChange={(v) => {
                        const next = { ...config };
                        next.providers = { ...next.providers, [activeProvider]: { ...selectedConfig, protocol: v } };
                        onChange(next);
                      }}
                    >
                      <SelectTrigger className="h-8 text-sm"><SelectValue /></SelectTrigger>
                      <SelectContent>
                        <SelectItem value="openai">OpenAI Responses</SelectItem>
                  <SelectItem value="openai_chatcompletions">OpenAI Chat Completions</SelectItem>
                        <SelectItem value="anthropic">Anthropic</SelectItem>
                      </SelectContent>
                    </Select>
                  </div>
                  )}
                  <div className="w-32">
                    <Label className="text-xs">超时 (ms)</Label>
                    <Input
                      type="number"
                      value={selectedConfig.timeout_ms}
                      onChange={(e) => updateProviderField('timeout_ms', parseInt(e.target.value) || 300000)}
                      className="text-sm h-8"
                    />
                  </div>
                </div>
                {/* DeepSeek 余额查询 */}
                <RequestHeadersEditor headers={selectedConfig.headers} onChange={(headers) => updateProviderField('headers', headers)} />
                {activeProvider === 'DeepSeek' && selectedConfig.api_key.trim() && (
                  <ProviderBalanceSection providerName={activeProvider} />
                )}
              </div>
            </div>

            <div className="border-t my-4" />

            {/* 模型列表 */}
            <div>
              <div className="flex items-center justify-between mb-3">
                <h4 className="text-sm font-medium">模型列表</h4>
                <Button size="sm" onClick={openAddModel}>
                  <Plus className="w-3 h-3 mr-1" />添加模型
                </Button>
              </div>
              {providerModels.length === 0 ? (
                <div className="text-center text-muted-foreground py-6 text-sm">暂无模型，点击上方按钮添加</div>
              ) : (
                <div className="space-y-2">
                  {providerModels.map(([key, m]) => (
                    <Card key={key}>
                      <CardContent className="p-3">
                        <div className="flex items-center justify-between">
                          <div>
                            <span className="font-medium text-sm">{key}</span>
                            <div className="text-xs text-muted-foreground mt-1">{m.model}</div>
                            <div className="flex gap-1 mt-1">
                              {m.capabilities.map((cap) => {
                                const capInfo = capabilities.find((c) => c.key === cap);
                                return <Badge key={cap} variant="secondary" className="text-[10px] px-1.5 py-0 h-5">{capInfo?.display_name || cap}</Badge>;
                              })}
                            </div>
                          </div>
                          <div className="flex items-center gap-1">
                            <Button variant="ghost" size="icon" className="h-7 w-7" onClick={() => openEditModel(key)} title="编辑"><Edit2 className="w-3.5 h-3.5" /></Button>
                            <Button variant="ghost" size="icon" className="h-7 w-7 hover:bg-destructive/20 hover:text-destructive" onClick={() => removeModel(key)} title="删除"><Trash2 className="w-3.5 h-3.5" /></Button>
                          </div>
                        </div>
                      </CardContent>
                    </Card>
                  ))}
                </div>
              )}
            </div>
          </>
        ) : (
          <div className="flex items-center justify-center h-full text-muted-foreground text-sm">请从左侧选择或添加一个供应商</div>
        )}
      </div>

      {/* 添加供应商 Modal */}
      <Dialog open={showAddProvider} onOpenChange={setShowAddProvider}>
        <DialogContent className="max-w-md">
          <DialogHeader><DialogTitle>添加自定义供应商</DialogTitle></DialogHeader>
          <div className="space-y-3 pt-2">
            <div>
              <Label className="text-xs">供应商名称</Label>
              <Input value={newProviderKey} onChange={(e) => setNewProviderKey(e.target.value)} className="text-sm h-8" placeholder="例如: MyProvider" />
            </div>
            <ProviderForm draft={newProviderDraft} setDraft={setNewProviderDraft} showApiKey={showNewApiKey} setShowApiKey={setShowNewApiKey} onSave={addCustomProvider} onCancel={() => setShowAddProvider(false)} saveLabel="添加" />
          </div>
        </DialogContent>
      </Dialog>

      {/* 添加/编辑模型 Modal */}
      <Dialog open={modelModalMode !== null} onOpenChange={(v) => { if (!v) { setModelModalMode(null); setAvailableModels([]); } }}>
        <DialogContent className="max-w-md">
          <DialogHeader><DialogTitle>{modelModalMode === 'add' ? '添加模型' : `编辑模型: ${editingModelKey}`}</DialogTitle></DialogHeader>
          <div className="pt-2 space-y-3">
            <div>
              <Label className="text-xs">供应商</Label>
              <div className="text-sm text-muted-foreground mt-0.5">{modelDraft.provider}</div>
            </div>
            <div>
              <div className="flex items-center justify-between">
                <Label className="text-xs">模型名称</Label>
                <Button variant="ghost" size="sm" className="h-5 text-xs px-2" onClick={() => fetchModelsForProvider(modelDraft.provider)} disabled={isFetchingModels}>
                  {isFetchingModels ? <><Loader2 className="w-3 h-3 mr-1 animate-spin" />获取中...</> : '获取模型列表'}
                </Button>
              </div>
              {availableModels.length > 0 ? (
                <Select value={modelDraft.model} onValueChange={(v) => setModelDraft({ ...modelDraft, model: v })}>
                  <SelectTrigger className="h-8 text-sm"><SelectValue placeholder="-- 选择模型 --" /></SelectTrigger>
                  <SelectContent>
                    {availableModels.map((m) => <SelectItem key={m} value={m}>{m}</SelectItem>)}
                  </SelectContent>
                </Select>
              ) : (
                <Input value={modelDraft.model} onChange={(e) => setModelDraft({ ...modelDraft, model: e.target.value })} className="text-sm h-8" placeholder="gpt-4o, deepseek-chat, ..." />
              )}
            </div>
            <div>
              <Label className="text-xs">能力</Label>
              <div className="flex flex-wrap gap-1.5 mt-1">
                {capabilities.filter((cap) => cap.key !== 'lite').map((cap) => (
                  <button
                    key={cap.key}
                    className={`px-2 py-0.5 text-xs rounded border transition-colors ${modelDraft.capabilities.includes(cap.key) ? 'bg-primary/20 text-primary border-primary/40' : 'bg-secondary text-muted-foreground border-border hover:text-foreground'}`}
                    onClick={() => toggleCapability(cap.key)}
                  >
                    {cap.display_name}
                  </button>
                ))}
              </div>
            </div>
            {(modelDraft.capabilities.includes('chat') || modelDraft.capabilities.includes('multimodal')) && (
              <div>
                <Label className="text-xs">上下文窗口 (context_window)</Label>
                <Input
                  type="number"
                  className="h-8 text-sm mt-1"
                  placeholder="留空使用模型默认值"
                  value={modelDraft.context_window ?? ''}
                  onChange={(e) => {
                    const v = e.target.value.trim();
                    setModelDraft({ ...modelDraft, context_window: v === '' ? undefined : Math.max(0, parseInt(v, 10) || 0) });
                  }}
                />
                <p className="text-xs text-muted-foreground mt-0.5">单位：token。留空时从 context_windows.json 映射表取默认值。</p>
              </div>
            )}
            {modelDraft.capabilities.includes('tts') && (
              <div>
                <div className="flex items-center justify-between">
                  <Label className="text-xs">TTS 音色 (voice)</Label>
                  {ttsVoices.length === 0 && (
                    <Button variant="ghost" size="sm" className="h-5 text-xs px-2" onClick={fetchTtsVoices} disabled={isFetchingVoices}>
                      {isFetchingVoices ? <><Loader2 className="w-3 h-3 mr-1 animate-spin" />获取中...</> : '获取可用音色'}
                    </Button>
                  )}
                </div>
                {ttsVoices.length > 0 ? (
                  <Select value={(modelDraft.options?.voice as string) || '__default__'} onValueChange={(v) => setModelDraft({ ...modelDraft, options: { ...modelDraft.options, voice: v === '__default__' ? undefined : v } })}>
                    <SelectTrigger className="h-8 text-sm"><SelectValue placeholder="-- 使用默认音色 --" /></SelectTrigger>
                    <SelectContent>
                      <SelectItem value="__default__">-- 使用默认音色 --</SelectItem>
                      {ttsVoices.map((v) => <SelectItem key={v.id} value={v.id}>{v.name}{v.gender ? ` (${v.gender})` : ''}</SelectItem>)}
                    </SelectContent>
                  </Select>
                ) : (
                  <Input value={(modelDraft.options?.voice as string) || ''} onChange={(e) => setModelDraft({ ...modelDraft, options: { ...modelDraft.options, voice: e.target.value || undefined } })} className="text-sm h-8" placeholder="输入音色名称" />
                )}
              </div>
            )}
            <div className="flex justify-end gap-2 pt-1">
              <Button variant="ghost" size="sm" onClick={() => { setModelModalMode(null); setAvailableModels([]); }}>取消</Button>
              <Button size="sm" onClick={modelModalMode === 'add' ? addModel : saveModelEdit} disabled={!modelDraft.model}>{modelModalMode === 'add' ? '添加' : '保存'}</Button>
            </div>
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function ProviderForm({
  draft,
  setDraft,
  showApiKey,
  setShowApiKey,
  onSave,
  onCancel,
  saveLabel = '保存',
}: {
  draft: ProviderConfigView;
  setDraft: (d: ProviderConfigView) => void;
  showApiKey: boolean;
  setShowApiKey: (v: boolean) => void;
  onSave: () => void;
  onCancel: () => void;
  saveLabel?: string;
}) {
  const handleProtocolChange = (protocol: string) => {
    const defaultUrl = PROTOCOL_DEFAULTS[protocol] || '';
    const oldDefault = PROTOCOL_DEFAULTS[draft.protocol] || '';
    const current = draft.base_url.trim();
    // 仅当地址为空或仍等于旧协议默认地址时跟随协议更新，避免覆盖用户自填地址。
    const keepUrl = current !== '' && current !== oldDefault;
    setDraft({ ...draft, protocol, base_url: keepUrl ? draft.base_url : defaultUrl });
  };

  return (
    <div className="space-y-2">
      <div>
        <Label className="text-xs">Base URL</Label>
        <Input
          value={draft.base_url}
          onChange={(e) => setDraft({ ...draft, base_url: e.target.value })}
          className="text-sm h-8"
          placeholder="https://api.openai.com/v1"
        />
      </div>
      <div>
        <Label className="text-xs">API Key</Label>
        <div className="relative">
          <Input
            type={showApiKey ? 'text' : 'password'}
            value={draft.api_key}
            onChange={(e) => setDraft({ ...draft, api_key: e.target.value })}
            className="text-sm h-8 pr-8"
            placeholder="sk-... 或 ${ENV_VAR}"
          />
          <button
            type="button"
            onClick={() => setShowApiKey(!showApiKey)}
            className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
          >
            {showApiKey ? <EyeOff className="w-3.5 h-3.5" /> : <Eye className="w-3.5 h-3.5" />}
          </button>
        </div>
        <p className="text-xs text-muted-foreground mt-1">支持 {'${ENV_VAR}'} 引用环境变量</p>
      </div>
      <div>
        <Label className="text-xs">超时 (毫秒)</Label>
        <Input
          type="number"
          value={draft.timeout_ms}
          onChange={(e) => setDraft({ ...draft, timeout_ms: parseInt(e.target.value) || 300000 })}
          className="text-sm h-8"
          placeholder="300000"
        />
      </div>
      <div>
        <Label className="text-xs">请求格式（协议类型）</Label>
        <Select
          value={draft.protocol || 'openai_chatcompletions'}
          onValueChange={handleProtocolChange}
        >
          <SelectTrigger className="text-sm h-8">
            <SelectValue placeholder="选择协议" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="openai">OpenAI Responses</SelectItem>
                  <SelectItem value="openai_chatcompletions">OpenAI Chat Completions</SelectItem>
            <SelectItem value="anthropic">Anthropic</SelectItem>
          </SelectContent>
        </Select>
      </div>
      <RequestHeadersEditor headers={draft.headers} onChange={(headers) => setDraft({ ...draft, headers })} />
      <div className="flex justify-end gap-2 pt-1">
        <Button variant="ghost" size="sm" onClick={onCancel}>
          取消
        </Button>
        <Button size="sm" onClick={onSave}>
          {saveLabel}
        </Button>
      </div>
    </div>
  );
}


// ---------------------------------------------------------------------------
// Routing 子区域
// ---------------------------------------------------------------------------

function RoutingSection({
  config,
  onChange,
}: {
  config: ModelsConfigView;
  onChange: (c: ModelsConfigView) => void;
}) {
  const modelKeys = Object.keys(config.models).sort();
  const [routeSearch, setRouteSearch] = useState<Record<string, string>>({});
  const modelLabel = (modelKey: string) => {
    const model = config.models[modelKey];
    if (!model) return modelKey;
    return `${model.provider} / ${model.model}`;
  };

  // 路由槽位定义（不依赖 capabilities 列表，直接枚举）
  const routingSlots = [
    { key: 'chat', display_name: '对话' },
    { key: 'lite', display_name: '轻量文本' },
    { key: 'multimodal', display_name: '多模态' },
    { key: 'image_generation', display_name: '图片生成' },
    { key: 'video_generation', display_name: '视频生成' },
    { key: 'stt', display_name: '语音识别' },
    { key: 'tts', display_name: '语音合成' },
  ];

  // 根据 routing entry 找到对应的 models key
  const findModelKeyForRoute = (slotKey: string): string | null => {
    const entry = config.routing[slotKey];
    if (!entry) return null;
    return modelKeys.find((mk) => {
      const m = config.models[mk];
      return m && m.provider === entry.provider && m.model === entry.model;
    }) || null;
  };

  const setRoute = (slotKey: string, modelKey: string) => {
    const next = { ...config };
    const newRouting = { ...next.routing };
    if (modelKey === '__none__') {
      delete newRouting[slotKey];
    } else {
      const entry = next.models[modelKey];
      if (entry) {
        newRouting[slotKey] = { ...entry };
      }
    }
    next.routing = newRouting;
    onChange(next);
    setRouteSearch((prev) => ({ ...prev, [slotKey]: '' }));
  };

  return (
    <div className="flex flex-col h-full">
      <div className="mb-3 shrink-0">
        <h4 className="text-sm font-medium text-muted-foreground">能力路由</h4>
        <p className="text-xs text-muted-foreground mt-1">
          为对话和多媒体能力选择默认模型；嵌入与重排模型在 Memory 插件页中配置。
        </p>
      </div>

      <div className="space-y-2 flex-1 min-h-0 overflow-y-auto">
        {routingSlots.map((slot) => {
          const currentModelKey = findModelKeyForRoute(slot.key);
          const search = routeSearch[slot.key] || '';
          const filtered = modelKeys
            .filter((mk) => {
              const m = config.models[mk];
              if (m.capabilities.length === 0) return true;
              if (m.capabilities.includes(slot.key)) return true;
              if (slot.key === 'lite' && m.capabilities.includes('chat')) return true;
              if (slot.key === 'chat' && m.capabilities.includes('multimodal')) return true;
              return false;
            })
            .filter((mk) => {
              if (!search) return true;
              const q = search.toLowerCase();
              return mk.toLowerCase().includes(q) || modelLabel(mk).toLowerCase().includes(q);
            });
          return (
            <Card key={slot.key}>
              <CardContent className="p-3">
                <div className="flex items-center gap-4">
                  <div className="w-28 shrink-0">
                    <div className="text-sm font-medium leading-tight">{slot.display_name}</div>
                    <div className="text-xs text-muted-foreground">({slot.key})</div>
                  </div>
                  <Select
                    value={currentModelKey || '__none__'}
                    onValueChange={(v) => setRoute(slot.key, v)}
                  >
                    <SelectTrigger className="h-8 text-sm flex-1">
                      <SelectValue placeholder="-- 未配置 --" />
                    </SelectTrigger>
                    <SelectContent>
                      <div className="p-1.5 border-b" onPointerDown={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
                        <Input
                          value={search}
                          onChange={(e) => setRouteSearch((prev) => ({ ...prev, [slot.key]: e.target.value }))}
                          className="h-7 text-xs"
                          placeholder="搜索模型..."
                        />
                      </div>
                      <SelectItem value="__none__">-- 未配置 --</SelectItem>
                      {filtered.map((mk) => (
                        <SelectItem key={mk} value={mk}>{modelLabel(mk)}</SelectItem>
                      ))}
                      {filtered.length === 0 && (
                        <div className="px-2 py-3 text-xs text-muted-foreground text-center">无匹配模型</div>
                      )}
                    </SelectContent>
                  </Select>
                </div>
              </CardContent>
            </Card>
          );
        })}
      </div>

      {modelKeys.length === 0 && (
        <p className="text-xs text-muted-foreground mt-3">
          请先在模型页中添加模型定义，然后回来配置路由
        </p>
      )}
    </div>
  );
}

// ============================================================================
// Server 设置组件
// ============================================================================

// ============================================================================
// Server 设置组件
// ============================================================================

function ServerSettings() {
  const [subTab, setSubTab] = useState<'config' | 'webhooks'>('config');

  return (
    <div className="flex flex-col h-full">
      <div className="flex gap-1 shrink-0 p-4 pb-0">
        {(['config', 'webhooks'] as const).map((tab) => (
          <button
            key={tab}
            className={`px-3 py-1.5 text-sm rounded-md transition-colors ${
              subTab === tab
                ? 'bg-primary text-primary-foreground'
                : 'bg-muted text-muted-foreground hover:text-foreground'
            }`}
            onClick={() => setSubTab(tab)}
          >
            {tab === 'config' ? 'Server 配置' : 'Webhook'}
          </button>
        ))}
      </div>
      <div className="flex-1 min-h-0 overflow-y-auto">
        {subTab === 'config' && <ServerConfigPanel />}
        {subTab === 'webhooks' && <ServerWebhookPanel />}
      </div>
    </div>
  );
}

function ServerWebhookPanel() {
  const [serverRunning, setServerRunning] = useState(false);

  useEffect(() => {
    const check = async () => {
      try {
        const cfg = await api.getServerConfig();
        setServerRunning(cfg.running);
      } catch { /* ignore */ }
    };
    check();
    const timer = window.setInterval(check, 5000);
    return () => window.clearInterval(timer);
  }, []);

  return <WebhookPanel serverRunning={serverRunning} />;
}

function ServerConfigPanel() {
  const [config, setConfig] = useState<ServerConfig>({
    host: '127.0.0.1',
    port: 8080,
    auth_token_masked: '',
    enabled: false,
    running: false,
    status: 'stopped',
  });
  const [editHost, setEditHost] = useState('');
  const [editPort, setEditPort] = useState('8080');
  const [editAuthToken, setEditAuthToken] = useState('');
  const [isLoading, setIsLoading] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [isToggling, setIsToggling] = useState(false);
  const { showSuccess, showError } = useToast();

  const loadConfig = async (showLoading = true) => {
    if (showLoading) setIsLoading(true);
    try {
      const cfg = await api.getServerConfig();
      setConfig(cfg);
      if (showLoading) {
        setEditHost(cfg.host);
        setEditPort(String(cfg.port));
        setEditAuthToken('');
      }
    } catch (error) {
      console.error('加载 Server 配置失败:', error);
      if (showLoading) {
        showError('加载失败', '无法加载 Server 配置');
      }
    } finally {
      if (showLoading) setIsLoading(false);
    }
  };

  useEffect(() => {
    loadConfig();
    const timer = window.setInterval(() => {
      loadConfig(false);
    }, 3000);
    return () => window.clearInterval(timer);
  }, []);

  const handleSave = async () => {
    setIsSaving(true);
    try {
      const port = parseInt(editPort, 10);
      if (isNaN(port) || port < 1 || port > 65535) {
        showError('端口无效', '请输入 1-65535 之间的端口号');
        return;
      }
      const authToken = editAuthToken.trim() || undefined;
      await api.setServerConfig(editHost, port, authToken);
      showSuccess('保存成功', 'Server 配置已更新');
      loadConfig();
    } catch (error) {
      console.error('保存 Server 配置失败:', error);
      showError('保存失败', '无法保存 Server 配置');
    } finally {
      setIsSaving(false);
    }
  };

  const saveCurrentConfig = async () => {
    const port = parseInt(editPort, 10);
    if (isNaN(port) || port < 1 || port > 65535) {
      showError('端口无效', '请输入 1-65535 之间的端口号');
      return false;
    }
    const authToken = editAuthToken.trim() || undefined;
    await api.setServerConfig(editHost, port, authToken);
    return true;
  };

  const handleToggleServer = async (enabled: boolean) => {
    setIsToggling(true);
    try {
      if (enabled) {
        const saved = await saveCurrentConfig();
        if (!saved) return;
        const message = await api.startServer();
        showSuccess('启动成功', message);
      } else {
        const message = await api.stopServer();
        showSuccess('已停止', message);
      }
      await loadConfig();
    } catch (error) {
      console.error('切换 Server 状态失败:', error);
      showError(
        enabled ? '启动失败' : '停止失败',
        error instanceof Error ? error.message : String(error || '无法切换 Server 运行状态')
      );
    } finally {
      setIsToggling(false);
    }
  };

  return (
    <div className="space-y-4 p-4">
      {isLoading ? (
        <div className="flex items-center justify-center py-8">
          <Loader2 className="w-6 h-6 animate-spin text-primary mr-2" />
          <span className="text-sm text-muted-foreground">加载配置中...</span>
        </div>
      ) : (
        <>
          {/* 运行状态 */}
          <div className="flex items-center justify-between gap-3 mb-4">
            <div className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground">状态：</span>
              <Badge
                variant={config.status === 'error' ? 'destructive' : config.running ? 'default' : 'secondary'}
              >
                {config.status === 'error' ? '运行异常' : config.running ? '运行中' : '未运行'}
              </Badge>
            </div>
            <div className="flex items-center gap-2">
              {isToggling && <Loader2 className="w-4 h-4 animate-spin text-muted-foreground" />}
              <Switch
                checked={config.enabled}
                disabled={isSaving || isToggling}
                onCheckedChange={handleToggleServer}
              />
            </div>
          </div>

          <div className="space-y-2">
            <Label htmlFor="serverHost">监听地址</Label>
            <Input
              id="serverHost"
              value={editHost}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) => setEditHost(e.target.value)}
              placeholder="127.0.0.1"
              disabled={isSaving || isToggling || config.running}
            />
          </div>

          <div className="space-y-2">
            <Label htmlFor="serverPort">端口</Label>
            <Input
              id="serverPort"
              type="number"
              value={editPort}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) => setEditPort(e.target.value)}
              placeholder="8080"
              disabled={isSaving || isToggling || config.running}
            />
          </div>

          <div className="space-y-2">
            <Label htmlFor="serverAuthToken">认证 Token</Label>
            <Input
              id="serverAuthToken"
              type="password"
              value={editAuthToken}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) => setEditAuthToken(e.target.value)}
              placeholder={config.auth_token_masked || '留空表示不鉴权'}
              disabled={isSaving || isToggling || config.running}
            />
            <p className="text-xs text-muted-foreground">
              当前: {config.auth_token_masked}（留空则保持不变）
            </p>
          </div>

          <div className="flex justify-end gap-2 pt-4">
            <Button onClick={handleSave} disabled={isSaving || isToggling || config.running}>
              {isSaving ? (
                <>
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  保存中...
                </>
              ) : (
                '保存'
              )}
            </Button>
          </div>
        </>
      )}
    </div>
  );
}

// ============================================================================
// 关于与更新组件
// ============================================================================

function formatBytes(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  let size = value;
  let unitIndex = 0;
  while (size >= 1024 && unitIndex < units.length - 1) {
    size /= 1024;
    unitIndex += 1;
  }
  return `${size.toFixed(unitIndex === 0 ? 0 : 1)} ${units[unitIndex]}`;
}

/// 插件设置页视图：选中时才加载 HTML，用 iframe 渲染。
function PluginView({ contribution }: { contribution: SlotContributionEntry }) {
  const [html, setHtml] = useState<string>('');

  useEffect(() => {
    let active = true;
    setHtml('');
    const load = contribution.source === 'manifest'
      ? api.pluginOpenEntry(contribution.plugin_id, contribution.contribution_id)
      : api.pluginOpenView(contribution.plugin_id, contribution.contribution_id);
    load
      .then((page) => {
        if (active) setHtml(page);
      })
      .catch(() => {
        if (active) setHtml('<p style="padding:16px;color:#888">页面加载失败</p>');
      });
    return () => {
      active = false;
    };
  }, [contribution.plugin_id, contribution.contribution_id, contribution.source]);

  if (!html) {
    return <div className="p-4 text-sm text-muted-foreground">加载中…</div>;
  }

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
      {contribution.source === 'manifest' ? (
        <PluginSandbox
          pluginId={contribution.plugin_id}
          contributionId={contribution.contribution_id}
          sandbox={contribution.sandbox}
          html={html}
        />
      ) : (
        <PluginIframe pluginId={contribution.plugin_id} html={html} />
      )}
    </div>
  );
}

function AppUpdateSettings() {
  const storeUpdate = useStore((s) => s.updateAvailable);
  const [currentVersion, setCurrentVersion] = useState('');
  const [availableUpdate, setAvailableUpdate] = useState<{
    version: string;
    currentVersion: string;
    date?: string;
    body?: string;
  } | null>(null);
  const [isChecking, setIsChecking] = useState(false);
  const [isInstalling, setIsInstalling] = useState(false);
  const [downloadedBytes, setDownloadedBytes] = useState(0);
  const [contentLength, setContentLength] = useState<number | undefined>();
  const updateRef = useRef<Update | null>(null);
  const { showSuccess, showError, showInfo } = useToast();

  useEffect(() => {
    let mounted = true;
    import('@tauri-apps/api/app')
      .then(({ getVersion }) => getVersion())
      .then((version) => {
        if (mounted) setCurrentVersion(version);
      })
      .catch((error) => {
        console.error('读取应用版本失败:', error);
        if (mounted) setCurrentVersion('未知');
      });
    return () => {
      mounted = false;
      updateRef.current?.close().catch(() => undefined);
      updateRef.current = null;
    };
  }, []);

  // 从全局 store 预填充更新信息并静默获取可用的 Update 对象
  useEffect(() => {
    if (!storeUpdate || updateRef.current) return;
    let mounted = true;
    setAvailableUpdate({
      version: storeUpdate.version,
      currentVersion: '',
      date: storeUpdate.date,
      body: storeUpdate.body,
    });
    (async () => {
      try {
        const { check } = await import('@tauri-apps/plugin-updater');
        const update = await check({ timeout: 30000 });
        if (!mounted) return;
        await updateRef.current?.close().catch(() => {});
        updateRef.current = update;
        if (update) {
          setAvailableUpdate({
            version: update.version,
            currentVersion: update.currentVersion,
            date: update.date,
            body: update.body,
          });
        }
      } catch {
        // 静默失败，用户可手动重试
      }
    })();
    return () => { mounted = false; };
  }, [storeUpdate]);

  const handleCheckUpdate = async () => {
    setIsChecking(true);
    setAvailableUpdate(null);
    setDownloadedBytes(0);
    setContentLength(undefined);
    try {
      const { check } = await import('@tauri-apps/plugin-updater');
      const update = await check({ timeout: 30000 });
      await updateRef.current?.close().catch(() => undefined);
      updateRef.current = update;

      if (!update) {
        showInfo('已是最新版本', '当前没有可用更新');
        return;
      }

      setAvailableUpdate({
        version: update.version,
        currentVersion: update.currentVersion,
        date: update.date,
        body: update.body,
      });
      showSuccess('发现新版本', `可更新到 ${update.version}`);
    } catch (error) {
      console.error('检查更新失败:', error);
      showError('检查失败', `${error}`);
    } finally {
      setIsChecking(false);
    }
  };

  const handleInstallUpdate = async () => {
    const update = updateRef.current;
    if (!update) {
      showError('没有可安装更新', '请先检查更新');
      return;
    }

    setIsInstalling(true);
    setDownloadedBytes(0);
    setContentLength(undefined);
    try {
      let downloaded = 0;
      await update.downloadAndInstall((event: DownloadEvent) => {
        if (event.event === 'Started') {
          downloaded = 0;
          setDownloadedBytes(0);
          setContentLength(event.data.contentLength);
        }
        if (event.event === 'Progress') {
          downloaded += event.data.chunkLength;
          setDownloadedBytes(downloaded);
        }
      });
      showSuccess('更新已安装', '应用将重新启动');
      const { relaunch } = await import('@tauri-apps/plugin-process');
      await relaunch();
    } catch (error) {
      console.error('安装更新失败:', error);
      showError('安装失败', `${error}`);
    } finally {
      setIsInstalling(false);
    }
  };

  const progressText = contentLength
    ? `${formatBytes(downloadedBytes)} / ${formatBytes(contentLength)}`
    : downloadedBytes > 0
      ? formatBytes(downloadedBytes)
      : '';

  return (
    <div className="space-y-4 p-4">
      <div className="flex items-center justify-between gap-4">
        <h3 className="text-lg font-medium">关于与更新</h3>
        <a
          href="https://github.com/silent-rs/silent-Tiangong"
          target="_blank"
          rel="noopener noreferrer"
          className="inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-sm text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          title="在 GitHub 上查看本项目"
        >
          <Github className="h-4 w-4" />
          <span>GitHub</span>
        </a>
      </div>

      <Card>
        <CardContent className="space-y-4 p-4">
          <div className="flex items-center justify-between gap-4">
            <div>
              <div className="text-sm text-muted-foreground">当前版本</div>
              <div className="mt-1 text-2xl font-semibold">{currentVersion || '读取中...'}</div>
            </div>
            <Badge variant="outline">GitHub / OSS</Badge>
          </div>

          {availableUpdate ? (
            <div className="rounded-md border p-3 text-sm">
              <div className="font-medium">发现新版本 {availableUpdate.version}</div>
              <div className="mt-1 text-muted-foreground">
                当前版本 {availableUpdate.currentVersion}
                {availableUpdate.date ? ` · ${availableUpdate.date}` : ''}
              </div>
              {availableUpdate.body && (
                <div className="mt-3 whitespace-pre-wrap text-muted-foreground">
                  {availableUpdate.body}
                </div>
              )}
            </div>
          ) : (
            <div className="rounded-md border p-3 text-sm text-muted-foreground">
              更新检查会依次从 GitHub Release 和阿里云 OSS 获取最新版本信息。
            </div>
          )}

          {isInstalling && progressText && (
            <div className="text-sm text-muted-foreground">正在下载：{progressText}</div>
          )}

          <div className="flex flex-wrap justify-end gap-2">
            <Button variant="outline" onClick={handleCheckUpdate} disabled={isChecking || isInstalling}>
              {isChecking ? (
                <>
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  检查中...
                </>
              ) : (
                <>
                  <RefreshCw className="w-4 h-4 mr-2" />
                  检查更新
                </>
              )}
            </Button>
            <Button onClick={handleInstallUpdate} disabled={!updateRef.current || isChecking || isInstalling}>
              {isInstalling ? (
                <>
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  安装中...
                </>
              ) : (
                '下载并安装'
              )}
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

function DataCleanupSettings() {
  const { showSuccess, showError } = useToast();
  const [trashed, setTrashed] = useState<TrashedSession[]>([]);
  const [loading, setLoading] = useState(false);
  const [purging, setPurging] = useState(false);
  const [progress, setProgress] = useState<{ current: number; total: number } | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const list = await api.listTrashedSessions();
      setTrashed(list);
    } catch (error) {
      showError('扫描失败', String(error));
    } finally {
      setLoading(false);
    }
  }, [showError]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const handlePurgeAll = async () => {
    if (trashed.length === 0) return;
    const total = trashed.length;
    setPurging(true);
    setProgress({ current: 0, total });
    let unlisten: (() => void) | undefined;
    try {
      // 先注册进度监听器，再调后端命令，防止漏掉早期事件。
      unlisten = await api.onPurgeProgress((p) => {
        setProgress({ current: p.current, total: p.total });
      });
      const count = await api.purgeAllDeletedSessions();
      // 清理后重新扫描回收区（不直接清空——部分可能清理失败仍残留）。
      await refresh();
      if (count < total) {
        showError('部分清理失败', `成功 ${count} 个，${total - count} 个失败，可稍后重试`);
      } else {
        showSuccess('清理完成', `已清理 ${count} 个已删除会话`);
      }
    } catch (error) {
      showError('清理失败', String(error));
      refresh();
    } finally {
      unlisten?.();
      setPurging(false);
      setProgress(null);
    }
  };

  const percent = progress && progress.total > 0
    ? Math.round((progress.current / progress.total) * 100)
    : 0;

  return (
    <div className="mx-auto max-w-2xl space-y-6 p-6">
      <div>
        <h2 className="text-lg font-semibold mb-1">数据清理</h2>
        <p className="text-sm text-muted-foreground">
          已删除的会话会暂存在回收区。点击下方按钮彻底清理它们占用的磁盘空间。
        </p>
      </div>

      <Card>
        <CardContent className="pt-6 space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <p className="text-sm font-medium">回收区会话</p>
              <p className="text-xs text-muted-foreground mt-0.5">
                {loading ? '扫描中...' : `${trashed.length} 个已删除会话`}
              </p>
            </div>
            <div className="flex gap-2">
              <Button
                variant="outline"
                size="sm"
                disabled={loading || purging}
                onClick={refresh}
              >
                {loading ? (
                  <Loader2 className="w-4 h-4 mr-1 animate-spin" />
                ) : (
                  <RefreshCw className="w-4 h-4 mr-1" />
                )}
                刷新
              </Button>
              <Button
                variant="destructive"
                size="sm"
                disabled={trashed.length === 0 || purging}
                onClick={handlePurgeAll}
              >
                {purging ? (
                  <Loader2 className="w-4 h-4 mr-1 animate-spin" />
                ) : (
                  <Trash2 className="w-4 h-4 mr-1" />
                )}
                全部清理
              </Button>
            </div>
          </div>

          {purging && progress && (
            <div className="space-y-1.5">
              <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted">
                <div
                  className="h-full bg-primary transition-[width] duration-150"
                  style={{ width: `${percent}%` }}
                />
              </div>
              <p className="text-xs text-muted-foreground text-center">
                {progress.current} / {progress.total}（{percent}%）
              </p>
            </div>
          )}

          {!loading && trashed.length > 0 && !purging && (
            <div className="border rounded-lg divide-y max-h-80 overflow-y-auto">
              {trashed.map((session) => (
                <div key={session.id} className="flex items-center justify-between px-3 py-2">
                  <div className="min-w-0 flex-1">
                    <p className="text-sm truncate">{session.title || '（无标题）'}</p>
                    <p className="text-xs text-muted-foreground">
                      {session.message_count} 条对话
                      {session.updated_at && ` · ${session.updated_at.slice(0, 10)}`}
                    </p>
                  </div>
                  {session.purging ? (
                    <Badge variant="secondary" className="ml-2 shrink-0 text-xs">
                      清理中
                    </Badge>
                  ) : (
                    <Button
                      variant="ghost"
                      size="sm"
                      className="ml-2 shrink-0"
                      onClick={async () => {
                        try {
                          await api.restoreDeletedSession(session.id);
                          showSuccess('已恢复', session.title || '会话已恢复到列表');
                          refresh();
                        } catch (error) {
                          showError('恢复失败', String(error));
                        }
                      }}
                    >
                      <RefreshCw className="w-3.5 h-3.5 mr-1" />
                      恢复
                    </Button>
                  )}
                </div>
              ))}
            </div>
          )}

          {!loading && trashed.length === 0 && !purging && (
            <p className="text-sm text-muted-foreground text-center py-8">
              回收区为空，没有待清理的会话。
            </p>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
