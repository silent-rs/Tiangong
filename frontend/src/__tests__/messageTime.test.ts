import { describe, expect, it } from 'vitest';
import { formatMessageTime } from '@/components/message/utils';

describe('formatMessageTime', () => {
  const now = new Date(2026, 9, 12, 15, 30);

  it('当天消息只显示时间', () => {
    expect(formatMessageTime('2026-10-12T00:05:00', now)).toBe('00:05');
    expect(formatMessageTime('2026-10-12T23:59:00', now)).toBe('23:59');
  });

  it('同年非当天消息显示月日与时间', () => {
    expect(formatMessageTime('2026-10-11T23:59:00', now)).toBe('10-11 23:59');
    expect(formatMessageTime('2026-01-03T08:07:00', now)).toBe('01-03 08:07');
  });

  it('跨年消息显示完整日期', () => {
    expect(formatMessageTime('2025-10-12T09:00:00', now)).toBe('2025-10-12 09:00');
  });

  it('空值与非法值兜底', () => {
    expect(formatMessageTime(undefined, now)).toBe('');
    expect(formatMessageTime('not-a-date', now)).toBe('not-a-date');
  });
});
