/**
 * 等待用户决策的覆盖层（waiting_user 协议保留——用户裁决 2026-11-16）。
 *
 * 用户实测两个歧义：任务全文太长不利决策；卡片悬在输入框上方而下方
 * 普通输入框仍可见可用（两个输入框并存，不知道在哪答）。本组件把决策
 * 卡片升级为**覆盖输入框的答题层**：存在 waiting_user 工作时整层覆盖
 * 输入区（背景遮蔽普通输入框，杜绝歧义），内含精炼内容行（两行截断、
 * 点击展开限高滚动）与专用答复输入；提交经 central_answer_waiting
 * 直接恢复该工作，状态转 running 后覆盖层消失、普通输入框回归。
 */
import { useCallback, useEffect, useState } from 'react';
import { MessageCircleQuestion } from 'lucide-react';
import { api } from '@/api/tauri';
import { listen } from '@tauri-apps/api/event';

interface WaitingWork {
  work_id: string;
  session_id: string;
  task: string;
}

export function CentralDecisionOverlay({ inputHeight = 0 }: { inputHeight?: number }) {
  const [work, setWork] = useState<WaitingWork | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [answer, setAnswer] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState('');

  const refresh = useCallback(() => {
    void api
      .getCentralWorkStatus()
      .then((items) => {
        // 最新一条 waiting_user（状态驱动：答复恢复 running 后消失）
        const waiting = [...items]
          .reverse()
          .find((item) => item.state === 'waiting_user');
        setWork(
          waiting
            ? { work_id: waiting.work_id, session_id: waiting.session_id, task: waiting.task }
            : null,
        );
        if (!waiting) {
          setAnswer('');
          setSubmitting(false);
          setExpanded(false);
        }
      })
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    refresh();
    let disposed = false;
    let unsubscribe: (() => void) | undefined;
    void listen('central-work-updated', () => refresh()).then((un) => {
      if (disposed) un();
      else unsubscribe = un;
    });
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, [refresh]);

  const submit = async () => {
    const text = answer.trim();
    if (!text || submitting || !work) return;
    setSubmitting(true);
    setError('');
    try {
      await api.centralAnswerWaiting(work.work_id, text);
      // 状态驱动消失：central-work-updated → refresh → work=null
    } catch (err) {
      setError(String(err));
      setSubmitting(false);
    }
  };

  if (!work) return null;
  return (
    <div
      data-testid="central-decision-overlay"
      role="dialog"
      aria-label="等待用户决策"
      className="absolute inset-x-0 bottom-0 z-[60] border-t border-primary/20 bg-background px-4 py-3"
      style={{ minHeight: inputHeight > 0 ? `${inputHeight}px` : undefined }}
    >
      <div className="mx-auto w-full max-w-3xl">
        <div
          data-testid="central-decision-card"
          className="rounded-lg border border-primary/20 bg-primary/[0.04] px-3 py-2 text-xs"
        >
          <div className="flex items-center gap-2">
            <MessageCircleQuestion className="h-4 w-4 shrink-0 text-primary" />
            <span className="font-medium">在等你的决定</span>
            <span className="ml-auto shrink-0 text-[10px] text-muted-foreground">
              答复提交后工作自动继续
            </span>
          </div>
          <button
            type="button"
            data-testid="central-decision-card-body"
            aria-expanded={expanded}
            onClick={() => setExpanded((value) => !value)}
            title={expanded ? '点击收起' : '点击展开完整内容'}
            className={`mt-1 w-full text-left text-muted-foreground ${
              expanded ? 'max-h-[38vh] overflow-y-auto whitespace-pre-wrap' : 'line-clamp-2'
            }`}
          >
            {work.task}
          </button>
          <div className="mt-2 flex items-center gap-2">
            <input
              data-testid="central-decision-card-input"
              value={answer}
              onChange={(event) => setAnswer(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter' && !event.nativeEvent.isComposing) {
                  void submit();
                }
              }}
              placeholder="在这里输入你的答复…"
              disabled={submitting}
              autoFocus
              className="min-w-0 flex-1 rounded-md border border-border/70 bg-background px-2 py-1.5 text-xs outline-none focus:border-primary/50"
            />
            <button
              type="button"
              data-testid="central-decision-card-submit"
              onClick={() => void submit()}
              disabled={submitting || !answer.trim()}
              className="shrink-0 rounded-md bg-primary px-2.5 py-1.5 text-[11px] font-medium text-primary-foreground transition-opacity hover:opacity-90 disabled:opacity-50"
            >
              {submitting ? '恢复中…' : '提交答复'}
            </button>
          </div>
          {error ? (
            <p className="mt-1 text-[10px] text-destructive" role="alert">
              {error}
            </p>
          ) : null}
        </div>
      </div>
    </div>
  );
}
