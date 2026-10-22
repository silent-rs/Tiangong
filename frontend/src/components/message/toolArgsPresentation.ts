import { useEffect, useState } from "react";
import { api, type ToolArgsPresentation } from "@/api/tauri";

/**
 * 工具参数摘要的展示提示：由插件运行时提供（过渡期内置表），前端不写死
 * 参数名。未加载或读取失败时为空提示，摘要按全部参数通用拼接。
 */
export const EMPTY_ARGS_PRESENTATION: ToolArgsPresentation = {
  lead_args: [],
  skip_args: [],
  hidden_value_prefixes: [],
};

let presentationTask: Promise<ToolArgsPresentation> | null = null;
let loaded: ToolArgsPresentation = EMPTY_ARGS_PRESENTATION;
const listeners = new Set<(value: ToolArgsPresentation) => void>();

function loadPresentation(): Promise<ToolArgsPresentation> {
  const task = Promise.resolve()
    .then(() => api.toolArgsPresentation())
    .then((value) => value ?? EMPTY_ARGS_PRESENTATION)
    .catch((error) => {
      console.warn("[tool-args] 读取参数摘要提示失败，使用通用摘要", error);
      return EMPTY_ARGS_PRESENTATION;
    });
  presentationTask = task;
  void task.then((value) => {
    if (presentationTask !== task) return;
    loaded = value;
    listeners.forEach((listener) => listener(value));
  });
  return task;
}

/** 订阅参数摘要提示（模块级共享，未加载完成前为空提示）。 */
export function useToolArgsPresentation(): ToolArgsPresentation {
  const [value, setValue] = useState<ToolArgsPresentation>(loaded);
  useEffect(() => {
    let disposed = false;
    listeners.add(setValue);
    void (presentationTask ?? loadPresentation()).then((next) => {
      if (!disposed) setValue(next);
    });
    return () => {
      disposed = true;
      listeners.delete(setValue);
    };
  }, []);
  return value;
}

/** 测试辅助：清空缓存。 */
export function resetToolArgsPresentationCache() {
  presentationTask = null;
  loaded = EMPTY_ARGS_PRESENTATION;
}
