import { describe, expect, it } from 'vitest';
import { classifyUpdateError, shouldRetryWithGithub, updateErrorHint, UPDATE_NETWORK_HINT } from '../lib/updateError';

describe('classifyUpdateError', () => {
  it('OSS 缺包 404 → missing，可回退 GitHub，且不提示 VPN', () => {
    const kind = classifyUpdateError('Download request failed with status: 404 Not Found');
    expect(kind).toBe('missing');
    expect(shouldRetryWithGithub(kind)).toBe(true);
    expect(updateErrorHint(kind)).not.toContain('VPN');
  });

  it('403 → missing', () => {
    expect(classifyUpdateError('Download request failed with status: 403 Forbidden')).toBe('missing');
  });

  it('5xx / 429 → server，可回退', () => {
    expect(classifyUpdateError('Download request failed with status: 502 Bad Gateway')).toBe('server');
    expect(classifyUpdateError('Download request failed with status: 429 Too Many Requests')).toBe('server');
    expect(shouldRetryWithGithub('server')).toBe(true);
  });

  it('网络错误 → network，提示 VPN/代理', () => {
    const kind = classifyUpdateError(
      'error sending request for url (https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/latest.json)',
    );
    expect(kind).toBe('network');
    expect(updateErrorHint(kind)).toBe(UPDATE_NETWORK_HINT);
  });

  it('签名错误 → signature，不回退', () => {
    const kind = classifyUpdateError(new Error('The signature verification failed'));
    expect(kind).toBe('signature');
    expect(shouldRetryWithGithub(kind)).toBe(false);
  });

  it('未知错误 → other，不回退', () => {
    expect(classifyUpdateError('Failed to install package')).toBe('other');
    expect(shouldRetryWithGithub('other')).toBe(false);
  });
});
