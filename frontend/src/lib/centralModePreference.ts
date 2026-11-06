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

/** 中枢对话绑定的 Session ID 键名（POC 阶段 1：中枢对话复用现有 Session 持久化）。 */
const SESSION_STORAGE_KEY = 'tiangong-central-session-id';

/** localStorage 键名（导出供测试断言）。 */
export const CENTRAL_SESSION_STORAGE_KEY = SESSION_STORAGE_KEY;

/** 读取持久化的中枢对话 Session ID；缺失或空白返回 null。 */
export function readCentralSessionId(): string | null {
  try {
    const id = localStorage.getItem(SESSION_STORAGE_KEY);
    return id && id.trim() ? id : null;
  } catch {
    // localStorage 不可用（极端环境）时视为无记录，进入中枢按新对话处理
    return null;
  }
}

/** 持久化中枢对话 Session ID；写入失败静默忽略（仅影响下次进入的会话定位）。 */
export function persistCentralSessionId(id: string): void {
  try {
    localStorage.setItem(SESSION_STORAGE_KEY, id);
  } catch {
    // 忽略：见上
  }
}

/** 判定给定会话 ID 是否绑定中枢对话（供普通模式侧栏过滤显示）。 */
export function isCentralSessionId(id: string | null | undefined): boolean {
  if (!id) return false;
  try {
    return localStorage.getItem(SESSION_STORAGE_KEY) === id;
  } catch {
    return false;
  }
}
