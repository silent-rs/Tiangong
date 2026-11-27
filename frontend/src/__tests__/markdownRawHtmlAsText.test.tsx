import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { MdPreview } from 'md-editor-rt';
import { setupMarkdownPreview } from '@/utils/markdownLinkify';
import rawHtmlPageMessage from './fixtures/raw-html-page-message.txt?raw';

setupMarkdownPreview();

/** 真实会话原文（2026-11-26「提交变更并规划框架」）：用户把 curl 输出整段贴进
 *  输入框，未用代码块包裹——页面自带的 <style> 以裸 HTML 进入消息正文。 */
const RAW = rawHtmlPageMessage;

async function render(md: string, root: Root) {
  await act(async () => {
    root.render(<MdPreview modelValue={md} theme="light" previewTheme="github" />);
  });
  await act(async () => { await new Promise((r) => setTimeout(r, 50)); });
}

function globalStyleSelectors(): string[] {
  const out: string[] = [];
  for (const sheet of [...document.styleSheets]) {
    let rules: CSSRule[];
    try {
      rules = [...sheet.cssRules];
    } catch {
      continue;
    }
    for (const rule of rules) {
      const sel = (rule as CSSStyleRule).selectorText;
      if (sel) out.push(sel);
    }
  }
  return out;
}

describe('消息预览只认 Markdown，原始 HTML 按文本显示', () => {
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

  it('真实会话原文：整页 HTML 成为一个 html 代码块，不进入 DOM、不污染样式表', async () => {
    await render(RAW, root);

    for (const sel of ['body', 'h1', 'form', 'input', 'button', 'ul', 'li', '.meta', '.error']) {
      expect(globalStyleSelectors()).not.toContain(sel);
    }
    expect(container.querySelector('style, script, link, meta, iframe, form, button')).toBeNull();
    expect(container.querySelector('input:not([type="checkbox"])')).toBeNull();
    expect(container.querySelector('h1')).toBeNull();
    // 整页进入唯一的 ```html 代码块，原文逐行保留
    const blocks = [...container.querySelectorAll('pre > code')];
    expect(blocks.length).toBe(1);
    expect(blocks[0].className).toContain('language-html');
    const codeText = blocks[0].textContent ?? '';
    for (const literal of [
      '<!doctype html>',
      '<style>',
      'body { font-family: -apple-system',
      '<button type="submit">新建文章</button>',
      '<script>',
      '</html>',
    ]) {
      expect(codeText).toContain(literal);
    }
    // 围栏外：curl 命令仍是段落（URL 成链），日志仍是文本
    expect(container.querySelector('a[href^="http://127.0.0.1:8080/healthz"]')).not.toBeNull();
    expect(container.textContent).toContain('shutdown signal received');
  });

  it('行内与单行裸 HTML 只是文本：不生成元素，class/style/事件属性随之失效', async () => {
    const md = [
      '行内 <b>粗体</b> <span style="color:red">红字</span> <img src="https://example.com/a.png" alt="图" onerror="alert(1)">',
      '',
      '<div class="fixed inset-0 z-50 bg-black" style="position:fixed" onclick="alert(1)">覆盖层</div>',
      '',
      '<a href="javascript:alert(1)">坏链</a> / <a href="https://example.com">裸外链</a>',
    ].join('\n');
    await render(md, root);

    expect(container.querySelector('b, span[style], img, div.fixed, style')).toBeNull();
    // 唯一生成的 <a> 来自 linkify 对 URL 本身的识别（链接文本就是 URL），
    // 裸标签既不生成元素、也不吞掉周围文本
    const links = [...container.querySelectorAll('a')];
    expect(links.map((a) => a.textContent)).toEqual(['https://example.com/a.png', 'https://example.com']);
    expect(links.some((a) => /javascript:/i.test(a.getAttribute('href') ?? ''))).toBe(false);
    for (const literal of [
      '<b>粗体</b>',
      '<span style="color:red">红字</span>',
      '<img src="https://example.com/a.png" alt="图" onerror="alert(1)">',
      '<div class="fixed inset-0 z-50 bg-black" style="position:fixed" onclick="alert(1)">覆盖层</div>',
      '<a href="javascript:alert(1)">坏链</a>',
      '<a href="https://example.com">裸外链</a>',
    ]) {
      expect(container.textContent).toContain(literal);
    }
  });

  it('Markdown 自身的链接、图片、代码块与任务列表不受影响', async () => {
    const md = [
      '[外链](https://example.com) 与 ![图](https://example.com/a.png) 与 [坏链](javascript:alert(1))',
      '',
      '- [x] 已完成',
      '- [ ] 未完成',
      '',
      '```html',
      '<style>button { color: red }</style>',
      '```',
    ].join('\n');
    await render(md, root);

    const links = [...container.querySelectorAll('a')];
    expect(links.map((a) => a.getAttribute('href'))).toEqual(['https://example.com']);
    expect(container.querySelector('img[src="https://example.com/a.png"]')).not.toBeNull();
    expect(container.textContent).toContain('[坏链](javascript:alert(1))');
    const boxes = [...container.querySelectorAll('input[type="checkbox"]')];
    expect(boxes.length).toBe(2);
    expect(boxes[0].className).toContain('task-list-item-checkbox');
    expect(boxes[0].hasAttribute('checked')).toBe(true);
    expect(container.textContent).toContain('<style>button { color: red }</style>');
  });
});
