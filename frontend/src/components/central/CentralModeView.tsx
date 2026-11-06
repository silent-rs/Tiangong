/**
 * 中枢模式主视图（POC 骨架，设计稿 §4.1 / §4.2）。
 *
 * 进入后：左侧 Session 列表由 MainApp 布局分支隐藏，本组件占据主区域，
 * 呈现单一中枢对话骨架——无上下文时的默认问候 + 输入占位。
 * 顶部不设独立标题栏：模式标识与退出入口统一在外层顶栏（对话 title 显示
 * 「中枢」，退出复用顶栏进入按钮）。「查看后台工作」入口与消息链路
 * （复用现有 Session 持久化）随后续片接入。
 */
const DEFAULT_GREETING =
  '你好，我是天工中枢。\n你可以直接告诉我目标，我会判断需要哪个 Workspace、安排后台工作、跟踪进展，并在关键节点向你汇报。';

export function CentralModeView() {
  return (
    <main className="flex flex-1 min-w-0 flex-col bg-background animate-central-view-in">
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
