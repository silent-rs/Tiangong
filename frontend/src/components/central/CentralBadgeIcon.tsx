/**
 * 中枢后台工作角标图标（用户实测五 v2：对齐全应用 lucide 图标语言）。
 *
 * 精品 lucide 图标 + 状态色 + 光环动画（Tailwind），随最高优先级
 * 状态切换；空闲态与顶栏中枢入口同用 Sparkles——全局视觉一致：
 *
 * | 状态 | 图标 | 表现 |
 * | --- | --- | --- |
 * | 执行中 | Orbit 慢速自转 + ping 光环 | 动画（violet，唯一动态） |
 * | 等你决定 | MessageCircleQuestion | 静态（amber） |
 * | 失败 | OctagonAlert | 静态（red） |
 * | 中断 | ZapOff | 静态（slate） |
 * | 暂停/排队 | PauseCircle | 静态（slate） |
 * | 空闲 | Sparkles（同顶栏中枢入口） | 静态（muted） |
 *
 * 完成即空闲（用户实测七）：completed/待汇报不设专用徽章状态——
 * 徽章回到 Sparkles 空闲态；动画只在执行中（实测六）。
 */
import {
  Sparkles,
  Orbit,
  MessageCircleQuestion,
  OctagonAlert,
  ZapOff,
  PauseCircle,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';

export type BadgeState =
  | 'idle'
  | 'running'
  | 'waiting_user'
  | 'failed'
  | 'interrupted'
  | 'paused';

interface BadgeVisual {
  icon: LucideIcon;
  /** 图标色类（含深色变体）。 */
  color: string;
  /** 光环色（Tailwind bg-*，用于 ping 动画层）。 */
  glow?: string;
  /** 图标类（动画/尺寸微调）。 */
  iconClass?: string;
}

const VISUALS: Record<BadgeState, BadgeVisual> = {
  idle: {
    icon: Sparkles,
    color: 'text-muted-foreground/60',
    iconClass: 'h-[15px] w-[15px]',
  },
  running: {
    icon: Orbit,
    color: 'text-violet-500 dark:text-violet-400',
    glow: 'bg-violet-400',
    iconClass: 'h-[15px] w-[15px] animate-spin [animation-duration:2.8s]',
  },
  waiting_user: {
    icon: MessageCircleQuestion,
    color: 'text-amber-500 dark:text-amber-400',
    iconClass: 'h-[15px] w-[15px]',
  },
  failed: {
    icon: OctagonAlert,
    color: 'text-red-500 dark:text-red-400',
    iconClass: 'h-[15px] w-[15px]',
  },
  interrupted: {
    icon: ZapOff,
    color: 'text-slate-500 dark:text-slate-400',
    iconClass: 'h-[15px] w-[15px]',
  },
  paused: {
    icon: PauseCircle,
    color: 'text-slate-500 dark:text-slate-400',
    iconClass: 'h-[15px] w-[15px]',
  },
};

/** 按状态渲染角标图标（lucide + 状态光环）。 */
export function CentralBadgeIcon({ state }: { state: BadgeState }) {
  const { icon: Icon, color, glow, iconClass } = VISUALS[state];
  return (
    <span
      data-testid={`central-badge-${state}`}
      className="relative inline-flex h-4 w-4 items-center justify-center transition-colors duration-300"
    >
      {glow ? (
        <span
          className={`absolute inline-flex h-4 w-4 rounded-full ${glow}/40 animate-ping`}
          aria-hidden="true"
        />
      ) : null}
      <Icon
        className={`relative ${iconClass ?? 'h-[15px] w-[15px]'} ${color}`}
        strokeWidth={2}
        aria-hidden="true"
      />
    </span>
  );
}
