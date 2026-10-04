import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  type ClipboardEvent,
  type FocusEvent,
  type FormEvent,
  type KeyboardEvent,
  type MouseEvent,
} from 'react';
import {
  deleteMentionSelection,
  deactivateBlocksInRange,
  getMentionBoundaries,
  insertTextAtMentionBoundary,
  normalizePastedText,
  resolveMentionKeyAction,
  type ActiveRange,
  type MentionBoundary,
  type MentionKey,
} from '@/utils/mentionEditorModel';
import {
  MENTION_LABEL_CLASS,
  mentionChipClass,
  mentionMarkClass,
} from './MentionChip';
import { parseBlocks, type Block } from '@/utils/mentionBlocks';
import { mentionMarkFor } from '@/utils/mentionMarks';
import { cn } from '@/lib/utils';

// 零宽空格：作为 mention chip 两侧的光标哨兵。
// contenteditable=false 的原子节点无法承载光标，浏览器在两个 chip 之间、
// 或 chip 与边界之间没有可编辑文本节点时，光标无处安放、点击无效。
// 在每个 chip 前后插入一个 ZWSP 文本节点即可提供可靠的光标锚点，
// 序列化时剥离（见 stripZwsp）。这是 Slack/Notion/ProseMirror 的通用做法。
const ZWSP = '\u200b';

// execCommand('insertHTML') 的内容按 HTML 解析，粘贴文本必须先转义
function escapeHtml(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export interface MentionEditorHandle {
  /** 聚焦编辑器并把光标置于文本末尾 */
  focus(): void;
  /** 当前序列化文本（从 DOM 读取，避免依赖外部受控值时序） */
  getText(): string;
  /** 选区信息：光标相对文本起点的偏移；用于 @ 补全定位 mentionStart */
  getSelection(): { start: number; end: number } | null;
  /** 把光标设置到给定文本偏移（用于 selectCandidate 后定位） */
  setSelection(offset: number): void;
  /** DOM 根元素 */
  element: HTMLDivElement | null;
}

interface MentionEditorProps {
  value: string;
  onChange: (text: string) => void;
  onKeyDown?: (e: KeyboardEvent<HTMLDivElement>) => void;
  onPaste?: (e: ClipboardEvent<HTMLDivElement>) => void;
  onCompositionStart?: () => void;
  onCompositionEnd?: () => void;
  /** 失焦回调：带原生事件，调用方可用 relatedTarget 判断焦点去向 */
  onBlur?: (e: FocusEvent<HTMLDivElement>) => void;
  /** mention 输入态活跃区（`@` 到光标）：区内的 mention 块降级为纯文本 */
  activeRange?: ActiveRange | null;
  /** 光标偏移变化通知：供外层判定「光标移出活跃区」退出输入态 */
  onCaretChange?: (offset: number) => void;
  placeholder?: string;
  disabled?: boolean;
  className?: string;
  /** 既有自动高度逻辑：clamp 到 [minH, maxH] */
  minHeight?: number;
  maxHeight?: number;
  autoFocus?: boolean;
}

/**
 * 受控的 contenteditable 编辑器，把 @ 提及渲染成原子块。
 *
 * 关键：每个 chip 两侧插入零宽空格（ZWSP）哨兵，提供可靠的光标锚点，
 * 解决"chip 紧邻/在边界时光标无处安放、点击无效"的 contenteditable 经典问题。
 *
 * 策略（避免 React 受控 contenteditable 的光标/IME 抖动）：
 *  - 外部 value 与当前 DOM 序列化结果不一致时重建内部 DOM；
 *  - 用户输入（onInput）只读 DOM → 序列化 → onChange 上抛，不触发内部重渲染，
 *    光标与 IME 由浏览器原生维护；
 *  - 整块删除 / 跨块方向键 / 边界方向键守卫在 keydown 拦截处理。
 */
export const MentionEditor = forwardRef<MentionEditorHandle, MentionEditorProps>(
  function MentionEditor(props, ref) {
    const {
      value,
      onChange,
      onKeyDown,
      onPaste,
      onCompositionStart,
      onCompositionEnd,
      onBlur,
      activeRange,
      onCaretChange,
      placeholder,
      disabled,
      className,
      minHeight = 60,
      maxHeight = 200,
      autoFocus,
    } = props;

    const rootRef = useRef<HTMLDivElement>(null);
    const isComposingRef = useRef(false);
    const compositionSnapshotRef = useRef<{ text: string; caret: number } | null>(null);
    // 重建 DOM 后需要恢复的光标偏移（解决 selectCandidate 先改 value、
    // 后设光标的时序竞态：DOM 重建会清掉光标）。
    const pendingSelectionRef = useRef<number | null>(null);
    // 异步恢复执行前若已有更新的程序化选区，则丢弃旧恢复，避免光标被拉回。
    const selectionRevisionRef = useRef(0);
    // 上次上报给外层的光标偏移：只在真正变化时通知，避免重复渲染。
    const lastCaretRef = useRef<number | null>(null);

    // ===== DOM 构建 =====
    const renderBlocks = useCallback((blocks: Block[]) => {
      const root = rootRef.current;
      if (!root) return;
      const frag = document.createDocumentFragment();
      for (const b of blocks) {
        if (b.type === 'text') {
          frag.appendChild(document.createTextNode(b.value));
        } else {
          // chip 前哨兵：保证光标可落在 chip 之前
          frag.appendChild(document.createTextNode(ZWSP));
          const span = document.createElement('span');
          span.className = mentionChipClass(b.kind);
          span.setAttribute('contenteditable', 'false');
          span.setAttribute('data-mention-token', b.token);
          span.setAttribute('data-mention-kind', b.kind);
          span.setAttribute('title', b.token);
          span.setAttribute('aria-label', b.token);
          const mark = mentionMarkFor(b.kind, b.token);
          if (mark) {
            const icon = document.createElement('span');
            icon.className = mentionMarkClass(b.kind);
            icon.textContent = mark;
            icon.setAttribute('aria-hidden', 'true');
            span.appendChild(icon);
          }
          const label = document.createElement('span');
          label.className = MENTION_LABEL_CLASS;
          label.textContent = b.label;
          span.appendChild(label);
          frag.appendChild(span);
          // chip 后哨兵：保证光标可落在 chip 之后
          frag.appendChild(document.createTextNode(ZWSP));
        }
      }
      root.replaceChildren(frag);
    }, []);

    // 仅在外部 value 与当前 DOM 序列化结果不一致时重建。
    // 用户输入时 commit() 已上抛新文本，父组件回填的 value 必然与 DOM 一致，
    // 因此 current === value 这道比较即可挡住重渲染，光标与 IME 由浏览器原生维护。
    // 额外：value 为空但 DOM 仍有残留（如删除最后一个 chip 后的孤立哨兵）也需清空，
    // 否则 :empty 占位符失效。
    // 重建后若存在待恢复光标（selectCandidate 触发），落到目标偏移。
    useEffect(() => {
      const root = rootRef.current;
      if (!root) return;
      const current = domToText(root);
      const needsRebuild = current !== value
        || (value === '' && root.childNodes.length > 0)
        || !existingMentionsMatchValue(root, value, activeRange);
      const selectionBeforeRebuild = needsRebuild && document.activeElement === root
        ? currentSelectionOffsets()?.start ?? null
        : null;
      if (needsRebuild) {
        renderBlocks(deactivateBlocksInRange(parseBlocks(value), activeRange));
      }
      const pending = pendingSelectionRef.current;
      if (pending != null) {
        pendingSelectionRef.current = null;
      }
      const selectionToRestore = pending ?? selectionBeforeRebuild;
      if (selectionToRestore != null) {
        const selectionRevision = selectionRevisionRef.current;
        requestAnimationFrame(() => {
          if (selectionRevisionRef.current !== selectionRevision) return;
          root.focus();
          const target = resolveOffset(selectionToRestore);
          if (target) {
            const range = document.createRange();
            range.setStart(target.node, target.offset);
            range.collapse(true);
            applyRange(range);
          }
        });
      }
    }, [value, activeRange, renderBlocks]);

    // 手工输入合法 @ 文本时仍保持普通文字；只有 DOM 已经存在标签时，才校验
    // 标签 token 是否仍与当前文本解析结果一致。
    // 期望集合按活跃区降级后计算：活跃区内的 mention 本就是纯文本，不能因为
    // 「解析出 mention 但 DOM 没有 chip」而反复重建。
    function existingMentionsMatchValue(
      root: HTMLElement,
      text: string,
      range: ActiveRange | null | undefined,
    ): boolean {
      const actual = Array.from(root.querySelectorAll<HTMLElement>('.mention-chip'))
        .map((chip) => chip.dataset.mentionToken ?? '');
      if (actual.length === 0) return true;
      const expected = deactivateBlocksInRange(parseBlocks(text), range)
        .filter((block) => block.type === 'mention')
        .map((block) => block.token);
      return actual.length === expected.length
        && actual.every((token, index) => token === expected[index]);
    }

    // 高度自适应
    useEffect(() => {
      const root = rootRef.current;
      if (!root) return;
      root.style.height = `${minHeight}px`;
      root.style.height = `${Math.min(root.scrollHeight, maxHeight)}px`;
    }, [value, minHeight, maxHeight]);

    // ===== DOM <-> 文本 =====
    // 序列化时剥离哨兵 ZWSP，保证 round-trip 等价
    function stripZwsp(s: string): string {
      return s.split(ZWSP).join('');
    }

    function domToText(root: HTMLElement): string {
      // contenteditable 在不同内核中可能用 CR、LF 或 BR 表示换行；对齐 textarea
      // 的 value 语义，统一向上层提交 LF。
      return stripZwsp(rawDomText(root)).replace(/\r\n?/g, '\n');
    }

    function rawDomText(root: HTMLElement): string {
      if (
        root.childNodes.length === 1
        && root.firstChild?.nodeType === Node.ELEMENT_NODE
        && (root.firstChild as HTMLElement).tagName === 'BR'
      ) return '';

      let text = '';
      root.childNodes.forEach((node) => {
        if (node.nodeType === Node.TEXT_NODE) {
          text += node.nodeValue ?? '';
        } else if (node.nodeType === Node.ELEMENT_NODE) {
          const el = node as HTMLElement;
          if (el.classList.contains('mention-chip')) {
            text += el.getAttribute('data-mention-token') ?? '';
          } else if (el.tagName === 'BR') {
            text += '\n';
          } else {
            text += rawDomText(el);
          }
        }
      });
      return text;
    }

    const commit = useCallback(() => {
      const root = rootRef.current;
      if (!root) return;
      onChange(domToText(root));
    }, [onChange]);

    // ===== 光标/选区相对文本偏移 =====
    // 收集叶子段（文本节点/哨兵/chip/br），按文档顺序，供偏移换算复用。
    interface Seg {
      node: Node;
      // 该段贡献到"纯文本（无 ZWSP）"的字符长度
      len: number;
      // 该段在"原始 DOM 文本（含 ZWSP）"里的长度，用于把光标端点（DOM 偏移）换算
      rawLen: number;
      // text=普通文本；sentinel=chip 两侧的 ZWSP 哨兵（纯文本贡献 0）；chip；br
      kind: 'text' | 'sentinel' | 'chip' | 'br';
      token?: string;
    }

    function collectSegments(root: HTMLElement): Seg[] {
      const segs: Seg[] = [];
      const walk = (node: Node) => {
        if (node.nodeType === Node.TEXT_NODE) {
          const value = node.nodeValue ?? '';
          // 纯 ZWSP 哨兵节点：对纯文本贡献 0，但是 chip 两侧的光标锚点
          if (value.split(ZWSP).join('') === '') {
            segs.push({ node, len: 0, rawLen: value.length, kind: 'sentinel' });
          } else {
            const len = stripZwsp(value).length;
            segs.push({ node, len, rawLen: value.length, kind: 'text' });
          }
          return;
        }
        if (node.nodeType !== Node.ELEMENT_NODE) return;
        const el = node as HTMLElement;
        if (el.classList.contains('mention-chip')) {
          const token = el.getAttribute('data-mention-token') ?? '';
          segs.push({ node: el, len: token.length, rawLen: 0, kind: 'chip', token });
          return;
        }
        if (el.tagName === 'BR') {
          segs.push({ node: el, len: 1, rawLen: 0, kind: 'br' });
          return;
        }
        el.childNodes.forEach(walk);
      };
      root.childNodes.forEach(walk);
      return segs;
    }

    // DOM 端点（container + offset）→ 相对纯文本起点的字符偏移
    function offsetFromStart(container: Node, offset: number): number {
      const root = rootRef.current!;
      const segs = collectSegments(root);

      // container 是文本节点：找到它，累加前面段长度，再加 offset（但跳过其内部的 ZWSP）
      if (container.nodeType === Node.TEXT_NODE) {
        let total = 0;
        for (const seg of segs) {
          if (seg.node === container) {
            const value = container.nodeValue ?? '';
            // offset 是 DOM 文本偏移（含 ZWSP），换算成纯文本偏移
            const before = value.slice(0, offset);
            total += stripZwsp(before).length;
            return total;
          }
          total += seg.len;
        }
        return total;
      }

      // container 是元素：offset 指向其第 offset 个子节点之前
      // 找到 offset 指向的子节点（或末尾）在 segs 中的累积位置
      if (container.nodeType === Node.ELEMENT_NODE || container === root) {
        const el = container as HTMLElement;
        const child = el.childNodes[offset];
        if (!child) {
          // 指向末尾：累加所有属于 container 子树的段
          return sumSegmentsUnder(segs, el, true);
        }
        return offsetOfNode(segs, child);
      }
      return 0;
    }

    function offsetOfNode(segs: Seg[], target: Node): number {
      let total = 0;
      for (const seg of segs) {
        if (seg.node === target) return total;
        total += seg.len;
      }
      return total;
    }

    // 累加落在 parent 子树内、且（end=true 时全部 / end=false 时到边界）的段
    function sumSegmentsUnder(segs: Seg[], parent: Node, end: boolean): number {
      let total = 0;
      for (const seg of segs) {
        if (parent.contains(seg.node)) total += seg.len;
        if (!end) break;
      }
      return total;
    }

    // 纯文本偏移 → DOM 端点。优先落在文本节点内；落在 chip 处时定位到 chip 前的哨兵。
    function resolveOffset(offset: number): { node: Node; offset: number } | null {
      const root = rootRef.current!;
      const segs = collectSegments(root);
      let remaining = offset;
      for (let i = 0; i < segs.length; i++) {
        const seg = segs[i];
        if (seg.kind === 'chip') {
          if (remaining < seg.len) {
            // 落在 chip 内部：定位到 chip 之前的哨兵文本节点末尾
            const sentinel = segs[i - 1];
            if (sentinel && (sentinel.kind === 'sentinel' || sentinel.kind === 'text')) {
              return { node: sentinel.node, offset: (sentinel.node.nodeValue ?? '').length };
            }
            return { node: root, offset: indexOf(seg.node) };
          }
          remaining -= seg.len;
          continue;
        }
        if (seg.kind === 'text') {
          const value = seg.node.nodeValue ?? '';
          const pure = stripZwsp(value);
          if (remaining <= pure.length) {
            // 把纯文本偏移换算回 DOM 文本偏移（含 ZWSP）
            let domOffset = 0;
            let pureCount = 0;
            for (const ch of value) {
              if (pureCount >= remaining) break;
              domOffset += 1;
              if (ch !== ZWSP) pureCount += 1;
            }
            return { node: seg.node, offset: domOffset };
          }
          remaining -= pure.length;
          continue;
        }
        if (seg.kind === 'sentinel') {
          // 哨兵对纯文本贡献 0：remaining<=0 时可在此定位
          if (remaining <= 0) {
            return { node: seg.node, offset: 0 };
          }
          continue;
        }
        // br
        if (remaining <= 0) {
          return { node: seg.node.parentNode!, offset: indexOf(seg.node) };
        }
        remaining -= 1;
      }
      // 落到末尾：选最后一个文本/哨兵节点末尾，否则 root 末尾
      for (let i = segs.length - 1; i >= 0; i--) {
        if (segs[i].kind === 'text' || segs[i].kind === 'sentinel') {
          return { node: segs[i].node, offset: (segs[i].node.nodeValue ?? '').length };
        }
      }
      return { node: root, offset: root.childNodes.length };
    }

    const indexOf = (node: Node) => {
      const parent = node.parentNode;
      if (!parent) return 0;
      let i = 0;
      for (const child of Array.from(parent.childNodes)) {
        if (child === node) return i;
        i += 1;
      }
      return 0;
    };

    // ===== 整块删除 / 跨块方向键 / 边界守卫 =====
    interface DomMentionBoundary extends MentionBoundary {
      chip: HTMLElement;
    }

    // 以序列化文本为准计算 mention 边界，避免浏览器合并空格与 ZWSP 后 DOM 邻接失真。
    function mentionBoundaries(): DomMentionBoundary[] {
      const root = rootRef.current;
      if (!root) return [];
      const text = domToText(root);
      const chips = Array.from(root.querySelectorAll<HTMLElement>('.mention-chip'));
      return getMentionBoundaries(text)
        .slice(0, chips.length)
        .map((boundary, index) => ({ ...boundary, chip: chips[index] }));
    }

    function boundaryForChip(chip: HTMLElement): MentionBoundary | null {
      return mentionBoundaries().find((boundary) => boundary.chip === chip) ?? null;
    }

    const isChip = (node: Node): boolean =>
      node.nodeType === Node.ELEMENT_NODE
      && (node as HTMLElement).classList.contains('mention-chip');

    // 文本节点段（普通文本或哨兵）都可作为光标锚点
    const isTextLike = (seg: Seg | undefined): seg is Seg =>
      !!seg && (seg.kind === 'text' || seg.kind === 'sentinel');

    // 光标是否落在 mention 输入态活跃区内（含端点：刚打出最后一个字符时
    // 光标就停在 end 上）。
    function isCaretInActiveRange(offsets: { start: number; end: number } | null): boolean {
      if (!activeRange || !offsets) return false;
      return offsets.start >= activeRange.start && offsets.start <= activeRange.end;
    }

    // 光标偏移变化时通知外层（仅报折叠光标，且只在真正变化时报）。
    const reportCaret = useCallback(() => {
      const offsets = currentSelectionOffsets();
      if (!offsets || offsets.start !== offsets.end) return;
      if (lastCaretRef.current === offsets.start) return;
      lastCaretRef.current = offsets.start;
      onCaretChange?.(offsets.start);
    }, [onCaretChange]);

    const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
      onKeyDown?.(e);
      if (e.defaultPrevented) return;
      if (disabled) return;

      const sel = window.getSelection();
      const hasSelection = !!sel && sel.rangeCount > 0 && !sel.getRangeAt(0).collapsed;
      const offsets = currentSelectionOffsets();

      // 光标落在活跃区内：该段文本正在作为过滤词编辑，一律不走 chip 边界守卫。
      // 守卫基于文本解析，会把 `@ab` 这类未确认片段当成已固化提及处理（补分隔
      // 空格、拦截删除、方向键跨块跳跃），直接打断输入态。
      if (isCaretInActiveRange(offsets)) {
        reportCaret();
        return;
      }

      if (
        !hasSelection
        && offsets
        && hasChip()
        && !isComposingRef.current
        && !e.metaKey
        && !e.ctrlKey
        && !e.altKey
        && e.key.length === 1
        && !/\s/.test(e.key)
      ) {
        const replacement = insertTextAtMentionBoundary(
          domToText(rootRef.current!),
          offsets.start,
          e.key,
        );
        if (replacement) {
          e.preventDefault();
          applyValue(replacement.value, replacement.offset);
          return;
        }
      }

      if (
        hasSelection
        && offsets
        && hasChip()
        && (e.key === 'Backspace' || e.key === 'Delete')
      ) {
        e.preventDefault();
        const text = domToText(rootRef.current!);
        const replacement = deleteMentionSelection(text, offsets.start, offsets.end);
        applyValue(replacement.value, replacement.offset);
        return;
      }

      if (
        !hasSelection
        && offsets
        && hasChip()
        && isMentionKey(e.key)
      ) {
        const text = domToText(rootRef.current!);
        const action = resolveMentionKeyAction(text, offsets.start, e.key);
        if (action) {
          e.preventDefault();
          if (action.type === 'move') {
            placeCaretAtOffset(action.offset);
          } else {
            applyValue(
              text.slice(0, action.start) + text.slice(action.end),
              action.offset,
            );
          }
          return;
        }
      }

      // WKWebView 方向键边界守卫（contenteditable 版，复刻 textarea 守卫语义）
      if (!hasSelection && isComposingRef.current === false) {
        const text = domToText(rootRef.current!);
        const offsets = currentSelectionOffsets();
        if (offsets && shouldPreventArrow(text, offsets.start, e.key)) {
          e.preventDefault();
        }
      }

      reportCaret();
    };

    function applyValue(text: string, offset: number) {
      const root = rootRef.current;
      if (!root) return;
      root.focus();
      selectionRevisionRef.current += 1;
      pendingSelectionRef.current = offset;
      onChange(text);
    }

    function isMentionKey(key: string): key is MentionKey {
      return key === 'Backspace'
        || key === 'Delete'
        || key === 'ArrowLeft'
        || key === 'ArrowRight';
    }

    function shouldPreventArrow(text: string, pos: number, key: string): boolean {
      switch (key) {
        case 'ArrowLeft':
          return pos === 0;
        case 'ArrowRight':
          return pos === text.length;
        case 'ArrowUp':
          return !text.slice(0, pos).includes('\n');
        case 'ArrowDown':
          return !text.slice(pos).includes('\n');
        default:
          return false;
      }
    }

    function currentSelectionOffsets(): { start: number; end: number } | null {
      const sel = window.getSelection();
      if (!sel || sel.rangeCount === 0) return null;
      const range = sel.getRangeAt(0);
      return {
        start: offsetFromStart(range.startContainer, range.startOffset),
        end: offsetFromStart(range.endContainer, range.endOffset),
      };
    }

    function placeBefore(chip: HTMLElement) {
      const boundary = boundaryForChip(chip);
      if (!boundary) return;
      placeCaretAtOffset(boundary.leadingSeparatorStart ?? boundary.start);
    }
    function placeAfter(chip: HTMLElement) {
      const boundary = boundaryForChip(chip);
      if (!boundary) return;
      placeCaretAtOffset(boundary.trailingSeparatorEnd ?? boundary.end);
    }
    function placeCaretAtOffset(offset: number) {
      const root = rootRef.current;
      if (!root) return;
      root.focus();
      const target = resolveOffset(offset);
      if (target) {
        const range = document.createRange();
        range.setStart(target.node, target.offset);
        range.collapse(true);
        applyRange(range);
      }
    }
    function applyRange(range: Range) {
      const sel = window.getSelection();
      sel?.removeAllRanges();
      sel?.addRange(range);
    }

    function restoreCaretEnd() {
      const root = rootRef.current!;
      root.focus();
      // 定位到最后一个文本节点末尾（哨兵优先），避免 selectNodeContents 把光标
      // 落进不可编辑的 chip 内部
      const segs = collectSegments(root);
      for (let i = segs.length - 1; i >= 0; i--) {
        if (isTextLike(segs[i])) {
          const range = document.createRange();
          range.setStart(segs[i].node, (segs[i].node.nodeValue ?? '').length);
          range.collapse(true);
          applyRange(range);
          return;
        }
      }
      const range = document.createRange();
      range.selectNodeContents(root);
      range.collapse(false);
      applyRange(range);
    }

    const commitInput = () => {
      // 仅当存在 chip 时才检查哨兵完整性（避免每次普通输入都跑 normalize 破坏光标）
      if (hasChip()) {
        normalizeSentinels();
        normalizeMentionBoundaries();
      }
      commit();
    };

    const handleInput = () => {
      if (isComposingRef.current) return;
      commitInput();
    };

    // 浏览器可能把“紧贴 chip 左侧”的文字直接写进 contenteditable=false 节点。
    // 在写入前手工插入文字与分隔，保持 chip 原子结构和后续连续输入位置。
    const handleBeforeInput = (e: FormEvent<HTMLDivElement>) => {
      if (disabled || isComposingRef.current) return;
      const inputEvent = e.nativeEvent as InputEvent;
      const insertedText = inputEvent.data;
      if (
        (inputEvent.inputType && inputEvent.inputType !== 'insertText')
        || !insertedText
        || /^\s+$/.test(insertedText)
      ) return;

      const selection = currentSelectionOffsets();
      if (!selection || selection.start !== selection.end) return;
      // 无 chip 时不存在边界问题；光标落在活跃区内时该段文本正在作为过滤词
      // 编辑，两者都不走边界插入（否则会把 `@ab` 拆成 `@ ab`）。
      if (!hasChip() || isCaretInActiveRange(selection)) return;
      const replacement = insertTextAtMentionBoundary(
        domToText(rootRef.current!),
        selection.start,
        insertedText,
      );
      if (!replacement) return;

      e.preventDefault();
      applyValue(replacement.value, replacement.offset);
    };

    const hasChip = (): boolean => {
      const root = rootRef.current;
      return !!root && !!root.querySelector('.mention-chip');
    };

    // 归一化：为缺少哨兵的 chip 补哨兵。仅在 chip 存在且哨兵缺失时修改 DOM，
    // 避免无谓的 normalize() 破坏光标。
    function normalizeSentinels() {
      const root = rootRef.current;
      if (!root) return;
      let changed = false;
      const childNodes = Array.from(root.childNodes);
      for (let i = 0; i < childNodes.length; i++) {
        const node = childNodes[i];
        if (node.nodeType === Node.ELEMENT_NODE && isChip(node)) {
          const prev = childNodes[i - 1];
          const next = childNodes[i + 1];
          if (!prev || prev.nodeType !== Node.TEXT_NODE) {
            node.parentNode?.insertBefore(document.createTextNode(ZWSP), node);
            changed = true;
          }
          if (!next || next.nodeType !== Node.TEXT_NODE) {
            node.parentNode?.insertBefore(document.createTextNode(ZWSP), node.nextSibling);
            changed = true;
          }
        }
      }
      return changed;
    }

    // 浏览器原生输入或粘贴可能紧贴 chip 落字；补齐两侧分隔，避免标签被当成普通文本。
    function normalizeMentionBoundaries() {
      const root = rootRef.current;
      if (!root) return;

      for (const chip of Array.from(root.querySelectorAll<HTMLElement>('.mention-chip'))) {
        let previous = chip.previousSibling;
        while (previous) {
          if (previous.nodeType === Node.TEXT_NODE) {
            const visibleText = stripZwsp(previous.nodeValue ?? '');
            if (visibleText.length === 0) {
              previous = previous.previousSibling;
              continue;
            }
            if (!/\s$/.test(visibleText)) {
              chip.parentNode?.insertBefore(document.createTextNode(' '), chip);
            }
          }
          break;
        }

        let next = chip.nextSibling;
        while (next) {
          if (next.nodeType === Node.TEXT_NODE) {
            const visibleText = stripZwsp(next.nodeValue ?? '');
            if (visibleText.length === 0) {
              next = next.nextSibling;
              continue;
            }
            if (!/^\s/.test(visibleText)) {
              chip.parentNode?.insertBefore(document.createTextNode(' '), chip.nextSibling);
            }
          }
          break;
        }
      }
    }

    const handlePaste = (e: ClipboardEvent<HTMLDivElement>) => {
      if (onPaste) {
        onPaste(e);
        if (e.defaultPrevented) return;
      }
      e.preventDefault();
      insertPlainText(e.clipboardData.getData('text/plain'));
    };

    // 仅插入文本与换行节点，避免富文本样式污染，同时保留多行粘贴语义。
    // 优先走 execCommand('insertHTML')：Range.insertNode 这类脚本 DOM 变更
    // 不进入浏览器原生撤销栈，Ctrl+Z 撤不掉粘贴内容、只能撤销键盘输入；
    // execCommand 的事务会入栈，且 undo/redo 以 historyUndo/historyRedo
    // 触发 input 事件，外部受控 value 能随之同步。
    function insertPlainText(text: string) {
      const root = rootRef.current;
      const selection = window.getSelection();
      if (!root || !selection) return;

      const lines = normalizePastedText(text).split('\n');
      if (lines.length === 1 && lines[0] === '') return;

      if (selection.rangeCount === 0
        || !root.contains(selection.getRangeAt(0).commonAncestorContainer)) {
        restoreCaretEnd();
      }
      root.focus();

      let inserted = false;
      try {
        inserted = document.execCommand(
          'insertHTML',
          false,
          lines.map((line) => escapeHtml(line)).join('<br>'),
        );
      } catch {
        inserted = false;
      }
      if (inserted) {
        if (hasChip()) {
          normalizeSentinels();
          normalizeMentionBoundaries();
        }
        commit();
        return;
      }

      // WebView 不支持 execCommand 时退回直接 DOM 插入（撤销栈不可用，行为同旧实现）
      const range = selection.rangeCount > 0
        ? selection.getRangeAt(0)
        : document.createRange();
      if (selection.rangeCount === 0 || !root.contains(range.commonAncestorContainer)) {
        range.selectNodeContents(root);
        range.collapse(false);
      }
      range.deleteContents();

      const fragment = document.createDocumentFragment();
      lines.forEach((line, index) => {
        if (index > 0) fragment.appendChild(document.createElement('br'));
        if (line) fragment.appendChild(document.createTextNode(line));
      });
      const lastInsertedNode = fragment.lastChild;
      range.insertNode(fragment);

      if (lastInsertedNode) {
        range.setStartAfter(lastInsertedNode);
      }
      range.collapse(true);
      applyRange(range);
      if (hasChip()) {
        normalizeSentinels();
        normalizeMentionBoundaries();
      }
      commit();
    }

    function separatorClientRect(chip: HTMLElement, after: boolean): DOMRect | null {
      let node: ChildNode | null = after ? chip.nextSibling : chip.previousSibling;
      while (node) {
        if (node.nodeType === Node.TEXT_NODE) {
          const value = node.nodeValue ?? '';
          const indexes = after
            ? Array.from(value, (_, index) => index)
            : Array.from(value, (_, index) => value.length - index - 1);
          for (const index of indexes) {
            if (value[index] === ZWSP) continue;
            if (value[index] !== ' ') return null;
            const range = document.createRange();
            range.setStart(node, index);
            range.setEnd(node, index + 1);
            if (typeof range.getBoundingClientRect !== 'function') return null;
            return range.getBoundingClientRect();
          }
        } else if (node.nodeType === Node.ELEMENT_NODE) {
          return null;
        }
        node = after ? node.nextSibling : node.previousSibling;
      }
      return null;
    }

    function pointInRect(x: number, y: number, rect: DOMRect): boolean {
      return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
    }

    // 点击定位：chip 与相邻分隔之间不是合法停靠点，统一吸附到原子块外侧。
    const handleMouseUp = (e: MouseEvent<HTMLDivElement>) => {
      const sel = window.getSelection();
      if (!sel || sel.rangeCount === 0) return;
      const range = sel.getRangeAt(0);
      if (!range.collapsed) return;
      // 若光标在 chip 内部，向上找 chip
      let node: Node | null = range.startContainer;
      while (node && node !== rootRef.current) {
        if (node.nodeType === Node.ELEMENT_NODE && isChip(node)) {
          // 夹到该 chip 的前哨兵
          placeBefore(node as HTMLElement);
          return;
        }
        node = node.parentNode;
      }

      const boundaries = mentionBoundaries();
      for (let index = 0; index < boundaries.length; index += 1) {
        const boundary = boundaries[index];
        if (boundary.trailingSeparatorEnd != null) {
          const rect = separatorClientRect(boundary.chip, true);
          if (rect && pointInRect(e.clientX, e.clientY, rect)) {
            const nextBoundary = boundaries[index + 1];
            const separatorIsShared = nextBoundary?.leadingSeparatorStart === boundary.end;
            if (separatorIsShared && e.clientX > (rect.left + rect.right) / 2) {
              placeBefore(nextBoundary.chip);
            } else {
              placeAfter(boundary.chip);
            }
            return;
          }
        }
        if (boundary.leadingSeparatorStart != null) {
          const rect = separatorClientRect(boundary.chip, false);
          if (rect && pointInRect(e.clientX, e.clientY, rect)) {
            placeBefore(boundary.chip);
            return;
          }
        }
      }

      const selection = currentSelectionOffsets();
      if (!selection || selection.start !== selection.end) return;
      const caret = selection.start;
      for (const boundary of boundaries) {
        if (boundary.trailingSeparatorEnd != null && caret === boundary.end) {
          placeAfter(boundary.chip);
          return;
        }
        if (boundary.leadingSeparatorStart != null && caret === boundary.start) {
          placeBefore(boundary.chip);
          return;
        }
      }
    };

    useImperativeHandle(ref, () => ({
      focus: () => {
        const root = rootRef.current;
        if (!root) return;
        root.focus();
        restoreCaretEnd();
      },
      getText: () => {
        const root = rootRef.current;
        return root ? domToText(root) : '';
      },
      getSelection: () => currentSelectionOffsets(),
      setSelection: (offset: number) => {
        const root = rootRef.current;
        if (!root) return;
        selectionRevisionRef.current += 1;
        // 同时登记 pending，应对紧接着的 DOM 重建清掉光标
        pendingSelectionRef.current = offset;
        root.focus();
        const target = resolveOffset(offset);
        if (target) {
          const range = document.createRange();
          range.setStart(target.node, target.offset);
          range.collapse(true);
          applyRange(range);
        }
        // setSelection 通常发生在 value 已重建之后。若本帧没有重建消费 pending，
        // 及时清理，避免下一次普通输入又把光标拉回旧位置。
        requestAnimationFrame(() => {
          if (pendingSelectionRef.current === offset) {
            pendingSelectionRef.current = null;
          }
        });
      },
      element: rootRef.current,
    }), []);

    // autoFocus
    useEffect(() => {
      if (autoFocus) {
        const root = rootRef.current;
        if (root) {
          root.focus();
          restoreCaretEnd();
        }
      }
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    return (
      <div
        ref={rootRef}
        className={cn(
          'mention-editor block w-full cursor-text overflow-x-hidden overflow-y-auto rounded-md border border-input bg-transparent px-3 py-2 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring aria-disabled:cursor-not-allowed aria-disabled:opacity-50',
          className,
        )}
        role="textbox"
        aria-multiline="true"
        aria-disabled={disabled}
        aria-placeholder={placeholder}
        data-placeholder={placeholder}
        contentEditable={!disabled}
        suppressContentEditableWarning
        spellCheck={false}
        onBeforeInput={handleBeforeInput}
        onInput={handleInput}
        onKeyDown={handleKeyDown}
        onPaste={handlePaste}
        onMouseUp={(event) => {
          handleMouseUp(event);
          // 点击后光标可能被移到活跃区外：上报供外层退出输入态
          reportCaret();
        }}
        onCompositionStart={() => {
          isComposingRef.current = true;
          const root = rootRef.current;
          const selection = currentSelectionOffsets();
          compositionSnapshotRef.current = root
            && selection
            && selection.start === selection.end
            ? { text: domToText(root), caret: selection.start }
            : null;
          onCompositionStart?.();
        }}
        onCompositionEnd={(event) => {
          isComposingRef.current = false;
          const snapshot = compositionSnapshotRef.current;
          const composedText = event.data;
          compositionSnapshotRef.current = null;
          // 合成结束：把合成结果提交
          // 部分浏览器 compositionEnd 在 DOM 更新前触发，下一帧提交
          requestAnimationFrame(() => {
            const replacement = snapshot
              ? insertTextAtMentionBoundary(snapshot.text, snapshot.caret, composedText)
              : null;
            if (replacement) {
              applyValue(replacement.value, replacement.offset);
            } else {
              commitInput();
            }
          });
          onCompositionEnd?.();
        }}
        onBlur={onBlur}
      />
    );
  },
);
