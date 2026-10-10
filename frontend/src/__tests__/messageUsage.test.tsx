import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { MessageUsage, TokenUsage } from '@/api/tauri';
import { cacheHitRate, sumUsage } from '@/components/message/usage';
import { CallUsageDetails } from '@/components/message/CallUsageDetails';
import type { MessageItem } from '@/components/message/types';
import { MessageActions } from '@/components/message/MessageActions';

let root: Root;
let container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

async function openUsage(messages: MessageItem[]) {
  await act(async () => root.render(<CallUsageDetails messages={messages} />));
  const trigger = container.querySelector('button')!;
  await act(async () => trigger.click());
  return document.querySelector('[role="dialog"]')!;
}

function usage(input: number, hit: number | null): TokenUsage {
  return { prompt_tokens: input, completion_tokens: 10, total_tokens: input + 10,
    prompt_cache_hit_tokens: hit, prompt_cache_miss_tokens: hit == null ? null : input - hit };
}

function message(id: string, tokens: TokenUsage): MessageItem {
  const call: MessageUsage = { ...tokens, model: 'test-model', agent_id: 'agent-1', turn_id: 'turn-1', source: 'react', status: 'success' };
  return { id, role: { type: 'assistant', usage: call }, content: [{ type: 'text', text: '回复' }], created_at: new Date().toISOString() };
}

describe('模型调用用量', () => {
  it('按输入量加权计算汇总命中率', () => {
    const total = sumUsage([usage(100, 100), usage(900, 0)]);
    expect(total.prompt_tokens).toBe(1000);
    expect(total.total_tokens).toBe(1020);
    expect(cacheHitRate(total)).toBe(0.1);
  });

  it('区分零命中、缺失和不完整统计', () => {
    expect(cacheHitRate(usage(100, 0))).toBe(0);
    expect(cacheHitRate(usage(100, null))).toBeNull();
    expect(cacheHitRate(usage(0, 0))).toBeNull();
    expect(cacheHitRate(sumUsage([usage(100, 80), usage(900, null)]))).toBeNull();
  });

  it('重新加载消息后在弹窗展示调用明细和加权汇总', async () => {
    const messages: MessageItem[] = JSON.parse(JSON.stringify([message('a', usage(100, 100)), message('b', usage(900, 0))]));
    const html = (await openUsage(messages)).innerHTML;
    expect(html).toContain('2');
    expect(html).toContain('1,000');
    expect(html).toContain('10.0%');
    expect(html).toContain('100.0%');
    expect(html).toContain('0.0%');
    expect(html).toContain('test-model');
  });

  it('旧消息不补造用量，未知缓存数据不展示为零', async () => {
    const old = message('old', usage(100, 80));
    if (old.role.type === 'assistant') delete old.role.usage;
    expect(renderToStaticMarkup(<CallUsageDetails messages={[old]} />)).toBe('');
    const html = (await openUsage([message('new', usage(100, null))])).innerHTML;
    expect(html).toContain('未知');
    expect(html).not.toContain('0.0%');
  });

  it('输入输出合计位于总时间右侧，明细不占用消息区，弹窗支持关闭', async () => {
    await act(async () => root.render(<MessageActions text="回复" showTts={false} durationMs={9000} usageMessages={[message('a', usage(100, 80))]} />));
    const time = container.querySelector('[title="本轮执行总时长"]')!;
    const trigger = container.querySelector('[aria-label="查看调用用量详情"]') as HTMLButtonElement;
    expect(time.nextElementSibling).toBe(trigger);
    expect(container.querySelector('[title="回复生成耗时"]')).toBeNull();
    expect(trigger.querySelector('[aria-label="输入 100"] .lucide-arrow-up')).not.toBeNull();
    expect(trigger.querySelector('[aria-label="输出 10"] .lucide-arrow-down')).not.toBeNull();
    expect(trigger.querySelector('[aria-label="缓存命中 80"] .lucide-database')).not.toBeNull();
    expect(container.querySelector('details')).toBeNull();
    expect(document.querySelector('table')).toBeNull();
    await act(async () => trigger.click());
    expect(document.querySelector('[role="dialog"]')).not.toBeNull();
    await act(async () => {
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    });
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    await act(async () => trigger.click());
    await act(async () => (document.querySelector('[aria-label="关闭调用用量详情"]') as HTMLButtonElement).click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });
});
