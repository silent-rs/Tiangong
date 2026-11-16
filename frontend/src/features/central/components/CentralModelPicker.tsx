/**
 * 中枢会话模型选择（二十一轮 review 后新功能：中枢视图对齐普通输入框）。
 *
 * 与普通模式 MessageInput 的会话级模型选择同源同语义：
 * - 引用来自中枢会话（null=跟随路由默认），选项来自模型注册表；
 * - 切换经 setSessionModel(CENTRAL_SESSION_ID, ...)——宿主 ensure_central_core
 *   构建时以中枢 session.model_ref 优先（§15.12），切换下一轮生效；
 * - 模型配置变更（设置页保存等）经 onModelsConfigChanged 刷新列表。
 * 复用 ui/select 与交互形态；中枢固定会话 ID，无新建对话回退分支。
 */
import { useCallback, useEffect, useState } from 'react';
import { Cpu } from 'lucide-react';
import { Select, SelectContent, SelectGroup, SelectItem, SelectLabel, SelectTrigger, SelectValue } from '@/components/ui/select';
import { api } from '@/api/tauri';
import { CENTRAL_SESSION_ID } from '@/features/central/lib/centralSession';

/** 「跟随路由默认」的哨兵值：Radix Select 不接受空字符串选项值。 */
const MODEL_DEFAULT_VALUE = '__default__';

export function CentralModelPicker() {
  const [sessionModelRef, setSessionModelRef] = useState<string | null>(null);
  const [modelOptions, setModelOptions] = useState<
    { key: string; label: string; provider: string }[]
  >([]);
  const [defaultModelLabel, setDefaultModelLabel] = useState<string>('默认');

  const refreshModelOptions = useCallback(async () => {
    try {
      const result = await api.listSessionChatModels();
      setModelOptions(
        result.models.map((item) => ({
          key: item.key,
          label: item.label || item.key,
          provider: item.provider,
        })),
      );
      setDefaultModelLabel(
        result.default_ref
          ? (result.models.find((item) => item.key === result.default_ref)?.label ?? '默认')
          : '默认',
      );
    } catch {
      // 模型列表不可用（宿主未就绪）：保留空列表，仅显示默认项
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    api
      .getSessionModel(CENTRAL_SESSION_ID)
      .then((ref) => {
        if (!cancelled) setSessionModelRef(ref);
      })
      .catch(() => undefined);
    void refreshModelOptions();
    return () => {
      cancelled = true;
    };
  }, [refreshModelOptions]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    api
      .onModelsConfigChanged(() => {
        void refreshModelOptions();
      })
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refreshModelOptions]);

  const handleChange = useCallback(async (value: string) => {
    const ref = value === MODEL_DEFAULT_VALUE ? null : value;
    try {
      await api.setSessionModel(CENTRAL_SESSION_ID, ref);
      setSessionModelRef(ref);
    } catch {
      // 切换失败保留现值（宿主侧报错经通知通道可见）
    }
  }, []);

  const byProvider = new Map<string, { key: string; label: string }[]>();
  for (const option of modelOptions) {
    const list = byProvider.get(option.provider) ?? [];
    list.push({ key: option.key, label: option.label });
    byProvider.set(option.provider, list);
  }
  const modelUnavailable =
    sessionModelRef != null && !modelOptions.some((item) => item.key === sessionModelRef);

  return (
    <Select
      value={sessionModelRef ?? MODEL_DEFAULT_VALUE}
      onValueChange={(value) => {
        void handleChange(value);
      }}
      onOpenChange={(open) => {
        if (open) void refreshModelOptions();
      }}
    >
      <SelectTrigger
        title="中枢会话模型（下一轮生效）"
        className="h-6 w-auto gap-1 rounded-md border-none bg-transparent px-1.5 text-xs text-muted-foreground shadow-none transition-colors hover:bg-accent hover:text-foreground focus:outline-none focus:ring-0 focus:ring-offset-0"
      >
        <Cpu className="w-3 h-3 shrink-0" />
        <SelectValue />
      </SelectTrigger>
      <SelectContent className="min-w-max">
        <SelectItem value={MODEL_DEFAULT_VALUE} className="text-xs">
          {defaultModelLabel}
        </SelectItem>
        {[...byProvider.entries()].map(([provider, options]) => (
          <SelectGroup key={provider}>
            <SelectLabel className="text-[11px] text-muted-foreground/70">{provider}</SelectLabel>
            {options.map((option) => (
              <SelectItem key={option.key} value={option.key} className="text-xs">
                {option.label}
              </SelectItem>
            ))}
          </SelectGroup>
        ))}
        {modelUnavailable && sessionModelRef != null && (
          <SelectItem value={sessionModelRef} className="text-xs">
            {sessionModelRef}（不在可用列表）
          </SelectItem>
        )}
      </SelectContent>
    </Select>
  );
}
