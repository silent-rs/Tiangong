import { useCallback, useEffect, useState } from 'react';
import { QRCodeSVG } from 'qrcode.react';
import {
  Check, ChevronDown, Copy, Globe, Loader2, QrCode, RefreshCw, Smartphone, Unlink, Wifi,
} from 'lucide-react';
import { api, type RemoteAccessMode, type RemoteAccessView, type RemotePairing } from '@/api/tauri';
import { cn } from '@/lib/utils';
import { Button } from './ui/button';
import { Card, CardContent } from './ui/card';
import { Input } from './ui/input';
import { Label } from './ui/label';
import { Switch } from './ui/switch';
import { Badge } from './ui/badge';
import { useToast } from './Toast';

type LinkState = RemoteAccessView['state'];

const STATE_LABEL: Record<RemoteAccessMode, Record<LinkState, string>> = {
  lan: { disabled: '未启用', connecting: '启动中', connected: '运行中', error: '启动失败' },
  relay: { disabled: '未启用', connecting: '连接中', connected: '已连接', error: '连接失败' },
};

const STATE_DOT: Record<LinkState, string> = {
  disabled: 'bg-muted-foreground/40',
  connecting: 'bg-amber-500 animate-pulse',
  connected: 'bg-emerald-500',
  error: 'bg-destructive',
};

const MODES: { value: RemoteAccessMode; title: string; desc: string; icon: typeof Wifi }[] = [
  { value: 'relay', title: '中继服务', desc: '任意网络可用，默认使用官方中继', icon: Globe },
  { value: 'lan', title: '局域网直连', desc: '同一 Wi-Fi 下直连，不经过外部服务', icon: Wifi },
];

interface Draft {
  mode: RemoteAccessMode;
  host: string;
  lanHost: string;
  lanPort: string;
}

function draftOf(view: RemoteAccessView): Draft {
  return {
    mode: view.mode,
    host: view.host,
    lanHost: view.lan_host,
    lanPort: String(view.lan_port),
  };
}

/**
 * 设置 → 远程访问：手机扫码使用天工对话。
 * - 中继（缺省）：经 tiangong-relay 跨网络访问，缺省使用官方中继，也可填写自部署地址；
 * - 局域网直连：手机与电脑在同一网络，桌面端自身监听局域网端口，不经过外部服务。
 * 只绑定一个设备，重新扫码会取代旧设备。
 */
export function RemoteAccessSettings() {
  const { showError, showSuccess } = useToast();
  const [view, setView] = useState<RemoteAccessView | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [saving, setSaving] = useState(false);
  const [pairing, setPairing] = useState<RemotePairing | null>(null);
  const [remaining, setRemaining] = useState(0);
  const [copied, setCopied] = useState(false);
  const [advanced, setAdvanced] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const next = await api.remoteGetConfig();
      setView(next);
      return next;
    } catch (error) {
      showError('读取远程访问配置失败', String(error));
      return null;
    }
  }, [showError]);

  useEffect(() => {
    void refresh().then((next) => {
      if (next) setDraft(draftOf(next));
    });
    const timer = window.setInterval(() => { void refresh(); }, 3000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  // 配对码倒计时；过期后清除二维码。
  useEffect(() => {
    if (!pairing) return;
    const deadline = Date.now() + pairing.expires_in_secs * 1000;
    const tick = () => {
      const left = Math.max(0, Math.round((deadline - Date.now()) / 1000));
      setRemaining(left);
      if (left === 0) setPairing(null);
    };
    tick();
    const timer = window.setInterval(tick, 1000);
    return () => window.clearInterval(timer);
  }, [pairing]);

  // 配对成功（设备上线）后收起二维码。
  useEffect(() => {
    if (pairing && view?.device_online) setPairing(null);
  }, [pairing, view?.device_online]);

  const save = async (enabled: boolean, next: Draft | null = draft) => {
    if (!next) return;
    const port = next.lanPort.trim() ? Number(next.lanPort) : null;
    if (port !== null && (!Number.isInteger(port) || port < 1 || port > 65535)) {
      showError('端口无效', '请输入 1–65535 之间的端口号');
      return;
    }
    setSaving(true);
    try {
      const updated = await api.remoteSetConfig({
        enabled,
        mode: next.mode,
        host: next.host,
        lanHost: next.lanHost,
        lanPort: port,
      });
      setView(updated);
      setDraft(draftOf(updated));
      setPairing(null);
      showSuccess(enabled ? '远程访问已启用' : '远程访问已关闭');
    } catch (error) {
      showError('保存失败', String(error));
    } finally {
      setSaving(false);
    }
  };

  const createPairing = async () => {
    try {
      setPairing(await api.remoteCreatePairing());
    } catch (error) {
      showError('生成二维码失败', String(error));
    }
  };

  const unbind = async () => {
    try {
      setView(await api.remoteUnbindDevice());
      setPairing(null);
      showSuccess('已解除设备绑定');
    } catch (error) {
      showError('解除绑定失败', String(error));
    }
  };

  const resetChannel = async () => {
    if (!window.confirm('重置通道后，原访问地址立即失效，已绑定的设备需要重新扫码。确定重置？')) return;
    try {
      setView(await api.remoteResetChannel());
      setPairing(null);
      showSuccess('已重置远程通道');
    } catch (error) {
      showError('重置通道失败', String(error));
    }
  };

  const copyAccessUrl = async (url: string) => {
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch (error) {
      showError('复制失败', String(error));
    }
  };

  if (!view || !draft) {
    return (
      <div className="flex items-center gap-2 p-6 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />加载中…
      </div>
    );
  }

  const saved = draftOf(view);
  const dirty = (Object.keys(saved) as (keyof Draft)[]).some((key) => draft[key].trim() !== saved[key]);
  const lan = draft.mode === 'lan';
  const running = view.enabled && view.state === 'connected';
  const ready = running && !dirty;

  const switchMode = (mode: RemoteAccessMode) => {
    if (mode === draft.mode) return;
    const next = { ...draft, mode };
    setDraft(next);
    // 已启用时切换方式立即生效。
    if (view.enabled) void save(true, next);
  };

  return (
    <div className="space-y-4 p-4 sm:p-6">
      {/* 总开关与运行状态 */}
      <Card>
        <CardContent className="space-y-4 pt-6">
          <div className="flex items-start justify-between gap-4">
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <Label className="text-base">远程访问</Label>
                <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
                  <span className={cn('h-2 w-2 rounded-full', STATE_DOT[view.state])} />
                  {STATE_LABEL[view.mode][view.state]}
                </span>
              </div>
              <p className="mt-1 text-xs text-muted-foreground">
                用手机浏览器扫码即可使用天工对话。远程端仅开放对话功能，同一时间只允许一台已绑定的设备使用。
              </p>
            </div>
            <Switch
              checked={view.enabled}
              disabled={saving}
              onCheckedChange={(checked) => { void save(checked); }}
            />
          </div>

          {running && view.access_url && (
            <div className="flex items-center gap-2 rounded-md bg-muted/50 px-3 py-2">
              <span className="shrink-0 text-xs text-muted-foreground">访问地址</span>
              <span className="min-w-0 flex-1 truncate font-mono text-xs" title={view.access_url}>
                {view.access_url}
              </span>
              <Button
                variant="ghost"
                size="icon"
                className="h-7 w-7 shrink-0"
                title="复制访问地址"
                onClick={() => { void copyAccessUrl(view.access_url!); }}
              >
                {copied ? <Check className="h-3.5 w-3.5 text-emerald-500" /> : <Copy className="h-3.5 w-3.5" />}
              </Button>
            </div>
          )}
          {view.last_error && (
            <p className="rounded-md bg-destructive/10 px-3 py-2 text-xs text-destructive break-all">
              {view.last_error}
            </p>
          )}
        </CardContent>
      </Card>

      {/* 连接方式 */}
      <Card>
        <CardContent className="space-y-4 pt-6">
          <Label className="text-base">连接方式</Label>
          <div className="grid gap-2 sm:grid-cols-2">
            {MODES.map(({ value, title, desc, icon: Icon }) => {
              const active = draft.mode === value;
              return (
                <button
                  key={value}
                  type="button"
                  disabled={saving}
                  onClick={() => switchMode(value)}
                  className={cn(
                    'flex items-start gap-3 rounded-lg border p-3 text-left transition-colors disabled:opacity-60',
                    active ? 'border-primary bg-primary/5' : 'hover:bg-accent',
                  )}
                >
                  <Icon className={cn('mt-0.5 h-4 w-4 shrink-0', active ? 'text-primary' : 'text-muted-foreground')} />
                  <span className="min-w-0">
                    <span className="block text-sm font-medium">{title}</span>
                    <span className="block text-xs text-muted-foreground">{desc}</span>
                  </span>
                </button>
              );
            })}
          </div>

          {lan ? (
            <div className="space-y-2">
              <div className="grid gap-3 sm:grid-cols-[1fr_7rem]">
                <div className="space-y-1.5">
                  <Label htmlFor="remote-lan-host" className="text-xs">局域网地址</Label>
                  <Input
                    id="remote-lan-host"
                    value={draft.lanHost}
                    onChange={(e) => setDraft({ ...draft, lanHost: e.target.value })}
                    placeholder={view.detected_lan_ip ? `自动（${view.detected_lan_ip}）` : '未探测到，请手动填写'}
                    autoComplete="off"
                  />
                </div>
                <div className="space-y-1.5">
                  <Label htmlFor="remote-lan-port" className="text-xs">端口</Label>
                  <Input
                    id="remote-lan-port"
                    inputMode="numeric"
                    value={draft.lanPort}
                    onChange={(e) => setDraft({ ...draft, lanPort: e.target.value.replace(/\D/g, '') })}
                    placeholder="8790"
                  />
                </div>
              </div>
              <p className="text-xs text-muted-foreground">
                手机需与电脑连接同一网络；局域网内为 HTTP 明文传输，请勿在公共网络中开启。首次启用时如系统询问是否允许传入连接，请选择允许。
              </p>
            </div>
          ) : (
            <div className="space-y-1.5">
              <Label htmlFor="remote-host" className="text-xs">中继地址</Label>
              <Input
                id="remote-host"
                value={draft.host}
                onChange={(e) => setDraft({ ...draft, host: e.target.value })}
                placeholder={`默认：${view.default_host}`}
                autoComplete="off"
              />
              <p className="text-xs text-muted-foreground">
                留空使用默认中继 {view.default_host}；也可填写自部署的 tiangong-relay 地址（无需令牌，公网请使用 HTTPS）。
              </p>
            </div>
          )}

          {(dirty || !view.enabled) && (
            <div className="flex justify-end">
              <Button disabled={saving} onClick={() => { void save(true); }}>
                {saving && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
                {view.enabled ? '保存' : '保存并启用'}
              </Button>
            </div>
          )}
        </CardContent>
      </Card>

      {/* 设备绑定 */}
      <Card>
        <CardContent className="space-y-4 pt-6">
          <div className="flex items-center justify-between gap-4">
            <div>
              <Label className="text-base">手机扫码</Label>
              <p className="mt-1 text-xs text-muted-foreground">
                二维码 10 分钟内有效且仅能使用一次；绑定新设备后，之前的设备将失效。
              </p>
            </div>
            <Button
              variant="outline"
              disabled={!ready}
              title={ready ? undefined : '请先启用远程访问并保存配置'}
              onClick={() => { void createPairing(); }}
            >
              {pairing ? <RefreshCw className="mr-1 h-4 w-4" /> : <QrCode className="mr-1 h-4 w-4" />}
              {pairing ? '重新生成' : '生成二维码'}
            </Button>
          </div>

          {pairing && (
            <div className="flex flex-col items-center gap-2 py-2">
              <div className="rounded-lg bg-white p-3 shadow-sm">
                <QRCodeSVG value={pairing.url} size={208} level="M" />
              </div>
              <span className="text-xs text-muted-foreground">
                请用手机浏览器扫码（{Math.floor(remaining / 60)}:{String(remaining % 60).padStart(2, '0')} 后失效）
              </span>
            </div>
          )}

          <div className="flex items-center justify-between gap-4 rounded-md border p-3">
            <div className="flex min-w-0 items-center gap-3">
              <Smartphone className="h-5 w-5 shrink-0 text-muted-foreground" />
              <div className="min-w-0 text-sm">
                {view.device_bound ? (
                  <>
                    <div className="flex items-center gap-2">
                      已绑定设备
                      <Badge variant={view.device_online ? 'default' : 'secondary'}>
                        {view.device_online ? '在线' : '离线'}
                      </Badge>
                    </div>
                    <div className="truncate text-xs text-muted-foreground" title={view.device_label ?? undefined}>
                      {view.device_bound_at ? `绑定于 ${view.device_bound_at}` : ''}
                      {view.device_label ? ` · ${view.device_label}` : ''}
                    </div>
                  </>
                ) : (
                  <span className="text-muted-foreground">尚未绑定设备</span>
                )}
              </div>
            </div>
            {view.device_bound && (
              <Button variant="ghost" size="sm" onClick={() => { void unbind(); }}>
                <Unlink className="mr-1 h-4 w-4" />解除绑定
              </Button>
            )}
          </div>
        </CardContent>
      </Card>

      {/* 高级：远程通道 */}
      <div className="rounded-lg border">
        <button
          type="button"
          className="flex w-full items-center justify-between px-4 py-3 text-sm text-muted-foreground hover:text-foreground"
          onClick={() => setAdvanced((value) => !value)}
        >
          高级
          <ChevronDown className={cn('h-4 w-4 transition-transform', advanced && 'rotate-180')} />
        </button>
        {advanced && (
          <div className="flex items-center justify-between gap-3 border-t px-4 py-3">
            <div className="min-w-0 text-xs">
              <div className="text-sm">远程通道</div>
              <div className="truncate font-mono text-muted-foreground" title={view.channel ?? undefined}>
                {view.channel ?? '启用后自动生成'}
              </div>
              <div className="mt-1 text-muted-foreground">
                通道密钥由天工自动生成并仅保存在本机。访问地址泄露或提示通道被占用时可重置，重置后需重新扫码。
              </div>
            </div>
            <Button variant="outline" size="sm" disabled={saving || !view.channel} onClick={() => { void resetChannel(); }}>
              <RefreshCw className="mr-1 h-4 w-4" />重置
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}
