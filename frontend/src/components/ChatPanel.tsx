/**
 * 聊天主区（消息列表 + 输入区 + 征询/输入覆盖插件宿主）。
 *
 * 从 MainApp 抽出的共享组件：普通模式与中枢模式（CentralModeView）渲染同一
 * 组件，保证两个模式下的对话链路（发送、流式、持久化、输入缓存、插件宿主）
 * 完全一致——中枢对话即普通 Session（设计稿 §16「不改变现有会话持久化格式的
 * 基本语义」），区别只在导航层（中枢模式隐藏 Session 列表）。
 */
import { useCallback, useState } from 'react';
import { LazyMessageList, LazyMessageInput } from '@/components/LazyComponents';
import { InteractionPluginHost } from '@/components/InteractionPluginHost';
import { InputOverlayPluginHost } from '@/components/InputOverlayPluginHost';

export function ChatPanel() {
  const [interactionVisible, setInteractionVisible] = useState(false);
  const [inputOverlayVisible, setInputOverlayVisible] = useState(false);
  const [inputOverlayHint, setInputOverlayHint] = useState<string | null>(null);
  const handleInputOverlayVisibility = useCallback((visible: boolean, hint: string | null) => {
    setInputOverlayVisible(visible);
    setInputOverlayHint(visible ? hint : null);
  }, []);
  const [messageInputHeight, setMessageInputHeight] = useState(0);

  return (
    <div className="relative isolate flex flex-col min-w-0 flex-1">
      <div className="flex-1 overflow-hidden">
        <LazyMessageList />
      </div>

      <LazyMessageInput
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
    </div>
  );
}
