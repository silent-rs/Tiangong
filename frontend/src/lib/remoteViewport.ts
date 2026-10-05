/**
 * 手机 H5（远程模式）视口适配：
 * - 禁止双击/聚焦输入框时的自动缩放（iOS 在输入框字号 < 16px 时会放大页面）；
 * - 根容器高度跟随可视视口（软键盘弹出时可视区域变小，100vh 不变，
 *   会把输入框和发送按钮顶出屏幕），并把被浏览器滚走的布局视口拉回顶部。
 */
const VIEWPORT =
  'width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no, viewport-fit=cover, interactive-widget=resizes-content';

let installed = false;

export function installRemoteViewport() {
  if (installed || typeof window === 'undefined') return;
  installed = true;
  const root = document.documentElement;
  root.classList.add('tg-remote');

  let meta = document.querySelector<HTMLMetaElement>('meta[name="viewport"]');
  if (!meta) {
    meta = document.createElement('meta');
    meta.name = 'viewport';
    document.head.appendChild(meta);
  }
  meta.content = VIEWPORT;

  const viewport = window.visualViewport;
  let frame = 0;
  const sync = () => {
    frame = 0;
    const height = viewport ? viewport.height : window.innerHeight;
    root.style.setProperty('--tg-app-height', `${Math.round(height)}px`);
    // 键盘弹出时 iOS 会滚动布局视口；页面本身不滚动，始终对齐顶部。
    if (window.scrollY !== 0 || window.scrollX !== 0) window.scrollTo(0, 0);
  };
  const schedule = () => {
    if (!frame) frame = window.requestAnimationFrame(sync);
  };
  sync();
  viewport?.addEventListener('resize', schedule);
  viewport?.addEventListener('scroll', schedule);
  window.addEventListener('resize', schedule);
  window.addEventListener('orientationchange', schedule);
  // 输入框失焦（键盘收起）后部分浏览器不触发 resize，补一次同步。
  window.addEventListener('focusout', () => window.setTimeout(schedule, 50));
}
