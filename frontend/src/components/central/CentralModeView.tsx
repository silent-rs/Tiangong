/**
 * 中枢模式主视图（POC 阶段 1：接入真实对话链路，设计稿 §4.1 / §4.2 / §16）。
 *
 * 进入后：左侧 Session 列表由 MainApp 布局分支隐藏，本组件占据主区域，
 * 渲染与普通模式相同的聊天主区（ChatPanel）——消息链路完全复用现有 Session
 * 持久化：中枢对话绑定一个专用 Session（见 store.ensureCentralConversation），
 * 发送、流式渲染、历史加载与普通会话一致。空对话时 MessageList 呈现中枢
 * 默认问候（§4.2）。
 *
 * 顶部不设独立标题栏：模式标识与退出入口统一在外层顶栏（对话 title 显示
 * 「中枢」，退出复用顶栏进入按钮）。「查看后台工作」入口与中枢助手工具
 * 白名单随后续片接入。
 */
import { ChatPanel } from '@/components/ChatPanel';

export function CentralModeView() {
  return (
    <main data-testid="central-mode-view" className="flex flex-1 min-w-0 flex-col bg-background animate-central-view-in">
      <ChatPanel />
    </main>
  );
}
