import type { Message } from "@/api/tauri";

/** 消息列表与轮次组件使用的消息类型（与后端 `Message` 同构）。 */
export type MessageItem = Message;

export interface MessageGroup {
  key: string;
  type: "user" | "agent_turn";
  messages: MessageItem[];
}

export interface SystemMessageMeta {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  summary: string;
  toolName?: string;
}
