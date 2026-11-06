/**
 * 中枢模式主视图（POC 骨架，设计稿 §4.1 / §4.2）。
 *
 * 进入后：左侧 Session 列表由 MainApp 布局分支隐藏，本组件占据主区域，
 * 呈现单一中枢对话骨架——无上下文时的默认问候 + 输入占位。
 * 「查看后台工作」入口与消息链路（复用现有 Session 持久化）随后续片接入；
 * 退出按钮恢复普通模式，Session 数据不受影响。
 */
import { LogOut, Sparkles } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useCentralMode } from '@/hooks/useCentralMode';

/** 无上下文时的默认问候（设计稿 §4.2 原文）；基于上下文的个性化问候随后续片接入。 */
const DEFAULT_GREETING =
  '你好，我是天工中枢。\n你可以直接告诉我目标，我会判断需要哪个 Workspace、安排后台工作、跟踪进展，并在关键节点向你汇报。';

export function CentralModeView() {
  const { exitCentral } = useCentralMode();

  return (
    <main className="flex flex-1 min-w-0 flex-col bg-background">
      {/* 顶部轻量状态行：模式标识与退出（§4.1：顶部保留模式与设置入口） */}
      <div className="flex h-12 shrink-0 items-center justify-between border-b px-4 select-none">
        <div className="flex items-center gap-2 text-sm font-medium">
          <Sparkles className="h-4 w-4 text-primary" aria-hidden />
          <span>天工中枢</span>
        </div>
        <Button
          variant="ghost"
          size="sm"
          onClick={exitCentral}
          title="退出中枢模式（Cmd/Ctrl+Shift+J）"
        >
          <LogOut className="h-4 w-4" aria-hidden />
          <span className="hidden sm:inline">退出中枢模式</span>
        </Button>
      </div>

      {/* 单一中枢对话骨架：问候 + 输入占位 */}
      <div className="flex flex-1 flex-col items-center justify-center gap-6 px-8">
        <p className="max-w-xl text-balance whitespace-pre-line text-center text-muted-foreground">
          {DEFAULT_GREETING}
        </p>
        <div className="w-full max-w-2xl rounded-lg border border-dashed p-4 text-center text-sm text-muted-foreground">
          中枢对话（POC 骨架）——消息链路与后台工作随后续片接入
        </div>
      </div>
    </main>
  );
}
