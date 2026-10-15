/**
 * 方向键边界拦截工具
 *
 * 背景：Tauri 在 macOS WKWebView 上存在已知 bug（tauri#5685），当输入框获得焦点
 * 且光标处于边界（无法移动）时按方向键，WKWebView 的文本输入系统（NSTextInputClient）
 * 会误把方向键转义序列（\x1b[A/B/C/D）当作文本输入插入，其中 ESC 字符（U+001B）
 * 无可见字形，渲染成方格 □。
 *
 * 修复策略：在 keydown 阶段判定光标是否已在边界、方向键无法移动光标，
 * 若是则 preventDefault() 阻止 WKWebView 把它当文本提交。对正常光标移动、
 * 文本选区、IME 组合输入零影响。
 */

const ARROW_KEYS = new Set(['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown']);

/**
 * 判定按下方向键时，光标是否处于"无法移动"的边界位置。
 *
 * @param el 触发事件的输入元素（input/textarea）
 * @param key KeyboardEvent.key
 * @returns true 表示应拦截该按键（preventDefault）
 */
export function shouldPreventArrowKey(
  el: HTMLInputElement | HTMLTextAreaElement,
  key: string,
): boolean {
  if (!ARROW_KEYS.has(key)) return false;

  const { selectionStart: ss, selectionEnd: se, value } = el;
  // selectionStart/End 在某些场景（如 type=number 的 input）可能为 null
  if (ss === null || se === null) return false;
  // 有选区时，方向键会取消选区/移动，属于有效操作，不拦截
  if (ss !== se) return false;

  const atStart = ss === 0;
  const atEnd = se === value.length;

  switch (key) {
    case 'ArrowLeft':
      return atStart;
    case 'ArrowRight':
      return atEnd;
    // 上/下键只在文本首/尾拦截：自动换行（软换行）没有 '\n'，按 '\n' 判定首末行
    // 会误拦视觉上的多行移动；macOS 上首行按上会移到文本开头、末行按下会移到
    // 文本末尾，只有光标已在首/尾时才真正无法移动。
    case 'ArrowUp':
      return atStart;
    case 'ArrowDown':
      return atEnd;
    default:
      return false;
  }
}
