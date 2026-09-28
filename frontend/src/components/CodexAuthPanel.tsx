import { useCallback, useEffect, useRef, useState } from 'react';
import { Copy, Loader2, LogIn, LogOut, RefreshCw } from 'lucide-react';
import { api } from '@/api/tauri';
import type { CodexAuthStatus, CodexLoginStart } from '@/api/tauri';
import { Button } from './ui/button';
import { Label } from './ui/label';
import { CodexUsagePanel } from './CodexUsagePanel';

/** ChatGPT（Codex 登录）固定供应商名，与 CLI 保持一致。 */
export const CODEX_PROVIDER_NAME = 'ChatGPT';

/** ChatGPT 固定供应商配置：鉴权来自账号登录，无需 API Key。 */
export const CODEX_PROVIDER_CONFIG = {
  base_url: 'https://chatgpt.com/backend-api/codex',
  api_key: '',
  timeout_ms: 300000,
  protocol: 'codex',
};

type Props = {
  /** 登录状态变化（登录成功 / 退出）后回调。 */
  onStatusChange?: (status: CodexAuthStatus) => void;
};

/**
 * ChatGPT 账号登录面板：展示登录状态，支持浏览器登录、设备码登录与退出。
 * 浏览器登录由后端打开系统浏览器并在本地 1455 端口接收回调。
 */
export function CodexAuthPanel({ onStatusChange }: Props) {
  const [status, setStatus] = useState<CodexAuthStatus | null>(null);
  const [pending, setPending] = useState<CodexLoginStart | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  // 组件卸载或发起新登录后丢弃旧的等待结果。
  const waitSeqRef = useRef(0);

  const updateStatus = useCallback((next: CodexAuthStatus) => {
    setStatus(next);
    onStatusChange?.(next);
  }, [onStatusChange]);

  useEffect(() => {
    let cancelled = false;
    api.codexAuthStatus()
      .then((next) => { if (!cancelled) updateStatus(next); })
      .catch((err) => { if (!cancelled) setError(String(err)); });
    return () => {
      cancelled = true;
      waitSeqRef.current += 1;
    };
    // 仅挂载时读取一次状态。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const startLogin = async (method: 'browser' | 'device') => {
    setError(null);
    setBusy(true);
    const seq = ++waitSeqRef.current;
    try {
      const start = await api.codexAuthStart(method);
      if (seq !== waitSeqRef.current) return;
      setPending(start);
      const next = await api.codexAuthWait();
      if (seq !== waitSeqRef.current) return;
      updateStatus(next);
      setPending(null);
    } catch (err) {
      if (seq !== waitSeqRef.current) return;
      setError(String(err));
      setPending(null);
    } finally {
      if (seq === waitSeqRef.current) setBusy(false);
    }
  };

  const cancelLogin = async () => {
    waitSeqRef.current += 1;
    setPending(null);
    setBusy(false);
    try { setStatus(await api.codexAuthCancel()); } catch { /* ignore */ }
  };

  const logout = async () => {
    setError(null);
    try {
      updateStatus(await api.codexAuthLogout());
    } catch (err) {
      setError(String(err));
    }
  };

  const [refreshing, setRefreshing] = useState(false);
  const refreshToken = async () => {
    setError(null);
    setRefreshing(true);
    try {
      updateStatus(await api.codexAuthRefresh());
    } catch (err) {
      setError(String(err));
    } finally {
      setRefreshing(false);
    }
  };

  const expiresAt = status?.expires_at ? new Date(status.expires_at * 1000) : null;
  const expired = expiresAt ? expiresAt.getTime() <= Date.now() : false;

  const copyUrl = async () => {
    if (!pending) return;
    try {
      await navigator.clipboard.writeText(pending.url);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch { /* ignore */ }
  };

  return (
    <div className="space-y-2">
      <Label className="text-xs">ChatGPT 账号</Label>
      {status === null ? (
        <div className="text-xs text-muted-foreground flex items-center gap-1">
          <Loader2 className="w-3 h-3 animate-spin" />读取登录状态...
        </div>
      ) : status.logged_in && !pending ? (
        <div className="flex items-center justify-between gap-2 rounded-md border px-3 py-2">
          <div className="min-w-0">
            <div className="text-sm truncate">{status.email || '已登录'}</div>
            <div className="text-xs text-muted-foreground">
              {status.plan_type && <span className="mr-2">套餐：{status.plan_type}</span>}
              {expiresAt && (
                <span className={expired ? 'text-destructive' : undefined}>
                  令牌{expired ? '已过期' : '有效至'}：{expiresAt.toLocaleString()}
                </span>
              )}
            </div>
          </div>
          <div className="flex shrink-0 items-center">
            <Button
              variant="ghost"
              size="sm"
              className="h-7"
              onClick={refreshToken}
              disabled={refreshing}
              title="对话会自动续期；生图等工具不会自动续期，令牌过期时在此手动刷新"
            >
              {refreshing
                ? <Loader2 className="w-3 h-3 mr-1 animate-spin" />
                : <RefreshCw className="w-3 h-3 mr-1" />}
              刷新令牌
            </Button>
            <Button variant="ghost" size="sm" className="h-7" onClick={logout}>
              <LogOut className="w-3 h-3 mr-1" />退出
            </Button>
          </div>
        </div>
      ) : pending ? (
        <div className="rounded-md border px-3 py-2 space-y-2">
          {pending.user_code ? (
            <p className="text-xs text-muted-foreground">
              已在浏览器打开验证页，请输入验证码：
              <span className="ml-1 font-mono text-base text-foreground tracking-widest">{pending.user_code}</span>
            </p>
          ) : (
            <p className="text-xs text-muted-foreground">已在浏览器打开 ChatGPT 授权页，完成登录后将自动返回。</p>
          )}
          <div className="flex items-center gap-2">
            <Loader2 className="w-3 h-3 animate-spin text-muted-foreground" />
            <span className="text-xs text-muted-foreground flex-1">等待授权...</span>
            <Button variant="ghost" size="sm" className="h-6 text-xs px-2" onClick={copyUrl}>
              <Copy className="w-3 h-3 mr-1" />{copied ? '已复制' : '复制链接'}
            </Button>
            <Button variant="ghost" size="sm" className="h-6 text-xs px-2" onClick={cancelLogin}>取消</Button>
          </div>
        </div>
      ) : (
        <div className="flex flex-wrap gap-2">
          <Button size="sm" className="h-8" onClick={() => startLogin('browser')} disabled={busy}>
            <LogIn className="w-3 h-3 mr-1" />登录 ChatGPT
          </Button>
          <Button variant="outline" size="sm" className="h-8" onClick={() => startLogin('device')} disabled={busy}>
            设备码登录
          </Button>
        </div>
      )}
      <p className="text-xs text-muted-foreground">
        使用 ChatGPT Plus / Pro 等订阅额度调用 GPT 模型，无需 API Key。
      </p>
      {status?.logged_in && !pending && <CodexUsagePanel key={status.expires_at ?? 0} />}
      {error && <p className="text-xs text-destructive break-all">{error}</p>}
    </div>
  );
}
