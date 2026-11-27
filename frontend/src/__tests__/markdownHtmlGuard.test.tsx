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

describe('消息裸 HTML 不污染宿主样式', () => {
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

  it('真实会话原文：<style>/<script> 不进入 DOM，样式表不出现页面规则', async () => {
    await render(RAW, root);

    const selectors = globalStyleSelectors();
    // 样式表里不得出现页面自带的元素选择器与通用类名
    for (const sel of ['body', 'h1', 'form', 'input', 'button', 'ul', 'li', '.meta', '.error']) {
      expect(selectors).not.toContain(sel);
    }
    expect(container.querySelector('style')).toBeNull();
    expect(container.querySelector('script')).toBeNull();
    expect(container.querySelector('form, button, iframe, link, meta')).toBeNull();
    // 表单里的文本输入框不得成为可交互控件
    expect(container.querySelector('input:not([type="checkbox"])')).toBeNull();
    // 页面正文文字保留，链接保持可点
    expect(container.textContent).toContain('这是脚手架自带的占位页面');
    const link = container.querySelector('a[href="/swagger/"]');
    expect(link).not.toBeNull();
    // 块内未闭合的 <script>（闭合在空行之后的另一块）截到块末：脚本前半段不残留为裸文本
    const preview = container.querySelector('[id$="-preview"]') as HTMLElement;
    const strayText = [...preview.childNodes]
      .filter((n) => n.nodeType === Node.TEXT_NODE && (n.textContent ?? '').trim())
      .map((n) => n.textContent);
    expect(strayText).toEqual([]);
  });

  it('行内 <style>（不在行首）同样不进入 DOM', async () => {
    await render('说明 <style>li { padding: 10px 0 }</style> 结束', root);
    expect(container.querySelector('style')).toBeNull();
    expect(globalStyleSelectors()).not.toContain('li');
  });

  it('裸 HTML 的 class / style / 事件属性被剥离，普通标签与链接保留', async () => {
    const md = [
      '<div class="fixed inset-0 z-50 bg-black" style="position:fixed;left:0" onclick="alert(1)">覆盖层</div>',
      '',
      '<b>粗体</b> <span style="color:red">红字</span> <a href="https://example.com" title="t">外链</a>',
      '',
      '<img src="https://example.com/a.png" alt="图" onerror="alert(1)">',
    ].join('\n');
    await render(md, root);

    const div = container.querySelector('div[id$="-preview"] > div') as HTMLElement;
    expect(div.getAttribute('class')).toBeNull();
    expect(div.getAttribute('style')).toBeNull();
    expect(div.hasAttribute('onclick')).toBe(false);
    expect(container.querySelector('b')?.textContent).toBe('粗体');
    expect(container.querySelector('span')?.getAttribute('style')).toBeNull();
    expect(container.querySelector('a')?.getAttribute('href')).toBe('https://example.com');
    const img = container.querySelector('img') as HTMLElement;
    expect(img.getAttribute('src')).toBe('https://example.com/a.png');
    expect(img.hasAttribute('onerror')).toBe(false);
  });

  it('裸 <a href="javascript:"> 与非复选框 <input> 被中和', async () => {
    await render('<a href="javascript:alert(1)">x</a> <input id="title" placeholder="标题"> <input type="checkbox" checked>', root);
    const a = container.querySelector('a') as HTMLElement;
    expect(a.getAttribute('href') ?? '').not.toMatch(/javascript:/i);
    expect(container.querySelector('input#title')).toBeNull();
    expect(container.querySelector('input[type="checkbox"]')).not.toBeNull();
  });

  it('代码块内的 <style> 原样显示，任务列表复选框保留', async () => {
    const md = ['```html', '<style>button { color: red }</style>', '```', '', '- [x] 已完成', '- [ ] 未完成'].join('\n');
    await render(md, root);
    expect(container.querySelector('style')).toBeNull();
    expect(container.textContent).toContain('<style>button { color: red }</style>');
    const boxes = [...container.querySelectorAll('input[type="checkbox"]')];
    expect(boxes.length).toBe(2);
    expect(boxes[0].className).toContain('task-list-item-checkbox');
    expect(boxes[0].hasAttribute('checked')).toBe(true);
  });
});
