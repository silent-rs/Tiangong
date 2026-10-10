import type { Message, MessageUsage, Role, UserSource } from '@/api/tauri';

/**
 * 测试夹具：以旧扁平字段描述消息，转换为按角色区分的新结构。
 *
 * 规则与后端读取旧格式时的迁移一致：
 * - phase=hostinjected/compressedresume → user.source；
 * - phase=summary 在消息结构上不再体现（最终答复由起点用户消息的 final_reply 指向，
 *   需要时用 `final_reply` 字段在用户消息上显式声明）。
 */
export interface LegacyMessageFields {
  id?: string;
  role?: string;
  content?: Message['content'] | string;
  created_at?: string;
  reasoning_content?: string;
  usage?: MessageUsage | null;
  tool_calls?: { id: string; name: string; arguments?: unknown }[];
  tool_call_id?: string;
  tool_name?: string;
  tool_result_is_error?: boolean;
  compact?: boolean;
  phase?: string;
  elapsed_ms?: number;
  turn_status?: 'processing' | 'success' | 'failed' | 'cancelled';
  final_reply?: string;
  reasoning_elapsed_ms?: number | null;
  text_elapsed_ms?: number | null;
  duration_ms?: number | null;
}

let legacySeq = 0;

export function legacyMessage(fields: LegacyMessageFields): Message {
  const kind = fields.role ?? 'user';
  let role: Role;
  switch (kind) {
    case 'user': {
      const source: UserSource = fields.phase === 'hostinjected'
        ? 'host_injected'
        : fields.phase === 'compressedresume'
          ? 'compressed_resume'
          : 'human';
      role = {
        type: 'user',
        ...(source !== 'human' ? { source } : {}),
        ...(fields.turn_status != null ? { turn_status: fields.turn_status } : {}),
        ...(fields.elapsed_ms != null ? { elapsed_ms: fields.elapsed_ms } : {}),
        ...(fields.final_reply ? { final_reply: fields.final_reply } : {}),
      };
      break;
    }
    case 'assistant':
      role = {
        type: 'assistant',
        ...(fields.reasoning_content ? { reasoning_content: fields.reasoning_content } : {}),
        ...(fields.tool_calls ? { tool_calls: fields.tool_calls } : {}),
        ...(fields.usage ? { usage: fields.usage } : {}),
        ...(fields.reasoning_elapsed_ms != null ? { reasoning_elapsed_ms: fields.reasoning_elapsed_ms } : {}),
        ...(fields.text_elapsed_ms != null ? { text_elapsed_ms: fields.text_elapsed_ms } : {}),
      };
      break;
    case 'tool':
      role = {
        type: 'tool',
        ...(fields.tool_call_id ? { tool_call_id: fields.tool_call_id } : {}),
        ...(fields.tool_name ? { tool_name: fields.tool_name } : {}),
        ...(fields.tool_result_is_error ? { is_error: true } : {}),
        ...(fields.duration_ms != null ? { duration_ms: fields.duration_ms } : {}),
      };
      break;
    case 'notice':
      role = { type: 'notice', ...(fields.usage ? { usage: fields.usage } : {}) };
      break;
    default:
      role = { type: 'system' };
  }
  const content = typeof fields.content === 'string'
    ? [{ type: 'text' as const, text: fields.content }]
    : fields.content ?? [];
  return {
    id: fields.id ?? `legacy-${++legacySeq}`,
    created_at: fields.created_at ?? '2026-01-01 00:00:00',
    role,
    content,
    ...(fields.compact ? { meta: { compact: true } } : {}),
  };
}

/**
 * 按旧 phase 语义补齐最终答复：每轮最后一条 phase=summary 的助手消息
 * （无则不设置）写入该轮起点用户消息的 final_reply。
 */
export function withLegacyFinalReplies(
  entries: LegacyMessageFields[],
): Message[] {
  const messages = entries.map(legacyMessage);
  let anchor: number | null = null;
  entries.forEach((entry, index) => {
    const message = messages[index];
    if (message.role.type === 'user' && (message.role.source ?? 'human') === 'human') {
      anchor = index;
      return;
    }
    if (entry.phase === 'summary' && anchor != null) {
      const anchorMessage = messages[anchor];
      if (anchorMessage.role.type === 'user') {
        messages[anchor] = {
          ...anchorMessage,
          role: { ...anchorMessage.role, final_reply: message.id },
        };
      }
    }
  });
  return messages;
}
