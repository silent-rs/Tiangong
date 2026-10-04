import { describe, expect, it } from 'vitest';
import {
  mentionMarkFor,
  registerMentionMark,
  registerMentionMarks,
} from '@/utils/mentionMarks';
// 注册表是模块级状态且无法清空，各用例使用互不相同的 kind 避免相互污染。
describe('mentionMarks 注册表', () => {
  it('token 精确匹配优先，kind 标记一致时兜底', () => {
    registerMentionMark('@tts:text-to-speech', 'tts', 'TTS');
    expect(mentionMarkFor('tts', '@tts:text-to-speech')).toBe('TTS');
    expect(mentionMarkFor('tts', '@tts:unknown')).toBe('TTS');
  });
  it('批量注册：统一标记的 kind 对未知 token 兜底', () => {
    registerMentionMarks([
      { value: '@skill:a', kind: 'skill', mark: 'S' },
      { value: '@skill:b', kind: 'skill', mark: 'S' },
    ]);
    expect(mentionMarkFor('skill', '@skill:a')).toBe('S');
    expect(mentionMarkFor('skill', '@skill:c')).toBe('S');
  });
  it('插件私有标记不借给同组其他插件', () => {
    registerMentionMarks([
      { value: '@plugin:computer-use', kind: 'plugin' },
      { value: '@plugin:dyncheck', kind: 'plugin', mark: '' },
      { value: '@plugin:volcengine', kind: 'plugin', mark: '火山' },
    ]);
    expect(mentionMarkFor('plugin', '@plugin:volcengine')).toBe('火山');
    expect(mentionMarkFor('plugin', '@plugin:computer-use')).toBe('');
    expect(mentionMarkFor('plugin', '@plugin:dyncheck')).toBe('');
    expect(mentionMarkFor('plugin', '@plugin:unknown')).toBe('');
  });
  it('先注册带标记插件、后出现无标记插件同样不串用', () => {
    registerMentionMark('@ext:volcengine', 'ext', '火山');
    registerMentionMark('@ext:scheduler', 'ext');
    expect(mentionMarkFor('ext', '@ext:volcengine')).toBe('火山');
    expect(mentionMarkFor('ext', '@ext:scheduler')).toBe('');
    expect(mentionMarkFor('ext', '@ext:unknown')).toBe('');
  });
  it('未注册 kind 不显示标记', () => {
    expect(mentionMarkFor('video', '@video:x')).toBe('');
  });
});
