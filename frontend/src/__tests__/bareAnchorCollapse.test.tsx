import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { MdPreview } from 'md-editor-rt';
import { setupMarkdownPreview } from '@/utils/markdownLinkify';
import { resolveMarkdownImages } from '@/components/message/utils';

setupMarkdownPreview();

/** 真实会话原文（2026-09-15 触发塌陷的助手回复）：反引号转义嵌套
 *  （\`）使代码段划分错位，裸 <a> 落入普通文本开标签吞后续块。 */
const RAW = "已完成，共三处修改。\n\n## 根因\n\n`resolveMarkdownImages` 已把本地路径改写成 `[\\`/…/x.html\\`](file:///…)`，但 markdown-it 默认 `validateLink` 把 `file:` 和 `javascript:`/`vbscript:`/`data:` 一起列入黑名单（`frontend/node_modules/markdown-it/lib/index.mjs:31`）。校验失败的链接不生成 `<a>`，整段 Markdown 语法退化为纯文本——就是截图里「代码块 + `](file:///…)` 裸文本」的形态。\n\n## 改动\n\n1. **放行 file: 协议** — [frontend/src/utils/markdownLinkify.ts:55](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/utils/markdownLinkify.ts)\n包装默认 `validateLink`，只额外放行 `file:`，其余危险协议仍由默认实现拒绝。点击行为不变，仍走 `MessageList.tsx:751` 的拦截进嵌入浏览器。\n\n2. **本地文件链接高亮** — [frontend/src/utils/markdownLinkify.ts:62](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/utils/markdownLinkify.ts) + [frontend/src/index.css:205](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/index.css)\n链式包装 `link_open` 渲染规则，给 file: 链接追加 `.md-local-file-link`；样式用主色描边 + 低透明度底色做 chip 感，并去掉内部行内代码的自带底色避免双层色块。外链不受影响。\n\n3. **Windows 路径兼容** — [frontend/src/components/message/utils.ts:134](/Users/hubertshelley/Documents/silent/tiangong/frontend/src/components/message/utils.ts)\n`toFileUrl` 原来只在「无 file: 前缀」分支做反斜杠转换，`file://C:\\…` 直接原样透传，且 `file://C:/…` 会被 WebView 当成主机名 `C:`。现在先按 Windows 形态（盘符/UNC，可带 file: 前缀）统一正斜杠化，再把主机位的盘符补正为 `file:///C:/…`。反斜杠转换限定在 Windows 形态，POSIX 文件名里合法的反斜杠不被误改。\n\n## 验证\n\n- 反向验证：去掉 `validateLink` 放行后，3 个新用例复现失败；恢复后通过。\n- 新增用例：file: 链接渲染、Windows 盘符/UNC href、高亮类名、`javascript:` 仍被拒；`toFileUrl` 的 `file://C:\\…`、UNC、Windows 中文空格路径。\n- 全量：`vitest run --pool=threads` → 29 文件 / 297 用例全通过；`tsc -b` 通过；`git diff --check` 无异常。\n\n注：默认 fork 池跑全量时 vitest 收尾 `kill` 报 `EPERM`（沙箱权限），与用例无关，改用 threads 池确认结果。\n\n两点供你决定：\n- 高亮样式目前是 chip 形态（描边+浅底），如果偏好更轻量（仅主色 + 下划虚线），告诉我改。\n- `.md-local-file-link` 作用于全部 `MdPreview`，用户消息气泡里的本地路径也会高亮，如需仅限 AI 回复我再加作用域限定。";

describe('复现大段链接（真实原文）', () => {
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

  it('逐层定位大段链接来源', async () => {
    const raw = RAW;
    const variants: [string, string][] = [
      ['raw', raw],
      ['processed', resolveMarkdownImages(raw)],
    ];
    for (const [name, md] of variants) {
      await act(async () => {
        root.render(<MdPreview modelValue={md} theme="light" previewTheme="github" />);
      });
      await act(async () => { await new Promise((r) => setTimeout(r, 50)); });
      const links = [...container.querySelectorAll('a')];
      console.log(name, '=> links:', links.length, 'lens:', links.map((l) => l.textContent?.length ?? 0).join(','), 'hrefs:', JSON.stringify(links.map((l) => l.getAttribute('href')?.slice(0, 40))));
      await act(async () => {
        root.render(<div />);
      });
      await act(async () => { await new Promise((r) => setTimeout(r, 20)); });
    }
    expect(true).toBe(true);
  });

  it('整段文本不应被包进单个链接', async () => {
    const raw = RAW;
    const processed = resolveMarkdownImages(raw);
    await act(async () => {
      root.render(<MdPreview modelValue={processed} theme="light" previewTheme="github" />);
    });
    await act(async () => { await new Promise((r) => setTimeout(r, 50)); });

    for (const a of [...container.querySelectorAll('a')]) {
      console.log('A:', JSON.stringify(a.getAttribute('href')?.slice(0, 90)), 'TEXT-LEN:', a.textContent?.length);
    }
    const big = [...container.querySelectorAll('a')].find((a) => (a.textContent?.length ?? 0) > 60);
    expect(big).toBeUndefined();
  });

  it('裸 <a> 字面量转义后显示为文本，code 内与带属性标签不受影响', async () => {
    const md = [
      '校验失败的链接不生成 <a>，整段语法退化。',
      '',
      '- `<a>` 在行内代码里原样',
      '- <a href="https://example.com">真实链接</a> 保持可点',
      '',
      '```',
      '<a> 代码块内原样',
      '```',
    ].join('\n');
    const processed = resolveMarkdownImages(md);
    await act(async () => {
      root.render(<MdPreview modelValue={processed} theme="light" previewTheme="github" />);
    });
    await act(async () => { await new Promise((r) => setTimeout(r, 50)); });

    const links = [...container.querySelectorAll('a')];
    expect(links.length).toBe(1);
    expect(links[0].getAttribute('href')).toBe('https://example.com');
    expect(links[0].textContent).toBe('真实链接');
    expect(container.textContent).toContain('不生成 <a>，整段');
    expect(container.textContent).toContain('<a> 在行内代码里原样');
    expect(container.textContent).toContain('<a> 代码块内原样');
  });
});
