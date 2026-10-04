/**
 * @提及标记（chip 角标字符）注册表。
 *
 * 标记完全由数据提供方注册，前端不做按 kind 的硬编码默认：
 * - 插件候选：动态候选的 `mark` 字段（wasm `__tiangong.mention_candidates.v1`）
 *   或清单 `mention.mark`（宿主生成 `@plugin:<id>` 静态候选），随候选加载注册；
 * - 前端本地候选：活跃 Agent / @all 由输入框在生成候选时注册。
 *
 * 消息气泡与输入框编辑器从 token 重建 chip 时手里只有文本，经本表还原标记。
 * 查找顺序：token 精确匹配 → kind 兜底 → 空串（不显示标记）。
 *
 * kind 兜底只在该 kind 下所有已见候选标记一致时生效：`plugin` 组的标记是
 * 各插件私有的（如火山引擎的「火山」），不能借给同组其他插件。未提供标记
 * 的候选直接不显示标记，插件补上 `mention.mark` 后自然显示。
 */
const markByToken = new Map<string, string>();
/** kind → 统一标记；值为 null 表示该 kind 标记不一致，不提供兜底。 */
const markByKind = new Map<string, string | null>();
/** 已见但未带标记的候选：不借用 kind 兜底。 */
const unmarkedTokens = new Set<string>();

/** 注册一个候选的标记（mark 为空时记为未带标记，并让该 kind 兜底失效）。 */
export function registerMentionMark(value: string, kind: string, mark?: string) {
  const trimmed = mark?.trim() ?? '';
  if (trimmed) {
    markByToken.set(value, trimmed);
    unmarkedTokens.delete(value);
  } else if (!markByToken.has(value)) {
    unmarkedTokens.add(value);
  }
  if (!markByKind.has(kind)) {
    markByKind.set(kind, trimmed || null);
  } else if (markByKind.get(kind) !== trimmed) {
    markByKind.set(kind, null);
  }
}
/** 批量注册（加载候选分组后调用）。 */
export function registerMentionMarks(
  candidates: { value: string; kind: string; mark?: string }[],
) {
  for (const c of candidates) registerMentionMark(c.value, c.kind, c.mark);
}
/** 取某提及的标记字符：token 精确匹配 → kind 兜底；都没有时返回空串（不显示标记）。 */
export function mentionMarkFor(kind: string, token?: string): string {
  if (token) {
    const mark = markByToken.get(token);
    if (mark) return mark;
    if (unmarkedTokens.has(token)) return '';
  }
  return markByKind.get(kind) || '';
}
