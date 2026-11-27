import { describe, expect, it } from 'vitest';

import { fenceRawHtml } from '@/utils/markdownRawHtmlFence';
import rawHtmlPageMessage from './fixtures/raw-html-page-message.txt?raw';

/** 真实会话原文（2026-11-26「提交变更并规划框架」）：curl 命令 + 整页 HTML + 日志。 */
const RAW = rawHtmlPageMessage.trimEnd();

function pageLines(): string[] {
  const start = RAW.indexOf('<!doctype html>');
  const end = RAW.indexOf('</html>') + '</html>'.length;
  return RAW.slice(start, end).split('\n');
}

describe('fenceRawHtml：整段原始 HTML 包进 ```html 围栏', () => {
  it('真实会话原文：整页 HTML 成为一个围栏，前后正文不动，内容逐行原样', () => {
    const out = fenceRawHtml(RAW);
    const lines = out.split('\n');
    const open = lines.indexOf('```html');
    const page = pageLines();
    expect(open).toBe(1); // 紧跟 curl 命令行
    expect(lines[0]).toBe(RAW.split('\n')[0]);
    expect(lines.slice(open + 1, open + 1 + page.length)).toEqual(page);
    expect(lines[open + 1 + page.length]).toBe('```');
    expect(lines[open + 2 + page.length]).toBe(''); // 页面后的空行保留
    expect(out.endsWith('shutdown signal received elapsed=38.274279625s tasks=0')).toBe(true);
    expect(out.split('\n').length).toBe(RAW.split('\n').length + 2);
  });

  it('片段后接空行与正文：只围栏片段', () => {
    const src = ['说明：', '<ul>', '  <li>a</li>', '</ul>', '', '后续正文 <b>不加粗</b>'].join('\n');
    expect(fenceRawHtml(src)).toBe(['说明：', '```html', '<ul>', '  <li>a</li>', '</ul>', '```', '', '后续正文 <b>不加粗</b>'].join('\n'));
  });

  it('多行元素内的纯文本行不会截断区域', () => {
    const src = ['<p>', '  第一行', '  第二行', '</p>', '', '正文'].join('\n');
    const out = fenceRawHtml(src).split('\n');
    expect(out.slice(0, 7)).toEqual(['```html', '<p>', '  第一行', '  第二行', '</p>', '```', '']);
  });

  it('script / style 原始文本内的标签、空行、引号不参与配对', () => {
    const src = [
      '<div>',
      '<style>',
      'a > b { color: red }',
      '</style>',
      '<script>',
      'const s = "<li><strong></strong><div class=\\"meta\\"></div></li>";',
      '',
      'if (a < b) { x(); }',
      '</script>',
      '</div>',
      '',
      '尾注',
    ].join('\n');
    const out = fenceRawHtml(src).split('\n');
    expect(out[0]).toBe('```html');
    expect(out[10]).toBe('</div>');
    expect(out[11]).toBe('```');
    expect(out[12]).toBe('');
    expect(out[13]).toBe('尾注');
  });

  it('属性里的 > 与跨行属性不会提前结束标签', () => {
    const src = ['<div title="a > b"', '   data-x="/x">', '  文本', '</div>', '', '正文'].join('\n');
    const out = fenceRawHtml(src).split('\n');
    expect(out[0]).toBe('```html');
    expect(out[5]).toBe('```');
  });

  it('未闭合标签：空行后接 Markdown 块标记 → 在空行前收口；否则包到文末', () => {
    const truncated = ['<div>', '  内容', '', '## 下一步', '- 项目'].join('\n');
    expect(fenceRawHtml(truncated).split('\n')).toEqual(['```html', '<div>', '  内容', '```', '', '## 下一步', '- 项目']);
    const toEof = ['<table>', '  <tr><td>x</td></tr>'].join('\n');
    expect(fenceRawHtml(toEof).split('\n')).toEqual(['```html', '<table>', '  <tr><td>x</td></tr>', '```']);
  });

  it('已有围栏 / 缩进代码块内的 HTML 不处理', () => {
    const src = [
      '```html',
      '<div>',
      '  围栏内',
      '</div>',
      '```',
      '',
      '~~~',
      '<ul>',
      '</ul>',
      '~~~',
      '',
      '    <div>',
      '    缩进代码',
      '    </div>',
    ].join('\n');
    expect(fenceRawHtml(src)).toBe(src);
  });

  it('纯行内标签区域与单行片段不围栏', () => {
    const inline = ['<b>加粗</b>', '<i>斜体</i>', '', '正文'].join('\n');
    expect(fenceRawHtml(inline)).toBe(inline);
    const single = ['<style>body { margin: 0 }</style>', '', '正文'].join('\n');
    expect(fenceRawHtml(single)).toBe(single);
    // 文本中间的标签不是区域起点
    const mid = ['使用 <div> 包裹内容', '<div> 单行</div>'].join('\n');
    expect(fenceRawHtml(mid)).toBe(mid);
  });

  it('区域内含行首反引号围栏时外层加长', () => {
    const src = ['<pre>', '```', '<b>', '```', '</pre>'].join('\n');
    expect(fenceRawHtml(src).split('\n')).toEqual(['````html', '<pre>', '```', '<b>', '```', '</pre>', '````']);
  });

  it('无 HTML 原样返回；流式截断的页面包到文末', () => {
    const plain = '# 标题\n\n正文 **粗体**\n\n```js\nconst a = 1;\n```';
    expect(fenceRawHtml(plain)).toBe(plain);
    const page = pageLines();
    const partial = [...page.slice(0, page.findIndex((l) => l.trim() === '<script>') + 1), '    async function call() {'].join('\n');
    const out = fenceRawHtml(partial).split('\n');
    expect(out[0]).toBe('```html');
    expect(out[out.length - 1]).toBe('```');
    expect(out.slice(1, -1)).toEqual(partial.split('\n'));
  });
});
