/**
 * 中枢模式主视图（POC 阶段 1：单一中枢对话，设计稿 §4.1–§4.4 / §5 / §13.1）。
 *
 * 消息列表复用基础组件 MessageList（虚拟化、流式、分页、思考块等渲染
 * 能力与普通模式一致，组件本身零改动）；中枢的独立性体现在：
 * - **空态**：无消息且未在生成时呈现中枢默认问候（§4.2），由本视图判断；
 * - **输入区**：复用普通 MessageInput（centralMode 形态：隐藏 workspace
 *   目录与 git 拓展槽；模型选择/用量统计保留）——用户裁决 2026-11-16；
 * - **后台状态**：CentralWorkStatusBar（执行中任务/待汇报条数）；
 * - **数据与执行**：中枢 Core 由宿主直接持有，固定 Session ID
 *   （lib/centralSession），切换逻辑独立于共用 store（lib/centralConversation，
 *   模式事实在宿主 ModeCoordinator）。结构化卡片条（CentralCardsBar）渲染
 *   授权/决定。
 */
import { LazyMessageInput, LazyMessageList } from '@/components/LazyComponents';
import { CentralCardsBar } from '@/features/central/components/CentralCardsBar';
import { CentralWorkStatusBar } from '@/features/central/components/CentralWorkStatusBar';
import { CENTRAL_DEFAULT_GREETING } from '@/features/central/components/centralGreeting';
import { CENTRAL_TRANSITION_PARAMS } from '@/features/central/components/centralTransitionParams';
import { useCallback, useState } from 'react';
import { useStore } from '@/store/useStore';
import { InteractionPluginHost } from '@/components/InteractionPluginHost';
import { InputOverlayPluginHost } from '@/components/InputOverlayPluginHost';

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
  const messageCount = useStore((s) => s.messages.length);
  const runStatus = useStore((s) => s.runStatus);
  // 征询接线（与 MainApp 普通分支同构——用户实测：中枢缺此接线时征询
  // 只剩消息流一行文本，无输入区/选项，对话卡死）：interactionVisible
  // 联动禁用输入；征询宿主绝对定位于输入框上方（inputHeight 实测）。
  const [interactionVisible, setInteractionVisible] = useState(false);
  const [inputOverlayVisible, setInputOverlayVisible] = useState(false);
  const [inputOverlayHint, setInputOverlayHint] = useState<string | null>(null);
  const [messageInputHeight, setMessageInputHeight] = useState(0);
  const handleInputOverlayVisibility = useCallback(
    (visible: boolean, hint: string | null) => {
      setInputOverlayVisible(visible);
      setInputOverlayHint(hint);
    },
    [],
  );
  // 空对话且未在生成：中枢问候；生成中（含首条消息已投递）交给 MessageList
  // 的 typing 态承接，有消息后正常渲染对话流。
  const showGreeting = messageCount === 0 && runStatus !== 'executing';

  return (
    <main
      data-testid="central-mode-view"
      className="relative flex flex-1 min-w-0 flex-col bg-background animate-central-view-in"
    >
      <CentralWorkStatusBar />
      {/* 消息列表常挂载（空态由问候覆盖层遮盖）：避免「消息到达才挂载
          列表」的虚拟化首帧定位竞态——中枢无会话切换信号，定位只能走
          新消息路径，延迟挂载会让 scrollToIndex 早于测量完成而失效。 */}
      <div className="relative min-h-0 flex-1">
        <LazyMessageList suppressWelcome />
        {showGreeting ? (
          <div className="absolute inset-0 z-10 bg-background">
            <CentralGreeting />
          </div>
        ) : null}
      </div>
      <CentralCardsBar />
      {/* 输入区复用普通 MessageInput（二十一轮 review 后用户裁决：直接
          复用现有输入组件——模型选择/用量/发送语义全套与普通会话一致；
          centralMode 下隐藏 workspace 目录区与 git 拓展槽）。 */}
      <LazyMessageInput
        centralMode
        interactionVisible={interactionVisible}
        inputOverlayVisible={inputOverlayVisible}
        inputOverlayHint={inputOverlayHint}
        inputOverlay={(
          <InputOverlayPluginHost
            suppressed={interactionVisible}
            onVisibilityChange={handleInputOverlayVisibility}
          />
        )}
        onHeightChange={setMessageInputHeight}
      />
      <InteractionPluginHost
        inputHeight={messageInputHeight}
        onVisibilityChange={setInteractionVisible}
      />
    </main>
  );
}
