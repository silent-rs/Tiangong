import { useCallback, useEffect, useRef, useState } from 'react';
import { Loader2 } from 'lucide-react';
import { api } from '@/api/tauri';
import { isRemoteHost } from '@/api/host';
import type { SandboxUpdateState, StartupPrepareResult } from '@/api/tauri';
import { useStore } from '@/store/useStore';
import { useToast } from './Toast';
import { startWindowDrag } from '@/lib/windowDrag';
import appLogo from '../../../src-tauri/icons/128x128.png';

interface DegradedInfo {
  sandboxReason: string | null;
  pluginFailures: string[];
}

/** 组装降级信息；只收集终态失败，准备中不作为异常提示（设置页展示状态）。 */
async function collectDegraded(result: StartupPrepareResult): Promise<DegradedInfo> {
  let sandboxReason = result.degraded_reason;
  const state: SandboxUpdateState | null = await api.getSandboxUpdateState().catch(() => null);
  if (state) {
    // 已查询到的权威状态写入全局 store，输入区"沙箱无效"指示直接消费。
    useStore.getState().setSandboxState(state);
    if (state.status === 'failed' && state.failure) {
      sandboxReason = state.failure;
    }
  }
  return {
    sandboxReason,
    pluginFailures: result.plugin_failures,
  };
}

/** 终态降级以右上角消息提示一次，不带操作：修复入口是设置页与
 *  输入区底部的常驻"沙箱无效"指示。 */
function notifyDegraded(info: DegradedInfo, showWarning: (title: string, message?: string, duration?: number) => void) {
  if (info.sandboxReason) {
    showWarning(
      '沙箱程序无效，插件工具暂不可用',
      `${info.sandboxReason}。可在 设置 → 沙箱管理 修复，对话不受影响`,
      8000,
    );
  }
  if (info.pluginFailures.length > 0) {
    showWarning(
      `${info.pluginFailures.length} 个插件启动失败`,
      '相关工具暂不可用（对话不受影响），详情见 设置 → 插件管理',
      8000,
    );
  }
}

/**
 * 运行环境与插件就绪后挂载主界面；启动准备失败不再阻断进入——
 * 对话不依赖插件与沙箱，降级只影响工具。终态失败以右上角消息
 * 提示一次，常驻状态位在输入区底部与设置页沙箱管理。
 */
export function StartupPrepareGate({ children }: { children: React.ReactNode }) {
  // 远程（手机 H5）：桌面端早已完成启动准备，手机端只是同一会话的另一块屏幕，
  // 不重复执行启动流程，也不提示宿主侧的沙箱/插件降级（修复入口在桌面端设置）。
  if (isRemoteHost()) return <>{children}</>;
  return <LocalStartupPrepareGate>{children}</LocalStartupPrepareGate>;
}

function LocalStartupPrepareGate({ children }: { children: React.ReactNode }) {
  const [ready, setReady] = useState(false);
  const attempt = useRef(0);
  const { showWarning } = useToast();

  const runPrepare = useCallback(async () => {
    const current = ++attempt.current;
    setReady(false);
    const result: StartupPrepareResult = await api.prepareStartupResources().catch(
      (error: unknown) =>
        // 后端已把沙箱/插件失败转为降级结果；这里的异常属于意外，同样放行。
        ({
          installed_version: null,
          degraded_reason: String(error),
          plugin_failures: [],
        }) satisfies StartupPrepareResult,
    );
    if (current !== attempt.current) return;
    // 状态机复检：以后端权威状态为准；沙箱仍准备中属进行时状态，
    // 短暂等待后直接放行，不提示，状态由设置页展示。
    for (let round = 0; round < 10; round += 1) {
      const state = await api.getSandboxUpdateState();
      if (current !== attempt.current) return;
      if (state.status === 'preparing') {
        await new Promise((resolve) => setTimeout(resolve, 500));
        continue;
      }
      break;
    }
    if (current !== attempt.current) return;
    const info = await collectDegraded(result);
    if (current !== attempt.current) return;
    notifyDegraded(info, showWarning);
    setReady(true);
  }, [showWarning]);

  useEffect(() => {
    void runPrepare();
    return () => {
      attempt.current += 1;
    };
  }, [runPrepare]);

  // 门闸在 preparing 阶段放行（5s 短窗口耗尽）后，沙箱准备可能稍后才
  // 落到终态：静默续查（进行时不提示），终态失败补一次消息并刷新全局
  // 状态点亮输入区指示，终态就绪只刷新状态。不走后端事件——主界面
  // 挂载晚于 emit，事件在首启场景必然丢失。
  useEffect(() => {
    if (!ready) return;
    if (useStore.getState().sandboxState?.status !== 'preparing') return;
    let stopped = false;
    const timer = window.setInterval(() => {
      void api
        .getSandboxUpdateState()
        .then((next) => {
          if (stopped || next.status === 'preparing') return;
          stopped = true;
          window.clearInterval(timer);
          useStore.getState().setSandboxState(next);
          if (next.status === 'failed') {
            notifyDegraded(
              { sandboxReason: next.failure ?? '沙箱程序安装失败', pluginFailures: [] },
              showWarning,
            );
          }
        })
        .catch(() => undefined);
    }, 5000);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, [ready, showWarning]);

  // 窗口为 macOS Overlay 标题栏，启动期间无系统拖动区；header 与主界面一致提供拖动。
  if (!ready) {
    return (
      <div className="flex h-screen w-full flex-col bg-background">
        <header
          className="flex h-12 shrink-0 border-b select-none"
          onMouseDown={startWindowDrag}
        />
        <div className="flex min-h-0 w-full flex-1 items-center justify-center p-6">
          <div className="w-full max-w-sm space-y-6 text-center" role="status" aria-live="polite" aria-busy="true">
            <div className="relative mx-auto flex h-20 w-20 items-center justify-center">
              <Loader2 className="absolute inset-0 h-20 w-20 animate-spin text-muted-foreground/40 motion-reduce:animate-none" strokeWidth={1} aria-hidden="true" />
              <img src={appLogo} alt="" className="h-12 w-12 object-contain motion-safe:animate-pulse" />
            </div>
            <div className="space-y-2">
              <h1 className="text-xl font-semibold">天工</h1>
              <p className="text-sm text-muted-foreground">
                天工正在启动中
              </p>
            </div>
          </div>
        </div>
      </div>
    );
  }

  return <>{children}</>;
}
