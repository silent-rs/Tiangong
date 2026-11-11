/**
 * 中枢对话输入区（设计稿 §4.4「你想让我做什么？」）。
 *
 * 中枢助手的轻量专属输入：多行文本 + 发送/停止，不复用普通工作会话的
 * MessageInput（无模型选择 / 目录 / 信任模式 / 附件 / 队列 / 提及——中枢
 * 对话是控制面交互，不是工作区会话）。草稿与发送复用 store 的输入缓存
 * 与 sendMessage 链路（revision / 冻结校验语义一致）。
 *
 * 征询卡片（interaction 插件）由 CentralModeView 挂在输入区上方，
 * 出现时覆盖并锁定本区（§4.4：回答或关闭后恢复）。
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowUp, Square } from 'lucide-react';
import { useStore } from '@/store/useStore';
import { api } from '@/api/tauri';
import { CENTRAL_SESSION_ID } from '@/lib/central/centralSession';

const MAX_INPUT_HEIGHT = 200;

export interface CentralChatInputProps {
  onHeightChange?: (height: number) => void;
}

export function CentralChatInput({ onHeightChange }: CentralChatInputProps) {
  const inputCache = useStore((state) => state.inputCaches[CENTRAL_SESSION_ID]);
  const setInputCacheText = useStore((state) => state.setInputCacheText);
  const sendMessage = useStore((state) => state.sendMessage);
  const appendMessage = useStore((state) => state.appendMessage);
  // 「执行中」判定与会话运行状态对齐（is_sending 只覆盖发送事务，投递成功
  // 即清除——不能用它判定 turn 执行中，否则执行中续发会被当作新开轮而被
  // 后端拒绝）。中枢激活时本会话即当前会话，runStatus 就是中枢运行状态。
  const runStatus = useStore((state) => state.runStatus);
  const isExecuting = runStatus === 'executing';
  const isSending = isExecuting || inputCache?.is_sending === true;
  const [sending, setSending] = useState(false);
  const [sendError, setSendError] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // 高度自适应：内容变化时重算（上限 MAX_INPUT_HEIGHT，超出滚动）
  const resize = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, MAX_INPUT_HEIGHT)}px`;
  }, []);

  useEffect(() => {
    resize();
    onHeightChange?.(textareaRef.current?.offsetHeight ?? 0);
  }, [inputCache?.text, resize, onHeightChange]);

  const handleSend = useCallback(async () => {
    // 中枢对话无限接收用户消息：执行状态不阻止发送（执行中走 Core
    // 原生引导，空闲新开轮）；保留本地发送事务防抖（sending）——防止
    // 双击/连按 Enter 把同一条消息重复投递（征询锁定不存在：request_user 为后续片）。竞态窗口内双向回落，消息一定落地：
    // - append 时后端 turn 已结束 → 回落 sendMessage 新开轮
    // - sendMessage 时 turn 已在后端开始（runStatus 尚未更新）→ 回落 append
    if (sending) return;
    const text = (inputCache?.text ?? '').trim();
    if (!text) return;
    const revision = inputCache?.revision ?? 0;
    setSending(true);
    setSendError(null);
    let delivered = false;
    try {
      if (isExecuting) {
        delivered = (await appendMessage(CENTRAL_SESSION_ID, text, [], revision)) === true;
        if (!delivered) {
          delivered =
            (await sendMessage(
              CENTRAL_SESSION_ID,
              text,
              [],
              revision,
              undefined,
              undefined,
            )) === true;
        }
      } else {
        delivered =
          (await sendMessage(
            CENTRAL_SESSION_ID,
            text,
            [],
            revision,
            undefined,
            undefined,
          )) === true;
        if (!delivered && useStore.getState().runStatus === 'executing') {
          delivered =
            (await appendMessage(CENTRAL_SESSION_ID, text, [], revision)) === true;
        }
      }
      if (delivered) {
        setInputCacheText(CENTRAL_SESSION_ID, '');
      } else {
        setSendError('发送失败，请重试（详情见控制台）');
      }
    } catch (error) {
      console.error('中枢消息发送失败:', error);
      setSendError('发送失败，请重试');
    } finally {
      setSending(false);
    }
  }, [
    appendMessage,
    inputCache?.text,
    inputCache?.revision,
    isExecuting,
    sending,
    sendMessage,
    setInputCacheText,
  ]);

  const handleCancel = useCallback(() => {
    api.cancelTurn(CENTRAL_SESSION_ID).catch((error) => console.error('取消失败:', error));
  }, []);

  const handleKeyDown = (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      void handleSend();
    }
  };

  return (
    <div className="border-t border-border/60 px-4 pb-4 pt-3">
      <div className="mx-auto flex max-w-3xl items-end gap-2 rounded-2xl border border-border bg-background p-2 shadow-sm focus-within:border-ring">
        <textarea
          ref={textareaRef}
          value={inputCache?.text ?? ''}
          onChange={(event) => {
            setInputCacheText(CENTRAL_SESSION_ID, event.target.value);
          }}
          onKeyDown={handleKeyDown}
          rows={1}
          placeholder="你想让我做什么？"
          aria-label="中枢对话输入"
          className="max-h-[200px] min-h-[36px] flex-1 resize-none bg-transparent px-2 py-1.5 text-sm outline-none placeholder:text-muted-foreground disabled:opacity-50"
        />
        {isSending ? (
          <button
            type="button"
            onClick={handleCancel}
            aria-label="停止"
            title="停止"
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-muted text-muted-foreground transition-colors hover:bg-muted-foreground/20"
          >
            <Square className="h-3.5 w-3.5 fill-current" />
          </button>
        ) : (
          <button
            type="button"
            onClick={() => void handleSend()}
            disabled={sending || !(inputCache?.text ?? '').trim()}
            aria-label="发送"
            title="发送"
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground transition-opacity hover:opacity-90 disabled:opacity-40"
          >
            <ArrowUp className="h-4 w-4" />
          </button>
        )}
      </div>
      <p className="mx-auto mt-1.5 max-w-3xl px-2 text-[11px] text-muted-foreground/70">
        {sendError ? (
          <span className="text-destructive">{sendError}</span>
        ) : (
          'Enter 发送 · Shift+Enter 换行'
        )}
      </p>
    </div>
  );
}
