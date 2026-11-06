/**
 * 中枢模式切换过渡层（设计稿 §4.1 模式切换的仪式感呈现）。
 *
 * 进入（entering，900ms）：深空径向光晕淡入 → 双层光环自中心扩散 →
 * 横向光束自上而下扫描 → 网格底纹浮现 → 「中枢」二字聚拢显形后随层淡出；
 * 退出（exiting，280ms）：同层快速收缩淡出，让普通视图即时回归。
 *
 * 层为 pointer-events-none 的 fixed 覆盖，不拦截交互；动画结束由
 * setTimeout 回调 onDone 卸载。纯视觉组件，无业务状态。
 */
import { useEffect } from 'react';

/** 进入 / 退出过渡时长（ms），与 tailwind.config.js 中动画时序一致。 */
export const CENTRAL_TRANSITION_MS = {
  entering: 900,
  exiting: 280,
} as const;

export type CentralTransitionPhase = 'entering' | 'exiting';

interface CentralTransitionProps {
  phase: CentralTransitionPhase;
  /** 过渡完成（时长到期）后回调，调用方据此卸载本层。 */
  onDone: () => void;
}

export function CentralTransition({ phase, onDone }: CentralTransitionProps) {
  useEffect(() => {
    const timer = window.setTimeout(onDone, CENTRAL_TRANSITION_MS[phase]);
    return () => window.clearTimeout(timer);
  }, [phase, onDone]);

  const entering = phase === 'entering';

  return (
    <div
      data-testid="central-transition"
      aria-hidden
      className={`pointer-events-none fixed inset-0 z-[100] overflow-hidden ${
        entering ? 'animate-central-overlay-in' : 'animate-central-overlay-out'
      }`}
    >
      {/* 深空底色：径向星云光晕 */}
      <div
        className={`absolute inset-0 ${entering ? 'animate-central-grid' : ''}`}
        style={{
          background:
            'radial-gradient(ellipse 80% 60% at 50% 46%, rgba(34,211,238,.16), rgba(167,139,250,.10) 42%, rgba(5,7,13,.96) 78%), #05070d',
        }}
      />
      {/* 科技网格底纹（仅进入时） */}
      {entering && (
        <div
          className="absolute inset-0 animate-central-grid"
          style={{
            backgroundImage:
              'linear-gradient(rgba(34,211,238,.08) 1px, transparent 1px), linear-gradient(90deg, rgba(34,211,238,.08) 1px, transparent 1px)',
            backgroundSize: '44px 44px',
            maskImage: 'radial-gradient(ellipse 70% 60% at 50% 50%, black 30%, transparent 75%)',
            WebkitMaskImage:
              'radial-gradient(ellipse 70% 60% at 50% 50%, black 30%, transparent 75%)',
          }}
        />
      )}
      {/* 双层扩散光环（错峰） */}
      {entering && (
        <>
          <div className="absolute left-1/2 top-1/2 h-64 w-64 animate-central-ring rounded-full border border-cyan-300/70 shadow-[0_0_48px_rgba(34,211,238,.55)]" />
          <div className="absolute left-1/2 top-1/2 h-64 w-64 animate-central-ring-late rounded-full border border-violet-300/60 shadow-[0_0_48px_rgba(167,139,250,.45)]" />
        </>
      )}
      {/* 横向扫描光束 */}
      {entering && (
        <div
          className="absolute left-0 right-0 top-0 h-px animate-central-sweep"
          style={{
            background:
              'linear-gradient(90deg, transparent, rgba(34,211,238,.9), rgba(167,139,250,.9), transparent)',
            boxShadow: '0 0 26px rgba(34,211,238,.75), 0 0 60px rgba(167,139,250,.45)',
          }}
        />
      )}
      {/* 中心「中枢」字样：letter-spacing 聚拢 + 光晕脉冲 */}
      {entering && (
        <div className="absolute inset-0 flex items-center justify-center">
          <span
            className="animate-central-title text-3xl font-semibold text-cyan-50"
            style={{
              textShadow:
                '0 0 16px rgba(34,211,238,.9), 0 0 44px rgba(167,139,250,.65)',
            }}
          >
            中枢
          </span>
        </div>
      )}
    </div>
  );
}
