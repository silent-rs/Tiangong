/**
 * 中枢模式偏好持久化（localStorage，键名惯例同 `tiangong-theme`）。
 *
 * 记录用户是否停留在中枢模式：进入后写入 true，重启应用时 store 初始化
 * 读取该值直接回到中枢模式。非 desktop 宿主读到 true 也不会切换视图
 * （布局守卫 `centralActive` 双保险，见 useCentralMode）。
 */
const STORAGE_KEY = 'tiangong-central-mode';

/** localStorage 键名（导出供测试断言）。 */
export const CENTRAL_MODE_STORAGE_KEY = STORAGE_KEY;

/** 读取持久化的中枢模式偏好；缺失或非法值一律回退普通模式。 */
export function readPersistedCentralMode(): boolean {
  try {
    return localStorage.getItem(STORAGE_KEY) === 'true';
  } catch {
    // localStorage 不可用（极端环境）时不阻断启动，回退普通模式
    return false;
  }
}

/** 持久化中枢模式偏好；写入失败静默忽略（仅影响重启默认，不影响当前会话）。 */
export function persistCentralMode(on: boolean): void {
  try {
    localStorage.setItem(STORAGE_KEY, on ? 'true' : 'false');
  } catch {
    // 忽略：见上
  }
}
