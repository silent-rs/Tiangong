import { describe, expect, it } from 'vitest';
import { resetLabel, windowLabel } from '@/components/CodexUsagePanel';

describe('ChatGPT 额度展示', () => {
  it('窗口时长换算为天 / 小时 / 分钟', () => {
    expect(windowLabel(604800)).toBe('7 天');
    expect(windowLabel(18000)).toBe('5 小时');
    expect(windowLabel(90)).toBe('2 分钟');
    expect(windowLabel(undefined)).toBe('额度');
  });

  it('重置时间显示为相对时长', () => {
    const now = 1_000_000 * 1000;
    expect(resetLabel(undefined, now)).toBeNull();
    expect(resetLabel(1_000_000 + 2 * 86400 + 3 * 3600, now)).toBe('2 天 3 小时后重置');
    expect(resetLabel(1_000_000 + 3600 + 5 * 60, now)).toBe('1 小时 5 分钟后重置');
    // 已过重置点时至少显示 1 分钟
    expect(resetLabel(1_000_000 - 10, now)).toBe('1 分钟后重置');
  });
});
