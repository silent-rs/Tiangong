import { XSSPlugin } from "md-editor-rt";
import type MarkdownIt from "markdown-it";

/** markdown-it 未从入口导出 StateCore，按 ruler.before 的签名反推 core 规则入参。 */
type CoreState = Parameters<Parameters<MarkdownIt["core"]["ruler"]["before"]>[2]>[0];

/** XSSPlugin 把 xss 模块传给回调；其类型声明是全局命名空间写法，按实际用到的结构标注。 */
type XssModule = { getDefaultWhiteList: () => Record<string, string[] | undefined> };

/**
 * 消息 Markdown 预览的裸 HTML 防护（由 setupMarkdownPreview 装到全部 MdPreview
 * 实例共用的 markdown-it 上）。
 *
 * 背景：MdPreview 以 html:true 渲染、默认 sanitize 是恒等函数，结果经
 * dangerouslySetInnerHTML 注入——消息正文里代码块之外的 HTML 原样进入文档。
 * 模型输出或用户粘贴的页面片段一旦带 <style>，其中 body/button/li 等无作用域
 * 选择器会直接改写天工本体样式；class 能命中宿主的 Tailwind 工具类，style 能把
 * 元素钉成全屏覆盖层，onerror / javascript: 则能在 WebView 内执行脚本。
 * md-editor-rt 自带的 XSSPlugin 正是为此准备的，但须由使用方注册。
 *
 * 两层处理都只作用于 html_block / html_inline token，即代码块之外的裸 HTML：
 *
 * 1. 文档级元素整段删除：<style>、<script>、<iframe>、<title>、<textarea>、
 *    <template>、<noscript> 及原始文本元素（xmp/noembed/noframes）连同内容移除。
 *    XSSPlugin 的 stripIgnoreTagBody 只在同一 token 内配对，html_inline 的孤立
 *    开标签会留下 "[removed]" 占位，故在 XSS 规则之前自行按正则删整段；块内残留
 *    的未闭合开标签（闭合在空行后的另一块，或根本没有）按 HTML 语义从这里到
 *    块末都是它的内容，一并截掉。doctype 声明块同样直接丢弃。
 * 2. XSSPlugin 白名单过滤其余裸 HTML：xss 默认白名单（a/b/i/span/img/table…）
 *    之外的标签删标签留内容；白名单标签上的 class / style / id / on* 属性一律丢弃。
 *    任务列表复选框由 md-editor-rt 以 html_inline 注入 <input type="checkbox">，
 *    为此放行 input 的 type/checked/disabled/class，其余类型的 input 整个删除，
 *    class 只认 task-list-item-checkbox。md-editor-rt 内置扩展里的 iframe 与
 *    img class 不采纳：嵌入框架与能命中宿主 Tailwind 的类名都是干扰面。
 *
 * 不受影响：代码块（fence 内容已转义为文本）、KaTeX / Mermaid / ECharts / 代码
 * 高亮 / 图片 figure（由各自渲染规则生成，不经过 html token）、Markdown 链接与
 * 图片（走 token 属性，不经 XSSPlugin）。
 */

/** 连同内容整段删除的元素。 */
const DROP_WITH_BODY = [
  "style",
  "script",
  "iframe",
  "template",
  "noscript",
  "title",
  "textarea",
  "xmp",
  "noembed",
  "noframes",
];

const DROP_WITH_BODY_ALT = DROP_WITH_BODY.join("|");
/** 同一 html 块内成对出现的开闭标签及其内容。 */
const DROP_ELEMENT_RE = new RegExp(
  `<(${DROP_WITH_BODY_ALT})\\b[^>]*>[\\s\\S]*?<\\/\\1\\s*>`,
  "gi",
);
/** 配对删除后仍残留的开标签：块内没有闭合（闭合在空行之后的另一块，或根本
 *  没有），按 HTML 语义从这里到块末（到空行/文末截止）都是它的内容，整段截掉。 */
const DROP_OPEN_TAG_RE = new RegExp(`<(?:${DROP_WITH_BODY_ALT})\\b[^>]*>`, "i");
const DOCTYPE_BLOCK_RE = /^\s*<!doctype\b/i;

function stripDocumentHtml(state: CoreState) {
  for (const token of state.tokens) {
    if (token.type !== "html_block") continue;
    if (DOCTYPE_BLOCK_RE.test(token.content)) {
      token.content = "";
      continue;
    }
    const stripped = token.content.replace(DROP_ELEMENT_RE, "");
    const unclosed = DROP_OPEN_TAG_RE.exec(stripped);
    token.content = unclosed ? stripped.slice(0, unclosed.index) : stripped;
  }
  return true;
}

/** 非复选框 input 整个删除：正文里不该出现可交互的文本框/按钮型控件。 */
const CHECKBOX_INPUT_RE = /\btype\s*=\s*(?:"checkbox"|'checkbox'|checkbox(?=[\s/>]))/i;

export function installMarkdownHtmlGuard(md: MarkdownIt): void {
  md.use(XSSPlugin, {
    xss: (xss: XssModule) => {
      const whiteList = xss.getDefaultWhiteList();
      whiteList.input = ["type", "checked", "disabled", "class"];
      return {
        whiteList,
        stripIgnoreTag: true,
        onTag(tag: string, html: string): string | undefined {
          if (tag === "input" && !CHECKBOX_INPUT_RE.test(html)) return "";
          return undefined;
        },
        onTagAttr(tag: string, name: string, value: string): string | undefined {
          if (tag === "input" && name === "class" && value !== "task-list-item-checkbox") return "";
          return undefined;
        },
      };
    },
  });
  md.core.ruler.before("xss", "tiangong-strip-document-html", stripDocumentHtml);
}
