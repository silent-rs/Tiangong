import { useState, KeyboardEvent, ClipboardEvent, DragEvent, useEffect, useLayoutEffect, useMemo, useRef, useCallback } from 'react';
import type { ReactNode, SetStateAction } from 'react';
import { selectCurrentInputCacheKey, selectCurrentInputCache, useStore } from '@/store/useStore';
import { MentionEditor, type MentionEditorHandle } from './MentionEditor';
import { Button } from './ui/button';
import { Send, Square, FolderOpen, Keyboard, MessageSquarePlus, ShieldCheck, ShieldOff, Circle, Paperclip, X, Brain, Clock, Unlock, AlertTriangle, Cpu } from 'lucide-react';
import { open } from '@tauri-apps/plugin-dialog';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import type { DragDropEvent } from '@tauri-apps/api/webview';
import { api, type MentionTarget, type RawAttachment } from '@/api/tauri';
import { Select, SelectContent, SelectGroup, SelectItem, SelectLabel, SelectTrigger, SelectValue } from './ui/select';
import { useMentionGroups } from '@/hooks/useMentionGroups';
import {
  type Attachment,
  MAX_ATTACHMENT_BASE64_BYTES,
  attachmentKindFromMime,
  imageMimeType,
  imageExtFromMime,
  clipboardImagePaths,
  fileToDataUrl,
  attachmentFromPath,
  estimatedBase64Size,
  resolveAttachmentUrl,
} from '@/utils/attachments';
import { mentionReplaceEnd, replaceMentionCompletion } from '@/utils/mentionEditorModel';
import { mentionMarkFor, registerMentionMarks } from '@/utils/mentionMarks';
import { selectDisplayGroups, selectableCandidates, isScanningPlaceholder, truncateMiddle } from '@/utils/mentionGroups';
import { formatDuration } from './message/utils';
import { SessionInputPluginHost } from './SessionInputPluginHost';
import { InputQueueBar } from './InputQueueBar';

const MOD_KEY_LABEL = navigator.platform.toUpperCase().includes('MAC') ? 'Cmd+Enter' : 'Ctrl+Enter';

/** token 数值紧凑显示（如 12345 -> 12.3k、1959320123 -> 1.9b），向下取整不进位；精确值由 title 提示展示。 */
const TOKEN_UNITS: Array<[divisor: number, unit: string]> = [
  [1_000_000_000, 'b'],
  [1_000_000, 'm'],
  [1_000, 'k'],
];

function formatTokenCount(value: number): string {
  if (value < 1000) return String(value);
  const [divisor, unit] = TOKEN_UNITS.find(([d]) => value >= d) ?? [1_000, 'k'];
  return `${Math.floor(value / (divisor / 10)) / 10}${unit}`;
}

interface MentionCandidate {
  value: string;
  label: string;
  kind: string;
  hint: string;
  /** 标记字符（chip 角标），由插件提供；为空时按 kind 回退默认。 */
  mark?: string;
}

/** mention 分组标题（后端 label 缺省是 kind 原文，这里映射为展示名）。 */
const MENTION_GROUP_TITLES: Record<string, string> = {
  skill: '技能',
  mcp: 'MCP 工具',
  agent: 'Agent',
  file: '文件',
  index: '工作区搜索',
  plugin: '插件',
  command: '命令',
};

/** mention 候选标记字符的底色（与气泡 chip 的 kind 色系一致）。 */
const MENTION_KIND_BADGE_CLASS: Record<string, string> = {
  skill: 'border-amber-500/30 bg-amber-500/10 text-amber-700 dark:text-amber-300',
  mcp: 'border-cyan-500/30 bg-cyan-500/10 text-cyan-700 dark:text-cyan-300',
  agent: 'border-blue-500/30 bg-blue-500/10 text-blue-700 dark:text-blue-300',
  all: 'border-rose-500/30 bg-rose-500/10 text-rose-700 dark:text-rose-300',
  file: 'border-slate-500/30 bg-slate-500/10 text-slate-700 dark:text-slate-300',
  index: 'border-emerald-500/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300',
  plugin: 'border-violet-500/30 bg-violet-500/10 text-violet-700 dark:text-violet-300',
};

const SLASH_COMMANDS: MentionCandidate[] = [  {
    value: '/压缩对话',
    label: '/压缩对话',
    kind: 'command',
    hint: '压缩早期上下文',
  },
  {
    value: '/清理对话',
    label: '/清理对话',
    kind: 'command',
    hint: '清理当前上下文',
  },
];

export interface MessageInputProps {
  /** 征询交互覆盖整个输入区时为 true：输入区整体不可交互。 */
  interactionVisible?: boolean;
  /** 输入覆盖层（`session.input-overlay`）接管编辑框时为 true：仅编辑框区域让位，
   *  上方运行状态 / 模型选择与下方目录 / 用量信息保持可用。 */
  inputOverlayVisible?: boolean;
  /** 输入覆盖层宿主节点，渲染在编辑框区域内并与其同尺寸。 */
  inputOverlay?: ReactNode;
  /** 输入覆盖层显示期间替换底栏快捷键提示的文案。 */
  inputOverlayHint?: string | null;
  onHeightChange?: (height: number) => void;
}

/** 「跟随路由默认」的哨兵值：Radix Select 不接受空字符串作为选项值。 */
const MODEL_DEFAULT_VALUE = '__default__';

export function MessageInput({
  interactionVisible = false,
  inputOverlayVisible = false,
  inputOverlay,
  inputOverlayHint = null,
  onHeightChange,
}: MessageInputProps) {
  const cacheKey = useStore(selectCurrentInputCacheKey);
  const inputCache = useStore(selectCurrentInputCache);
  const inputContent = inputCache.text;
  const attachments = inputCache.attachments;
  const isSending = inputCache.is_sending;
  const setInputCacheText = useStore((state) => state.setInputCacheText);
  const setInputCacheAttachments = useStore((state) => state.setInputCacheAttachments);
  const sandboxDisabled = useStore((state) => state.sandboxDisabled);
  const loadSandboxDisabled = useStore((state) => state.loadSandboxDisabled);
  const sandboxState = useStore((state) => state.sandboxState);
  const loadSandboxState = useStore((state) => state.loadSandboxState);
  const setPendingSettingsTab = useStore((state) => state.setPendingSettingsTab);
  const sendMessage = useStore((state) => state.sendMessage);
  const appendMessage = useStore((state) => state.appendMessage);
  const enqueueInputMessage = useStore((state) => state.enqueueInputMessage);
  const cancelTurn = useStore((state) => state.cancelTurn);
  const beginContextManagement = useStore((state) => state.beginContextManagement);
  const endContextManagement = useStore((state) => state.endContextManagement);
  const runStatus = useStore((state) => state.runStatus);
  const runSummary = useStore((state) => state.runSummary);
  const lastDurationMs = useStore((state) => state.lastDurationMs);
  const isNewConversation = useStore((state) => state.isNewConversation);
  const activeSessionId = useStore((state) => state.activeSessionId);
  const currentSessionRunStatus = useStore((state) => (
    state.activeSessionId ? state.sessionRunStatuses[state.activeSessionId] : undefined
  ));
  const sessionCwd = useStore((state) => state.sessionCwd);
  const setSessionCwd = useStore((state) => state.setSessionCwd);
  const lastUsage = useStore((state) => state.lastUsage);
  const tokenStats = useStore((state) => state.tokenStats);
  const reasoningEffort = useStore((state) => state.reasoningEffort);
  const setReasoningEffort = useStore((state) => state.setReasoningEffort);
  const isComposingRef = useRef(false);
  const editorRef = useRef<MentionEditorHandle>(null);
  const inputAreaRef = useRef<HTMLDivElement>(null);
  const interactionContentRef = useRef<HTMLDivElement>(null);
  const editorContentRef = useRef<HTMLDivElement>(null);
  const lastNativeDropAtRef = useRef(0);

  // @提及补全状态
  const [mentionOpen, setMentionOpen] = useState(false);
  const [mentionFilter, setMentionFilter] = useState('');
  const [mentionIndex, setMentionIndex] = useState(0);
  const [mentionStart, setMentionStart] = useState(-1);
  // 待替换区间末端：触发 mention 时 `@` 到光标的范围。过滤词由面板内的独立
  // 搜索框承载（不写入消息文本），选中候选时整段替换为 chip token。
  const [mentionEnd, setMentionEnd] = useState(-1);
  const [completionMode, setCompletionMode] = useState<'mention' | 'slash'>('mention');
  const mentionRef = useRef<HTMLDivElement>(null);
  const mentionSearchRef = useRef<HTMLInputElement>(null);
  const candidateRefs = useRef<Array<HTMLButtonElement | null>>([]);

  // 信任模式
  const [trustMode, setTrustMode] = useState('full_trust');
  // 事件回调（非渲染流）读取信任模式的最新值。
  const trustModeRef = useRef(trustMode);
  trustModeRef.current = trustMode;
  const [isDraggingFiles, setIsDraggingFiles] = useState(false);

  useLayoutEffect(() => {
    const content = interactionContentRef.current;
    if (!content) return;
    if (interactionVisible) {
      content.setAttribute('inert', '');
      setMentionOpen(false);
      setIsDraggingFiles(false);
      const activeElement = document.activeElement;
      if (activeElement instanceof HTMLElement && content.contains(activeElement)) {
        activeElement.blur();
      }
    } else {
      content.removeAttribute('inert');
    }
    return () => content.removeAttribute('inert');
  }, [interactionVisible]);

  // 输入覆盖层只接管编辑框：编辑框与按钮区置为 inert，覆盖层本身不受影响。
  useLayoutEffect(() => {
    const content = editorContentRef.current;
    if (!content) return;
    if (inputOverlayVisible) {
      content.setAttribute('inert', '');
      setMentionOpen(false);
      setIsDraggingFiles(false);
      const activeElement = document.activeElement;
      if (activeElement instanceof HTMLElement && content.contains(activeElement)) {
        activeElement.blur();
      }
    } else {
      content.removeAttribute('inert');
    }
    return () => content.removeAttribute('inert');
  }, [inputOverlayVisible]);

  const setInputContent = useCallback((content: string) => {
    if (cacheKey) setInputCacheText(cacheKey, content);
  }, [cacheKey, setInputCacheText]);

  const setAttachments = useCallback((update: SetStateAction<Attachment[]>) => {
    if (!cacheKey) return;
    const current = useStore.getState().inputCaches[cacheKey]?.attachments ?? [];
    const next = typeof update === 'function' ? update(current) : update;
    setInputCacheAttachments(cacheKey, next);
  }, [cacheKey, setInputCacheAttachments]);

  const currentRunStatus = isNewConversation
    ? 'idle'
    : currentSessionRunStatus || runStatus;

  // 会话级模型选择：引用来自会话（null=跟随路由默认）；选项来自模型注册表。
  // Radix Select 不允许空字符串 value，「跟随默认」用哨兵值表示。
  const [sessionModelRef, setSessionModelRef] = useState<string | null>(null);
  const [modelOptions, setModelOptions] = useState<{ key: string; label: string; provider: string }[]>([]);
  const [defaultModelLabel, setDefaultModelLabel] = useState<string>('默认');
  const [modelSwitching, setModelSwitching] = useState(false);
  // 拉取可选模型；设置页保存、下拉展开时都会重新拉取，保证与 models.json 同步。
  const refreshModelOptions = useCallback(async (isCancelled: () => boolean = () => false) => {
    try {
      const result = await api.listSessionChatModels();
      if (isCancelled()) return;
      setModelOptions(result.models.map((item) => ({
        key: item.key,
        label: item.label || item.key,
        provider: item.provider,
      })));
      // 跟随默认时显示路由配置的实际模型名。
      setDefaultModelLabel(
        result.default_ref
          ? (result.models.find((item) => item.key === result.default_ref)?.label ?? '默认')
          : '默认',
      );
    } catch (error) {
      console.error(error);
    }
  }, []);
  useEffect(() => {
    let cancelled = false;
    if (activeSessionId) {
      api.getSessionModel(activeSessionId)
        .then((ref) => { if (!cancelled) setSessionModelRef(ref); })
        .catch(console.error);
    } else {
      // 新建对话不沿用上一会话的模型选择，回到跟随路由默认。
      setSessionModelRef(null);
    }
    void refreshModelOptions(() => cancelled);
    return () => { cancelled = true; };
  }, [activeSessionId, refreshModelOptions]);
  // 模型配置变更（设置页保存、登录后自动拉取模型等）后刷新列表。
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    api.onModelsConfigChanged(() => { void refreshModelOptions(() => disposed); })
      .then((fn) => { if (disposed) fn(); else unlisten = fn; })
      .catch(console.error);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refreshModelOptions]);
  const modelUnavailable = sessionModelRef != null
    && !modelOptions.some((option) => option.key === sessionModelRef);
  const modelDisplay = modelUnavailable
    ? `${sessionModelRef}（不可用）`
    : (modelOptions.find((option) => option.key === sessionModelRef)?.label ?? defaultModelLabel);
  // 按服务提供方分组：同名模型可能来自不同提供方（如各平台都有的开源
  // 模型），只列模型名无从区分。后端已按 provider→key 排序，这里顺序
  // 聚合即可保持稳定分组。
  const modelGroups = useMemo(() => {
    const groups: { provider: string; options: typeof modelOptions }[] = [];
    for (const option of modelOptions) {
      const last = groups[groups.length - 1];
      if (last && last.provider === option.provider) {
        last.options.push(option);
      } else {
        groups.push({ provider: option.provider, options: [option] });
      }
    }
    return groups;
  }, [modelOptions]);
  // 新对话也可选择（随首条消息作为初始引用写入会话）；已有会话立即写
  // 引用，执行端点在下一次发送时按引用校正——切走又切回时端点不变。
  const modelSelectorDisabled = currentRunStatus !== 'idle' || modelSwitching;
  const modelSelectorTitle = currentRunStatus !== 'idle'
    ? '会话正在执行，当前回合结束后可切换模型'
    : modelSwitching
      ? '正在切换模型…'
      : modelUnavailable
        ? `${sessionModelRef} 已不在配置中，请重选模型或恢复配置`
        : '会话模型（下一次发送时生效；切换会先整理上下文）';
  const handleSessionModelChange = async (value: string) => {
    if (modelSwitching || currentRunStatus !== 'idle') return;
    const nextRef = value === MODEL_DEFAULT_VALUE ? null : value;
    // 新对话：仅缓存选择，随首条消息作为初始引用写入会话。
    if (!activeSessionId) {
      setSessionModelRef(nextRef);
      return;
    }
    setModelSwitching(true);
    try {
      await api.setSessionModel(activeSessionId, nextRef);
      setSessionModelRef(nextRef);
    } catch (error) {
      console.error('切换会话模型失败：', error);
      alert(error instanceof Error ? error.message : String(error));
    } finally {
      setModelSwitching(false);
    }
  };
  const displayTokens = tokenStats?.current_tokens ?? 0;
  const compressionThreshold = tokenStats?.compression_threshold_tokens ?? 0;
  const compressionProgress = compressionThreshold > 0
    ? Math.min(100, Math.round((displayTokens / compressionThreshold) * 100))
    : 0;
  const totalTokens = tokenStats?.total_tokens ?? lastUsage?.total_tokens ?? 0;

  useEffect(() => {
    let cancelled = false;
    const loadTrustMode = activeSessionId
      ? api.getTrustMode(activeSessionId)
      : api.getDefaultTrustMode();
    loadTrustMode
      .then((mode) => {
        if (!cancelled) setTrustMode(mode);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [activeSessionId]);

  // 按需进程沙箱开关（全局）：状态图标来源；null 表示尚未加载。
  useEffect(() => {
    if (sandboxDisabled === null) {
      void loadSandboxDisabled();
    }
  }, [sandboxDisabled, loadSandboxDisabled]);

  // 沙箱程序状态：底部"沙箱无效"指示来源；启动门闸与设置页会写入共享
  // 状态，此处兜底加载一次。
  useEffect(() => {
    if (sandboxState === null) {
      void loadSandboxState();
    }
  }, [sandboxState, loadSandboxState]);

  const toggleTrustMode = async () => {
    const newMode = trustMode === 'full_trust' ? 'supervised' : 'full_trust';
    try {
      if (activeSessionId) {
        await api.setTrustMode(newMode, activeSessionId);
      }
      setTrustMode(newMode);
    } catch (e) {
      console.error('切换信任模式失败:', e);
    }
  };

  // 当前会话是否空闲
  const currentSessionStatus = isNewConversation
    ? 'idle'
    : currentSessionRunStatus || runStatus;
  const isIdle = currentSessionStatus === 'idle';
  const editorBlocked = interactionVisible || inputOverlayVisible;
  const canSend = !editorBlocked
    && !isSending
    && !!cacheKey
    && (inputContent.trim().length > 0 || attachments.length > 0);
  const isTextDropTargetActive = !editorBlocked && !!cacheKey;

  // 运行中实时计时：维护单调递增的显示基准（baseMs@baseAt），事件到达与本地
  // tick 都只向前推进——事件值与外推值取大，杜绝显示回跳；TurnElapsed 事件
  // 稀疏（如工具执行阶段）时秒数依然持续跳动。
  const liveTimerRef = useRef<{ baseMs: number; baseAt: number }>({ baseMs: 0, baseAt: 0 });
  const [durationTick, setDurationTick] = useState(0);
  useEffect(() => {
    if (isIdle) return;
    const timer = setInterval(() => setDurationTick((tick) => tick + 1), 1000);
    return () => clearInterval(timer);
  }, [isIdle]);
  const liveDurationLabel = (() => {
    void durationTick;
    const { baseMs, baseAt } = liveTimerRef.current;
    if (isIdle || lastDurationMs == null) {
      // 空闲或新一轮尚未收到首个计时事件：复位基准。
      liveTimerRef.current = { baseMs: 0, baseAt: 0 };
      return '';
    }
    const now = Date.now();
    const extrapolated = baseMs > 0 ? baseMs + (now - baseAt) : 0;
    const candidate = Math.max(lastDurationMs, extrapolated);
    liveTimerRef.current = { baseMs: candidate, baseAt: now };
    return candidate < 1000 ? '' : formatDuration(candidate);
  })();
  // 自动调整文本框高度（MentionEditor 内部按 value 自适应，这里不再单独维护）

  // ===== 文字模式相关 =====
  const mentionTarget = useMemo<MentionTarget>(
    () =>
      !isNewConversation && activeSessionId
        ? { kind: 'session', session_id: activeSessionId }
        : sessionCwd ? { kind: 'draft', workspace: sessionCwd } : { kind: 'global' },
    [isNewConversation, activeSessionId, sessionCwd]
  );
  const mentionGroups = useMentionGroups(
    mentionTarget,
    mentionOpen && completionMode === 'mention' ? mentionFilter : '',
    mentionOpen && completionMode === 'mention',
  );
  // 预热 @提及标记表：消息气泡从 token 重建 chip 时查表，需在用户打开
  // 补全面板前完成注册（面板查询命中时也会注册，这里覆盖未打开的场景）。
  // 只取枚举型候选的 mark：file 组候选量随工作区规模增长，且 mark 固定为
  // "F"，不该为它触发一次全量文件检索（会话切换/草稿目录变更都会重跑）。
  useEffect(() => {
    let cancelled = false;
    api.getMentionGroups(undefined, undefined, {
      target: mentionTarget,
      query: '',
      max_per_group: 1000,
      allowed_kinds: ['skill', 'mcp', 'agent', 'plugin', 'index'],
    })
      .then(groups => {
        if (!cancelled) registerMentionMarks(groups.flatMap(group => group.candidates));
      })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [mentionTarget]);
  const filteredGroups = completionMode === 'slash'
    ? []
    : selectDisplayGroups(mentionGroups, mentionFilter);

  // 关闭 mention 面板并清空锚点。文本原样保留（含触发时的 `@` 与已打字符），
  // 不删字；焦点回消息框、光标落在 `@` 之后，用户可继续编辑。
  const closeMentionPanel = useCallback(() => {
    const caret = mentionStart >= 0 ? mentionStart + 1 : null;
    setMentionOpen(false);
    setMentionFilter('');
    setMentionStart(-1);
    setMentionEnd(-1);
    setMentionIndex(0);
    const editor = editorRef.current;
    if (editor && caret != null) {
      editor.focus();
      editor.setSelection(caret);
    }
  }, [mentionStart]);

  // 面板打开时把焦点移到面板内的独立搜索框：此后所有键入都进搜索框，消息
  // 文本不再被输入态污染——因此不需要活跃区降级、光标越界判定那一套机制。
  useEffect(() => {
    if (!mentionOpen || completionMode !== 'mention') return;
    mentionSearchRef.current?.focus();
    mentionSearchRef.current?.select();
  }, [mentionOpen, completionMode]);

  // 平铺所有候选（用于键盘导航与选中；slash 模式用 SLASH_COMMANDS）。
  // 索引建立中的占位候选（value 为空）不可选中，也不参与键盘导航。
  const filteredCandidates = completionMode === 'slash'
    ? (() => {
        const filter = mentionFilter.toLowerCase();
        if (!filter) return SLASH_COMMANDS;
        return SLASH_COMMANDS.filter(c => c.value.toLowerCase().startsWith(filter));
      })()
    : selectableCandidates(filteredGroups.flatMap(group => group.candidates));

  useEffect(() => {
    if (!mentionOpen) return;
    candidateRefs.current[mentionIndex]?.scrollIntoView({
      block: 'nearest',
    });
  }, [mentionIndex, mentionOpen, filteredCandidates.length]);

  const executeSlashCommand = useCallback(async (command: string) => {
    const trimmed = command.trim();
    if (trimmed === '/压缩对话' || trimmed === '/compress') {
      setInputContent('');
      beginContextManagement('正在压缩早期上下文...');
      try {
        const ok = await api.compressContext();
        if (!ok) {
          endContextManagement();
          alert('压缩对话没有执行成功，请稍后重试。');
        }
      } catch (e) {
        endContextManagement();
        console.error('压缩对话失败:', e);
        alert(e instanceof Error ? e.message : '压缩对话失败');
      }
      return true;
    }
    if (trimmed === '/清理对话' || trimmed === '/reset') {
      setInputContent('');
      beginContextManagement('正在清理上下文...');
      try {
        const ok = await api.resetContext();
        if (!ok) {
          endContextManagement();
          alert('清理对话没有执行成功，请稍后重试。');
        }
      } catch (e) {
        endContextManagement();
        console.error('清理对话失败:', e);
        alert(e instanceof Error ? e.message : '清理对话失败');
      }
      return true;
    }
    return false;
  }, [beginContextManagement, endContextManagement, setInputContent]);

  const handleInputChange = (value: string) => {
    setInputContent(value);
    const cursorPos = editorRef.current?.getSelection()?.start ?? value.length;
    const beforeCursor = value.slice(0, cursorPos);
    if (beforeCursor.startsWith('/') && !/\s/.test(beforeCursor)) {
      setMentionStart(0);
      setMentionEnd(cursorPos);
      setMentionFilter(beforeCursor);
      setMentionIndex(0);
      setCompletionMode('slash');
      setMentionOpen(true);
      return;
    }

    // 面板已开（mention）：消息文本的变化只可能是用户回到编辑器继续打字，
    // 视为取消——过滤词从此由面板搜索框承载，不再从消息文本推导。
    if (completionMode === 'mention' && mentionOpen) {
      closeMentionPanel();
      return;
    }

    // 进入 mention：从光标回扫找 `@`（遇空白/换行即止），须位于行首或前置
    // 空白，避免邮箱 `user@example.com` 误触发。过滤词初值取自消息文本里
    // `@` 之后已输入的部分，随后交由面板搜索框接管。
    let atPos = -1;
    for (let i = cursorPos - 1; i >= 0; i--) {
      const ch = value[i];
      if (ch === '\n' || /\s/.test(ch)) break;
      if (ch === '@') {
        if (i === 0 || /\s/.test(value[i - 1])) { atPos = i; }
        break;
      }
    }
    if (atPos >= 0) {
      setMentionStart(atPos);
      setMentionEnd(cursorPos);
      setMentionFilter(value.slice(atPos + 1, cursorPos));
      setMentionIndex(0);
      setCompletionMode('mention');
      setMentionOpen(true);
    } else {
      setMentionOpen(false);
    }
  };

  const selectCandidate = (candidate: MentionCandidate) => {
    if (mentionStart < 0) return;
    if (candidate.kind === 'command') {
      closeMentionPanel();
      void executeSlashCommand(candidate.value);
      return;
    }
    const editor = editorRef.current;
    // 替换区间是 `@` 到触发时的光标（mentionEnd）：过滤词在面板搜索框里，
    // 不在消息文本中，因此不能用当前光标位置当区间末端。
    const replaceEnd = mentionReplaceEnd(mentionStart, mentionEnd);
    const replacement = replaceMentionCompletion(
      inputContent,
      mentionStart,
      replaceEnd,
      candidate.value,
    );
    if (!replacement) return;
    setInputContent(replacement.value);
    // 选中即关闭面板并清空锚点，避免残留状态影响后续输入判定
    closeMentionPanel();
    setTimeout(() => {
      if (editor) {
        editor.focus();
        editor.setSelection(replacement.offset);
      }
    }, 0);
  };

  const addAttachments = useCallback((items: Attachment[]) => {
    if (items.length === 0) return;
    setAttachments(prev => {
      const next = [...prev];
      for (const item of items) {
        if (!next.some(existing => existing.source === item.source)) {
          next.push(item);
        }
      }
      return next;
    });
  }, [setAttachments]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void api.onSessionInputAttachment(({ attachment }) => {
      if (disposed || !cacheKey) return;
      // 文本输入项：mode=insert 写入草稿不发送（如语音转写后编辑）；否则
      // 直接发送给当前会话的 Agent（如创作页「开始创建」、语音输入，可携带
      // 音频附件）。经 store 层读写（事件回调里组件闭包可能是旧快照）。
      if (attachment.kind === 'text') {
        const content = (attachment.text ?? '').trim();
        if (!content) return;
        if (attachment.mode === 'insert') {
          const current = useStore.getState().inputCaches[cacheKey]?.text ?? '';
          const separator = current && !current.endsWith('\n') ? ' ' : '';
          setInputCacheText(cacheKey, `${current}${separator}${content}`);
          editorRef.current?.focus();
          return;
        }
        const extra: RawAttachment[] = (attachment.attachments ?? []).filter(
          (item) => item.kind === 'audio',
        );
        // 「用户普通 Enter」语义：保护草稿、运行中入队、空闲发送、
        // 信任模式用界面当前选择——是否立即引导由用户决定。
        useStore.getState().submitExternalText(cacheKey, content, trustModeRef.current, extra);
        editorRef.current?.focus();
        return;
      }
      if (attachment.kind === 'audio') {
        addAttachments([attachment as Attachment]);
        editorRef.current?.focus();
        return;
      }
      if (attachment.kind !== 'image' || attachment.mime_type !== 'image/png') return;
      addAttachments([attachment as Attachment]);
      editorRef.current?.focus();
    }).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    }).catch((error) => console.warn('监听插件输入附件失败:', error));
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [addAttachments, cacheKey, setInputCacheText]);



  const addAttachmentsFromPaths = useCallback((paths: string[]) => {
    addAttachments(paths.map(attachmentFromPath));
  }, [addAttachments]);

  useEffect(() => {
    if (!isTextDropTargetActive) {
      setIsDraggingFiles(false);
      return;
    }

    let disposed = false;
    let unlisten: (() => void) | undefined;

    getCurrentWebview().onDragDropEvent((event) => {
      const payload = event.payload as DragDropEvent;
      if (payload.type === 'leave') {
        setIsDraggingFiles(false);
        return;
      }
      if (payload.type === 'enter' || payload.type === 'over') {
        setIsDraggingFiles(true);
        return;
      }
      if (payload.type === 'drop') {
        setIsDraggingFiles(false);
        if (payload.paths.length > 0) {
          lastNativeDropAtRef.current = Date.now();
          addAttachmentsFromPaths(payload.paths);
          editorRef.current?.focus();
        }
      }
    }).then((stopListening) => {
      if (disposed) {
        stopListening();
      } else {
        unlisten = stopListening;
      }
    }).catch((err) => {
      console.error('监听文件拖放失败:', err);
    });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [addAttachmentsFromPaths, isTextDropTargetActive]);

  const filesToAttachments = async (files: File[]) => {
    const items = await Promise.all(files.map(async (file, index): Promise<Attachment> => {
      const title = file.name || `dropped-file-${index + 1}`;
      const fileWithPath = file as File & { path?: string };
      if (fileWithPath.path) {
        return attachmentFromPath(fileWithPath.path);
      }
      if (estimatedBase64Size(file.size) > MAX_ATTACHMENT_BASE64_BYTES) {
        throw new Error(`附件“${title}”超过 50MB，已停止添加。`);
      }
      const mimeType = file.type || 'application/octet-stream';
      return {
        kind: attachmentKindFromMime(mimeType),
        source: await fileToDataUrl(file),
        original_name: file.name || (
          mimeType.startsWith('image/')
            ? `dropped-image-${Date.now()}-${index + 1}.${imageExtFromMime(mimeType)}`
            : title
        ),
        mime_type: mimeType,
      };
    }));
    addAttachments(items);
  };

  const handleDragOver = (e: DragEvent<HTMLDivElement>) => {
    if (Array.from(e.dataTransfer.types).includes('Files')) {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'copy';
      setIsDraggingFiles(true);
    }
  };

  const handleDragLeave = (e: DragEvent<HTMLDivElement>) => {
    if (!inputAreaRef.current?.contains(e.relatedTarget as Node | null)) {
      setIsDraggingFiles(false);
    }
  };

  const handleDrop = async (e: DragEvent<HTMLDivElement>) => {
    const files = Array.from(e.dataTransfer.files);
    if (files.length === 0) return;
    e.preventDefault();
    setIsDraggingFiles(false);
    if (Date.now() - lastNativeDropAtRef.current < 500) return;
    try {
      await filesToAttachments(files);
      editorRef.current?.focus();
    } catch (err) {
      console.error('读取拖放文件失败:', err);
      alert(err instanceof Error ? err.message : '读取拖放文件失败');
    }
  };

  const handlePaste = async (e: ClipboardEvent<HTMLDivElement>) => {
    const files = Array.from(e.clipboardData.files);
    if (files.length > 0) {
      e.preventDefault();
      try {
        const pasted = await Promise.all(files.map(async (file, index) => {
          const mimeType = file.type || 'application/octet-stream';
          const title = file.name || (mimeType.startsWith('image/')
            ? `pasted-image-${Date.now()}-${index + 1}.${imageExtFromMime(mimeType)}`
            : `pasted-file-${Date.now()}-${index + 1}`);
          if (estimatedBase64Size(file.size) > MAX_ATTACHMENT_BASE64_BYTES) {
            throw new Error(`附件“${title}”超过 50MB，已停止添加。`);
          }
          return {
            kind: attachmentKindFromMime(mimeType),
            source: await fileToDataUrl(file),
            original_name: title,
            mime_type: mimeType,
          };
        }));
        addAttachments(pasted);
      } catch (err) {
        console.error('读取粘贴图片失败:', err);
        alert(err instanceof Error ? err.message : '读取粘贴图片失败');
      }
      return;
    }

    const fileItems = Array.from(e.clipboardData.items).filter(item => item.kind === 'file');
    if (fileItems.length > 0) {
      e.preventDefault();
      try {
        const pasted = await Promise.all(fileItems.map(async (item, index): Promise<Attachment | null> => {
          const file = item.getAsFile();
          if (!file) return null;
          const mimeType = file.type || item.type || 'application/octet-stream';
          const title = file.name || (mimeType.startsWith('image/')
            ? `pasted-image-${Date.now()}-${index + 1}.${imageExtFromMime(mimeType)}`
            : `pasted-file-${Date.now()}-${index + 1}`);
          if (estimatedBase64Size(file.size) > MAX_ATTACHMENT_BASE64_BYTES) {
            throw new Error(`附件“${title}”超过 50MB，已停止添加。`);
          }
          return {
            kind: attachmentKindFromMime(mimeType),
            source: await fileToDataUrl(file),
            original_name: title,
            mime_type: mimeType,
          };
        }));
        addAttachments(pasted.filter((item): item is Attachment => item !== null));
      } catch (err) {
        console.error('读取粘贴图片失败:', err);
        alert(err instanceof Error ? err.message : '读取粘贴图片失败');
      }
      return;
    }

    const text = e.clipboardData.getData('text/plain');
    const paths = clipboardImagePaths(text);
    const nonEmptyLines = text
      .split(/\r?\n/)
      .map(line => line.trim())
      .filter(Boolean);
    if (paths.length > 0 && paths.length === nonEmptyLines.length) {
      e.preventDefault();
      addAttachments(paths.map(path => ({
        kind: 'image',
        source: path,
        original_name: path.split(/[\\/]/).pop() || path,
        mime_type: imageMimeType(path),
      })));
    }
  };

  // 面板打开时键盘由面板内的独立搜索框处理（见 handleMentionSearchKeyDown），
  // 消息框只在焦点意外留在编辑器时兜底关闭面板。
  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (mentionOpen && completionMode === 'mention' && e.key === 'Escape') {
      e.preventDefault();
      closeMentionPanel();
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !isComposingRef.current && !e.nativeEvent.isComposing && e.keyCode !== 229) {
      e.preventDefault();
      // Cmd/Ctrl+Enter 立即投递（空闲发送、执行中引导）；执行中普通 Enter 排入队列。
      if ((e.metaKey || e.ctrlKey) || isIdle) {
        handleSend();
      } else {
        void handleEnqueue();
      }
    }
  };

  // 面板搜索框键盘：选取仅 Enter 与鼠标点击，Tab 不选取；方向键导航高亮项；
  // Esc 关闭并退出，焦点回消息框。
  const handleMentionSearchKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (completionMode !== 'mention') return;
    const count = filteredCandidates.length;
    if (e.key === 'ArrowDown' && count > 0) {
      e.preventDefault();
      setMentionIndex(i => (i + 1) % count);
      return;
    }
    if (e.key === 'ArrowUp' && count > 0) {
      e.preventDefault();
      setMentionIndex(i => (i - 1 + count) % count);
      return;
    }
    if (e.key === 'Enter' && !e.metaKey && !e.ctrlKey) {
      e.preventDefault();
      if (count > 0) {
        selectCandidate(filteredCandidates[mentionIndex]);
      } else {
        // 无候选：关闭面板、文本留作普通文字，不发送
        closeMentionPanel();
      }
      return;
    }
    if (e.key === 'Escape' || e.key === 'Tab') {
      e.preventDefault();
      closeMentionPanel();
    }
  };

  const handleSend = async () => {
    if (!canSend) return;
    setMentionOpen(false);

    // 在任何异步调用前固定目标与完整输入快照，后续切换会话不改变投递目标。
    const targetCacheKey = cacheKey;
    if (!targetCacheKey) return;
    const inputSnapshot = {
      ...inputCache,
      attachments: inputCache.attachments.map((attachment) => ({ ...attachment })),
    };

    // slash command 拦截
    const trimmed = inputSnapshot.text.trim();
    if (await executeSlashCommand(trimmed)) {
      return;
    }
    const content = inputSnapshot.text.trim()
      || (inputSnapshot.attachments.length > 0 ? '请处理这些附件。' : inputSnapshot.text);
    if (isIdle) {
      await sendMessage(
        targetCacheKey,
        content,
        inputSnapshot.attachments,
        inputSnapshot.revision,
        trustMode,
        sessionModelRef,
      );
    } else {
      // 执行中：追加消息到正在执行的 turn
      const appended = await appendMessage(
        targetCacheKey,
        content,
        inputSnapshot.attachments,
        inputSnapshot.revision,
      );
      if (!appended) {
        console.warn('当前会话没有正在执行的任务，追加消息未发送');
      }
    }
  };

  const handleCancel = () => { cancelTurn(); };

  // 执行中普通 Enter：当前输入（含附件）排入会话队列并清空草稿，等空闲后自动放行。
  const handleEnqueue = async () => {
    if (!canSend) return;
    setMentionOpen(false);
    const targetCacheKey = cacheKey;
    if (!targetCacheKey) return;
    // slash 命令不入队，直接执行（与发送行为一致）。
    if (await executeSlashCommand(inputContent.trim())) {
      return;
    }
    enqueueInputMessage(targetCacheKey);
  };

  const handleAttachFiles = async () => {
    try {
      const selected = await open({
        multiple: true,
        directory: false,
        title: '选择图片或文件',
        filters: [
          {
            name: '图片、音视频和文件',
            extensions: [
              'png', 'jpg', 'jpeg', 'webp', 'gif', 'svg',
              'mp3', 'wav', 'm4a', 'ogg', 'flac',
              'mp4', 'mov', 'webm', 'mkv',
              'pdf', 'docx', 'xlsx', 'pptx', 'txt', 'md', 'json', 'csv',
            ],
          },
        ],
      });
      const paths = Array.isArray(selected) ? selected : selected ? [selected] : [];
      if (paths.length === 0) return;
      addAttachmentsFromPaths(paths);
    } catch (e) {
      console.error('选择附件失败:', e);
    }
  };

  const removeAttachment = (source: string) => {
    setAttachments(prev => prev.filter(item => item.source !== source));
  };

  const handleChangeCwd = async () => {
    try {
      const selected = await open({ directory: true, multiple: false, defaultPath: sessionCwd || undefined, title: '选择对话目录' });
      if (selected && typeof selected === 'string') { await setSessionCwd(selected); }
    } catch (error) { console.error('选择目录失败:', error); }
  };

  const displayCwd = sessionCwd
    ? sessionCwd.split('/').filter(Boolean).slice(-2).join('/')
    : '';

  // ===== 渲染 =====
  const containerRef = useRef<HTMLDivElement>(null);
  const [compact, setCompact] = useState(false);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const observer = new ResizeObserver(() => {
      setCompact(el.clientWidth < 500);
      onHeightChange?.(el.offsetHeight);
    });
    observer.observe(el);
    setCompact(el.clientWidth < 500);
    onHeightChange?.(el.offsetHeight);
    return () => {
      observer.disconnect();
      onHeightChange?.(0);
    };
  }, [onHeightChange]);

  return (
    <div ref={containerRef} className="relative isolate border-t bg-background p-4">
      <div
        ref={interactionContentRef}
        aria-hidden={interactionVisible}
        className="max-w-3xl mx-auto"
      >
        <div>
            {/* 输入框上方：运行状态 + 思考强度 */}
            <div className="mb-1 flex items-center justify-between text-xs text-muted-foreground">
              <div className="flex items-center gap-2 min-w-0">
                {liveDurationLabel && (
                  <span className="inline-flex items-center gap-0.5 shrink-0 text-blue-500 tabular-nums" title="本轮已用时长">
                    <Clock className="w-3 h-3" />
                    {liveDurationLabel}
                  </span>
                )}
                {currentRunStatus !== 'idle' ? (
                  <span className="flex items-center gap-1 text-yellow-500 truncate" title={runSummary || '执行中'}>
                    <Circle className="w-1.5 h-1.5 animate-pulse shrink-0" />
                    <span className="truncate">{runSummary && runSummary.length > 30 ? runSummary.slice(0, 30) + '...' : (runSummary || '执行中')}</span>
                  </span>
                ) : (
                  <span className="flex items-center gap-1 text-green-500">
                    <Circle className="w-1.5 h-1.5 fill-current shrink-0" />
                    <span>就绪</span>
                  </span>
                )}
              </div>
              <div className="flex items-center gap-0.5 shrink-0">
                {/* 会话区过窄时隐藏插件拓展 UI，把空间让给左侧运行状态。 */}
                {!compact && <SessionInputPluginHost slot="session.input-status" />}
                <Select
                  value={sessionModelRef ?? MODEL_DEFAULT_VALUE}
                  onValueChange={(value) => { void handleSessionModelChange(value); }}
                  onOpenChange={(open) => { if (open) void refreshModelOptions(); }}
                  disabled={modelSelectorDisabled}
                >
                  <SelectTrigger
                    title={modelSelectorTitle}
                    className="h-6 w-auto gap-1 rounded-md border-none bg-transparent px-1.5 text-xs text-muted-foreground shadow-none transition-colors hover:bg-accent hover:text-foreground focus:outline-none focus:ring-0 focus:ring-offset-0"
                  >
                    <Cpu className="w-3 h-3 shrink-0" />
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent className="min-w-max">
                    <SelectItem value={MODEL_DEFAULT_VALUE} className="text-xs">{defaultModelLabel}</SelectItem>
                    {modelGroups.map((group) => (
                      <SelectGroup key={group.provider}>
                        <SelectLabel className="text-[11px] text-muted-foreground/70">
                          {group.provider}
                        </SelectLabel>
                        {group.options.map((option) => (
                          <SelectItem key={option.key} value={option.key} className="text-xs">
                            {option.label}
                          </SelectItem>
                        ))}
                      </SelectGroup>
                    ))}
                    {modelUnavailable && sessionModelRef != null && (
                      <SelectItem value={sessionModelRef} className="text-xs">
                        {modelDisplay}
                      </SelectItem>
                    )}
                  </SelectContent>
                </Select>
                <Select value={reasoningEffort} onValueChange={setReasoningEffort}>
                  <SelectTrigger
                    title="思考强度"
                    className="h-6 w-auto gap-1 rounded-md border-none bg-transparent px-1.5 text-xs text-muted-foreground shadow-none transition-colors hover:bg-accent hover:text-foreground focus:outline-none focus:ring-0 focus:ring-offset-0"
                  >
                    <Brain className="w-3 h-3 shrink-0" />
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent className="min-w-max">
                    <SelectItem value="none" className="text-xs">不思考</SelectItem>
                    <SelectItem value="low" className="text-xs">低强度</SelectItem>
                    <SelectItem value="medium" className="text-xs">中强度</SelectItem>
                    <SelectItem value="high" className="text-xs">高强度</SelectItem>
                    <SelectItem value="max" className="text-xs">最大强度</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>
            <SessionInputPluginHost slot="session.before-input" />
            <InputQueueBar cacheKey={cacheKey} isIdle={isIdle} />
            <div
              ref={inputAreaRef}
              className="relative"
              onDragOver={handleDragOver}
              onDragLeave={handleDragLeave}
              onDrop={handleDrop}
            >
              {/* @提及补全面板：过滤词由面板内独立搜索框承载，不复用消息输入框 */}
              {mentionOpen && (
                <div
                  ref={mentionRef}
                  className="mention-completion-menu absolute bottom-full left-0 z-50 mb-1 max-h-80 w-[min(36rem,calc(100vw-2rem))] overflow-y-auto overflow-x-hidden rounded-md border bg-popover shadow-lg"
                >
                  {/* 吸顶：面板内容可滚动，搜索框必须始终可见（否则滚到长列表
                      中部后就看不到当前过滤词，也不敢改） */}
                  {completionMode === 'mention' && (
                    <div className="sticky top-0 z-10 border-b bg-popover px-3 py-1.5">
                      <input
                        ref={mentionSearchRef}
                        type="text"
                        value={mentionFilter}
                        onChange={(e) => { setMentionFilter(e.target.value); setMentionIndex(0); }}
                        onKeyDown={handleMentionSearchKeyDown}
                        placeholder="搜索技能 / 工具 / Agent / 文件…"
                        aria-label="提及候选过滤"
                        className="w-full bg-transparent text-sm outline-none placeholder:text-muted-foreground"
                      />
                    </div>
                  )}
                  {filteredCandidates.length === 0 ? (
                    <div className="px-3 py-3 text-center text-sm text-muted-foreground">
                      无匹配 · Enter 或 Esc 关闭
                    </div>
                  ) : completionMode === 'slash' ? (
                    // slash 命令：平铺渲染。标签完整显示（不截断），
                    // 描述允许换行，并用 title 提供 hover 全文提示。
                    filteredCandidates.map((c, i) => (
                      <button
                        key={c.value}
                        ref={(el) => { candidateRefs.current[i] = el; }}
                        className={`flex w-full items-start gap-2 px-3 py-1.5 text-left text-sm transition-colors hover:bg-accent ${
                          i === mentionIndex ? 'bg-accent' : ''
                        }`}
                        onMouseDown={(e) => { e.preventDefault(); selectCandidate(c); }}
                        onMouseEnter={() => setMentionIndex(i)}
                      >
                        <Keyboard className="mt-0.5 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                        <div className="flex min-w-0 flex-1 flex-wrap items-baseline gap-x-2 gap-y-0.5">
                          <span className="max-w-full whitespace-normal break-words font-medium">
                            {c.label}
                          </span>
                          {c.hint && (
                            <span
                              className="min-w-0 flex-1 truncate text-xs leading-4 text-muted-foreground"
                              title={c.hint}
                            >
                              {truncateMiddle(c.hint)}
                            </span>
                          )}
                        </div>
                      </button>
                    ))
                  ) : (
                    // mention：按组渲染（组标题 + 组内候选）
                    (() => {
                      let flatIndex = 0;
                      return filteredGroups.map((group) => (
                        <div key={group.kind}>
                          <div className="px-3 pt-1.5 pb-0.5 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
                            {MENTION_GROUP_TITLES[group.kind] ?? group.label}
                          </div>
                          {group.candidates.map((c) => {
                            // 索引建立中的占位候选：不可选中、不参与键盘导航，
                            // 只作为提示行展示（避免把空结果误显示成"无匹配"）。
                            if (isScanningPlaceholder(c)) {
                              return (
                                <div
                                  key={`${group.kind}-scanning`}
                                  className="flex w-full items-start gap-2 px-3 py-1.5 text-left text-sm text-muted-foreground"
                                >
                                  <span
                                    className="mt-0.5 inline-flex h-4 min-w-4 shrink-0 items-center justify-center rounded-sm border text-[10px] font-semibold leading-none"
                                    aria-hidden="true"
                                  >
                                    {c.mark?.trim() || '…'}
                                  </span>
                                  <div className="flex min-w-0 flex-1 flex-wrap items-baseline gap-x-2 gap-y-0.5">
                                    <span className="max-w-full whitespace-normal break-words font-medium">
                                      {c.label}
                                    </span>
                                    {c.hint && (
                                      <span
                                        className="min-w-0 flex-1 truncate text-xs leading-4 text-muted-foreground"
                                        title={c.hint}
                                      >
                                        {truncateMiddle(c.hint)}
                                      </span>
                                    )}
                                  </div>
                                </div>
                              );
                            }
                            const i = flatIndex++;
                            return (
                              <button
                                key={c.value}
                                ref={(el) => { candidateRefs.current[i] = el; }}
                                className={`flex w-full items-start gap-2 px-3 py-1.5 text-left text-sm transition-colors hover:bg-accent ${
                                  i === mentionIndex ? 'bg-accent' : ''
                                }`}
                                onMouseDown={(e) => { e.preventDefault(); selectCandidate(c); }}
                                onMouseEnter={() => setMentionIndex(i)}
                              >
                                {/* 标记字符与气泡 chip 同源（插件提供，缺省按 kind 回退） */}
                                <span
                                  className={`mt-0.5 inline-flex h-4 min-w-4 shrink-0 items-center justify-center rounded-sm border text-[10px] font-semibold leading-none ${MENTION_KIND_BADGE_CLASS[c.kind] ?? MENTION_KIND_BADGE_CLASS.plugin}`}
                                  aria-hidden="true"
                                >
                                  {c.mark?.trim() || mentionMarkFor(c.kind, c.value)}
                                </span>
                                <div className="flex min-w-0 flex-1 flex-wrap items-baseline gap-x-2 gap-y-0.5">
                                  {/* 主体完整显示：不截断，放不下就换行——截断后
                                      用户无法确认选中的是哪一个 */}
                                  <span className="max-w-full whitespace-normal break-words font-medium">
                                    {c.label}
                                  </span>
                                  {c.kind === 'skill' && c.value.includes('@') && (
                                    <span className="whitespace-nowrap text-xs text-muted-foreground">
                                      {c.value.replace(/^@/, '')}
                                    </span>
                                  )}
                                  {c.hint && (
                                    // 头尾截断：保留主体与收尾信息，中间省略；
                                    // 单行不换行，全文靠 title 的 hover 提示读。
                                    // truncate 是兜底：主轴被 label/技能名挤占时，
                                    // 由 CSS 裁掉并补省略号，绝不允许冲出面板边界。
                                    <span
                                      className="min-w-0 flex-1 truncate text-xs leading-4 text-muted-foreground"
                                      title={c.hint}
                                    >
                                      {truncateMiddle(c.hint)}
                                    </span>
                                  )}
                                </div>
                              </button>
                            );
                          })}
                        </div>
                      ));
                    })()
                  )}
                </div>
              )}

              {/* 编辑框内容：输入覆盖层显示时整体 inert（不设定位，按钮区仍相对外层定位） */}
              <div ref={editorContentRef}>
              {attachments.length > 0 && (
                <div className="mb-2 flex flex-wrap items-center gap-2">
                  {attachments.map(item => (
                    item.kind === 'image' ? (
                      <span
                        key={(item.original_name ?? '') + item.source.slice(0, 40)}
                        className="relative inline-flex h-11 w-11 shrink-0 items-center justify-center rounded-md border bg-muted/40"
                        title={item.original_name ?? item.source}
                      >
                        <img
                          src={resolveAttachmentUrl(item.source)}
                          alt={item.original_name ?? '附件'}
                          className="h-10 w-10 rounded object-cover"
                        />
                        <button
                          type="button"
                          onClick={() => removeAttachment(item.source)}
                          className="absolute -right-1.5 -top-1.5 flex h-4 w-4 items-center justify-center rounded-full border bg-background text-muted-foreground hover:text-foreground"
                          title="移除附件"
                        >
                          <X className="h-2.5 w-2.5" />
                        </button>
                      </span>
                    ) : (
                      <span
                        key={(item.original_name ?? '') + item.source.slice(0, 40)}
                        className="relative inline-flex h-11 shrink-0 items-center gap-1.5 rounded-md border bg-muted/40 px-2.5 text-xs"
                        title={item.original_name ?? item.source}
                      >
                        <Paperclip className="h-3.5 w-3.5 shrink-0" />
                        <span className="truncate">
                          {(item.original_name ?? item.source).length > 3
                            ? (item.original_name ?? item.source).slice(0, 3) + '…'
                            : (item.original_name ?? item.source)}
                        </span>
                        <button
                          type="button"
                          onClick={() => removeAttachment(item.source)}
                          className="absolute -right-1.5 -top-1.5 flex h-4 w-4 items-center justify-center rounded-full border bg-background text-muted-foreground hover:text-foreground"
                          title="移除附件"
                        >
                          <X className="h-2.5 w-2.5" />
                        </button>
                      </span>
                    )
                  ))}
                </div>
              )}

              <MentionEditor
                ref={editorRef}
                value={inputContent}
                onChange={handleInputChange}
                onKeyDown={handleKeyDown}
                onPaste={handlePaste}
                onCompositionStart={() => { isComposingRef.current = true; }}
                onCompositionEnd={() => {
                  setTimeout(() => { isComposingRef.current = false; }, 0);
                }}
                onBlur={(e) => {
                  // 焦点移进面板（搜索框/候选按钮）不关闭；移到别处才关。
                  const next = e.relatedTarget as Node | null;
                  if (next && mentionRef.current?.contains(next)) return;
                  setTimeout(() => setMentionOpen(false), 150);
                }}
                disabled={!cacheKey}
                placeholder={
                  isIdle
                    ? '输入消息... (Enter 发送，@ 引用 Skill/MCP)'
                    : `追加指示... (Enter 排队，${MOD_KEY_LABEL} 立即引导)`
                }
                className="min-h-[60px] max-h-[200px] resize-none pr-32 bg-muted/50 focus-visible:ring-ring"
              />
              {isDraggingFiles && (
                <div className="pointer-events-none absolute inset-0 flex items-center justify-center rounded-md border border-dashed border-primary bg-background/80 text-sm text-primary">
                  松开添加文件
                </div>
              )}
              {/* 按钮区域 */}
              <div className="absolute right-2 bottom-2 flex items-center gap-1">
                <SessionInputPluginHost slot="session.input-action" />
                <Button
                  onClick={handleAttachFiles}
                  disabled={!cacheKey}
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8 rounded-md text-muted-foreground hover:text-foreground"
                  title="添加图片、音视频或文件"
                >
                  <Paperclip className="w-4 h-4" />
                </Button>
                {!isIdle && (
                  <Button
                    onClick={handleCancel}
                    size="icon"
                    variant="ghost"
                    className="h-8 w-8 rounded-md text-destructive hover:bg-destructive/10"
                    title="取消执行"
                  >
                    <Square className="w-4 h-4" />
                  </Button>
                )}
                <Button
                  onClick={handleSend}
                  disabled={!canSend}
                  size="icon"
                  className={`h-8 w-8 rounded-md ${
                    canSend
                      ? isIdle
                        ? 'bg-green-600 hover:bg-green-700 text-white'
                        : 'bg-blue-600 hover:bg-blue-700 text-white'
                      : 'bg-muted text-muted-foreground'
                  }`}
                  title={isIdle ? '发送消息' : `立即引导 (${MOD_KEY_LABEL})`}
                >
                  {isIdle ? (
                    <Send className="w-4 h-4" />
                  ) : (
                    <MessageSquarePlus className="w-4 h-4" />
                  )}
                </Button>
              </div>
              </div>
              {inputOverlay}
            </div>
            <div className="mt-1.5 flex items-center justify-between gap-2">
              <SessionInputPluginHost slot="session.after-input" />
              <div className="ml-auto flex flex-1 items-center justify-between text-xs text-muted-foreground">
              <div className="flex items-center gap-3 min-w-0">
                <button
                  onClick={handleChangeCwd}
                  disabled={!isIdle}
                  className={`flex items-center gap-1 hover:text-foreground transition-colors disabled:opacity-50 disabled:cursor-default disabled:hover:text-muted-foreground shrink-0 ${compact ? '' : 'truncate max-w-[300px]'}`}
                  title={sessionCwd || '点击设置对话目录'}
                >
                  <FolderOpen className="w-3 h-3 shrink-0" />
                  {!compact && <span className="truncate">{displayCwd || '设置对话目录'}</span>}
                </button>
              </div>
              <div className="flex items-center gap-2 shrink-0">
                {(displayTokens > 0 || totalTokens > 0) && (
                  <div
                    className="flex items-center gap-2 text-muted-foreground/60 tabular-nums"
                    title={`当前 ${displayTokens.toLocaleString()} tokens\n压缩阈值 ${compressionThreshold.toLocaleString()} tokens\n总计 ${totalTokens.toLocaleString()} tokens`}
                  >
                    {compressionThreshold > 0 && (
                      <div className="h-1.5 w-20 overflow-hidden rounded-full bg-muted">
                        <div
                          className={`h-full rounded-full transition-all ${
                            compressionProgress >= 95
                              ? 'bg-destructive'
                              : compressionProgress >= 80
                                ? 'bg-amber-500'
                                : 'bg-green-500'
                          }`}
                          style={{ width: `${compressionProgress}%` }}
                        />
                      </div>
                    )}
                    {!compact && (
                      <>
                        <span>
                          {formatTokenCount(displayTokens)}
                        </span>
                        <span>总计 {formatTokenCount(totalTokens)}</span>
                      </>
                    )}
                  </div>
                )}
                {sandboxState?.status === 'failed' && (
                  <button
                    onClick={() => setPendingSettingsTab('sandbox')}
                    className="flex items-center gap-1 text-amber-500 transition-colors hover:text-amber-400"
                    title={
                      sandboxState.failure
                        ? `沙箱程序无效：${sandboxState.failure}。插件工具暂不可用，点击前往设置修复`
                        : '沙箱程序无效，插件工具暂不可用，点击前往设置修复'
                    }
                  >
                    <AlertTriangle className="w-3 h-3" />
                    <span>沙箱无效</span>
                  </button>
                )}
                {sandboxDisabled === true && (
                  <button
                    onClick={() => setPendingSettingsTab('sandbox')}
                    className="flex items-center gap-1 text-red-500 transition-colors hover:text-red-400"
                    title="按需进程沙箱已关闭：终端、命令及其他按需 Sidecar 以完整用户权限启动，点击前往设置重新开启"
                  >
                    <Unlock className="w-3 h-3" />
                    <span>沙箱关</span>
                  </button>
                )}
                <button
                  onClick={toggleTrustMode}
                  className={`flex items-center gap-1 transition-colors ${
                    trustMode === 'supervised'
                      ? 'text-amber-500 hover:text-amber-400'
                      : 'hover:text-foreground'
                  }`}
                  title={trustMode === 'supervised' ? '监督模式（高风险操作需确认）' : '完全信任模式（自动执行）'}
                >
                  {trustMode === 'supervised' ? (
                    <><ShieldCheck className="w-3 h-3" /><span>监督</span></>
                  ) : (
                    <><ShieldOff className="w-3 h-3" /><span>信任</span></>
                  )}
                </button>
                <span>
                  {inputOverlayVisible && inputOverlayHint
                    ? inputOverlayHint
                    : isIdle
                      ? 'Enter 发送 · Shift+Enter 换行'
                      : `Enter 排队 · ${MOD_KEY_LABEL} 立即引导 · Shift+Enter 换行`}
                </span>
              </div>
              </div>
            </div>
          </div>
      </div>
    </div>
  );
}
