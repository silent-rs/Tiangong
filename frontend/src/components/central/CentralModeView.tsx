/**
 * 中枢模式主视图（POC 阶段 1：单一中枢对话，设计稿 §4.1–§4.4 / §5 / §13.1）。
 *
 * 中枢 UI 完全独立（CentralMessageList + CentralChatInput），不复用普通
 * 模式的任何聊天组件——普通模式文件保持最小改动。中枢对话是同一
 * CoreManager 管理下的特殊 Core（单独存储 / 空插件白名单，见
 * `src-tauri/src/central.rs`），前端固定 Session ID（lib/centralSession）。
 *
 * 顶部不设独立标题栏：模式标识与退出入口统一在外层顶栏（对话 title 显示
 * 「中枢」，退出复用顶栏进入按钮）。「查看后台工作」入口与 §5.3 白名单
 * 工具集随后续片接入。
 */
import { useCallback, useState } from 'react';
import { InteractionPluginHost } from '@/components/InteractionPluginHost';
import { CentralChatInput } from '@/components/central/CentralChatInput';
import { CentralMessageList } from '@/components/central/CentralMessageList';

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
      <div className="min-h-0 flex-1">
        <CentralMessageList />
      </div>
      <CentralChatInput disabled={interactionVisible} onHeightChange={handleHeightChange} />
      <InteractionPluginHost
        inputHeight={inputHeight}
        onVisibilityChange={setInteractionVisible}
      />
    </main>
  );
}
