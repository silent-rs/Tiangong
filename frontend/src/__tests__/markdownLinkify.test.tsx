import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { MdPreview } from 'md-editor-rt';
import { setupMarkdownPreview } from '@/utils/markdownLinkify';

setupMarkdownPreview();

describe('链接识别修正', () => {
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

  async function renderMd(text: string) {
    await act(async () => {
      root.render(<MdPreview modelValue={text} theme="light" previewTheme="github" />);
    });
    await act(async () => { await new Promise((r) => setTimeout(r, 50)); });
  }

  it('带后缀的文件名不被识别为链接', async () => {
    await renderMd('先改 slots.rs，再同步 manifest.rs 和 session.input-status');
    expect(container.querySelector('a')).toBeNull();
    expect(container.textContent).toContain('slots.rs');
    expect(container.textContent).toContain('session.input-status');
  });

  it('完整 URL 仍然识别为链接，尾部星号剥离并参与加粗', async () => {
    await renderMd('详见 **https://github.com/silent-rs/silent-Tiangong/pull/543**');
    const link = container.querySelector('a');
    expect(link).not.toBeNull();
    expect(link!.getAttribute('href')).toBe('https://github.com/silent-rs/silent-Tiangong/pull/543');
    expect(link!.textContent).not.toContain('*');
    expect(container.querySelector('strong')).not.toBeNull();
  });

  it('URL 后直接跟下划线加粗同样剥离', async () => {
    await renderMd('入口 __https://example.com/book__ 收尾');
    const link = container.querySelector('a');
    expect(link).not.toBeNull();
    expect(link!.getAttribute('href')).toBe('https://example.com/book');
    expect(link!.textContent).not.toContain('_');
  });

  it('星号后紧跟中文（无空格）时在星号处截断，加粗恢复', async () => {
    await renderMd('PR 已建好：**https://github.com/silent-rs/silent-Tiangong/pull/543**（分支 feature/coding-plugin-upgrade）');
    const link = container.querySelector('a');
    expect(link).not.toBeNull();
    expect(link!.getAttribute('href')).toBe('https://github.com/silent-rs/silent-Tiangong/pull/543');
    expect(link!.textContent).not.toContain('*');
    expect(link!.textContent).not.toContain('（分支');
    expect(container.querySelector('strong')).not.toBeNull();
  });

  it('URL 路径中段的下划线不受截断影响', async () => {
    await renderMd('见 https://example.com/my_page_name 末尾');
    const link = container.querySelector('a');
    expect(link).not.toBeNull();
    expect(link!.getAttribute('href')).toBe('https://example.com/my_page_name');
  });

  it('普通 URL 与邮箱识别不受影响', async () => {
    await renderMd('访问 https://example.com/path?x=1 联系 foo@example.com');
    const links = container.querySelectorAll('a');
    expect(links.length).toBeGreaterThanOrEqual(2);
    expect(links[0].getAttribute('href')).toBe('https://example.com/path?x=1');
    expect([...links].some((l) => l.getAttribute('href')?.includes('foo@example.com'))).toBe(true);
  });

  it('http 前缀的 URL 正常识别为链接', async () => {
    await renderMd('旧的入口 http://example.com/legacy 仍然有效');
    const link = container.querySelector('a');
    expect(link).not.toBeNull();
    expect(link!.getAttribute('href')).toBe('http://example.com/legacy');
    expect(link!.textContent).toBe('http://example.com/legacy');
  });

  it('URL 尾部句号按既有语义剥离', async () => {
    await renderMd('见 https://example.com/page. 结束');
    const link = container.querySelector('a');
    expect(link).not.toBeNull();
    expect(link!.getAttribute('href')).toBe('https://example.com/page');
  });

  it('file: 链接正常渲染为可点击链接（不退化成裸文本）', async () => {
    await renderMd('文件：[`/Users/test/a.html`](file:///Users/test/a.html)（41.8 KB）');
    const link = container.querySelector('a');
    expect(link).not.toBeNull();
    expect(link!.getAttribute('href')).toBe('file:///Users/test/a.html');
    expect(link!.querySelector('code')).not.toBeNull();
    expect(container.textContent).not.toContain('](file://');
  });

  it('本地文件链接按可渲染后缀裁决：不可渲染类型不可点击但保留结构', async () => {
    // 模型 review 常见形态：显示文本为相对路径:行号，href 为绝对路径
    await renderMd('[frontend/src/utils/markdownLinkify.ts:55](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/utils/markdownLinkify.ts) 包装默认 validateLink');
    expect(container.querySelector('a')).toBeNull();
    const ref = container.querySelector('span.md-local-file-ref');
    expect(ref).not.toBeNull();
    expect(ref!.textContent).toBe('frontend/src/utils/markdownLinkify.ts:55');
    // 链接语法不得作为字面文本泄漏
    expect(container.textContent).not.toContain('](');
    expect(container.textContent).not.toContain('/Users/hubertshelley');

    await renderMd('详见 [文档](file:///tmp/readme.md) 与 [入口](C:\\Users\\test\\main.rs)');
    expect(container.querySelector('a')).toBeNull();
    expect(container.querySelectorAll('span.md-local-file-ref').length).toBe(2);
    expect(container.textContent).not.toContain('](');
  });

  it('裸绝对路径的可渲染链接补正 file:// 并高亮', async () => {
    await renderMd('报告在 [这里](/Users/test/report.pdf) 与 [页面](/Users/test/intro.html)');
    const links = [...container.querySelectorAll('a')];
    expect(links.map((l) => l.getAttribute('href'))).toEqual([
      'file:///Users/test/report.pdf',
      'file:///Users/test/intro.html',
    ]);
    expect(links.every((l) => l.classList.contains('md-local-file-link'))).toBe(true);
  });

  it('Windows 盘符与 UNC 的 file: 链接同样渲染', async () => {
    await renderMd([
      '[a](file:///C:/Users/test/b.htm)',
      '[b](file://server/share/a.html)',
    ].join('\n\n'));
    const links = [...container.querySelectorAll('a')];
    expect(links.map((l) => l.getAttribute('href'))).toEqual([
      'file:///C:/Users/test/b.htm',
      'file://server/share/a.html',
    ]);
  });

  it('本地文件链接带高亮类名，外链不带', async () => {
    await renderMd('[本地](file:///Users/test/a.html) 与 [外链](https://example.com/p)');
    const links = [...container.querySelectorAll('a')];
    expect(links[0].classList.contains('md-local-file-link')).toBe(true);
    expect(links[1].classList.contains('md-local-file-link')).toBe(false);
  });

  it('javascript: 等危险协议仍被拒绝', async () => {
    await renderMd('[点我](javascript:alert(1))');
    expect(container.querySelector('a')).toBeNull();
  });
});
