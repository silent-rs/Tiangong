/**
 * 中枢模式主视图（POC 阶段 1：单一中枢对话，设计稿 §4.1–§4.4 / §5 / §13.1）。
 *
 * 进入后：左侧 Session 列表由 MainApp 布局分支隐藏，本组件占据主区域。
 * 中枢对话是独立实体，不与普通会话混用：
 * - **单独存储**：固定 Session ID（lib/centralSession），后端独立
 *   storage_root，get_sessions / 删除 / 回收站不触达；
 * - **专属输入区**：CentralChatInput（「你想让我做什么？」），非普通
 *   MessageInput——中枢是控制面交互，无模型选择 / 目录 / 附件等工作
 *   会话控件；
 * - **消息渲染**复用 MessageList（空对话时呈现中枢默认问候 §4.2），
 *   后端中枢 Core 插件集为空（§5 权限白名单隔离，见 src-tauri central.rs）；
 * - **征询卡片**（interaction 插件，§4.4/§5.4）挂在输入区上方，
 * 出现时覆盖并锁定输入区。
 *
 * 顶部不设独立标题栏：模式标识与退出入口统一在外层顶栏（对话 title 显示
 * 「中枢」，退出复用顶栏进入按钮）。「查看后台工作」入口与 §5.3 白名单
 * 工具集随后续片接入。
 */
import { useCallback, useState } from 'react';
import { LazyMessageList } from '@/components/LazyComponents';
import { InteractionPluginHost } from '@/components/InteractionPluginHost';
import { CentralChatInput } from '@/components/central/CentralChatInput';

export function CentralModeView() {
  const [interactionVisible, setInteractionVisible] = useState(false);
  const [inputHeight, setInputHeight] = useState(0);
  const handleHeightChange = useCallback((height: number) => {
    setInputHeight(height);
  }, []);

  return (
    <main
      data-testid="central-mode-view"
      className="flex flex-1 min-w-0 flex-col bg-background animate-central-view-in"
    >
      <div className="flex-1 overflow-hidden">
        <LazyMessageList />
      </div>
      <CentralChatInput disabled={interactionVisible} onHeightChange={handleHeightChange} />
      <InteractionPluginHost
        inputHeight={inputHeight}
        onVisibilityChange={setInteractionVisible}
      />
    </main>
  );
}
