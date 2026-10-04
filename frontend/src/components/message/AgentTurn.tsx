import { memo, useEffect, useMemo, useState } from "react";
import { FileText, ChevronRight, ChevronDown, Camera } from "lucide-react";
import { useSearchStore } from "@/store/useSearchStore";
import { useStore } from "@/store/useStore";
import { findTextOccurrences } from "@/utils/search";
import { HighlightText } from "../HighlightText";
import { ThinkingBlock } from "../ThinkingBlock";
import { AgentReplyCard } from "./AgentReplyCard";
import { useResolvedTheme } from "@/hooks/useTheme";
import { hasMediaBlocks, textContent } from "@/api/tauri";
import {
  formatMessageTime,
  formatDuration,
  msgReasoning,
  resolveMarkdownImages,
  extractLlmExplanation,
  llmOutputHasToolCalls,
  sameMessageRefs,
  hasMessage,
  extractAgentRoles,
  parseAgentReply,
  displayTextContent,
  stripSummaryStatusMarker,
  isNeedMoreWorkMessage,
  TURN_STATUS_META,
} from "./utils";
import type { MessageItem } from "./types";
import { useExpansionState } from "./useExpansionState";
import { LazyMdPreview } from "./LazyMdPreview";
import { StreamingMessage } from "./StreamingMessage";
import { MessageActions } from "./MessageActions";
import { ContentMedia } from "./ContentMedia";
import { ToolGroup } from "./ToolGroup";
import { CollapsibleUserText } from "./CollapsibleUserText";
import { CallUsageDetails } from "./CallUsageDetails";

interface AgentTurnProps {
  messages: MessageItem[];
  streamingMessageId: string | null;
  streamingContent: string;
  streamingReasoningContent: string;
  isActive?: boolean;
  /** 本轮执行总时长（毫秒）：来自本轮用户消息的 elapsed_ms。 */
  turnElapsedMs?: number;
  /** 本轮最终状态（success/failed/cancelled）：失败/取消时轮次末尾展示状态行。 */
  turnStatus?: string;
}

function AgentTurnView({
  messages,
  streamingMessageId,
  streamingContent,
  streamingReasoningContent,
  isActive = false,
  turnElapsedMs,
  turnStatus,
}: AgentTurnProps) {
  const searchQuery = useSearchStore((s) => s.searchQuery);
  const currentMessageId = useSearchStore((s) => s.currentMessageId);
  const currentMatchStart = useSearchStore((s) => s.currentMatchStart);
  const caseSensitive = useSearchStore((s) => s.caseSensitive);
  const toolGroupExpansion = useExpansionState();
  const agents = useStore((state) => state.agents);
  const toolCallStartedAt = useStore((state) => state.toolCallStartedAt);
  const toolCallFinished = useStore((state) => state.toolCallFinished);
  const resolvedTheme = useResolvedTheme();

  // 已完成轮次默认折叠「过程」（思考/解释/工具/ReAct 文本等），仅保留总结回复可见，
  // 使历史会话整洁；用户点击摘要行可展开查看完整过程。活跃轮次保持展开。
  const [showProcess, setShowProcess] = useState(isActive);
  // 活跃态结束（isActive: true→false）时自动折叠过程，避免完成后仍展开堆叠。
  useEffect(() => {
    setShowProcess(isActive);
  }, [isActive]);

  type Fragment =
    | { type: "explanation"; text: string; time?: string }
    | { type: "thinking"; content: string; time?: string; elapsedMs?: number | null }
    | { type: "tool_group"; key: string; tools: MessageItem[] }
    | { type: "injected_media"; msg: MessageItem }
    | { type: "user"; msg: MessageItem }
    | { type: "assistant"; msg: MessageItem; isStreaming: boolean }
    | { type: "error_system"; msg: MessageItem }
    | { type: "retry_system"; msg: MessageItem }
    | { type: "context_management"; msg: MessageItem }
    | { type: "agent_event"; category: string; content: string; agentRoles: string[] }
    | { type: "subagent_report"; agentName: string; status: string; content: string; time: string; msgId: string }
    | { type: "other_system"; msg: MessageItem };

  // 消息分类/合并/分桶全量计算装进 useMemo：依赖仅 messages 引用、流式
  // 消息 id 与 agents——流式正文高频更新、父级重渲、搜索高亮变化都不再
  // 触发重算（长轮次上百条消息的分类是毫秒级同步计算，重算会在切换
  // 会话挂载可见组时叠加成可感知的卡顿）
  const {
    toolCallArgs,
    settledToolCallIds,
    userFrag,
    blocks,
    processFrags,
    errorFrags,
    usageAnchorId,
    lastToolGroupKey,
  } = useMemo(() => {
    // 工具调用参数配对：assistant 消息携带 tool_calls（含参数），
    // role:'tool' 结果消息按 tool_call_id 反查参数，用于派生单行摘要与展开内容。
    const toolCallArgs = new Map<string, { id: string; name: string; arguments?: unknown }>();
    const settledToolCallIds = new Set<string>();
    for (const msg of messages) {
      for (const call of msg.tool_calls ?? []) toolCallArgs.set(call.id, call);
      if (msg.role === "tool" && msg.tool_call_id) settledToolCallIds.add(msg.tool_call_id);
    }

    const fragments: Fragment[] = [];
    const shownReasonings = new Set<string>();
    let pendingTools: MessageItem[] = [];

    // 同一消息在分类链上会被多次取文本（前缀判断 + 正文提取），
    // 全量拼接按消息缓存一份，避免长轮次（上百条消息）重复分配
    const textCache = new Map<MessageItem, string>();
    const textOf = (msg: MessageItem): string => {
      let text = textCache.get(msg);
      if (text === undefined) {
        text = textContent(msg);
        textCache.set(msg, text);
      }
      return text;
    };

    const flushTools = () => {
      if (pendingTools.length === 0) return;
      fragments.push({ type: "tool_group", key: pendingTools[0].id, tools: [...pendingTools] });
      pendingTools = [];
    };

    for (const msg of messages) {
      if (msg.role === "notice" && msg.usage) continue;
      if (msg.role === "user" && msg.phase === "hostinjected") {
        // 宿主注入媒体（RFC 0017）：归助手轮次过程片段，媒体本体在
        // assistant 侧展示，provenance 文本不进 UI。
        flushTools();
        fragments.push({ type: "injected_media", msg });
      } else if (msg.role === "user" && textOf(msg).startsWith("[Subagent·")) {
        flushTools();
        const raw = textOf(msg);
        const match = raw.match(/^\[Subagent·([^\]]+)\]\s*(.*)$/s);
        if (match) {
          const [, name, body] = match;
          const statusMatch = body.match(/^(任务完成|执行失败|运行阻塞|等待审批)：?\s*/);
          const status = statusMatch ? statusMatch[1] : "消息";
          const content = statusMatch ? body.slice(statusMatch[0].length) : body;
          fragments.push({ type: "subagent_report", agentName: name.trim(), status, content: content.trim(), time: msg.created_at, msgId: msg.id });
        } else {
          fragments.push({ type: "user", msg });
        }
      } else if (msg.role === "user") {
        flushTools();
        fragments.push({ type: "user", msg });
      } else if (msg.role === "system" && textOf(msg).startsWith("[记忆检索] 策略:")) {
        pendingTools.push(msg);
      } else if (msg.role === "system" && textOf(msg).startsWith("[记忆检索]")) {
        pendingTools.push(msg);
      } else if (msg.role === "system" && textOf(msg).startsWith("LLM 输出")) {
        const reasoning = msgReasoning(msg);
        const explanation = extractLlmExplanation(textOf(msg));
        if (!reasoning && !explanation && llmOutputHasToolCalls(textOf(msg))) continue;
        flushTools();
        if (reasoning && !shownReasonings.has(reasoning)) {
          shownReasonings.add(reasoning);
          fragments.push({ type: "thinking", content: reasoning, time: msg.created_at });
        }
        if (explanation) fragments.push({ type: "explanation", text: explanation, time: msg.created_at });
      } else if (msg.role === "system" && (textOf(msg).includes("tool_name:") || textOf(msg).includes("exit_code") || textOf(msg).startsWith("工具执行 ["))) {
        pendingTools.push(msg);
      } else if (msg.role === "tool") {
        pendingTools.push(msg);
        continue;
      } else if (msg.role === "assistant") {
        const isStreaming = msg.id === streamingMessageId;
        const assistantReasoning = msgReasoning(msg);
        const hasVisibleAssistantContent = isStreaming || textOf(msg).trim().length > 0 || assistantReasoning.length > 0 || !!msg.media?.length || hasMediaBlocks(msg);
        if (!hasVisibleAssistantContent) continue;
        flushTools();
        const prevFrag = fragments[fragments.length - 1];
        if (prevFrag?.type === "explanation" && prevFrag.text === textOf(msg).trim() && !isStreaming) fragments.pop();
        if (!isStreaming && assistantReasoning && !shownReasonings.has(assistantReasoning)) {
          shownReasonings.add(assistantReasoning);
          fragments.push({ type: "thinking", content: assistantReasoning, time: msg.created_at, elapsedMs: msg.reasoning_elapsed_ms });
        }
        // 总结阶段判定"任务未完成、需重入 Loop"的回复（[NEED_MORE_WORK] 标头）：
        // 前端作为思考过程展示，剥除标头，不作为最终回复正文。
        if (isNeedMoreWorkMessage(msg)) {
          const needMoreWorkBody = stripSummaryStatusMarker(textOf(msg)).trim();
          if (needMoreWorkBody || isStreaming) {
            fragments.push({ type: "thinking", content: needMoreWorkBody, time: msg.created_at, elapsedMs: msg.text_elapsed_ms });
          }
          continue;
        }
        fragments.push({ type: "assistant", msg, isStreaming });
      } else if ((msg.role === "notice" || msg.role === "system") && textOf(msg).startsWith("[错误]")) {
        flushTools();
        fragments.push({ type: "error_system", msg });
      } else if (msg.role === "system" && textOf(msg).startsWith("[重试]")) {
        flushTools();
        fragments.push({ type: "retry_system", msg });
      } else if ((msg.role === "system" || msg.role === "notice") && textOf(msg).startsWith("[上下文管理]")) {
        if (textOf(msg).includes("正在压缩")) continue;
        flushTools();
        fragments.push({ type: "context_management", msg });
      } else if (msg.role === "system" && (textOf(msg).startsWith("[Agent]") || textOf(msg).startsWith("[文件锁]"))) {
        flushTools();
        const category = textOf(msg).startsWith("[文件锁]") ? "lock" : "info";
        fragments.push({ type: "agent_event", category, content: textOf(msg), agentRoles: extractAgentRoles(textOf(msg), agents) });
      } else if (msg.role === "system" || msg.role === "notice") {
        flushTools();
        fragments.push({ type: "other_system", msg });
      }
    }
    flushTools();

    const mergedFragments: Fragment[] = [];
    for (const frag of fragments) {
      const previous = mergedFragments[mergedFragments.length - 1];
      if (frag.type === "tool_group" && previous?.type === "tool_group") {
        previous.tools.push(...frag.tools);
        continue;
      }
      mergedFragments.push(frag);
    }

    // 分组：用户消息（锚点）/ 过程片段（思考、解释、工具、ReAct 文本等）/
    // 通知 / 总结回复。已完成轮次默认折叠「过程」，通知与总结始终可见。
    //
    // 渲染顺序严格按消息时序：轮次内容被切成「块」序列，通知与总结回复
    // 各自成块，其余过程片段聚成可折叠块。这样通知出现在它实际发生的
    // 位置——压缩/切换若发生在回复之后，就显示在回复之后，不会被统一
    // 提前到总结之前。
    //
    // summaryFrags 收集同一轮次内全部非 react 的助手回复（含总结阶段产出），
    // 全部渲染而非仅取最后一条，避免遗漏或互相覆盖。
    type TurnBlock =
      | { kind: "process"; frags: Fragment[] }
      | { kind: "notice"; frag: Fragment }
      | { kind: "summary"; frag: Fragment };
    const blocks: TurnBlock[] = [];
    const summaryFrags: Fragment[] = [];
    let currentSection: Fragment[] = [];
    const flushSection = () => {
      if (currentSection.length > 0) {
        blocks.push({ kind: "process", frags: currentSection });
        currentSection = [];
      }
    };
    // 错误通知不随过程折叠：失败轮次往往没有其他可见输出，错误原因必须始终可见。
    const errorFrags: Fragment[] = [];
    let userFrag: Fragment | null = null;
    for (const frag of mergedFragments) {
      if (frag.type === "user") {
        userFrag = frag;
      } else if (frag.type === "error_system") {
        errorFrags.push(frag);
      } else if (frag.type === "assistant" && frag.msg.phase !== "react") {
        flushSection();
        summaryFrags.push(frag);
        blocks.push({ kind: "summary", frag });
      } else if (frag.type === "context_management" || frag.type === "other_system") {
        flushSection();
        blocks.push({ kind: "notice", frag });
      } else {
        currentSection.push(frag);
      }
    }
    flushSection();
    // 折叠统计与「是否有可折叠内容」按全部过程块合计。
    const processFrags: Fragment[] = blocks.flatMap((block) =>
      block.kind === "process" ? block.frags : []
    );

    // 同一轮的合计只挂在最后一个带操作栏的回复上。
    const usageAnchor = [...summaryFrags].reverse().find((frag) =>
      frag.type === "assistant" && !frag.isStreaming && displayTextContent(frag.msg) && !parseAgentReply(displayTextContent(frag.msg))
    );
    const usageAnchorId = usageAnchor?.type === "assistant" ? usageAnchor.msg.id : null;
    // 运行行挂在尾部工具组：执行中的调用总是出现在过程尾部。
    // 只有最后一个片段是工具组时才挂上去；本轮尚无工具结果，或工具组之后
    // 又出现了思考、解释等片段时为 null，运行行改在轮次尾部单独渲染，
    // 避免首个调用执行期间界面空白、或运行行被插回较早的工具组。
    // 用工具组自身的 key 定位而非渲染下标——渲染按时序分块后下标不再连续。
    const lastToolGroupKey = (() => {
      const last = mergedFragments[mergedFragments.length - 1];
      return last?.type === "tool_group" ? last.key : null;
    })();

    return {
      toolCallArgs,
      settledToolCallIds,
      userFrag,
      blocks,
      processFrags,
      errorFrags,
      usageAnchorId,
      lastToolGroupKey,
    };
  }, [messages, streamingMessageId, agents]);

  const argsOfToolMessage = (msg: MessageItem): unknown =>
    msg.tool_call_id ? toolCallArgs.get(msg.tool_call_id)?.arguments : undefined;

  const renderWithHighlight = (msgId: string, text: string) => {
    if (!searchQuery) return text;
    const occurrences = findTextOccurrences(text, searchQuery, caseSensitive);
    if (occurrences.length === 0) return text;
    const isCurrent = msgId === currentMessageId;
    return <HighlightText text={text} matches={occurrences} currentMatchStart={isCurrent ? currentMatchStart : null} />;
  };
  // 执行中的调用：tool_calls 已发出但结果未到达，仅活跃轮次渲染为运行行。
  // 携带发起时刻（store 记录），运行行据此实时跳秒。
  const runningToolCalls = isActive
    ? [...toolCallArgs.values()]
        .filter((call) => !settledToolCallIds.has(call.id))
        .map((call) => ({
          ...call,
          startedAt: toolCallStartedAt[call.id],
          finished: toolCallFinished[call.id],
        }))
    : [];

  const renderFragment = (frag: Fragment, i: number) => {
    if (frag.type === "thinking") {
      // 历史/已完成思考块一律视为非活跃且默认折叠。
      return <div key={`think-${i}`} title={formatMessageTime(frag.time)}><ThinkingBlock content={frag.content} isActive={false} defaultExpanded={false} elapsedMs={frag.elapsedMs} /></div>;
    }
        if (frag.type === "explanation") {
          return <p key={`expl-${i}`} className="text-sm text-muted-foreground leading-relaxed whitespace-pre-wrap break-words" title={formatMessageTime(frag.time)}>{frag.text}</p>;
        }
        if (frag.type === "tool_group") {
          return (
            <ToolGroup
              key={`tools-${frag.key}`}
              tools={frag.tools}
              expansion={toolGroupExpansion}
              argsOf={argsOfToolMessage}
              runningCalls={frag.key === lastToolGroupKey && runningToolCalls.length > 0 ? runningToolCalls : undefined}
            />
          );
        }
        if (frag.type === "injected_media") {
          const msg = frag.msg;
          const content = Array.isArray(msg.content) ? msg.content : [];
          const hasImage = content.some(
            (block) =>
              (block.type === "media" && block.kind === "image") ||
              ((block.type === "asset_reference" || block.type === "image") && block.asset.kind === "image"),
          );
          return (
            <div key={msg.id} className="my-1.5 rounded-lg border border-border/70 bg-muted/20 px-3 py-2" title={formatMessageTime(msg.created_at)}>
              <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                <Camera className="h-3.5 w-3.5 shrink-0" />
                <span>{hasImage ? "截图已注入 · 模型可见" : "媒体已注入 · 模型可见"}</span>
              </div>
              <ContentMedia message={msg} />
            </div>
          );
        }
        if (frag.type === "user") {
          const statusMeta = frag.msg.turn_status ? TURN_STATUS_META[frag.msg.turn_status] : null;
          return (
            <div key={frag.msg.id} className="flex flex-col items-end gap-0.5" title={formatMessageTime(frag.msg.created_at)}>
              <div className="flex justify-end">
                <div className="max-w-[85%] rounded-2xl bg-primary/10 px-4 py-2.5 text-foreground">
                  <CollapsibleUserText messageId={frag.msg.id}>
                    {textContent(frag.msg)}
                  </CollapsibleUserText>
                </div>
              </div>
              {(frag.msg.elapsed_ms != null || statusMeta) && (
                <div className="flex items-center gap-1.5 pr-1 text-[11px] text-muted-foreground/80 tabular-nums">
                  {statusMeta && (
                    <span className={`inline-flex items-center gap-1 ${statusMeta.className}`}>
                      <span className={`inline-block w-1.5 h-1.5 rounded-full ${statusMeta.dot}`} />
                      {statusMeta.label}
                    </span>
                  )}
                  {frag.msg.elapsed_ms != null && <span>⏱ {formatDuration(frag.msg.elapsed_ms)}</span>}
                </div>
              )}
            </div>
          );
        }
        if (frag.type === "assistant") {
          const { msg, isStreaming } = frag;
          const visibleText = displayTextContent(msg);
          const isReactPhase = msg.phase === "react";
          // 流式输出无条件剥离状态标记（[DONE]/[NEED_MORE_WORK]），
          // 即使后端 phase 尚未传播到也兜底，避免标记泄漏到界面。
          const visibleStreamingContent = stripSummaryStatusMarker(streamingContent);
          const agentReply = !isStreaming ? parseAgentReply(visibleText) : null;
          if (agentReply) {
            // Sub Agent 汇报：默认折叠为一行摘要，点击展开完整正文。
            // 搜索命中时强制展开，便于定位。
            const hasSearchHit = !!searchQuery && findTextOccurrences(visibleText, searchQuery, caseSensitive).length > 0;
            return (
              <div key={msg.id} className="py-0.5">
                <AgentReplyCard
                  label={agentReply.label}
                  body={hasSearchHit ? visibleText : agentReply.body}
                  time={msg.created_at}
                  defaultExpanded={hasSearchHit}
                />
              </div>
            );
          }
          // ReAct 工具执行阶段的过程性文本：紧凑展示，不提供复制按钮。
          if (isReactPhase) {
            const body = isStreaming ? visibleStreamingContent : visibleText;
            if (!body && !streamingReasoningContent) return null;
            return (
              <div key={msg.id} className="text-xs text-muted-foreground leading-relaxed whitespace-pre-wrap break-words" title={formatMessageTime(msg.created_at)}>
                {isStreaming ? (
                  <StreamingMessage content={body} reasoningContent={streamingReasoningContent} />
                ) : (
                  renderWithHighlight(msg.id, body)
                )}
              </div>
            );
          }
          return (
            <div key={msg.id} className="text-foreground" title={formatMessageTime(msg.created_at)}>
              {isStreaming ? (
                <StreamingMessage content={visibleStreamingContent} reasoningContent={streamingReasoningContent} />
              ) : visibleText || (msg.media && msg.media.length > 0) || hasMediaBlocks(msg) ? (
                <div>
                  <ContentMedia message={msg} />
                  {searchQuery && findTextOccurrences(visibleText, searchQuery, caseSensitive).length > 0
                    ? <div className="text-sm whitespace-pre-wrap break-words">{renderWithHighlight(msg.id, visibleText)}</div>
                    : <LazyMdPreview modelValue={resolveMarkdownImages(visibleText)} theme={resolvedTheme} />}
                </div>
              ) : null}
              {!isStreaming && msg.content && visibleText && (
                <div className="mt-1 border-t border-border/50 pt-1">
                  <MessageActions
                    messageId={msg.id}
                    text={visibleText}
                    durationMs={!isActive && turnStatusMeta == null ? (turnElapsedMs ?? userFrag?.msg.elapsed_ms) : undefined}
                    usageMessages={msg.id === usageAnchorId ? messages : undefined}
                  />
                </div>
              )}
            </div>
          );
        }
        if (frag.type === "error_system") {
          return <div key={frag.msg.id} className="text-sm text-destructive bg-destructive/10 rounded-md px-3 py-2 my-1">{textContent(frag.msg).replace("[错误] ", "")}</div>;
        }
        if (frag.type === "retry_system") {
          return <div key={frag.msg.id} className="text-xs text-yellow-600 dark:text-yellow-400 bg-yellow-500/10 rounded-md px-3 py-1.5 my-0.5">{textContent(frag.msg).replace("[重试] ", "")}</div>;
        }
        if (frag.type === "context_management") {
          const text = textContent(frag.msg).replace("[上下文管理] ", "");
          return (
            <div key={frag.msg.id} className="inline-flex max-w-full items-center gap-2 rounded-md border border-border/70 bg-muted/30 px-2.5 py-1 text-xs text-muted-foreground" title={formatMessageTime(frag.msg.created_at)}>
              <FileText className="h-3.5 w-3.5 shrink-0" />
              <span className="truncate">{text}</span>
            </div>
          );
        }
        if (frag.type === "subagent_report") {
          const statusIcon: Record<string, string> = { "任务完成": "✓", "执行失败": "✗", "运行阻塞": "⏳", "等待审批": "?" };
          const statusColor: Record<string, string> = {
            "任务完成": "text-emerald-600 dark:text-emerald-400 bg-emerald-500/10 border-emerald-500/20",
            "执行失败": "text-red-600 dark:text-red-400 bg-red-500/10 border-red-500/20",
            "运行阻塞": "text-amber-600 dark:text-amber-400 bg-amber-500/10 border-amber-500/20",
            "等待审批": "text-blue-600 dark:text-blue-400 bg-blue-500/10 border-blue-500/20",
            "消息": "text-muted-foreground bg-muted/30 border-border",
          };
          const sc = statusColor[frag.status] || statusColor["消息"];
          return (
            <div key={`sr-${frag.msgId}`} className="my-1.5 rounded-lg border bg-card shadow-sm overflow-hidden" title={formatMessageTime(frag.time)}>
              <div className={`flex items-center gap-2 px-3 py-1.5 border-b text-xs font-medium ${sc}`}>
                <span className="inline-flex items-center justify-center w-5 h-5 rounded-full bg-primary/10 text-primary text-[10px] font-bold">S</span>
                <span className="font-semibold">{frag.agentName}</span>
                <span className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-medium border">
                  {statusIcon[frag.status] || "•"} {frag.status}
                </span>
              </div>
              <div className="px-3 py-2 text-sm leading-relaxed whitespace-pre-wrap break-words text-card-foreground">
                {frag.content}
              </div>
            </div>
          );
        }
        if (frag.type === "agent_event") {
          const colorMap: Record<string, string> = { lock: "border-yellow-500/30 bg-yellow-500/5", info: "border-border bg-muted/30" };
          return <div key={`agent-${i}`} className={`text-xs text-muted-foreground border rounded px-2 py-1 my-0.5 ${colorMap[frag.category] || colorMap.info}`}>{frag.content}</div>;
        }
        if (frag.type === "other_system") {
          return <p key={frag.msg.id} className="text-xs text-muted-foreground">{textContent(frag.msg).split("\n")[0]}</p>;
        }
        return null;
  };

  // 过程片段计数：用于折叠态摘要文案（思考/工具条数）。
  const processStats = {
    thinking: processFrags.filter((f) => f.type === "thinking").length,
    tools: processFrags.filter((f) => f.type === "tool_group").reduce((acc, f) => acc + (f.type === "tool_group" ? f.tools.length : 0), 0),
  };
  const collapseProcess = !isActive && processFrags.length > 0;
  // 失败/取消的轮次：状态与总时长在轮次末尾（工具或回复下方）展示；
  // 成功轮次的总时长在回复底部操作栏展示。
  const turnStatusMeta = TURN_STATUS_META[turnStatus ?? ""] ?? null;

  return (
    <div className="space-y-1.5">
      {userFrag && renderFragment(userFrag, 0)}
      {/* 轮次内容严格按时序渲染：过程块可折叠，通知与总结回复始终可见。
          通知因此出现在它实际发生的位置——压缩/切换若发生在回复之后就
          显示在回复之后，不再被统一提前到总结之前。 */}
      {blocks.map((block, blockIndex) => {
        const base = blockIndex * 1000;
        if (block.kind === "notice") {
          return <div key={`notice-${blockIndex}`}>{renderFragment(block.frag, base)}</div>;
        }
        if (block.kind === "summary") {
          return <div key={`summary-${blockIndex}`}>{renderFragment(block.frag, base)}</div>;
        }
        // 过程块：折叠态只在**第一个**过程块位置显示展开入口，避免多段
        // 过程各出一个按钮把轮次切碎。
        const isFirstProcess = blocks.findIndex((item) => item.kind === "process") === blockIndex;
        if (collapseProcess && !showProcess) {
          return isFirstProcess ? (
            <button
              key={`toggle-${blockIndex}`}
              type="button"
              onClick={() => setShowProcess(true)}
              className="inline-flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors py-0.5"
            >
              <ChevronRight className="w-3 h-3" />
              <span>展开过程</span>
              <span className="opacity-60">
                （{processStats.thinking > 0 ? `思考 ${processStats.thinking}·` : ""}{processStats.tools > 0 ? `工具 ${processStats.tools}` : ""}{processStats.thinking === 0 && processStats.tools === 0 ? `${processFrags.length} 条` : ""}）
              </span>
            </button>
          ) : null;
        }
        return (
          <div key={`proc-${blockIndex}`} className="space-y-1.5">
            {collapseProcess && showProcess && isFirstProcess && (
              <button
                type="button"
                onClick={() => setShowProcess(false)}
                className="inline-flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors py-0.5"
              >
                <ChevronDown className="w-3 h-3" />
                <span>收起过程</span>
              </button>
            )}
            {block.frags.map((frag, i) => renderFragment(frag, base + i))}
          </div>
        );
      })}
      {!lastToolGroupKey && runningToolCalls.length > 0 && (
        <ToolGroup
          tools={[]}
          expansion={toolGroupExpansion}
          argsOf={argsOfToolMessage}
          runningCalls={runningToolCalls}
        />
      )}
      {errorFrags.map((frag, i) => renderFragment(frag, i))}
      {turnStatusMeta && !isActive && (
        <div className="flex flex-wrap items-center gap-1.5 text-[11px] text-muted-foreground/80 tabular-nums">
          <span className={`inline-flex items-center gap-1 ${turnStatusMeta.className}`}>
            <span className={`inline-block w-1.5 h-1.5 rounded-full ${turnStatusMeta.dot}`} />
            {turnStatusMeta.label}
          </span>
          {turnElapsedMs != null && (
            <span title="本轮执行总时长">⏱ {formatDuration(turnElapsedMs)}</span>
          )}
          {!usageAnchorId && <CallUsageDetails messages={messages} />}
        </div>
      )}
      {!usageAnchorId && (!turnStatusMeta || isActive) && <CallUsageDetails messages={messages} />}
    </div>
  );
}

const AgentTurn = memo(AgentTurnView, (prev, next) => {
  if (!sameMessageRefs(prev.messages, next.messages) || prev.isActive !== next.isActive || prev.turnElapsedMs !== next.turnElapsedMs || prev.turnStatus !== next.turnStatus) return false;
  const touchesStreamingMessage = hasMessage(prev.messages, prev.streamingMessageId) || hasMessage(prev.messages, next.streamingMessageId);
  if (!touchesStreamingMessage) return true;
  return prev.streamingMessageId === next.streamingMessageId && prev.streamingContent === next.streamingContent && prev.streamingReasoningContent === next.streamingReasoningContent;
});

export { AgentTurn, AgentTurnView };
