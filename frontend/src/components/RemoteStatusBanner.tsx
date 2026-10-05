import { useEffect, useState } from 'react';
import { getRemoteStatus, onRemoteStatus, type RemoteStatus } from '@/api/remote';

/** 远程 H5 连接状态条：连接中/离线/被拒绝时显示，就绪后隐藏。 */
export function RemoteStatusBanner() {
  const [status, setStatus] = useState<RemoteStatus>(getRemoteStatus());
  useEffect(() => onRemoteStatus(setStatus), []);

  if (status.kind === 'ready') return null;
  const text = status.kind === 'connecting' ? '正在连接天工桌面端…' : status.reason;
  const tone = status.kind === 'denied'
    ? 'bg-destructive/10 text-destructive'
    : 'bg-amber-500/10 text-amber-600 dark:text-amber-400';
  return (
    <div role="status" className={`shrink-0 px-4 py-2 text-center text-xs ${tone}`}>
      {text}
    </div>
  );
}
