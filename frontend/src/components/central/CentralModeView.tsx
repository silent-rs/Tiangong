/**
 * 中枢模式主视图（POC 阶段 1：单一中枢对话，设计稿 §4.1–§4.4 / §5 / §13.1）。
 *
 * 消息列表复用基础组件 MessageList（虚拟化、流式、分页、思考块等渲染
 * 能力与普通模式一致，组件本身零改动）；中枢的独立性体现在：
 * - **空态**：无消息且未在生成时呈现中枢默认问候（§4.2），不使用普通
 *   模式的「欢迎使用天工」空态——由本视图判断，不给 MessageList 加分支；
 * - **输入区**：CentralChatInput（「你想让我做什么？」），不复用普通
 *   MessageInput；
 * - **数据与执行**：中枢对话是同一 CoreManager 管理下的特殊 Core（单独
 *   存储 / 空插件白名单，见 `src-tauri/src/central.rs`），固定 Session ID
 *   （lib/centralSession），切换逻辑独立于共用 store（lib/centralConversation）。
 *
 * 顶部不设独立标题栏：模式标识与退出入口统一在外层顶栏。「查看后台工作」
 * 入口与 §5.3 白名单工具集随后续片接入。
 */
import { useCallback, useState } from 'react';
import { useStore } from '@/store/useStore';
import { LazyMessageList } from '@/components/LazyComponents';
import { InteractionPluginHost } from '@/components/InteractionPluginHost';
import { CentralChatInput } from '@/components/central/CentralChatInput';
import { CentralWorkStatusBar } from '@/components/central/CentralWorkStatusBar';
import { CENTRAL_DEFAULT_GREETING } from '@/components/central/centralGreeting';
import { CENTRAL_TRANSITION_PARAMS } from '@/components/central/centralTransitionParams';

/** 中枢空对话问候（§4.2 无上下文版本）。 */
function CentralGreeting() {
  return (
    <div className="flex h-full flex-col items-center justify-center px-8 text-center">
      <div
        className="mb-4 flex h-16 w-16 items-center justify-center rounded-full text-2xl"
        style={{
          backgroundColor: CENTRAL_TRANSITION_PARAMS.accent,
          color: CENTRAL_TRANSITION_PARAMS.accent2,
        }}
        aria-hidden="true"
      >
        ✦
      </div>
      <p className="max-w-xl whitespace-pre-line text-balance text-sm leading-relaxed text-muted-foreground">
        {CENTRAL_DEFAULT_GREETING}
      </p>
    </div>
  );
}

export function CentralModeView() {
  const [interactionVisible, setInteractionVisible] = useState(false);
  const [inputHeight, setInputHeight] = useState(0);
  const handleHeightChange = useCallback((height: number) => {
    setInputHeight(height);
  }, []);

  const messageCount = useStore((s) => s.messages.length);
  const runStatus = useStore((s) => s.runStatus);
  // 空对话且未在生成：中枢问候；生成中（含首条消息已投递）交给 MessageList
  // 的 typing 态承接，有消息后正常渲染对话流。
  const showGreeting = messageCount === 0 && runStatus !== 'executing';

  return (
    <main
      data-testid="central-mode-view"
      className="flex flex-1 min-w-0 flex-col bg-background animate-central-view-in"
    >
      <div className="min-h-0 flex-1">
        {showGreeting ? <CentralGreeting /> : <LazyMessageList />}
      </div>
      <CentralWorkStatusBar />
      <CentralChatInput disabled={interactionVisible} onHeightChange={handleHeightChange} />
      <InteractionPluginHost
        inputHeight={inputHeight}
        onVisibilityChange={setInteractionVisible}
      />
    </main>
  );
}
