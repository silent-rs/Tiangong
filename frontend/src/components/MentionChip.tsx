import type { MentionKind } from '@/utils/mentionBlocks';
import { mentionMarkFor } from '@/utils/mentionMarks';

interface MentionChipProps {
  kind: MentionKind;
  label: string;
  token?: string;
}

export const MENTION_CHIP_BASE_CLASS =
  'mention-chip inline-flex max-w-[14rem] items-center gap-1 rounded border px-1.5 align-baseline text-xs leading-5 text-foreground';

export const MENTION_MARK_BASE_CLASS =
  'inline-flex h-4 min-w-4 shrink-0 items-center justify-center rounded-sm bg-background/80 px-0.5 text-[10px] font-semibold leading-none';

export const MENTION_LABEL_CLASS = 'min-w-0 truncate font-medium';

const MENTION_KIND_CLASS: Record<MentionKind, string> = {
  skill: 'border-amber-500/30 bg-amber-500/10',
  mcp: 'border-cyan-500/30 bg-cyan-500/10',
  agent: 'border-blue-500/30 bg-blue-500/10',
  all: 'border-rose-500/30 bg-rose-500/10',
  index: 'border-emerald-500/30 bg-emerald-500/10',
  plugin: 'border-violet-500/30 bg-violet-500/10',
  file: 'border-slate-500/30 bg-slate-500/10',
};

const MENTION_MARK_KIND_CLASS: Record<MentionKind, string> = {
  skill: 'text-amber-700 dark:text-amber-300',
  mcp: 'text-cyan-700 dark:text-cyan-300',
  agent: 'text-blue-700 dark:text-blue-300',
  all: 'text-rose-700 dark:text-rose-300',
  index: 'text-emerald-700 dark:text-emerald-300',
  plugin: 'text-violet-700 dark:text-violet-300',
  file: 'text-slate-700 dark:text-slate-300',
};

export function mentionChipClass(kind: MentionKind): string {
  return `${MENTION_CHIP_BASE_CLASS} ${MENTION_KIND_CLASS[kind]}`;
}

export function mentionMarkClass(kind: MentionKind): string {
  return `${MENTION_MARK_BASE_CLASS} ${MENTION_MARK_KIND_CLASS[kind]}`;
}

const TOKEN_PREFIX: Record<MentionKind, string> = {
  skill: '@skill:',
  mcp: '@mcp:',
  agent: '@',
  all: '@',
  index: '@',
  plugin: '@plugin:',
  file: '@file:',
};

/**
 * @提及内联标签块（消息气泡展示用，React 渲染）。
 *
 * 输入框编辑器（MentionEditor）走 contenteditable 直建 DOM 路径，复用同一
 * 样式常量与标记，两条路径视觉一致。标记字符优先取插件提供的值（经
 * mentionMarks 注册表按 token/kind 查找），插件未提供时回退前端默认。
 */
export function MentionChip({ kind, label, token }: MentionChipProps) {
  const rawToken = token ?? `${TOKEN_PREFIX[kind]}${label}`;
  const mark = mentionMarkFor(kind, token ?? rawToken);
  return (
    <span
      className={mentionChipClass(kind)}
      title={rawToken}
      aria-label={rawToken}
      data-mention-token={token}
      data-mention-kind={kind}
    >
      {mark && (
        <span className={mentionMarkClass(kind)} aria-hidden="true">
          {mark}
        </span>
      )}
      <span className={MENTION_LABEL_CLASS}>{label}</span>
    </span>
  );
}
