/**
 * 中枢对话消息列表（中枢模式专属，不复用普通模式的 MessageList——
 * 中枢 UI 完全独立，普通模式文件保持最小改动）。
 *
 * POC 渲染范围：用户消息气泡 + 助手回复（思考过程折叠 + Markdown 正文）、
 * 生成中的流式增量、空对话时的中枢默认问候（设计稿 §4.2）。分页历史、
 * 工具调用组、消息操作等属于工作会话视图的能力，中枢对话不携带。
 */
import { useEffect, useRef } from 'react';
import { MdPreview } from 'md-editor-rt';
import 'md-editor-rt/lib/preview.css';
import { useStore } from '@/store/useStore';
import type { Message } from '@/api/tauri';
import { reasoningOf, turnStatusOf } from '@/api/message';
import { CENTRAL_DEFAULT_GREETING } from '@/components/central/centralGreeting';
import { CENTRAL_TRANSITION_PARAMS } from '@/components/central/centralTransitionParams';

/** 提取消息文本块（中枢对话无附件/工具，正文即 text 块）。 */
function textOf(message: Message): string {
  return (message.content ?? [])
    .filter((block): block is Extract<typeof block, { type: 'text' }> => block.type === 'text')
    .map((block) => block.text)
    .join('\n');
}

function CentralGreeting() {
  return (
    <div className="flex h-full flex-col items-center justify-center px-8 text-center">
      <div
        className="mb-4 flex h-16 w-16 items-center justify-center rounded-full text-2xl"
        style={{
          backgroundColor: CENTRAL_TRANSITION_PARAMS.accent,
          color: CENTRAL_TRANSITION_PARAMS.accent2,
        }}
        aria-hidden="true"
      >
        ✦
      </div>
      <p className="max-w-xl whitespace-pre-line text-balance text-sm leading-relaxed text-muted-foreground">
        {CENTRAL_DEFAULT_GREETING}
      </p>
    </div>
  );
}

function ReasoningBlock({ content }: { content: string }) {
  if (!content.trim()) return null;
  return (
    <details className="mb-2 rounded-lg border border-border/60 bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
      <summary className="cursor-pointer select-none">思考过程</summary>
      <p className="mt-2 whitespace-pre-wrap break-words leading-relaxed">{content}</p>
    </details>
  );
}

function UserBubble({ message }: { message: Message }) {
  return (
    <div className="flex justify-end">
      <div className="max-w-[85%] whitespace-pre-wrap break-words rounded-2xl rounded-br-md bg-primary px-4 py-2.5 text-sm text-primary-foreground">
        {textOf(message)}
      </div>
    </div>
  );
}

function AssistantReply({ message }: { message: Message }) {
  const failed = turnStatusOf(message) === 'failed';
  const reasoning = reasoningOf(message);
  return (
    <div className="min-w-0">
      {reasoning ? <ReasoningBlock content={reasoning} /> : null}
      <div className="central-md-body break-words text-sm leading-relaxed">
        <MdPreview modelValue={textOf(message) || (failed ? '（本轮失败）' : '')} previewTheme="github" />
      </div>
      {failed ? (
        <p className="mt-1 text-xs text-destructive">执行失败，可重试或换个说法</p>
      ) : null}
    </div>
  );
}

function StreamingReply({ content, reasoning }: { content: string; reasoning: string }) {
  if (!content && !reasoning) {
    return <p className="text-sm text-muted-foreground">正在思考…</p>;
  }
  return (
    <div className="min-w-0">
      {reasoning ? <ReasoningBlock content={reasoning} /> : null}
      {content ? (
        <div className="central-md-body break-words text-sm leading-relaxed after:content-['▍'] after:animate-pulse after:text-muted-foreground">
          <MdPreview modelValue={content} previewTheme="github" />
        </div>
      ) : null}
    </div>
  );
}

export function CentralMessageList() {
  const messages = useStore((s) => s.messages);
  const streamingMessageId = useStore((s) => s.streamingMessageId);
  const streamingContent = useStore((s) => s.streamingContent);
  const streamingReasoningContent = useStore((s) => s.streamingReasoningContent);
  const runStatus = useStore((s) => s.runStatus);
  const bottomRef = useRef<HTMLDivElement>(null);

  const isGenerating = runStatus === 'executing' && !!streamingMessageId;
  const completed = streamingMessageId
    ? messages.filter((message) => message.id !== streamingMessageId)
    : messages;

  // 新消息或流式增量时保持贴底跟随（scrollIntoView 可选调用：jsdom 等环境缺失时不报错）
  useEffect(() => {
    bottomRef.current?.scrollIntoView?.({ block: 'end' });
  }, [completed.length, streamingContent, streamingReasoningContent, isGenerating]);

  if (completed.length === 0 && !isGenerating) {
    return <CentralGreeting />;
  }

  return (
    <div className="h-full overflow-y-auto px-4 py-4">
      <div className="mx-auto max-w-3xl space-y-4">
        {completed.map((message) =>
          message.role === 'user' ? (
            <UserBubble key={message.id} message={message} />
          ) : (
            <AssistantReply key={message.id} message={message} />
          ),
        )}
        {isGenerating ? (
          <StreamingReply content={streamingContent} reasoning={streamingReasoningContent} />
        ) : null}
        <div ref={bottomRef} />
      </div>
    </div>
  );
}
