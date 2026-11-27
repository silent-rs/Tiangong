import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { MdPreview } from 'md-editor-rt';
import { setupMarkdownPreview } from '@/utils/markdownLinkify';
import { resolveMarkdownImages } from '@/components/message/utils';

setupMarkdownPreview();

const MD = `放行 file: 协议 — [frontend/src/utils/markdownLinkify.ts:55](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/utils/markdownLinkify.ts)
包装默认 \`validateLink\`，只额外放行 \`file:\`，其余危险协议仍由默认实现拒绝。

本地文件链接高亮 — [frontend/src/utils/markdownLinkify.ts:62](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/utils/markdownLinkify.ts) + [frontend/src/index.css:205](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/index.css)
链式包装 \`link_open\` 渲染规则。

Windows 路径兼容 — [frontend/src/components/message/utils.ts:134](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/components/message/utils.ts)
\`toFileUrl\` 原来只在「无 file: 前缀」分支做反斜杠转换。`;

describe('不可渲染后缀的本地路径链接：保留 Markdown 结构但不可点击', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    delete (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT;
  });

  async function render(md: string) {
    await act(async () => {
      root.render(<MdPreview modelValue={md} theme="light" previewTheme="github" />);
    });
    await act(async () => { await new Promise((r) => setTimeout(r, 50)); });
  }

  it('.ts/.css 本地路径不生成可点击链接，但渲染为标记 span', async () => {
    await render(MD);
    expect(container.querySelectorAll('a').length).toBe(0);
    const refs = [...container.querySelectorAll('span.md-local-file-ref')];
    expect(refs.length).toBe(4);
    expect(refs[0].textContent).toBe('frontend/src/utils/markdownLinkify.ts:55');
  });

  it('不得把链接语法当字面文本输出（正文结构不被破坏）', async () => {
    await render(MD);
    const text = container.textContent ?? '';
    // 原始 Markdown 语法残留即为回归：链接文本照常渲染，方括号与目标不出现
    expect(text).not.toContain('](');
    expect(text).not.toContain('[frontend/src/utils/markdownLinkify.ts:55]');
    expect(text).not.toContain('/Users/hubertshelley/Documents/silent/tiangong/frontend');
    // 链接文本与相邻行内代码仍正常渲染
    expect(text).toContain('markdownLinkify.ts:55');
    expect(container.querySelectorAll('code').length).toBeGreaterThan(0);
  });

  it('经消息预处理链后同样保持不可点击且结构完好', async () => {
    await render(resolveMarkdownImages(MD));
    expect(container.querySelectorAll('a').length).toBe(0);
    expect(container.querySelectorAll('span.md-local-file-ref').length).toBe(4);
    expect(container.textContent ?? '').not.toContain('](');
  });
});
