// Vitest 全局 setup（R7+）：jsdom 环境噪音消音。
// HTMLCanvasElement.getContext 在 jsdom 未实现（打印 Not implemented 并抛错）
// ——中枢转场动画已有 try-catch 降级，这里 mock 为返回 null 消除噪音并
// 保持「无 Canvas 环境」路径可测。
if (typeof HTMLCanvasElement !== 'undefined') {
  Object.defineProperty(HTMLCanvasElement.prototype, 'getContext', {
    configurable: true,
    value: () => null,
  });
}
