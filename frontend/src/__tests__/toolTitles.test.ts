import { describe, expect, it } from 'vitest';

import { manifestToolTitle } from '@/components/message/toolTitles';

/**
 * 工具行标题（清单 tool_titles 查询表）：先对外名再原名，特指声明按调用
 * 参数取值命中，未声明或空标题返回 null（调用方回落视图声明或通用标题）。
 */
describe('工具行标题（清单声明）', () => {
  const table = {
    generate_image: { title: '生成图片' },
    volcengine__text_to_speech: {
      title: '语音合成',
      variants: { voice: { zh_female: '女声合成' } },
    },
    only_variants: { title: '', variants: { action: { list: '列出目录' } } },
  };

  it('按对外名优先、原名其次查表', () => {
    expect(manifestToolTitle(table, 'generate_image')).toBe('生成图片');
    expect(manifestToolTitle(table, 'browser__generate_image')).toBe('生成图片');
    expect(manifestToolTitle(table, 'volcengine__text_to_speech')).toBe('语音合成');
    expect(manifestToolTitle(table, 'unknown_tool')).toBeNull();
    expect(manifestToolTitle(table, '')).toBeNull();
  });

  it('特指声明按调用参数取值命中，未命中回落通用标题', () => {
    expect(manifestToolTitle(table, 'volcengine__text_to_speech', { voice: 'zh_female' })).toBe('女声合成');
    expect(manifestToolTitle(table, 'volcengine__text_to_speech', { voice: 'other' })).toBe('语音合成');
    expect(manifestToolTitle(table, 'volcengine__text_to_speech')).toBe('语音合成');
  });

  it('参数支持字符串与数值取值，非对象参数走通用标题', () => {
    expect(manifestToolTitle(table, 'only_variants', { action: 'list' })).toBe('列出目录');
    expect(manifestToolTitle(table, 'only_variants', { action: 42 })).toBeNull();
    expect(manifestToolTitle(table, 'generate_image', 'prompt')).toBe('生成图片');
  });

  it('仅特指且无通用标题时未命中返回 null', () => {
    expect(manifestToolTitle(table, 'only_variants')).toBeNull();
    expect(manifestToolTitle(table, 'only_variants', { action: 'write' })).toBeNull();
  });
});
