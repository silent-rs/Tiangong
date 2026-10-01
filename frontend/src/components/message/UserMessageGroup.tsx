import { useState } from "react";
import { useSearchStore } from "@/store/useSearchStore";
import { findTextOccurrences } from "@/utils/search";
import { HighlightText } from "../HighlightText";
import { MentionChip } from "../MentionChip";
import { MentionEditor, type MentionEditorHandle } from "../MentionEditor";
import { Clock3, Mic, Webhook as WebhookIcon, X, Paperclip } from "lucide-react";
import { resolveAttachmentUrl, type Attachment } from "@/utils/attachments";
import { parseScheduledTaskMessage } from "@/utils/scheduledTaskMessage";
import { parseWebhookMessage } from "@/utils/webhookMessage";
import { textContent } from "@/api/tauri";
import { hasMention, parseBlocks } from "@/utils/mentionBlocks";
import { formatMessageTime } from "./utils";
import type { MessageGroup, MessageItem } from "./types";
import { UserMessageActions } from "./UserMessageActions";
import { ContentMedia } from "./ContentMedia";
import { MessagePluginHost } from "../MessagePluginHost";
import type { HostMessageContext } from "../pluginHostContext";
import { CollapsibleMarkdownText, CollapsibleUserText } from "./CollapsibleUserText";



/// 计算左侧空白长度：裁剪后的文本须补上被去掉的前缀，才能换算回
/// 原消息中的位置（搜索高亮依赖原消息坐标）。
const leadingWhitespace = (s: string) => s.length - s.trimStart().length;

/// 手风琴行：独立展开/收起，标题常显、内容按需展开。
function AccordionRow({ title, children, contentOffset, renderText }: {
  title: string; children: string; contentOffset: number;
  renderText: (text: string, sourceOffset: number) => React.ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className="border-b border-border/15 last:border-b-0">
      <button type="button" className="flex items-center gap-1.5 w-full px-4 py-1.5 text-xs text-muted-foreground hover:text-foreground hover:bg-muted/[0.08] transition-colors" onClick={() => setOpen(!open)}>
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={`w-3 h-3 transition-transform shrink-0 ${open ? 'rotate-90' : ''}`}><path d="m9 18 6-6-6-6"/></svg>
        <span className="font-medium">{title}</span>
        {!open && <span className="ml-auto text-[10px] text-muted-foreground/40 truncate max-w-[50%]">{children.slice(0, 40).replace(/\n/g, ' ')}…</span>}
      </button>
      {open && (
        <div className="px-4 pb-2 pt-0.5">
          <div className="rounded-md bg-muted/[0.1] border border-border/15 px-3 py-2 text-xs leading-relaxed whitespace-pre-wrap break-words text-muted-foreground max-h-36 overflow-y-auto">
            {renderText(children, contentOffset)}
          </div>
        </div>
      )}
    </div>
  );
}

/// Subagent 任务指派卡片：分层展示核心任务与元信息。
/// 正文结构：任务/消息内容 →【任务工作区】→【长期指令】→【长期记忆】
/// →【成长约定】→【工作区状态】→（运行标记）。
function SubagentTaskCard({ body, bodyOffset, source, kind, time, onCopy, renderText }: {
  body: string; bodyOffset: number; source: string; kind: string; time: string;
  onCopy: () => void; renderText: (text: string, sourceOffset: number) => React.ReactNode;
}) {
  const [expanded, setExpanded] = useState(false);

  // 标记支持新旧两种格式：【任务工作区】(新) / 任务工作区：(旧)
  const metaDefs: [string, RegExp][] = [
    ['任务工作区', /\u3010任务工作区\u3011|任务工作区：/],
    ['长期指令', /\u3010长期指令\u3011/],
    ['长期记忆', /\u3010长期记忆\u3011/],
    ['成长约定', /\u3010成长约定\u3011/],
    ['工作区状态', /\u3010工作区状态\u3011/],
  ];
  // 先在全文中定位所有标记的起始位置（互不截断），再统一提取
  const positions: { start: number; title: string; markerLen: number; raw: string }[] = [];
  for (const [title, re] of metaDefs) {
    const m = re.exec(body);
    if (!m) continue;
    positions.push({ start: m.index, title, markerLen: m[0].length, raw: '' });
  }
  // 加上运行标记位置作为最终截断
  const runTagIdx = body.indexOf('\uff08运行标记');
  positions.sort((a, b) => a.start - b.start);
  // 逐段填充内容：从标记结束到下一个标记或运行标记或末尾。
  // 只切除标记本身（旧格式含“标题：”前缀），保留其后内容。
  for (let i = 0; i < positions.length; i++) {
    const cur = positions[i];
    const nextStart = i + 1 < positions.length ? positions[i + 1].start
      : (runTagIdx >= 0 ? runTagIdx : body.length);
    cur.raw = body.slice(cur.start + cur.markerLen, nextStart);
  }
  // 各段记录自身在原消息中的起始偏移，供搜索高亮换算
  const metaSections = positions.map(p => {
    const content = p.raw.trim();
    return {
      title: p.title,
      content,
      offset: bodyOffset + p.start + p.markerLen + leadingWhitespace(p.raw),
    };
  }).filter(s => s.content);
  // 核心内容 = 第一个标记之前的文本
  const firstMarkerIdx = positions.length > 0 ? positions[0].start : body.length;
  const coreRaw = body.slice(0, firstMarkerIdx).trimEnd().replace(/\n*\uff08运行标记[^\uff09]*\uff09\s*$/, '');
  const coreContent = coreRaw.trim();
  const coreOffset = bodyOffset + leadingWhitespace(coreRaw);

  const MAX_LINES = 5;
  const lines = coreContent.split('\n');
  const isLong = lines.length > MAX_LINES;
  const displayContent = expanded || !isLong ? coreContent : lines.slice(0, MAX_LINES).join('\n') + '…';

  const isTask = kind === '任务';

  return (
    <div className="w-full max-w-[92%] sm:max-w-[80%]">
      <div className="rounded-xl border border-border/60 bg-card shadow-sm overflow-hidden">
        <div className="flex items-center gap-2.5 px-4 py-2 border-b border-border/30 bg-muted/[0.1]">
          <div className="flex items-center justify-center w-7 h-7 rounded-lg bg-primary/10 border border-primary/15 shrink-0">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="w-3.5 h-3.5 text-primary">
              {isTask ? <><path d="M9 11l3 3L22 4"/><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"/></> : <path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z"/>}
            </svg>
          </div>
          <div className="min-w-0 flex-1">
            <span className="text-sm font-semibold text-foreground">{isTask ? '任务指派' : '协作消息'}</span>
            <span className="ml-2 text-[11px] text-muted-foreground">来自 {source.trim()}</span>
          </div>
          <span className="text-[10px] text-muted-foreground/50 shrink-0">{time}</span>
          <button type="button" aria-label="复制" className="inline-flex items-center justify-center w-6 h-6 rounded-md text-muted-foreground/40 hover:text-foreground hover:bg-foreground/5 transition-colors shrink-0" onClick={onCopy}>
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="w-3.5 h-3.5"><rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>
          </button>
        </div>

        <div className="px-4 py-3">
          <div className="text-sm leading-relaxed whitespace-pre-wrap break-words text-card-foreground">
            {renderText(displayContent, coreOffset)}
          </div>
          {isLong && (
            <button type="button" className="mt-2 text-xs text-primary hover:text-primary/70 transition-colors" onClick={() => setExpanded(!expanded)}>
              {expanded ? '收起' : `展开全部（共 ${lines.length} 行）`}
            </button>
          )}
        </div>

        {metaSections.length > 0 && (
          <div className="border-t border-border/30 py-1">
            {metaSections.map((section) => (
              <AccordionRow key={section.title} title={section.title} contentOffset={section.offset} renderText={renderText}>
                {section.content}
              </AccordionRow>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}


/// Subagent Hook 回报卡片：状态色竖条+状态图标+成员名+徽章+可折叠内容。
function SubagentReportCard({ agentName, status, content, contentOffset, time, onCopy, renderText }: {
  agentName: string; status: string; content: string; contentOffset: number; time: string;
  onCopy: () => void; renderText: (text: string, sourceOffset: number) => React.ReactNode;
}) {
  const [expanded, setExpanded] = useState(false);
  const statusIcon: Record<string, string> = { '任务完成': '✓', '执行失败': '✗', '运行阻塞': '⏳', '等待审批': '?' };
  const accentColor: Record<string, string> = {
    '任务完成': 'border-l-emerald-500', '执行失败': 'border-l-red-500',
    '运行阻塞': 'border-l-amber-500', '等待审批': 'border-l-blue-500', '消息': 'border-l-border',
  };
  const iconBg: Record<string, string> = {
    '任务完成': 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 border-emerald-500/20',
    '执行失败': 'bg-red-500/10 text-red-600 dark:text-red-400 border-red-500/20',
    '运行阻塞': 'bg-amber-500/10 text-amber-600 dark:text-amber-400 border-amber-500/20',
    '等待审批': 'bg-blue-500/10 text-blue-600 dark:text-blue-400 border-blue-500/20',
    '消息': 'bg-muted text-muted-foreground border-border',
  };
  const pillColor: Record<string, string> = {
    '任务完成': 'bg-emerald-500/10 text-emerald-700 dark:text-emerald-300 border-emerald-500/15',
    '执行失败': 'bg-red-500/10 text-red-700 dark:text-red-300 border-red-500/15',
    '运行阻塞': 'bg-amber-500/10 text-amber-700 dark:text-amber-300 border-amber-500/15',
    '等待审批': 'bg-blue-500/10 text-blue-700 dark:text-blue-300 border-blue-500/15',
    '消息': 'bg-muted/50 text-muted-foreground border-border',
  };
  const ac = accentColor[status] || accentColor['消息'];
  const ib = iconBg[status] || iconBg['消息'];
  const pc = pillColor[status] || pillColor['消息'];
  const MAX_LINES = 5;
  const lines = content.split('\n');
  const isLong = lines.length > MAX_LINES;
  const displayContent = expanded || !isLong ? content : lines.slice(0, MAX_LINES).join('\n') + '…';
  return (
    <div className="w-full max-w-[92%] sm:max-w-[80%]">
      <div className={`rounded-xl border border-border/60 border-l-[3px] ${ac} bg-card shadow-sm overflow-hidden`}>
        <div className="flex items-center gap-2.5 px-4 py-2.5 border-b border-border/30 bg-muted/[0.15]">
          <div className={`flex items-center justify-center w-7 h-7 rounded-full border shrink-0 ${ib}`}>
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" className="w-3.5 h-3.5">
              {status === '任务完成' && <path d="M20 6 9 17l-5-5"/>}
              {status === '执行失败' && <><path d="M18 6 6 18"/><path d="m6 6 12 12"/></>}
              {status === '运行阻塞' && <><circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/></>}
              {status === '等待审批' && <><circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/></>}
              {status === '消息' && <path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z"/>}
            </svg>
          </div>
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <span className="text-sm font-semibold text-foreground">{agentName}</span>
              <span className={`inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-medium border ${pc}`}>
                {statusIcon[status] || '•'} {status}
              </span>
            </div>
          </div>
          <span className="text-[10px] text-muted-foreground/50 shrink-0">{time}</span>
          <button type="button" aria-label="复制" className="inline-flex items-center justify-center w-6 h-6 rounded-md text-muted-foreground/40 hover:text-foreground hover:bg-foreground/5 transition-colors shrink-0" onClick={onCopy}>
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="w-3.5 h-3.5"><rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>
          </button>
        </div>
        <div className="px-4 py-3">
          <div className="text-sm leading-relaxed whitespace-pre-wrap break-words text-card-foreground">
            {renderText(displayContent, contentOffset)}
          </div>
          {isLong && (
            <button type="button" className="mt-2 text-xs text-primary hover:text-primary/70 transition-colors" onClick={() => setExpanded(!expanded)}>
              {expanded ? '收起' : `展开全部（共 ${lines.length} 行）`}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/// 语音输入消息卡片（录音附件 + 识别文本）：与 Subagent 回报卡片同一视觉
/// 语言。录音播放由插件经 session.message-item 注入，宿主不再渲染原生
/// 音频控件；识别文本与录音绑定，不提供编辑重发（编辑会丢失录音）。
function VoiceMessageCard({ time, onCopy, plugin, children }: {
  time: string; onCopy: () => void; plugin: React.ReactNode; children: React.ReactNode;
}) {
  return (
    <div className="w-full max-w-[92%] sm:max-w-[80%]">
      <div className="rounded-xl border border-border/60 border-l-[3px] border-l-primary/60 bg-card shadow-sm overflow-hidden">
        <div className="flex items-center gap-2.5 px-4 py-2 border-b border-border/30 bg-muted/[0.15]">
          <div className="flex items-center justify-center w-7 h-7 rounded-full border shrink-0 bg-primary/10 text-primary border-primary/20">
            <Mic className="w-3.5 h-3.5" aria-hidden="true" />
          </div>
          <span className="min-w-0 flex-1 text-sm font-semibold text-foreground">语音输入</span>
          <span className="text-[10px] text-muted-foreground/50 shrink-0">{time}</span>
          <button type="button" aria-label="复制" title="复制识别文本" className="inline-flex items-center justify-center w-6 h-6 rounded-md text-muted-foreground/40 hover:text-foreground hover:bg-foreground/5 transition-colors shrink-0" onClick={onCopy}>
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="w-3.5 h-3.5"><rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>
          </button>
        </div>
        <div className="px-4 py-2.5">
          {plugin}
          <div className="text-sm leading-relaxed whitespace-pre-wrap break-words text-card-foreground">
            {children}
          </div>
        </div>
      </div>
    </div>
  );
}

/// 语音输入消息：带插件录音附件（发送时命名「语音消息」或录音文件前缀
/// stt_rec_）的用户消息。与语音插件的消息附加区识别规则一致。
function isVoiceMessage(message: MessageItem): boolean {
  const content = Array.isArray(message.content) ? message.content : [];
  return content.some((block) => {
    if (block.type !== "asset_reference" || block.asset.kind !== "audio") return false;
    const name = block.asset.original_name ?? "";
    const file = (block.asset.local_path ?? "").split(/[\\/]/).pop() ?? "";
    return name.startsWith("语音消息") || file.startsWith("stt_rec_");
  });
}

export function UserMessageGroup({ group, runStatus, nonEditableIds, editingMessageId, editingContent, editingAttachments, editingTextareaRef, onStartEdit, onConfirmEdit, onCancelEdit, onSetEditingContent, onSetEditingAttachments, onAttachFiles, onEditPaste }: {
  group: MessageGroup;
  runStatus: string;
  nonEditableIds: Set<string>;
  editingMessageId: string | null;
  editingContent: string;
  editingAttachments: Attachment[];
  editingTextareaRef: React.RefObject<MentionEditorHandle>;
  onStartEdit: (messageId: string, text: string) => void;
  onConfirmEdit: () => void;
  onCancelEdit: () => void;
  onSetEditingContent: (v: string) => void;
  onSetEditingAttachments: React.Dispatch<React.SetStateAction<Attachment[]>>;
  onAttachFiles: () => void;
  onEditPaste: (e: React.ClipboardEvent<HTMLDivElement>) => void;
}) {
  const message = group.messages[0];
  const messageText = textContent(message);
  const scheduledTask = parseScheduledTaskMessage(messageText);
  const webhook = parseWebhookMessage(messageText);
  // Subagent 回报：Hook 消息以 [Subagent·成员名] 开头，渲染为卡片
  // 而非普通用户消息气泡。
  const subagentMatch = messageText.match(/^\[Subagent·([^\]]+)\]\s*(.*)$/s);
  // Subagent 任务投递：专属会话中收到的任务/消息以【Subagent 消息/任务】开头。
  const subagentTaskMatch = messageText.match(/^【Subagent (消息|任务)】来自(.+?)：\n?([\s\S]*)$/);
  // Subagent 回报（纯消息投递路径）：接收方收到的成员回报。
  const subagentReportMessageMatch = messageText.match(/^【Subagent 回报】来自(.+?)：\n?([\s\S]*)$/);
  const isEditing = editingMessageId === message.id && !scheduledTask && !webhook;
  const pluginMessage = userMessageContext(message, messageText);
  const voiceMessage = !subagentMatch && !subagentTaskMatch && !subagentReportMessageMatch
    && !scheduledTask && !webhook && isVoiceMessage(message);
  const searchQuery = useSearchStore((s) => s.searchQuery);
  const currentMessageId = useSearchStore((s) => s.currentMessageId);
  const currentMatchStart = useSearchStore((s) => s.currentMatchStart);
  const caseSensitive = useSearchStore((s) => s.caseSensitive);

  const renderUserText = (text: string, sourceOffset = 0) => {
    const isCurrent = message.id === currentMessageId;
    const localCurrentMatch = isCurrent
      && currentMatchStart !== null
      && currentMatchStart >= sourceOffset
      && currentMatchStart < sourceOffset + text.length
      ? currentMatchStart - sourceOffset
      : null;

    // 无提及：走原有纯文本 / 高亮路径，行为完全不变
    if (!hasMention(text)) {
      if (!searchQuery) return text;
      const occurrences = findTextOccurrences(text, searchQuery, caseSensitive);
      if (occurrences.length === 0) return text;
      return <HighlightText text={text} matches={occurrences} currentMatchStart={localCurrentMatch} />;
    }

    // 有提及：按块分段渲染。搜索高亮的 offset 空间仍是原始字符串，
    // 因此对每个 text 段切分其在全串中的匹配，保留既有高亮语义。
    const allOccurrences = searchQuery
      ? findTextOccurrences(text, searchQuery, caseSensitive)
      : [];
    const nodes: React.ReactNode[] = [];
    let segStart = 0;
    let key = 0;
    for (const block of parseBlocks(text)) {
      if (block.type === 'text') {
        const segEnd = segStart + block.value.length;
        const segMatches = allOccurrences
          .filter((m) => m.start >= segStart && m.end <= segEnd)
          .map((m) => ({ start: m.start - segStart, end: m.end - segStart }));
        if (segMatches.length === 0) {
          nodes.push(<span key={key++}>{block.value}</span>);
        } else {
          // 当前搜索位置已换算到本分区，落在本段内时再按段起点换算。
          const rebasedCurrent = localCurrentMatch != null
            && localCurrentMatch >= segStart && localCurrentMatch < segEnd
              ? localCurrentMatch - segStart
              : null;
          nodes.push(
            <HighlightText
              key={key++}
              text={block.value}
              matches={segMatches}
              currentMatchStart={rebasedCurrent}
            />,
          );
        }
        segStart = segEnd;
      } else {
        nodes.push(
          <MentionChip
            key={key++}
            kind={block.kind}
            label={block.label}
            token={block.token}
          />,
        );
        segStart += block.token.length;
      }
    }
    return <>{nodes}</>;
  };

  return (
    <div className="mt-3 first:mt-0">
      {isEditing ? (
        <div className="w-full">
          {editingAttachments.length > 0 && (
            <div className="mb-2 flex flex-wrap gap-1.5">
              {editingAttachments.map((item) => (
                <span key={(item.original_name ?? '') + item.source.slice(0, 40)} className="inline-flex h-9 max-w-[260px] items-center gap-1.5 rounded-md border bg-muted/40 px-2 text-xs" title={item.original_name ?? item.source}>
                  {item.kind === "image" ? <img src={resolveAttachmentUrl(item.source)} alt={item.original_name ?? '附件'} className="h-6 w-6 shrink-0 rounded object-cover" /> : <Paperclip className="h-3 w-3 shrink-0" />}
                  <span className="truncate">{item.original_name ?? item.source}</span>
                  <button type="button" onClick={() => onSetEditingAttachments((prev) => prev.filter((a) => a.source !== item.source))} className="ml-1 text-muted-foreground hover:text-foreground" title="移除附件">
                    <X className="h-3 w-3" />
                  </button>
                </span>
              ))}
            </div>
          )}
          <MentionEditor
            ref={editingTextareaRef}
            value={editingContent}
            onChange={onSetEditingContent}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing && e.keyCode !== 229) { e.preventDefault(); onConfirmEdit(); }
              if (e.key === "Escape") { onCancelEdit(); }
            }}
            onPaste={onEditPaste}
            className="min-h-[60px] max-h-[200px] resize-none text-sm w-full"
            autoFocus
          />
          <div className="flex justify-between items-center mt-1">
            <span className="text-[10px] text-muted-foreground">Enter 发送 · Shift+Enter 换行 · Esc 取消</span>
            <div className="flex gap-1.5">
              <button onClick={onAttachFiles} className="flex items-center gap-1 px-2 py-1 text-xs text-muted-foreground hover:text-foreground transition-colors" title="添加附件">
                <Paperclip className="w-3 h-3" />
              </button>
              <button onClick={onCancelEdit} className="flex items-center gap-1 px-2 py-1 text-xs text-muted-foreground hover:text-foreground transition-colors">
                <X className="h-3 w-3" /> 取消
              </button>
              <button onClick={onConfirmEdit} className="px-2.5 py-1 text-xs bg-green-600 hover:bg-green-700 text-white rounded transition-colors">发送</button>
            </div>
          </div>
        </div>
      ) : (
        <div className="flex justify-end" title={formatMessageTime(message.created_at)}>
          {subagentTaskMatch ? (() => {
            const [, kind, source, body] = subagentTaskMatch;
            return (
              <SubagentTaskCard
                body={body}
                bodyOffset={messageText.length - body.length}
                source={source}
                kind={kind}
                time={formatMessageTime(message.created_at)}
                onCopy={() => navigator.clipboard.writeText(messageText).catch(() => {})}
                renderText={renderUserText}
              />
            );
          })() : subagentReportMessageMatch ? (() => {
            // 纯消息投递路径的成员回报：成员署名渲染为回报卡片。
            const [, source, body] = subagentReportMessageMatch;
            const agentName = source.replace(/^成员「|」$/g, '').trim() || source;
            return (
              <SubagentReportCard
                agentName={agentName}
                status="回报"
                content={body}
                contentOffset={messageText.length - body.length}
                time={formatMessageTime(message.created_at)}
                onCopy={() => navigator.clipboard.writeText(messageText).catch(() => {})}
                renderText={renderUserText}
              />
            );
          })() : subagentMatch ? (() => {
            const [, agentName, body] = subagentMatch;
            const bodyOffset = messageText.length - body.length;
            const statusMatch = body.match(/^(任务完成|执行失败|运行阻塞|等待审批)：?\s*/);
            const status = statusMatch ? statusMatch[1] : '消息';
            const contentRaw = statusMatch ? body.slice(statusMatch[0].length) : body;
            const content = contentRaw.trim();
            const contentOffset = bodyOffset + (statusMatch ? statusMatch[0].length : 0) + leadingWhitespace(contentRaw);
            return (
              <SubagentReportCard
                agentName={agentName.trim()}
                status={status}
                content={content}
                contentOffset={contentOffset}
                time={formatMessageTime(message.created_at)}
                onCopy={() => navigator.clipboard.writeText(messageText).catch(() => {})}
                renderText={renderUserText}
              />
            );
          })() : scheduledTask || webhook ? (
            (() => {
              const data = scheduledTask ?? webhook!;
              return (
                <div className="w-full max-w-[92%] overflow-hidden rounded-lg border border-border/70 bg-card text-foreground shadow-sm sm:max-w-[85%]">
                  <div className="flex items-start gap-2.5 bg-muted/35 px-3 py-2.5">
                    {webhook
                      ? <WebhookIcon className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" aria-hidden="true" />
                      : <Clock3 className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" aria-hidden="true" />}
                    <div className="min-w-0 flex-1">
                      <p className="text-xs font-medium text-muted-foreground">{webhook ? 'Webhook 触发' : '定时任务'}</p>
                      <p className="mt-0.5 break-words text-sm font-medium">
                        {data.name
                          ? renderUserText(data.name, data.offsets.name)
                          : webhook ? '未命名触发' : '未命名任务'}
                      </p>
                      {data.description && (
                        <p className="mt-1 whitespace-pre-wrap break-words text-xs text-muted-foreground">
                          {renderUserText(data.description, data.offsets.description)}
                        </p>
                      )}
                    </div>
                  </div>
                  <div className="border-t border-border/60 px-3 py-2.5">
                    <p className="mb-1 text-[11px] font-medium text-muted-foreground">执行内容</p>
                    <ContentMedia message={message} />
                    {data.payload ? (
                      <p className="whitespace-pre-wrap break-words text-sm leading-6">
                        {renderUserText(data.payload, data.offsets.payload)}
                      </p>
                    ) : (
                      <p className="text-sm text-muted-foreground">无执行内容</p>
                    )}
                  </div>
                </div>
              );
            })()
          ) : voiceMessage ? (
            <VoiceMessageCard
              time={formatMessageTime(message.created_at)}
              onCopy={() => navigator.clipboard.writeText(messageText).catch(() => {})}
              plugin={<MessagePluginHost slot="session.message-item" message={pluginMessage} />}
            >
              {renderUserText(messageText)}
            </VoiceMessageCard>
          ) : (
            <div className="max-w-[85%] rounded-2xl bg-primary/10 px-4 py-2.5 text-foreground">
                <div>
                  {/* 插件消息附加区（如语音消息播放）：由插件经 session.message-item 注入 */}
                  <MessagePluginHost slot="session.message-item" message={pluginMessage} />
                  <ContentMedia message={message} />
                  {messageText && (
                    // 换行与 Markdown 走 Markdown 预览渲染（md-editor-rt 默认
                    // breaks，单换行渲染为真实换行）；mention 消息优先保 chip
                    // 展示，搜索命中时退回纯文本高亮（与助手消息路径一致：
                    // Markdown 渲染后文本被打散，高亮无法定位）。
                    hasMention(messageText) || (searchQuery && findTextOccurrences(messageText, searchQuery, caseSensitive).length > 0) ? (
                      <CollapsibleUserText messageId={message.id}>
                        {renderUserText(messageText)}
                      </CollapsibleUserText>
                    ) : (
                      <CollapsibleMarkdownText messageId={message.id} text={messageText} />
                    )
                  )}
                </div>
            </div>
          )}
        </div>
      )}
      {messageText && !isEditing && !subagentMatch && !subagentTaskMatch && !subagentReportMessageMatch && !voiceMessage && (
        <div className="flex justify-end">
          <UserMessageActions text={messageText} messageId={message.id} runStatus={runStatus} canEdit={!nonEditableIds.has(message.id)} showEdit={!scheduledTask && !webhook} onStartEdit={onStartEdit} />
        </div>
      )}
    </div>
  );
}

/** 用户消息的插件上下文：文本与引用的本地媒体附件。 */
function userMessageContext(message: MessageItem, text: string): HostMessageContext {
  const content = Array.isArray(message.content) ? message.content : [];
  const attachments = content.flatMap((block) =>
    block.type === "asset_reference" || block.type === "image"
      ? [{
          kind: block.asset.kind,
          path: block.asset.local_path,
          mime_type: block.asset.mime_type,
          name: block.asset.original_name,
        }]
      : [],
  );
  return { id: message.id, role: "user", text, attachments };
}
