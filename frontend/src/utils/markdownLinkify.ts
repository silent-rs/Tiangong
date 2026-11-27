import { config } from "md-editor-rt";
import type LinkifyIt from "linkify-it";
import { installMarkdownHtmlGuard } from "@/utils/markdownHtmlGuard";
import {
  BROWSER_RENDERABLE_EXT_RE,
  isLocalFileUrl,
  toFileUrl,
} from "@/components/message/utils";

/**
 * 消息 Markdown 预览的 markdown-it 全局定制（作用于全部 MdPreview 实例，须在
 * 首个预览挂载前调用，应用入口执行一次）。config() 对 markdownItConfig 是
 * 整体替换而非叠加，裸 HTML 防护与链接修正必须在同一个回调里装配。
 *
 * 0. 裸 HTML 防护：代码块之外的 <style>/<script> 等不进入文档，class/style/
 *    事件属性剥离——见 markdownHtmlGuard.ts。
 *
 * 1. 文件名误识别为链接：linkify 默认开启 fuzzyLink，把「单词.顶级域」
 *    样式的裸词识别为无协议链接——文件后缀大量撞真实 TLD（.rs/.sh/.py/
 *    .name/.status/.io…），slots.rs、manifest.name、session.input-status
 *    均被链化。关闭后无协议前缀不再链化，带 http(s):// 的完整 URL 与
 *    邮箱识别（fuzzyEmail）不受影响。
 *
 * 2. URL 吞并尾部 Markdown 定界符：默认终止字符集（空白、括号引号、
 *    .,;:?!"- 等）不含 Markdown 符号，「https://…\*\*加粗\*\*」的星号
 *    被吞进链接文本与 href（加粗同时失去闭合）；星号后常紧跟中文
 *    （「…\*\*（分支…」到空格才停），仅靠尾部剥离会因末尾是中文而失效。
 *    校验在首个定界符（* ~ `）处截断——它们不存在于真实 URL；下划线
 *    在 URL 中段常见（foo_bar 路径），仅按 GFM autolink 尾随字节集
 *    （?!.,:*_~'）做尾部剥离。https:/ftp: 是 http: 的别名，编译时共
 *    享同一校验函数，一并生效。
 *
 * 3. 本地文件链接按「浏览器可渲染后缀」在渲染层裁决可点击性：模型输出
 *    里显式 Markdown 链接（[相对路径:行号](/绝对/路径.ts)）与 file: 链接
 *    的来源不可控，改写层白名单管不到它们。裁决**不能**放在
 *    validateLink——校验失败时 markdown-it 会把整个 [文本](目标) 语法当
 *    字面文本输出，正文结构被破坏。因此本地路径在 validateLink 一律放行，
 *    在 link_open/link_close 层分流：后缀浏览器可渲染的补正 file:// 形态
 *    并加高亮类，其余降级为不可点击的 <span>（链接文本照常按 Markdown
 *    渲染，仅无点击行为；后续有能打开其他类型的插件时再放开）。远程链接
 *    与危险协议仍走默认实现。
 *
 * 4. 本地文件链接高亮：file: 链接指向本机文件而非网页，行为与外链不同，
 *    link_open 渲染时追加 .md-local-file-link 类名，由样式做差异化高亮。
 *
 * 5. 裸 <a> 标签塌陷：html:true 下模型讨论渲染问题时输出的字面 <a>（无
 *    属性、常无闭合）被当作真实 HTML 开标签，HTML 解析把后续标题、列
 *    表等块级内容整个吞进一个大链接。在 html_inline 渲染层拦截裸开标
 *    签输出为字面文本——判定发生在 markdown-it 内部，代码段划分天然
 *    正确（预处理方案不可行：反引号转义嵌套会让预处理与引擎的代码段
 *    划分不一致）。带属性的真实 <a> 与孤立 </a>（HTML 解析忽略）不受
 *    影响。
 */

/** linkify-it 的 re 在类型上只暴露 RegExp 索引，src_* 模板串与惰性
 * 编译的 http 正则只存在于运行时，此处按实际结构局部断言。 */
type LinkifyRuntimeRe = {
  http?: RegExp;
  src_auth: string;
  src_host_port_strict: string;
  src_path: string;
};

/** markdown-it 解析链接时会把 url 百分号归一化（Windows 反斜杠变
 * %5C、中文变 %E4%xx…），本地路径判断与后缀裁决前先解码还原。
 * 畸形 % 序列解码会抛 URIError，原样返回交由后续默认逻辑处理。 */
function decodeLinkUrl(url: string): string {
  try {
    return decodeURI(url);
  } catch {
    return url;
  }
}

let configured = false;

export function setupMarkdownPreview() {
  if (configured) return;
  configured = true;

  config({
    markdownItConfig(md) {
      installMarkdownHtmlGuard(md);

      // 本地文件链接一律通过校验（见头部注释 3）：校验失败会让 markdown-it
      // 把整个 [文本](目标) 语法当字面文本输出，正文结构被破坏。是否可点击
      // 的裁决下移到 link_open/link_close 渲染层。远程链接与危险协议仍走
      // 默认实现。
      const defaultValidateLink = md.validateLink.bind(md);
      md.validateLink = (url: string) => {
        const trimmed = url.trim();
        if (isLocalFileUrl(decodeLinkUrl(trimmed))) return true;
        return defaultValidateLink(trimmed);
      };

      md.linkify.set({ fuzzyLink: false });

      // 本地文件链接按「浏览器可渲染后缀」裁决可点击性：可渲染的补正
      // file:// 形态（裸绝对路径、盘符误落主机位、反斜杠编码等）并打标记
      // 类名供样式高亮（见 index.css 的 .md-local-file-link）；不可渲染的
      // 降级为不可点击的 <span>——链接文本与内部行内代码照常按 Markdown
      // 渲染，只是没有点击行为（后续有能打开其他类型的插件时再放开）。
      // Markdown 链接不可嵌套，用 env 上的布尔栈把 link_close 与对应
      // link_open 的裁决配对。
      type LocalRefEnv = { __localFileRefStack?: boolean[] };
      const refStack = (env: unknown): boolean[] => {
        const holder = (env ?? {}) as LocalRefEnv;
        holder.__localFileRefStack ??= [];
        return holder.__localFileRefStack;
      };
      const defaultLinkOpen = md.renderer.rules.link_open
        ?? ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
      md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
        const href = tokens[idx].attrGet("href") ?? "";
        const decoded = decodeLinkUrl(href);
        if (href && isLocalFileUrl(decoded)) {
          if (!BROWSER_RENDERABLE_EXT_RE.test(decoded.split(/[?#]/)[0])) {
            refStack(env).push(true);
            return '<span class="md-local-file-ref">';
          }
          tokens[idx].attrSet("href", toFileUrl(decoded));
          tokens[idx].attrJoin("class", "md-local-file-link");
        }
        refStack(env).push(false);
        return defaultLinkOpen(tokens, idx, options, env, self);
      };

      const defaultLinkClose = md.renderer.rules.link_close
        ?? ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
      md.renderer.rules.link_close = (tokens, idx, options, env, self) => {
        if (refStack(env).pop() === true) return "</span>";
        return defaultLinkClose(tokens, idx, options, env, self);
      };

      md.linkify.add("http:", {
        validate(text: string, pos: number, self: LinkifyIt) {
          const re = self.re as unknown as LinkifyRuntimeRe;
          const tail = text.slice(pos);
          if (!re.http) {
            re.http = new RegExp(
              `^\\/\\/${re.src_auth}${re.src_host_port_strict}${re.src_path}`,
              "i",
            );
          }
          const matched = tail.match(re.http);
          if (!matched) return 0;
          // 星号等定界符后常紧跟中文（「…\*\*（分支…」），仅尾部剥离会因
          // 末尾是中文字符整段失效：这类符号不存在于真实 URL，在首个
          // 出现处直接截断；下划线在 URL 中段常见，仅做尾部剥离。
          const mdStop = matched[0].search(/[*~`]/);
          const url = mdStop >= 0 ? matched[0].slice(0, mdStop) : matched[0];
          // GFM autolink 尾随字节集（含下划线），从尾部剥离
          return url.replace(/[?!.,:*_~']+$/, "").length;
        },
      });

      // 裸 <a> 开标签输出为字面文本（见头部注释 5）：阻止未闭合的开标签
      // 在 HTML 解析时吞掉后续块级内容。链式包装保留其余 html_inline
      // 标签的原有渲染。
      const defaultHtmlInline = md.renderer.rules.html_inline;
      md.renderer.rules.html_inline = (tokens, idx, options, env, self) => {
        if (/^<a>$/i.test(tokens[idx].content)) {
          return "&lt;a&gt;";
        }
        return defaultHtmlInline
          ? defaultHtmlInline(tokens, idx, options, env, self)
          : self.renderToken(tokens, idx, options);
      };
    },
  });
}
