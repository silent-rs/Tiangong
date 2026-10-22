import { LongContent } from "./LongContent";

/**
 * 工具调用的文本默认显示（插件未提供工具视图时）：上段调用参数，下段执行
 * 结果（完成后才有）。不含任何工具语义。
 */
export function ToolCallCard({
  argsText,
  outputText,
  isError,
}: {
  argsText: string | null;
  /** 执行结果；运行中为 null。 */
  outputText: string | null;
  isError?: boolean;
}) {
  if (!argsText && !outputText) return null;
  return (
    <div className="rounded-md border border-border/60 bg-muted/30 overflow-hidden" data-tool-call-card>
      {argsText && (
        <div data-tool-call-args>
          <div className="px-3 pt-1.5 text-[10px] font-semibold tracking-wider text-muted-foreground/70 uppercase">参数</div>
          <LongContent
            content={argsText}
            className="px-3 pb-2 pt-1 text-xs font-mono text-muted-foreground whitespace-pre-wrap break-words max-h-40 overflow-y-auto"
          />
        </div>
      )}
      {outputText && (
        <div className={argsText ? "border-t border-border/50" : ""} data-tool-result>
          <LongContent
            content={outputText}
            className={`px-3 py-2 text-xs font-mono whitespace-pre-wrap break-words max-h-72 overflow-y-auto ${
              isError ? "text-destructive" : "text-foreground/85"
            }`}
          />
        </div>
      )}
    </div>
  );
}
