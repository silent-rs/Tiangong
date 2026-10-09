/**
 * 会话消息结构与只读访问函数（与后端 `tiangong_types::Message` 同构）。
 *
 * 纯类型与纯函数，不依赖宿主 IPC；`@/api/tauri` 重新导出本模块。
 */
import type { ContentBlock, MessageRole, MessageUsage, TurnStatus } from './tauri';

/** 用户消息来源：只有 human（真人输入）作为轮次锚点。未知值按 human 处理。 */
export type UserSource = 'human' | 'host_injected' | 'compressed_resume' | 'agent';

export interface MessageToolCall {
  id: string;
  name: string;
  arguments?: unknown;
}

/** 角色及其必带字段（与后端 `Role` 一致，按 `type` 区分）。 */
export type Role =
  | { type: 'system' }
  | {
      type: 'user';
      source?: UserSource;
      /** 该用户消息发起的轮次状态。仅起轮消息携带，引导消息为空。 */
      turn_status?: TurnStatus;
      /** 该用户消息发起的轮次执行时长（毫秒）。仅起轮消息携带。 */
      elapsed_ms?: number;
      /** 本轮最终答复的消息 ID（成功完成时写入）。 */
      final_reply?: string;
    }
  | {
      type: 'assistant';
      reasoning_content?: string;
      reasoning_signature?: string;
      tool_calls?: MessageToolCall[];
      usage?: MessageUsage | null;
      /** 本次模型输出思考阶段的耗时（毫秒）。 */
      reasoning_elapsed_ms?: number | null;
      /** 本次模型输出正文生成阶段的耗时（毫秒）。 */
      text_elapsed_ms?: number | null;
    }
  | {
      type: 'tool';
      /** 为空表示无配对调用的运行时上下文。 */
      tool_call_id?: string;
      tool_name?: string;
      is_error?: boolean;
      /** 单次工具调用耗时（毫秒）。 */
      duration_ms?: number | null;
    }
  | { type: 'notice'; usage?: MessageUsage | null };

/** 与角色无关的消息字段。 */
export interface MessageMeta {
  compact?: boolean;
}

export interface Message {
  id: string;
  created_at: string;
  role: Role;
  content: ContentBlock[];
  meta?: MessageMeta;
}

// ── 只读访问：按角色取字段，其他角色返回空值（与后端访问器一致）──

type RoleOf<T extends Role['type']> = Extract<Role, { type: T }>;

function roleAs<T extends Role['type']>(message: Message, type: T): RoleOf<T> | undefined {
  return message.role?.type === type ? (message.role as RoleOf<T>) : undefined;
}

export function messageKind(message: Message): MessageRole {
  return message.role?.type;
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
  const role = message.role;
  if (role?.type === 'assistant' || role?.type === 'notice') return role.usage ?? null;
  return null;
}

export function reasoningElapsedMsOf(message: Message): number | null {
  return roleAs(message, 'assistant')?.reasoning_elapsed_ms ?? null;
}

export function textElapsedMsOf(message: Message): number | null {
  return roleAs(message, 'assistant')?.text_elapsed_ms ?? null;
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
  return source === 'host_injected' || source === 'compressed_resume' || source === 'agent'
    ? source
    : 'human';
}

/** 是否为用户真实输入（轮次锚点），与后端 `Message::is_user_input` 一致。 */
export function isUserInput(message: Message): boolean {
  return userSourceOf(message) === 'human';
}

export function isCompact(message: Message): boolean {
  return message.meta?.compact === true;
}
