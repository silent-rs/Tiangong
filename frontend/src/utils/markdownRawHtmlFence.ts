import type MarkdownIt from "markdown-it";

/**
 * 消息正文里的整段原始 HTML 自动围成代码块（由 setupMarkdownPreview 装到全部
 * MdPreview 实例共用的 markdown-it 上：core 规则，在 block 解析前改写 src）。
 *
 * 背景：预览只认 Markdown（html:false），裸 HTML 按文本显示——用户把整页
 * HTML 贴进输入框时，页面被 Markdown 切成碎片：标签行成段落、4 空格缩进的
 * CSS/JS 行成缩进代码块、空行再切一刀，一半文本一半代码。这里在解析前做
 * 「包围盒检测」：从行首的 HTML 标签起，按标签配对找到这段标记的结束行，整体
 * 包进 ```html 围栏，渲染成一个带高亮的代码块，原文一行不动。
 *
 * 规则：
 * - 只在行首（≤3 空格缩进）以开标签 / doctype 起始的行触发；已有围栏代码块
 *   （``` / ~~~）内不处理，4 空格缩进的缩进代码块也不触发；
 * - 配对计数：void 元素与自闭合不计，<script>/<style>/<pre>/<textarea> 等原始
 *   文本元素直接跳到对应闭合标签（内部的 <li>、空行、引号都不参与计数），
 *   注释、声明以及带引号、可跨行的属性同样整体跳过；
 * - 结束：配对归零的行之后若接空行或非标签行即止；未归零时跨空行继续，但空行
 *   后紧跟 Markdown 块标记（标题/列表/引用/围栏）视为正文接续，在空行前收口；
 *   到文末仍未归零则包到文末（流式输出中途也走这条，后续内容到达再重算）；
 * - 只有包含块级/文档级标签的区域才围栏：纯行内标签（<b>/<span>…）仍按文本；
 * - 区域内若出现行首的反引号围栏，外层围栏自动加长。
 *
 * 改写后同步刷新 env.srcLines：md-editor-rt 用它判断 mermaid/echarts 围栏是否
 * 已闭合，行号必须与改写后的 src 一致。
 */

/** 不配对的元素。 */
const VOID_TAGS = new Set([
  "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr",
]);

/** 内容为原始文本的元素：开标签之后直接找闭合标签，中间不做任何解析。 */
const RAW_TEXT_TAGS = new Set(["script", "style", "pre", "textarea", "xmp", "noembed", "noframes"]);

/** 只有包含这些标签的区域才围栏。 */
const BLOCKISH_TAGS = new Set([
  "html", "head", "body", "style", "script", "link", "meta", "title", "noscript", "template", "iframe",
  "div", "p", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li", "dl", "dt", "dd",
  "table", "thead", "tbody", "tfoot", "tr", "td", "th", "caption", "colgroup",
  "form", "fieldset", "legend", "section", "article", "aside", "header", "footer", "nav", "main",
  "address", "blockquote", "figure", "figcaption", "details", "summary", "dialog", "hr", "center", "pre",
]);

/** 行首开标签 / doctype：区域起点。 */
const REGION_START_RE = /^ {0,3}<([a-zA-Z][a-zA-Z0-9-]*)(?=[\s/>]|$)/;
const DOCTYPE_START_RE = /^ {0,3}<!doctype\b/i;
/** 以标签、闭合标签、声明或注释开头的行：配对归零后仍视为同一区域的延续。 */
const TAG_LINE_RE = /^\s*<[a-zA-Z!/?]/;
/** 已有围栏代码块的开/闭行。 */
const FENCE_LINE_RE = /^ {0,3}(`{3,}|~{3,})(.*)$/;
/** Markdown 块标记：未闭合区域跨空行后遇到它，视为正文接续。 */
const MD_BLOCK_MARK_RE = /^ {0,3}(#{1,6}\s|[-*+]\s|>\s?|`{3,}|~{3,}|\d{1,9}[.)]\s)/;
const TAG_OPEN_RE = /<\s*(\/?)\s*([a-zA-Z][a-zA-Z0-9-]*)(?=[\s/>]|$)/y;
const BARE_NAME_SELF_CLOSE_RE = /^<\s*[a-zA-Z][a-zA-Z0-9-]*\/$/;

type Region = { end: number; blockish: boolean };

/** 从 lines[start] 起扫描 HTML 区域，返回独占的结束行号。 */
function scanRegion(lines: string[], start: number): Region {
  const text = lines.slice(start).join("\n");
  const eof: Region = { end: lines.length, blockish: DOCTYPE_START_RE.test(lines[start]) };
  let p = 0;
  let line = 0;
  let depth = 0;
  let raw: string | null = null;
  let blockish = eof.blockish;
  /** 推进到 offset，顺带数换行。 */
  const advance = (to: number) => {
    for (let i = p; i < to; i++) if (text.charCodeAt(i) === 10) line++;
    p = to;
  };

  while (p < text.length) {
    if (raw !== null) {
      const close = new RegExp(`</${raw}\\s*>`, "ig");
      close.lastIndex = p;
      const m = close.exec(text);
      if (!m) return eof;
      advance(m.index + m[0].length);
      depth--;
      raw = null;
      continue;
    }
    const c = text[p];
    if (c === "<") {
      if (text.startsWith("<!--", p)) {
        const e = text.indexOf("-->", p + 4);
        if (e < 0) return eof;
        advance(e + 3);
        continue;
      }
      if (text[p + 1] === "!" || text[p + 1] === "?") {
        const e = text.indexOf(">", p);
        if (e < 0) return eof;
        advance(e + 1);
        continue;
      }
      TAG_OPEN_RE.lastIndex = p;
      const m = TAG_OPEN_RE.exec(text);
      if (!m) {
        p++;
        continue;
      }
      // 属性：引号内的 > 不结束标签，可跨行
      let q = TAG_OPEN_RE.lastIndex;
      let quote: string | null = null;
      let end = -1;
      for (; q < text.length; q++) {
        const ch = text[q];
        if (quote) {
          if (ch === quote) quote = null;
        } else if (ch === '"' || ch === "'") quote = ch;
        else if (ch === ">") {
          end = q;
          break;
        }
      }
      if (end < 0) return eof;
      const name = m[2].toLowerCase();
      const selfClosing =
        text[end - 1] === "/" && (/\s/.test(text[end - 2] ?? " ") || BARE_NAME_SELF_CLOSE_RE.test(text.slice(p, end)));
      if (m[1] === "/") {
        if (!VOID_TAGS.has(name)) depth = Math.max(0, depth - 1);
      } else if (!selfClosing && !VOID_TAGS.has(name)) {
        depth++;
        if (RAW_TEXT_TAGS.has(name)) raw = name;
      }
      if (BLOCKISH_TAGS.has(name)) blockish = true;
      advance(end + 1);
      continue;
    }
    if (c === "\n") {
      if (depth === 0) {
        const nextIdx = start + line + 1;
        if (nextIdx >= lines.length) return { end: lines.length, blockish };
        const next = lines[nextIdx];
        if (next.trim() === "" || !TAG_LINE_RE.test(next)) return { end: nextIdx, blockish };
      } else if (lines[start + line].trim() === "") {
        let k = start + line + 1;
        while (k < lines.length && lines[k].trim() === "") k++;
        if (k < lines.length && MD_BLOCK_MARK_RE.test(lines[k])) return { end: start + line, blockish };
      }
      line++;
      p++;
      continue;
    }
    p++;
  }
  return { end: lines.length, blockish };
}

function fenceCloseLen(line: string, ch: string, len: number): boolean {
  const m = /^ {0,3}(`{3,}|~{3,})\s*$/.exec(line);
  return !!m && m[1][0] === ch && m[1].length >= len;
}

/** 区域内行首反引号围栏的最大长度 → 外层围栏要比它长。 */
function fenceFor(region: string[]): string {
  let max = 3;
  for (const l of region) {
    const m = /^ {0,3}(`{3,})/.exec(l);
    if (m && m[1].length >= max) max = m[1].length + 1;
  }
  return "`".repeat(max);
}

/** 把正文里检测到的整段原始 HTML 包进 ```html 围栏；无 HTML 时原样返回。 */
export function fenceRawHtml(src: string): string {
  if (!src.includes("<")) return src;
  const lines = src.split("\n");
  const out: string[] = [];
  let openFence: { ch: string; len: number } | null = null;
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (openFence) {
      out.push(line);
      if (fenceCloseLen(line, openFence.ch, openFence.len)) openFence = null;
      i++;
      continue;
    }
    const fm = FENCE_LINE_RE.exec(line);
    if (fm && !(fm[1][0] === "`" && fm[2].includes("`"))) {
      openFence = { ch: fm[1][0], len: fm[1].length };
      out.push(line);
      i++;
      continue;
    }
    if (REGION_START_RE.test(line) || DOCTYPE_START_RE.test(line)) {
      const region = scanRegion(lines, i);
      let end = region.end;
      while (end > i && lines[end - 1].trim() === "") end--;
      const block = lines.slice(i, end);
      if (region.blockish && block.length >= 2) {
        const fence = fenceFor(block);
        out.push(`${fence}html`, ...block, fence);
      } else {
        out.push(...block);
      }
      i = region.end;
      continue;
    }
    out.push(line);
    i++;
  }
  return out.join("\n");
}

type CoreState = Parameters<Parameters<MarkdownIt["core"]["ruler"]["after"]>[2]>[0];

export function installMarkdownRawHtmlFence(md: MarkdownIt): void {
  md.core.ruler.after("normalize", "tiangong-fence-raw-html", (state: CoreState) => {
    const fenced = fenceRawHtml(state.src);
    if (fenced !== state.src) {
      state.src = fenced;
      const env = state.env as { srcLines?: unknown } | undefined;
      if (Array.isArray(env?.srcLines)) env.srcLines = fenced.split("\n");
    }
    return true;
  });
}
