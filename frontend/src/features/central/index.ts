// 中枢模式特性模块公共出口（R6 前端抽离）。
//
// ⚠️ 使用边界（防模块环）：仅供「App 顶壳」（App.tsx）与跨特性数据消费方
// （MainApp 亦可直连子模块）使用；**渲染链上的宿主组件**
// （StatusPanel —— 经 LazyComponents 被 CentralModeView 引用；useStore ——
// 被 useCentralMode 引用）必须直连 lib/hooks 子模块，不得走本出口
//（聚合 components 会与 LazyComponents/StatusPanel 成环，模块图死锁）。
export { CentralModeView } from './components/CentralModeView';
export { CentralTransition } from './components/CentralTransition';
export { CentralNotificationBridge } from './components/CentralNotificationBridge';
export { useCentralMode, useCentralModeHotkey } from './hooks/useCentralMode';
export {
  ensureCentralConversation,
  restoreFromCentralConversation,
} from './lib/centralConversation';
export { CENTRAL_SESSION_ID } from './lib/centralSession';
export {
  persistCentralMode,
  readPersistedCentralMode,
} from './lib/centralModePreference';
