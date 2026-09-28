import { useEffect, useRef, useState } from 'react';
import { Eye, EyeOff, Loader2, RefreshCw } from 'lucide-react';
import { api } from '@/api/tauri';
import type { ModelEntryView, ModelsConfigView, ProviderConfigView } from '@/api/tauri';
import { CODEX_PROVIDER_CONFIG, CODEX_PROVIDER_NAME, CodexAuthPanel } from './CodexAuthPanel';
import { Button } from './ui/button';
import { Input } from './ui/input';
import { Label } from './ui/label';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from './ui/dialog';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from './ui/select';

/** 与设置页一致的预设供应商；快速配置从这里起步。 */
const PRESET_PROVIDERS: Record<string, ProviderConfigView> = {
  DeepSeek: { base_url: 'https://api.deepseek.com', api_key: '', timeout_ms: 300000, protocol: 'deepseek' },
  智谱: { base_url: 'https://open.bigmodel.cn/api/paas/v4', api_key: '', timeout_ms: 300000, protocol: 'openai_chatcompletions' },
  [CODEX_PROVIDER_NAME]: { ...CODEX_PROVIDER_CONFIG },
};

const PROTOCOL_DEFAULT_URLS: Record<string, string> = {
  openai: 'https://api.openai.com/v1',
  openai_chatcompletions: 'https://api.openai.com/v1',
  anthropic: 'https://api.anthropic.com',
};

/** 连接信息固定的预设供应商：Base URL 与协议不可修改。 */
const LOCKED_PRESET_PROVIDERS = new Set(['DeepSeek', CODEX_PROVIDER_NAME]);

const PROTOCOL_OPTIONS = [
  { value: 'openai', label: 'OpenAI Responses' },
  { value: 'openai_chatcompletions', label: 'OpenAI Chat Completions' },
  { value: 'anthropic', label: 'Anthropic' },
];

type Props = {
  /** 是否显示快速配置窗口。 */
  open: boolean;
  /** 关闭/打开变化；关闭即跳过，不阻断其他功能。 */
  onOpenChange: (open: boolean) => void;
};

/**
 * 主页面首次运行模型快速配置弹窗：
 * 未配置主对话模型（chat 路由）时引导用户一步完成供应商连接信息与模型选择，
 * 提交时自动写入模型定义并配置 chat 路由，完成后即可直接发起对话。
 */
export function FirstRunModelSetup({ open, onOpenChange }: Props) {
  const [saving, setSaving] = useState(false);
  const [providerKey, setProviderKey] = useState<string>('DeepSeek');
  const [isCustomProvider, setIsCustomProvider] = useState(false);
  const [customName, setCustomName] = useState('');
  const [draft, setDraft] = useState<ProviderConfigView>({ ...PRESET_PROVIDERS.DeepSeek });
  const [availableModels, setAvailableModels] = useState<string[]>([]);
  const [fetchingModels, setFetchingModels] = useState(false);
  const [modelName, setModelName] = useState('');
  const [showApiKey, setShowApiKey] = useState(false);
  const [codexLoggedIn, setCodexLoggedIn] = useState(false);
  // 表单内联错误提示：不依赖全局 Toast Provider，首次运行场景自包含。
  const [error, setError] = useState<{ title: string; detail?: string } | null>(null);
  // 模型列表请求代号：切换供应商或重新打开弹窗时递增，旧请求返回后直接丢弃，
  // 避免把 A 供应商的模型写入 B 供应商的表单。
  const fetchSeqRef = useRef(0);

  // 每次打开重置表单，避免残留上次的输入。
  useEffect(() => {
    if (!open) return;
    fetchSeqRef.current += 1;
    setFetchingModels(false);
    setProviderKey('DeepSeek');
    setIsCustomProvider(false);
    setCustomName('');
    setDraft({ ...PRESET_PROVIDERS.DeepSeek });
    setAvailableModels([]);
    setFetchingModels(false);
    setModelName('');
    setShowApiKey(false);
    setError(null);
  }, [open]);

  const selectPreset = (key: string) => {
    setError(null);
    fetchSeqRef.current += 1;
    setFetchingModels(false);
    setProviderKey(key);
    setIsCustomProvider(false);
    setCustomName('');
    setDraft({ ...(PRESET_PROVIDERS[key] ?? PRESET_PROVIDERS.DeepSeek) });
    setAvailableModels([]);
    setModelName('');
  };

  const selectCustom = () => {
    setError(null);
    fetchSeqRef.current += 1;
    setFetchingModels(false);
    setIsCustomProvider(true);
    setProviderKey('');
    setDraft({ base_url: '', api_key: '', timeout_ms: 300000, protocol: 'openai_chatcompletions' });
    setAvailableModels([]);
    setModelName('');
  };

  // 锁定供应商（如 DeepSeek）：Base URL 与协议固定为官方值。
  const urlLocked = !isCustomProvider && LOCKED_PRESET_PROVIDERS.has(providerKey);
  // 账号登录鉴权的供应商（ChatGPT）：无需 API Key / Base URL。
  const isOAuth = draft.protocol === 'codex';

  const handleProtocolChange = (protocol: string) => {
    setDraft((prev) => {
      const oldDefault = PROTOCOL_DEFAULT_URLS[prev.protocol] || '';
      const current = prev.base_url.trim();
      // 仅当地址为空或仍等于旧协议默认地址时跟随协议更新，避免覆盖用户自填地址。
      const keepUrl = current !== '' && current !== oldDefault;
      return { ...prev, protocol, base_url: keepUrl ? prev.base_url : (PROTOCOL_DEFAULT_URLS[protocol] || '') };
    });
  };

  // loggedIn：登录回调中传入最新状态，避免读到尚未刷新的 codexLoggedIn。
  const fetchModels = async (loggedIn: boolean = codexLoggedIn) => {
    if (isOAuth && !loggedIn) {
      setError({ title: '请先登录 ChatGPT 账号' });
      return;
    }
    if (!draft.base_url.trim() || (!isOAuth && !draft.api_key.trim())) {
      setError({ title: '请先填写 Base URL 和 API Key' });
      return;
    }
    const seq = ++fetchSeqRef.current;
    setFetchingModels(true);
    try {
      const models = await api.fetchProviderModels(draft.base_url.trim(), draft.api_key.trim(), draft.timeout_ms, draft.protocol);
      // 请求期间已切换供应商或重开弹窗：结果已过期，丢弃且不改状态。
      if (seq !== fetchSeqRef.current) return;
      if (models.length === 0) {
        setError({ title: '该供应商未返回任何模型', detail: '可手动输入模型名称' });
      } else {
        setError(null);
        // 手动输入的模型名不在返回列表中时清空，避免保存界面上不可见的模型。
        setModelName((prev) => (prev.trim() !== '' && !models.includes(prev.trim()) ? '' : prev));
      }
      setAvailableModels(models);
    } catch (err) {
      if (seq !== fetchSeqRef.current) return;
      setError({ title: '无法获取模型列表', detail: String(err) });
      setAvailableModels([]);
    } finally {
      if (seq === fetchSeqRef.current) setFetchingModels(false);
    }
  };

  const submit = async () => {
    const name = isCustomProvider ? customName.trim() : providerKey;
    if (!name) {
      setError({ title: '请为自定义供应商填写名称' });
      return;
    }
    if (!draft.base_url.trim()) {
      setError({ title: '请填写服务商接口地址（Base URL）' });
      return;
    }
    if (isOAuth && !codexLoggedIn) {
      setError({ title: '请先登录 ChatGPT 账号' });
      return;
    }
    if (!isOAuth && !draft.api_key.trim()) {
      setError({ title: '请填写 API Key' });
      return;
    }
    if (!modelName.trim()) {
      setError({ title: '请选择或输入模型名称' });
      return;
    }
    setSaving(true);
    setError(null);
    try {
      // 以服务端最新配置为基底合并，保留既有供应商/模型/其他路由。
      const cfg: ModelsConfigView = await api.getModelsConfig();
      cfg.providers = {
        ...cfg.providers,
        [name]: { ...draft, base_url: draft.base_url.trim(), api_key: draft.api_key.trim() },
      };
      let key = modelName.trim();
      if (cfg.models[key]) key = `${name}-${key}`;
      // ChatGPT（Codex）全系原生支持图片理解：同时注册多模态能力。
      const capabilities = isOAuth ? ['chat', 'multimodal'] : ['chat'];
      const entry: ModelEntryView = { provider: name, model: modelName.trim(), capabilities, options: {} };
      // 补全上下文窗口默认值；失败不阻塞保存。
      try {
        const ctx = await api.resolveModelContextWindow(modelName.trim());
        if (ctx > 0) entry.context_window = ctx;
      } catch { /* ignore */ }
      cfg.models = { ...cfg.models, [key]: { ...entry } };
      // 自动配置主对话路由。
      cfg.routing = { ...cfg.routing, chat: { ...entry } };
      await api.setModelsConfig(cfg);
      onOpenChange(false);
    } catch (err) {
      console.error('保存模型配置失败:', err);
      setError({ title: '保存失败', detail: String(err) });
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(next) => (next ? null : onOpenChange(false))}>
      <DialogContent className="max-w-md" overlayClassName="z-[90]" showCloseButton={!saving}>
        <DialogHeader>
          <DialogTitle>配置对话模型</DialogTitle>
          <DialogDescription>
            尚未配置主对话模型，暂时无法发起对话。填写下方信息即可完成配置；
            更多能力路由可稍后在「设置 → 模型配置」中调整。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-3">
          {/* 供应商选择 */}
          <div>
            <Label className="text-xs">供应商</Label>
            <div className="flex flex-wrap gap-1.5 mt-1">
              {Object.keys(PRESET_PROVIDERS).map((key) => (
                <button
                  key={key}
                  type="button"
                  className={`px-2.5 py-1 text-xs rounded border transition-colors ${
                    !isCustomProvider && providerKey === key
                      ? 'bg-primary/20 text-primary border-primary/40'
                      : 'bg-secondary text-muted-foreground border-border hover:text-foreground'
                  }`}
                  onClick={() => selectPreset(key)}
                >
                  {key}
                </button>
              ))}
              <button
                type="button"
                className={`px-2.5 py-1 text-xs rounded border transition-colors ${
                  isCustomProvider
                    ? 'bg-primary/20 text-primary border-primary/40'
                    : 'bg-secondary text-muted-foreground border-border hover:text-foreground'
                }`}
                onClick={selectCustom}
              >
                自定义
              </button>
            </div>
          </div>

          {isCustomProvider && (
            <div>
              <Label className="text-xs">供应商名称</Label>
              <Input
                value={customName}
                onChange={(e) => setCustomName(e.target.value)}
                className="text-sm h-8 mt-1"
                placeholder="例如 OpenAI、Moonshot"
              />
            </div>
          )}

          {isOAuth ? (
            <CodexAuthPanel
              onStatusChange={(status) => {
                setCodexLoggedIn(status.logged_in);
                if (status.logged_in) void fetchModels(true);
              }}
            />
          ) : (
          <>
          <div>
            <Label className="text-xs">Base URL</Label>
            <Input
              value={draft.base_url}
              onChange={(e) => setDraft({ ...draft, base_url: e.target.value })}
              className="text-sm h-8 mt-1"
              placeholder="https://api.openai.com/v1"
              disabled={urlLocked}
            />
          </div>

          <div>
            <Label className="text-xs">API Key</Label>
            <div className="relative mt-1">
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
          </div>

          <div>
            <Label className="text-xs">请求格式</Label>
            {urlLocked ? (
              <div className="h-8 mt-1 px-3 flex items-center rounded-md border bg-muted/40 text-sm text-muted-foreground">
                {providerKey} 官方协议（{draft.protocol}）
              </div>
            ) : (
              <Select value={draft.protocol || 'openai_chatcompletions'} onValueChange={handleProtocolChange}>
                <SelectTrigger className="text-sm h-8 mt-1">
                  <SelectValue placeholder="选择协议" />
                </SelectTrigger>
                <SelectContent className="z-[95]">
                  {PROTOCOL_OPTIONS.map((opt) => (
                    <SelectItem key={opt.value} value={opt.value}>{opt.label}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            )}
          </div>
          </>
          )}

          <div>
            <div className="flex items-center justify-between">
              <Label className="text-xs">模型</Label>
              <Button variant="ghost" size="sm" className="h-5 text-xs px-2" onClick={() => fetchModels()} disabled={fetchingModels}>
                {fetchingModels
                  ? <><Loader2 className="w-3 h-3 mr-1 animate-spin" />获取中...</>
                  : <><RefreshCw className="w-3 h-3 mr-1" />获取模型列表</>}
              </Button>
            </div>
            {availableModels.length > 0 ? (
              <Select value={modelName} onValueChange={setModelName}>
                <SelectTrigger className="h-8 text-sm mt-1"><SelectValue placeholder="-- 选择模型 --" /></SelectTrigger>
                <SelectContent className="z-[95]">
                  {availableModels.map((m) => <SelectItem key={m} value={m}>{m}</SelectItem>)}
                </SelectContent>
              </Select>
            ) : (
              <Input
                value={modelName}
                onChange={(e) => setModelName(e.target.value)}
                className="text-sm h-8 mt-1"
                placeholder={isOAuth ? '例如 gpt-5.5' : '例如 deepseek-chat、glm-4.6'}
              />
            )}
          </div>
        </div>

        {error && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2">
            <p className="text-xs font-medium text-destructive">{error.title}</p>
            {error.detail && <p className="mt-0.5 text-xs text-destructive/80 break-all">{error.detail}</p>}
          </div>
        )}

        <DialogFooter className="gap-2 sm:gap-0">
          <Button variant="ghost" size="sm" onClick={() => onOpenChange(false)} disabled={saving}>
            稍后配置
          </Button>
          <Button size="sm" onClick={submit} disabled={saving}>
            {saving && <Loader2 className="w-3 h-3 mr-1 animate-spin" />}
            完成配置
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
