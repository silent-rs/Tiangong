import type { ContentBlock, MessagePhase, MessageRole, MessageUsage } from "@/api/tauri";

export interface MessageItem {
  id: string;
  role: MessageRole;
  content: ContentBlock[];
  reasoning_content: string;
  usage?: MessageUsage | null;
  worker_id?: string;
  media?: {
    kind: "image" | "video" | "audio" | "file";
    url: string;
    mime_type?: string;
    title?: string;
    capability?: string;
  }[];
  tool_calls?: { id: string; name: string; arguments?: unknown }[];
  tool_call_id?: string;
  tool_name?: string;
  tool_result_is_error?: boolean;
  compact?: boolean;
  phase?: MessagePhase;
  created_at: string;
  /** 该用户消息发起的轮次的执行时长（毫秒）。仅起轮的用户消息携带。 */
  elapsed_ms?: number;
  /** 该用户消息发起的轮次的状态。仅起轮的用户消息携带，引导消息为空。 */
  turn_status?: "processing" | "success" | "failed" | "cancelled";
  /** 本次模型输出思考阶段的耗时（毫秒）。仅 assistant 消息携带。 */
  reasoning_elapsed_ms?: number | null;
  /** 本次模型输出正文生成阶段的耗时（毫秒）。仅 assistant 消息携带。 */
  text_elapsed_ms?: number | null;
  /** 单次工具调用耗时（毫秒）。由 ToolResult 流式事件写入工具消息。 */
  duration_ms?: number | null;
}

export interface MessageGroup {
  key: string;
  type: "user" | "agent_turn" | "worker";
  worker_id?: string;
  messages: MessageItem[];
}

export interface SystemMessageMeta {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  summary: string;
  toolName?: string;
}
