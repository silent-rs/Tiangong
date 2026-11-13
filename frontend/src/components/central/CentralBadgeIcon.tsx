/**
 * 中枢后台工作角标——「枢纽轨道」原创 SVG 素材（用户实测八/九：
 * 独立 .svg 文件加载，不用图标库）。
 *
 * 素材在 `src/assets/central/badge-*.svg`（可视化管理/替换）；
 * 同一构图（三同心弧 + 中心核）的多个相位：
 *
 * | 相位 | 素材 | 表现 |
 * | --- | --- | --- |
 * | idle 静默轨道 | badge-idle.svg | 静态 |
 * | running 轨道运转 | badge-running.svg | 渐变双弧反向旋转 + 核脉冲（唯一动态） |
 * | waiting_user 呼出信号 | badge-waiting-user.svg | 静态（amber） |
 * | failed 核心警报 | badge-failed.svg | 静态（red） |
 * | interrupted 断链 | badge-interrupted.svg | 静态（slate） |
 * | paused 静止 | badge-paused.svg | 静态（slate） |
 *
 * 动画为 SVG 内置 SMIL——`<img>` 加载即播放，无需 JS；完成即空闲
 * （实测七），非运行态不闪烁（实测六）。
 */
import idleSvg from '@/assets/central/badge-idle.svg';
import runningSvg from '@/assets/central/badge-running.svg';
import waitingUserSvg from '@/assets/central/badge-waiting-user.svg';
import failedSvg from '@/assets/central/badge-failed.svg';
import interruptedSvg from '@/assets/central/badge-interrupted.svg';
import pausedSvg from '@/assets/central/badge-paused.svg';

export type BadgeState =
  | 'idle'
  | 'running'
  | 'waiting_user'
  | 'failed'
  | 'interrupted'
  | 'paused';

const SOURCES: Record<BadgeState, string> = {
  idle: idleSvg,
  running: runningSvg,
  waiting_user: waitingUserSvg,
  failed: failedSvg,
  interrupted: interruptedSvg,
  paused: pausedSvg,
};

/** 按状态加载对应 SVG 素材。 */
export function CentralBadgeIcon({ state }: { state: BadgeState }) {
  return (
    <img
      src={SOURCES[state]}
      alt=""
      draggable={false}
      data-testid={`central-badge-${state}`}
      className="h-[22px] w-[22px] select-none transition-opacity duration-300"
    />
  );
}
