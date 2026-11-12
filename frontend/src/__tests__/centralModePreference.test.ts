/**
 * 中枢模式偏好持久化（lib/central/centralModePreference）：
 * 读写 localStorage、损坏值容错、缺省 false（重启回普通模式的默认）。
 */
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

const {
  readPersistedCentralMode,
  persistCentralMode,
  CENTRAL_MODE_STORAGE_KEY,
} = await import('@/lib/central/centralModePreference');

describe('centralModePreference', () => {
  beforeEach(() => {
    localStorage.clear();
  });
  afterEach(() => {
    localStorage.clear();
  });

  it('缺省返回 false（未记录偏好→重启回普通模式）', () => {
    expect(readPersistedCentralMode()).toBe(false);
  });

  it('写入后可读回（true 持久化——重启直接回中枢）', () => {
    persistCentralMode(true);
    expect(readPersistedCentralMode()).toBe(true);
    expect(localStorage.getItem(CENTRAL_MODE_STORAGE_KEY)).toBe('true');
  });

  it('损坏/非法值容错回退 false', () => {
    localStorage.setItem(CENTRAL_MODE_STORAGE_KEY, 'not-a-bool');
    expect(readPersistedCentralMode()).toBe(false);
  });
});
