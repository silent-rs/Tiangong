import { useState } from 'react';
import { GripVertical, ListOrdered, Paperclip, Pencil, X } from 'lucide-react';
import { useStore } from '@/store/useStore';

const EMPTY_QUEUE: never[] = [];

interface InputQueueBarProps {
  cacheKey: string | null;
  isIdle: boolean;
}

/** 输入框上方的待投递队列：逐条展示，支持拖拽排序、单独引导/发送、编辑回填与移除。 */
export function InputQueueBar({ cacheKey, isIdle }: InputQueueBarProps) {
  const queue = useStore((state) => (cacheKey ? state.inputQueues[cacheKey] : undefined)) ?? EMPTY_QUEUE;
  const removeQueuedInputMessage = useStore((state) => state.removeQueuedInputMessage);
  const steerQueuedInputMessage = useStore((state) => state.steerQueuedInputMessage);
  const moveQueuedInputMessage = useStore((state) => state.moveQueuedInputMessage);
  const editQueuedInputMessage = useStore((state) => state.editQueuedInputMessage);
  const [dragIndex, setDragIndex] = useState<number | null>(null);
  const [dropIndex, setDropIndex] = useState<number | null>(null);
  if (!cacheKey || queue.length === 0) return null;

  const finishDrag = () => {
    setDragIndex(null);
    setDropIndex(null);
  };

  return (
    <div className="mb-2 flex flex-col gap-1">
      {queue.map((message, index) => (
        <div
          key={message.id}
          draggable
          onDragStart={(e) => {
            setDragIndex(index);
            e.dataTransfer.effectAllowed = 'move';
          }}
          onDragOver={(e) => {
            e.preventDefault();
            e.dataTransfer.dropEffect = 'move';
            setDropIndex(index);
          }}
          onDrop={(e) => {
            e.preventDefault();
            if (dragIndex != null && dragIndex !== index) {
              moveQueuedInputMessage(cacheKey, dragIndex, index);
            }
            finishDrag();
          }}
          onDragEnd={finishDrag}
          className={`flex cursor-grab items-center gap-2 rounded-md border bg-muted/30 px-2 py-1.5 text-xs transition-colors ${
            dragIndex === index ? 'opacity-40' : ''
          } ${
            dropIndex === index && dragIndex != null && dragIndex !== index
              ? 'border-primary bg-primary/5'
              : ''
          }`}
          title="拖拽调整投递顺序"
        >
          <GripVertical className="h-3 w-3 shrink-0 text-muted-foreground/50" />
          <span className="flex shrink-0 items-center gap-1 text-muted-foreground/70 tabular-nums">
            <ListOrdered className="h-3 w-3" />
            {index + 1}
          </span>
          <span className="min-w-0 flex-1 truncate text-muted-foreground" title={message.text}>
            {message.text.trim() || '（仅附件）'}
          </span>
          {message.attachments.length > 0 && (
            <span
              className="flex shrink-0 items-center gap-0.5 text-muted-foreground/70"
              title={message.attachments.map((item) => item.original_name ?? item.source).join('\n')}
            >
              <Paperclip className="h-3 w-3" />
              {message.attachments.length}
            </span>
          )}
          <button
            type="button"
            onClick={() => { void steerQueuedInputMessage(cacheKey, message.id); }}
            className="shrink-0 rounded border border-border/60 px-1.5 py-0.5 text-[11px] leading-4 text-muted-foreground transition-colors hover:border-primary/60 hover:bg-primary/10 hover:text-foreground"
            title={isIdle ? '立即发送这条消息' : '立即发送（打断当前执行，作为引导投递）'}
          >
            立即发送
          </button>
          <button
            type="button"
            onClick={() => editQueuedInputMessage(cacheKey, message.id)}
            className="shrink-0 text-muted-foreground transition-colors hover:text-foreground"
            title="编辑：回填到输入框"
          >
            <Pencil className="h-3.5 w-3.5" />
          </button>
          <button
            type="button"
            onClick={() => removeQueuedInputMessage(cacheKey, message.id)}
            className="shrink-0 text-muted-foreground transition-colors hover:text-foreground"
            title="从队列移除"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
      ))}
    </div>
  );
}
