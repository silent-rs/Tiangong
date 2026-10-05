import { useCallback, useEffect, useState } from 'react';
import { QRCodeSVG } from 'qrcode.react';
import { Loader2, QrCode, RefreshCw, Smartphone, Unlink, KeyRound, Wifi, Globe } from 'lucide-react';
import { api, type RemoteAccessMode, type RemoteAccessView, type RemotePairing } from '@/api/tauri';
import { Button } from './ui/button';
import { Card, CardContent } from './ui/card';
import { Input } from './ui/input';
import { Label } from './ui/label';
import { Switch } from './ui/switch';
import { Badge } from './ui/badge';
import { useToast } from './Toast';

const STATE_LABEL: Record<RemoteAccessMode, Record<RemoteAccessView['state'], string>> = {
  lan: { disabled: '未启用', connecting: '启动中', connected: '局域网服务运行中', error: '启动失败' },
  relay: { disabled: '未启用', connecting: '连接中', connected: '已连接中继', error: '连接失败' },
};

interface Draft {
  mode: RemoteAccessMode;
  host: string;
  token: string;
  lanHost: string;
  lanPort: string;
}

function draftOf(view: RemoteAccessView): Draft {
  return {
    mode: view.mode,
    host: view.host,
    token: view.token,
    lanHost: view.lan_host,
    lanPort: String(view.lan_port),
  };
}

/**
 * 设置 → 远程访问：手机扫码使用天工对话。
 * - 局域网直连：手机与电脑在同一网络，桌面端自身监听局域网端口，无需部署任何服务；
 * - 中继：经自部署的 tiangong-relay 跨网络访问。
 * 只绑定一个设备，重新扫码会取代旧设备。
 */
export function RemoteAccessSettings() {
  const { showError, showSuccess } = useToast();
  const [view, setView] = useState<RemoteAccessView | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [saving, setSaving] = useState(false);
  const [pairing, setPairing] = useState<RemotePairing | null>(null);
  const [remaining, setRemaining] = useState(0);

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
        token: next.token,
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
  const ready = view.enabled && view.state === 'connected' && !dirty;

  const switchMode = (mode: RemoteAccessMode) => {
    if (mode === draft.mode) return;
    const next = { ...draft, mode };
    setDraft(next);
    // 已启用时切换方式立即生效。
    if (view.enabled) void save(true, next);
  };

  return (
    <div className="space-y-4 p-4 sm:p-6">
      <Card>
        <CardContent className="space-y-4 pt-6">
          <div className="flex items-start justify-between gap-4">
            <div>
              <Label className="text-base">远程访问</Label>
              <p className="mt-1 text-xs text-muted-foreground">
                在手机浏览器中扫码使用天工对话。远程端只能使用对话功能，不开放设置与拓展区；
                同一时间只允许一台已绑定的设备使用。
              </p>
            </div>
            <Switch
              checked={view.enabled}
              disabled={saving}
              onCheckedChange={(checked) => { void save(checked); }}
            />
          </div>

          <div className="grid grid-cols-2 gap-2">
            <Button
              variant={lan ? 'default' : 'outline'}
              className="justify-start"
              disabled={saving}
              onClick={() => switchMode('lan')}
            >
              <Wifi className="mr-2 h-4 w-4" />局域网直连
            </Button>
            <Button
              variant={!lan ? 'default' : 'outline'}
              className="justify-start"
              disabled={saving}
              onClick={() => switchMode('relay')}
            >
              <Globe className="mr-2 h-4 w-4" />中继服务
            </Button>
          </div>

          {lan ? (
            <>
              <p className="text-xs text-muted-foreground">
                手机与电脑连接同一 Wi-Fi 即可扫码使用，无需部署任何服务。首次启用时系统可能询问是否允许天工接受传入连接，请选择允许。
              </p>
              <div className="grid gap-3 sm:grid-cols-[1fr_8rem]">
                <div className="space-y-2">
                  <Label htmlFor="remote-lan-host">局域网地址</Label>
                  <Input
                    id="remote-lan-host"
                    value={draft.lanHost}
                    onChange={(e) => setDraft({ ...draft, lanHost: e.target.value })}
                    placeholder={view.detected_lan_ip ? `自动：${view.detected_lan_ip}` : '自动探测失败，请手动填写'}
                    autoComplete="off"
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="remote-lan-port">端口</Label>
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
                局域网地址留空时自动探测；多网卡或探测不准时可手动填写电脑在当前 Wi-Fi 下的 IP。局域网内为 HTTP 明文传输，请勿在公共网络中开启。
              </p>
            </>
          ) : (
            <>
              <div className="space-y-2">
                <Label htmlFor="remote-host">中继地址（Host）</Label>
                <Input
                  id="remote-host"
                  value={draft.host}
                  onChange={(e) => setDraft({ ...draft, host: e.target.value })}
                  placeholder="https://relay.example.com"
                  autoComplete="off"
                />
                <p className="text-xs text-muted-foreground">公网部署请使用 HTTPS 地址，手机扫码后直接打开该地址。</p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="remote-token">接入令牌</Label>
                <div className="flex gap-2">
                  <Input
                    id="remote-token"
                    type="password"
                    value={draft.token}
                    onChange={(e) => setDraft({ ...draft, token: e.target.value })}
                    placeholder="与中继 TIANGONG_RELAY_TOKEN 一致，至少 16 位"
                    autoComplete="off"
                  />
                  <Button
                    variant="outline"
                    title="生成随机令牌"
                    onClick={() => { void api.remoteGenerateToken().then((token) => setDraft({ ...draft, token })); }}
                  >
                    <KeyRound className="mr-1 h-4 w-4" />生成
                  </Button>
                </div>
              </div>
            </>
          )}

          <div className="flex flex-wrap items-center gap-2">
            <Button disabled={saving || (!dirty && view.enabled)} onClick={() => { void save(true); }}>
              {saving && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
              {lan ? '保存并启动' : '保存并连接'}
            </Button>
            <Badge variant={view.state === 'connected' ? 'default' : view.state === 'error' ? 'destructive' : 'secondary'}>
              {STATE_LABEL[view.mode][view.state]}
            </Badge>
            {view.enabled && view.access_url && view.state === 'connected' && (
              <span className="text-xs text-muted-foreground break-all">访问地址：{view.access_url}</span>
            )}
            {view.last_error && (
              <span className="text-xs text-destructive break-all">{view.last_error}</span>
            )}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="space-y-4 pt-6">
          <div className="flex items-center justify-between gap-4">
            <div>
              <Label className="text-base">手机扫码</Label>
              <p className="mt-1 text-xs text-muted-foreground">
                二维码 10 分钟内有效且只能使用一次。扫码绑定新设备后，之前绑定的设备将失效。
              </p>
            </div>
            <Button variant="outline" disabled={!ready} onClick={() => { void createPairing(); }}>
              {pairing ? <RefreshCw className="mr-1 h-4 w-4" /> : <QrCode className="mr-1 h-4 w-4" />}
              {pairing ? '重新生成' : '生成二维码'}
            </Button>
          </div>

          {pairing && (
            <div className="flex flex-col items-center gap-2 py-2">
              <div className="rounded-lg bg-white p-3">
                <QRCodeSVG value={pairing.url} size={208} level="M" />
              </div>
              <span className="text-xs text-muted-foreground">
                请用手机浏览器扫码打开（{Math.floor(remaining / 60)}:{String(remaining % 60).padStart(2, '0')} 后失效）
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
    </div>
  );
}
