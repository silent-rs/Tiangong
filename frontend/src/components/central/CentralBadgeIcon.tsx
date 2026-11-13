/**
 * 中枢后台工作角标图标（用户实测五：文字徽章 → 状态化 SVG）。
 *
 * 一个 22×22 的手绘 SVG 图标，随最高优先级状态转换：
 * - waiting_user：对话气泡 + 问号，呼吸脉动（等你决定，最优先）
 * - running：双弧轨道 + 中心核点，持续旋转（执行中）
 * - failed：警示核 + 三刺脉冲（失败）
 * - interrupted：断口圆环（中断）
 * - done：圆环对勾（待汇报/完成，静态轻光晕）
 * - paused：双竖杆圆环（暂停/取消/排队）
 * - idle：四角星（中枢 ✦，低饱和静止）
 *
 * 动画用 SVG 原生 SMIL（animate/animateTransform）——无需全局 CSS
 * keyframes，WebKit（Tauri）原生支持。
 */

export type BadgeState =
  | 'idle'
  | 'running'
  | 'waiting_user'
  | 'failed'
  | 'interrupted'
  | 'done'
  | 'paused';

const STROKE = { fill: 'none', strokeWidth: 1.6, strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const };

/** 等你决定：对话气泡 + 问号（呼吸）。 */
function WaitingIcon() {
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true">
      <g stroke="var(--badge-accent, #f59e0b)" {...STROKE}>
        <path d="M4.5 5.5 A2.2 2.2 0 0 1 6.7 3.3 H15.3 A2.2 2.2 0 0 1 17.5 5.5 V11.7 A2.2 2.2 0 0 1 15.3 13.9 H10.2 L6.6 17 V13.9 H6.7 A2.2 2.2 0 0 1 4.5 11.7 Z">
          <animate attributeName="opacity" values="1;0.45;1" dur="2.2s" repeatCount="indefinite" />
        </path>
        <path d="M9 6.8 A2 2 0 0 1 13 7.4 C13 8.8 11 9 11 10.4" strokeWidth="1.5">
          <animate attributeName="opacity" values="1;0.5;1" dur="2.2s" repeatCount="indefinite" />
        </path>
        <circle cx="11" cy="12.3" r="0.4" fill="var(--badge-accent, #f59e0b)" stroke="none" />
      </g>
    </svg>
  );
}

/** 执行中：双弧轨道旋转 + 中心核点（微脉冲）。 */
function RunningIcon() {
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true">
      <g transform="translate(11 11)">
        <g stroke="var(--badge-accent, #8b5cf6)" fill="none" strokeWidth="1.7" strokeLinecap="round">
          <path d="M 0 -7.6 A 7.6 7.6 0 0 1 7.6 0">
            <animateTransform
              attributeName="transform"
              type="rotate"
              from="0"
              to="360"
              dur="1.6s"
              repeatCount="indefinite"
            />
          </path>
          <path d="M 0 7.6 A 7.6 7.6 0 0 1 -7.6 0" opacity="0.45">
            <animateTransform
              attributeName="transform"
              type="rotate"
              from="0"
              to="360"
              dur="1.6s"
              repeatCount="indefinite"
            />
          </path>
        </g>
        <circle r="2.1" fill="var(--badge-accent, #8b5cf6)">
          <animate attributeName="r" values="2.1;1.5;2.1" dur="1.6s" repeatCount="indefinite" />
        </circle>
        <circle r="4.6" fill="none" stroke="var(--badge-accent, #8b5cf6)" strokeWidth="0.7" opacity="0.25" />
      </g>
    </svg>
  );
}

/** 失败：警示核 + 三刺外脉冲。 */
function FailedIcon() {
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true">
      <g transform="translate(11 11)">
        <g stroke="var(--badge-accent, #ef4444)" strokeWidth="1.5" strokeLinecap="round">
          <line x1="0" y1="-4.5" x2="0" y2="-8.5">
            <animate attributeName="y1" values="-4.5;-6.5;-4.5" dur="1.4s" repeatCount="indefinite" />
          </line>
          <line x1="4.4" y1="-2.2" x2="7.8" y2="-4">
            <animate attributeName="opacity" values="0.3;1;0.3" dur="1.4s" repeatCount="indefinite" />
          </line>
          <line x1="-4.4" y1="-2.2" x2="-7.8" y2="-4">
            <animate attributeName="opacity" values="0.3;1;0.3" dur="1.4s" repeatCount="indefinite" />
          </line>
        </g>
        <circle r="3.4" fill="var(--badge-accent, #ef4444)">
          <animate attributeName="r" values="3.4;2.8;3.4" dur="1.4s" repeatCount="indefinite" />
        </circle>
        <rect x="-0.7" y="-2.2" width="1.4" height="3" rx="0.7" fill="#fff" />
        <rect x="-0.7" y="1.4" width="1.4" height="1.4" rx="0.7" fill="#fff" />
      </g>
    </svg>
  );
}

/** 中断：断口圆环（西南缺口）+ 中心点。 */
function InterruptedIcon() {
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true">
      <g transform="translate(11 11)" stroke="var(--badge-accent, #94a3b8)" fill="none" strokeWidth="1.7" strokeLinecap="round">
        <path d="M 0 -7.4 A 7.4 7.4 0 1 1 -5.2 5.2" opacity="0.85" />
        <line x1="-7.4" y1="7.4" x2="-8.8" y2="8.8" opacity="0.5" />
        <circle r="1.6" fill="var(--badge-accent, #94a3b8)" stroke="none" opacity="0.6" />
      </g>
    </svg>
  );
}

/** 待汇报/完成：圆环对勾（一次淡入光晕）。 */
function DoneIcon() {
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true">
      <g transform="translate(11 11)">
        <circle r="7.4" fill="none" stroke="var(--badge-accent, #10b981)" strokeWidth="1.7" opacity="0.9">
          <animate attributeName="opacity" values="0.9;0.5;0.9" dur="3s" repeatCount="indefinite" />
        </circle>
        <path
          d="M -3.2 0.2 L -0.8 2.6 L 3.4 -2.4"
          fill="none"
          stroke="var(--badge-accent, #10b981)"
          strokeWidth="1.9"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </g>
    </svg>
  );
}

/** 暂停/取消/排队：双竖杆 + 静态圆环。 */
function PausedIcon() {
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true">
      <g transform="translate(11 11)" stroke="var(--badge-accent, #94a3b8)" fill="none" strokeWidth="1.7" strokeLinecap="round">
        <circle r="7.4" opacity="0.45" />
        <line x1="-1.7" y1="-3" x2="-1.7" y2="3" />
        <line x1="1.7" y1="-3" x2="1.7" y2="3" />
      </g>
    </svg>
  );
}

/** 空闲：四角星（中枢 ✦），低饱和静止。 */
function IdleIcon() {
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true">
      <g transform="translate(11 11)">
        <path
          d="M 0 -7.2 C 0.8 -2.6 2.6 -0.8 7.2 0 C 2.6 0.8 0.8 2.6 0 7.2 C -0.8 2.6 -2.6 0.8 -7.2 0 C -2.6 -0.8 -0.8 -2.6 0 -7.2 Z"
          fill="var(--badge-accent, #94a3b8)"
          opacity="0.5"
        />
        <circle r="1.1" fill="var(--badge-accent, #94a3b8)" opacity="0.75" />
      </g>
    </svg>
  );
}

const ACCENTS: Record<BadgeState, string> = {
  idle: '#94a3b8',
  running: '#8b5cf6',
  waiting_user: '#f59e0b',
  failed: '#ef4444',
  interrupted: '#94a3b8',
  done: '#10b981',
  paused: '#94a3b8',
};

/** 按状态渲染角标图标（accent 经 CSS 变量注入子图形）。 */
export function CentralBadgeIcon({ state }: { state: BadgeState }) {
  const icon = (() => {
    switch (state) {
      case 'running':
        return <RunningIcon />;
      case 'waiting_user':
        return <WaitingIcon />;
      case 'failed':
        return <FailedIcon />;
      case 'interrupted':
        return <InterruptedIcon />;
      case 'done':
        return <DoneIcon />;
      case 'paused':
        return <PausedIcon />;
      default:
        return <IdleIcon />;
    }
  })();
  return (
    <span
      data-testid={`central-badge-${state}`}
      style={{ ['--badge-accent' as string]: ACCENTS[state] }}
      className="inline-flex items-center justify-center transition-transform duration-300"
    >
      {icon}
    </span>
  );
}
