/**
 * 浏览器配置页（`tiangong config` / CLI `/config`）：复用桌面端设置组件，
 * 只包含配置相关分区（智能体、模型、Server、Bot、插件管理与插件配置页）。
 */
import { useEffect, useState } from 'react';
import { Bot as BotIcon, Brain, Globe, Loader2, LogOut, Package, Puzzle, Settings, ShieldCheck } from 'lucide-react';
import { api, type AvailablePlugin, type PluginStatus, type SlotContributionEntry } from '@/api/tauri';
import { closeWebHost } from '@/api/host';
import { useStore } from '@/store/useStore';
import { Button } from '@/components/ui/button';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { PluginManagerSettings } from '@/components/PluginManagerSettings';
import { BotPanel } from '@/components/bots/BotPanel';
import {
  AgentSettings,
  LLMSettings,
  PluginView,
  ServerConfigPanel,
  type SaveStatus,
} from '@/components/SettingsDialog';

const BASE_TABS = ['agent', 'llm', 'server', 'bots', 'plugin-manager'];

function initialTab(): string {
  const hash = window.location.hash.replace(/^#/, '');
  if (hash === 'models' || hash === 'providers') return 'llm';
  if (hash === 'plugins') return 'plugin-manager';
  if (hash === 'bot') return 'bots';
  return BASE_TABS.includes(hash) ? hash : 'agent';
}

function contributionIcon(name: string) {
  const Icon = name === 'brain' ? Brain : Puzzle;
  return <Icon className="w-4 h-4 sm:mr-2" />;
}

export function ConfigApp() {
  const [activeTab, setActiveTab] = useState(initialTab);
  const [saveStatus, setSaveStatus] = useState<SaveStatus>('idle');
  const [contributions, setContributions] = useState<SlotContributionEntry[]>([]);
  const [plugins, setPlugins] = useState<PluginStatus[]>([]);
  const [available, setAvailable] = useState<AvailablePlugin[]>([]);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [pluginsLoaded, setPluginsLoaded] = useState(false);
  const [refreshMask, setRefreshMask] = useState(false);
  const [closed, setClosed] = useState(false);

  useEffect(() => {
    // 智能体设置读取 store 中的工作区（桌面端由主界面启动时加载）。
    api.getWorkspaceDir()
      .then((dir) => useStore.setState({ workspaceDir: dir }))
      .catch(() => {});
    api.listSlotContributions('settings.plugin-page')
      .then((entries) => setContributions(entries.filter((entry) => entry.has_view)))
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (activeTab !== 'plugin-manager' || pluginsLoaded) return;
    setPluginsLoaded(true);
    void api.listPlugins().then(setPlugins).catch(() => {});
    void api.listAvailablePlugins()
      .then((list) => {
        setAvailable(list);
        setCatalogError(null);
      })
      .catch((error) => setCatalogError(String(error)));
  }, [activeTab, pluginsLoaded]);

  const finish = async () => {
    try {
      await closeWebHost();
    } finally {
      setClosed(true);
    }
  };

  if (closed) {
    return (
      <div className="flex h-screen items-center justify-center text-sm text-muted-foreground">
        配置已保存，服务已关闭，可以关闭此页面。
      </div>
    );
  }

  return (
    <div className="flex h-screen flex-col overflow-hidden bg-background text-foreground">
      <header className="flex h-12 shrink-0 items-center border-b px-4">
        <span className="text-sm font-medium">天工配置</span>
        <span className={`ml-auto mr-4 flex items-center text-xs transition-opacity ${saveStatus === 'idle' ? 'opacity-0' : 'opacity-100'} ${saveStatus === 'error' ? 'text-destructive' : 'text-muted-foreground'}`}>
          {saveStatus === 'saving' && <><Loader2 className="mr-1 h-3 w-3 animate-spin" />保存中...</>}
          {(saveStatus === 'saved' || saveStatus === 'idle') && '已自动保存'}
          {saveStatus === 'error' && '保存失败'}
        </span>
        <Button size="sm" variant="outline" onClick={() => void finish()}>
          <LogOut className="mr-1 h-4 w-4" />完成并关闭
        </Button>
      </header>

      <Tabs value={activeTab} onValueChange={setActiveTab} className="flex min-h-0 min-w-0 flex-1 overflow-hidden">
        <aside className="flex w-14 shrink-0 flex-col border-r bg-muted/30 sm:w-60">
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
              <span className="sr-only sm:not-sr-only">Bot</span>
            </TabsTrigger>
            <TabsTrigger value="plugin-manager" className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
              <Package className="w-4 h-4 sm:mr-2" />
              <span className="sr-only sm:not-sr-only">插件管理</span>
            </TabsTrigger>
            {contributions.map((entry) => (
              <TabsTrigger key={`plugin:${entry.plugin_id}:${entry.contribution_id}`} value={`plugin:${entry.plugin_id}:${entry.contribution_id}`} className="w-full justify-center px-0 py-2 sm:justify-start sm:px-3">
                {contributionIcon(entry.icon)}
                <span className="sr-only sm:not-sr-only">{entry.title}</span>
              </TabsTrigger>
            ))}
          </TabsList>
        </aside>

        <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
          <TabsContent value="agent" className="m-0 flex min-h-0 flex-1 flex-col overflow-hidden">
            <AgentSettings onSaveStatusChange={setSaveStatus} />
          </TabsContent>
          <TabsContent value="llm" className="m-0 min-h-0 flex-1 overflow-hidden">
            <LLMSettings onSaveStatusChange={setSaveStatus} />
          </TabsContent>
          <TabsContent value="server" className="m-0 min-h-0 flex-1 overflow-y-auto">
            <ServerConfigPanel />
          </TabsContent>
          <TabsContent value="bots" className="m-0 min-h-0 flex-1 overflow-y-auto">
            <BotPanel />
          </TabsContent>
          <TabsContent value="plugin-manager" className="m-0 min-h-0 flex-1 overflow-hidden">
            <PluginManagerSettings
              onContributionsChanged={setContributions}
              initialPlugins={plugins}
              initialAvailable={available}
              initialCatalogError={catalogError}
              onRefreshStateChange={setRefreshMask}
            />
          </TabsContent>
          {contributions.map((entry) => (
            <TabsContent key={`plugin:${entry.plugin_id}:${entry.contribution_id}`} value={`plugin:${entry.plugin_id}:${entry.contribution_id}`} className="m-0 flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
              <PluginView contribution={entry} />
            </TabsContent>
          ))}
        </div>
      </Tabs>
      {refreshMask && (
        <div className="fixed inset-0 z-[100] flex items-center justify-center bg-background/80 backdrop-blur-[2px]" role="status" aria-live="polite">
          <div className="flex items-center gap-2 rounded-md border bg-background px-4 py-3 text-sm shadow-xl">
            <Loader2 className="h-4 w-4 animate-spin text-primary" />
            正在刷新插件目录和运行状态…
          </div>
        </div>
      )}
    </div>
  );
}
