/**
 * 会话消息结构与只读访问函数（与后端 `tiangong_types::Message` 同构）。
 *
 * 纯类型与纯函数，不依赖宿主 IPC；`@/api/tauri` 重新导出本模块。
 */
import type { ContentBlock, MessageRole, MessageUsage, TurnStatus } from './tauri';

/** 用户消息来源：只有 human（真人输入）作为轮次锚点。未知值按 human 处理。 */
export type UserSource = 'human' | 'host_injected' | 'compressed_resume';

export interface MessageToolCall {
  id: string;
  name: string;
  arguments?: unknown;
}

/**
 * 角色及其必带字段（与后端 `Role` 一致，按 `role` 区分）。
 * 后端以 flatten 展开到消息顶层，角色字段与 `id` / `content` 平级。
 */
export type Role =
  | { role: 'system' }
  | {
      role: 'user';
      source?: UserSource;
      /** 该用户消息发起的轮次状态。仅起轮消息携带，引导消息为空。 */
      turn_status?: TurnStatus;
      /** 该用户消息发起的轮次执行时长（毫秒）。仅起轮消息携带。 */
      elapsed_ms?: number;
      /** 本轮最终答复的消息 ID（成功完成时写入）。 */
      final_reply?: string;
    }
  | {
      role: 'assistant';
      reasoning_content?: string;
      reasoning_signature?: string;
      tool_calls?: MessageToolCall[];
      usage?: MessageUsage | null;
      /** 本次模型输出思考阶段的耗时（毫秒）。 */
      reasoning_elapsed_ms?: number | null;
      /** 本次模型输出正文生成阶段的耗时（毫秒）。 */
      text_elapsed_ms?: number | null;
      /** 正文生成速度（tokens/s），定稿时后端算好持久化，供历史回看与
       *  token 统计直接复现；旧数据缺该字段时按 usage/耗时折算兼容。 */
      text_tps?: number | null;
    }
  | {
      role: 'tool';
      /** 为空表示无配对调用的运行时上下文。 */
      tool_call_id?: string;
      tool_name?: string;
      is_error?: boolean;
      /** 单次工具调用耗时（毫秒）。 */
      duration_ms?: number | null;
    }
  | { role: 'notice'; usage?: MessageUsage | null };

/** 插件渲染声明：只影响显示，插件缺失时按角色默认渲染。 */
export interface MessageRender {
  plugin: string;
  view: string;
  data?: unknown;
}

/** 与角色无关的消息字段。 */
export interface MessageMeta {
  compact?: boolean;
  render?: MessageRender;
}

/** 与角色无关的消息字段。 */
export interface MessageBase {
  id: string;
  created_at: string;
  content: ContentBlock[];
  meta?: MessageMeta;
}

/** 对话消息：公共字段与角色字段平级（按 `role` 区分）。 */
export type Message = MessageBase & Role;

// ── 只读访问：按角色取字段，其他角色返回空值（与后端访问器一致）──

type RoleOf<T extends Role['role']> = Extract<Message, { role: T }>;

function roleAs<T extends Role['role']>(message: Message, role: T): RoleOf<T> | undefined {
  return message.role === role ? (message as RoleOf<T>) : undefined;
}

export function messageKind(message: Message): MessageRole {
  return message.role;
}

export function reasoningOf(message: Message): string {
  return roleAs(message, 'assistant')?.reasoning_content ?? '';
}

export function toolCallsOf(message: Message): MessageToolCall[] {
  return roleAs(message, 'assistant')?.tool_calls ?? [];
}

/** 工具结果配对的调用 ID（无配对调用或非工具消息时为 undefined）。 */
export function toolCallIdOf(message: Message): string | undefined {
  return roleAs(message, 'tool')?.tool_call_id || undefined;
}

export function toolNameOf(message: Message): string | undefined {
  return roleAs(message, 'tool')?.tool_name || undefined;
}

export function toolIsError(message: Message): boolean {
  return roleAs(message, 'tool')?.is_error === true;
}

export function durationMsOf(message: Message): number | null {
  return roleAs(message, 'tool')?.duration_ms ?? null;
}

export function usageOf(message: Message): MessageUsage | null {
  if (message.role === 'assistant' || message.role === 'notice') return message.usage ?? null;
  return null;
}

export function reasoningElapsedMsOf(message: Message): number | null {
  return roleAs(message, 'assistant')?.reasoning_elapsed_ms ?? null;
}

export function textElapsedMsOf(message: Message): number | null {
  return roleAs(message, 'assistant')?.text_elapsed_ms ?? null;
}

export function textTpsOf(message: Message): number | null {
  return roleAs(message, 'assistant')?.text_tps ?? null;
}

/**
 * 单条 assistant 消息的正文生成速度（tokens/s）。
 *
 * 优先读后端定稿时持久化的 `text_tps`（历史回看直接复现）；
 * 旧数据缺该字段时按 `usage.completion_tokens / (text_elapsed_ms / 1000)`
 * 折算兼容（仅正文阶段，不含思考）。任一字段缺失/非法/耗时非正数时
 * 返回 `null`，调用方不展示、不占位误导。
 */
export function messageTpsOf(message: Message): number | null {
  const persisted = roleAs(message, 'assistant')?.text_tps;
  if (typeof persisted === 'number' && Number.isFinite(persisted) && persisted > 0) {
    return Math.round(persisted * 10) / 10;
  }
  if (message.role !== 'assistant') return null;
  const usage = message.usage;
  const tokens = usage?.completion_tokens;
  const elapsedMs = message.text_elapsed_ms;
  if (typeof tokens !== 'number' || !Number.isFinite(tokens) || tokens <= 0) return null;
  if (typeof elapsedMs !== 'number' || !Number.isFinite(elapsedMs) || elapsedMs <= 0) return null;
  return Math.round((tokens / (elapsedMs / 1000)) * 10) / 10;
}

/**
 * 会话内最近一次可展示的正文生成速度：从末尾往前找第一条带有效速度的
 * assistant 消息。
 *
 * 用于前端实时 tracker 归零（页面重载、切换会话后）时的空闲兜底——输入框
 * 的「最近生成速度」不应因为内存态丢失而消失；实时样本优先，回退值只补空。
 */
export function latestTpsOf(messages: Message[]): number | null {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const tps = messageTpsOf(messages[i]);
    if (tps != null && tps > 0) return tps;
  }
  return null;
}

export function turnStatusOf(message: Message): TurnStatus | undefined {
  return roleAs(message, 'user')?.turn_status ?? undefined;
}

export function elapsedMsOf(message: Message): number | undefined {
  return roleAs(message, 'user')?.elapsed_ms ?? undefined;
}

export function finalReplyOf(message: Message): string | undefined {
  return roleAs(message, 'user')?.final_reply || undefined;
}

/** 用户消息来源；非用户消息为 undefined，未知值按 human。 */
export function userSourceOf(message: Message): UserSource | undefined {
  const role = roleAs(message, 'user');
  if (!role) return undefined;
  const source = role.source;
  return source === 'host_injected' || source === 'compressed_resume'
    ? source
    : 'human';
}

/** 是否为用户真实输入（轮次锚点），与后端 `Message::is_user_input` 一致。 */
export function isUserInput(message: Message): boolean {
  return userSourceOf(message) === 'human';
}

/**
 * 轮次锚点（起轮用户消息）的 ID 集合，与后端 `view::turn_anchor_flags` 同一规则。
 *
 * 只有发起一次执行的用户消息是锚点；引导消息（执行中追加、或接续失败轮次
 * 的消息）不是锚点。按时序逐条判定真人用户消息：
 * - 带轮次状态：上一个带状态的用户消息为 `failed` 时，属失败轮次的接续
 *   （历史数据按引导处理），否则是锚点；
 * - 不带状态：此前没有任何带状态的用户消息（轮次状态出现前的旧数据）时是
 *   锚点，否则是所属轮次的引导消息。
 * 用户取消（`cancelled`）后的消息照常起新轮。
 */
export function turnAnchorIds(messages: Message[]): Set<string> {
  const anchors = new Set<string>();
  let previousStatus: TurnStatus | undefined;
  for (const message of messages) {
    if (!isUserInput(message)) continue;
    const status = turnStatusOf(message);
    if (status == null) {
      if (previousStatus == null) anchors.add(message.id);
      continue;
    }
    if (previousStatus !== 'failed') anchors.add(message.id);
    previousStatus = status;
  }
  return anchors;
}

/** 最后一个轮次锚点的下标（规则同 {@link turnAnchorIds}），没有返回 -1。
 * 从末尾向前扫描，只走到确定结论为止，供流式事件频繁调用。 */
export function latestTurnAnchorIndex(messages: Message[]): number {
  let latestUser = -1;
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (!isUserInput(message)) continue;
    if (latestUser < 0) latestUser = index;
    if (turnStatusOf(message) == null) continue;
    // 最近一个带状态的用户消息：向前跳过接续失败轮次的链节，取链首
    let anchor = index;
    for (let prev = index - 1; prev >= 0; prev -= 1) {
      const prevMessage = messages[prev];
      if (!isUserInput(prevMessage)) continue;
      const prevStatus = turnStatusOf(prevMessage);
      if (prevStatus == null) continue;
      if (prevStatus !== 'failed') break;
      anchor = prev;
    }
    return anchor;
  }
  // 全部不带状态（旧数据）：每条都是锚点
  return latestUser;
}

export function isCompact(message: Message): boolean {
  return message.meta?.compact === true;
}
