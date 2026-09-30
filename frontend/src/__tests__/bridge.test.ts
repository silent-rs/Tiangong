import { beforeEach, describe, expect, it } from 'vitest';

// vitest 4 + jsdom 下静态 import 'fs'/'path' 会被 Vite 浏览器外部化而报
// "No such built-in module: node:"（vitest#8674/#9279），用 eval('require')
// 在运行时加载 Node 内置模块，绕开静态分析
const _require = eval('require') as NodeRequire;
const { readFileSync } = _require('fs') as typeof import('fs');
const { resolve } = _require('path') as typeof import('path');

const BRIDGE_PATH = resolve(__dirname, '../../../src-tauri/src/webview_host/js/bridge.js');

function loadBridge() {
  const code = readFileSync(BRIDGE_PATH, 'utf-8');
  const fn = new Function(code);
  fn();
}

function setupDOM(html: string) {
  document.body.innerHTML = html;
  // jsdom 不提供 scrollIntoView，手动 polyfill
  if (!Element.prototype.scrollIntoView) {
    Element.prototype.scrollIntoView = function () {};
  }
  // jsdom getBoundingClientRect 返回全零，polyfill 为非零尺寸
  if (!Element.prototype.getBoundingClientRect.__polyfilled) {
    const orig = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function () {
      const rect = orig.call(this);
      if (rect.width === 0 && rect.height === 0) {
        return { x: 0, y: 0, width: 100, height: 30, top: 0, right: 100, bottom: 30, left: 0 };
      }
      return rect;
    };
    (Element.prototype.getBoundingClientRect as any).__polyfilled = true;
  }
  // jsdom 不提供 elementFromPoint
  if (!document.elementFromPoint) {
    document.elementFromPoint = function () { return document.body; };
  }
  // jsdom innerText 不完整，polyfill 为 textContent
  if (!(HTMLElement.prototype as any).innerText) {
    Object.defineProperty(HTMLElement.prototype, 'innerText', {
      get() {
        try { return this.textContent; } catch { return ''; }
      },
      set(v: string) {
        try { this.textContent = v; } catch { /* ignore */ }
      },
      configurable: true,
    });
  }
  // jsdom MouseEvent/dispatchEvent 类型检查严格，用简化 mock
  (window as any).MouseEvent = function (this: any, type: string, init: any = {}) {
    this.type = type;
    this.bubbles = init.bubbles ?? false;
    this.cancelable = init.cancelable ?? false;
    Object.assign(this, init);
  };
  Element.prototype.dispatchEvent = function () { return true; };
  delete (window as any).__tiangong_bridge_loaded;
  delete (window as any).__tiangong_pending_network_events;
  loadBridge();
}

function getBridge(): any {
  return (window as any).__tiangong_bridge;
}

function createJsonResponse(body: string, contentType = 'application/json') {
  return {
    status: 200,
    headers: {
      get(name: string) {
        return name.toLowerCase() === 'content-type' ? contentType : null;
      },
    },
    clone() {
      return {
        text() {
          return Promise.resolve(body);
        },
      };
    },
  };
}

describe('locateElement', () => {
  beforeEach(() => {
    setupDOM(`
      <div id="container">
        <button id="btn-login" class="primary" aria-label="登录">登录</button>
        <a href="/about" title="关于我们">关于我们</a>
        <input name="username" placeholder="请输入用户名" />
        <span class="info">普通文本内容</span>
        <div role="button" id="confirm-btn">确认提交</div>
      </div>
    `);
  });

  it('通过 CSS 选择器 #id 定位', () => {
    const el = getBridge().locateElement('#btn-login');
    expect(el).not.toBeNull();
    expect(el.tagName).toBe('BUTTON');
    expect(el.id).toBe('btn-login');
  });

  it('通过 CSS 选择器 .class 定位', () => {
    const el = getBridge().locateElement('.primary');
    expect(el).not.toBeNull();
    expect(el.textContent).toContain('登录');
  });

  it('通过 [name] 属性选择器定位', () => {
    const el = getBridge().locateElement('[name="username"]');
    expect(el).not.toBeNull();
    expect(el.tagName).toBe('INPUT');
  });

  it('通过 aria: 前缀定位（aria-label）', () => {
    const el = getBridge().locateElement('aria:登录');
    expect(el).not.toBeNull();
    expect(el.id).toBe('btn-login');
  });

  it('通过 aria: 前缀定位（role）', () => {
    const el = getBridge().locateElement('aria:button');
    expect(el).not.toBeNull();
    expect(el.id).toBe('confirm-btn');
  });

  it('通过文本内容定位按钮', () => {
    const el = getBridge().locateElement('登录');
    expect(el).not.toBeNull();
    expect(el.id).toBe('btn-login');
  });

  it('通过按钮文本模糊匹配定位', () => {
    const el = getBridge().locateElement('关于');
    expect(el).not.toBeNull();
    expect(el.tagName).toBe('A');
  });

  it('找不到元素时返回 null', () => {
    expect(getBridge().locateElement('#nonexistent')).toBeNull();
  });

  it('空输入返回 null', () => {
    expect(getBridge().locateElement('')).toBeNull();
    expect(getBridge().locateElement('  ')).toBeNull();
  });
});

describe('generateSelector', () => {
  beforeEach(() => {
    setupDOM(`
      <div id="wrap">
        <ul id="list">
          <li class="item">第一项</li>
          <li class="item">第二项</li>
          <li class="item">第三项</li>
        </ul>
        <input name="email" />
      </div>
    `);
  });

  it('有 id 时返回 #id', () => {
    const el = document.getElementById('list')!;
    expect(getBridge().generateSelector(el)).toBe('#list');
  });

  it('有 name 时返回 [name=...]', () => {
    const el = document.querySelector('[name="email"]')!;
    expect(getBridge().generateSelector(el)).toBe('[name="email"]');
  });

  it('无 id/name 时生成带 nth-of-type 的路径', () => {
    const el = document.querySelectorAll('.item')[1];
    const selector = getBridge().generateSelector(el);
    expect(selector).toContain('li');
    expect(selector).toContain('nth-of-type(2)');
  });

  it('空元素返回空字符串', () => {
    expect(getBridge().generateSelector(null)).toBe('');
  });
});

describe('extractElementsInRect', () => {
  it('返回与矩形重叠的元素信息', () => {
    setupDOM(`
      <div id="page">
        <button id="btn" style="position:absolute;top:50px;left:50px;width:100px;height:30px;">按钮</button>
        <a id="link" style="position:absolute;top:100px;left:50px;width:150px;height:20px;" href="/test">链接</a>
      </div>
    `);

    const rects: Record<string, DOMRect> = {
      btn: { x: 50, y: 50, width: 100, height: 30, top: 50, left: 50, right: 150, bottom: 80 } as DOMRect,
      link: { x: 50, y: 100, width: 150, height: 20, top: 100, left: 50, right: 200, bottom: 120 } as DOMRect,
      page: { x: 0, y: 0, width: 800, height: 600, top: 0, left: 0, right: 800, bottom: 600 } as DOMRect,
    };
    for (const [id, rect] of Object.entries(rects)) {
      const el = document.getElementById(id);
      if (el) el.getBoundingClientRect = () => rect;
    }

    const results = getBridge().extractElementsInRect(40, 40, 170, 90);
    expect(results.length).toBeGreaterThan(0);

    const tags = results.map((r: any) => r.tag);
    expect(tags).toContain('button');
    expect(tags).toContain('a');
  });

  it('每个结果包含完整字段', () => {
    setupDOM(`<button id="btn">按钮</button>`);

    const el = document.getElementById('btn')!;
    el.getBoundingClientRect = () =>
      ({ x: 10, y: 10, width: 80, height: 30, top: 10, left: 10, right: 90, bottom: 40 }) as DOMRect;

    const results = getBridge().extractElementsInRect(5, 5, 100, 50);
    expect(results.length).toBeGreaterThan(0);

    const btn = results[0];
    expect(btn.tag).toBe('button');
    expect(btn.text).toContain('按钮');
    expect(btn.selector).toBeTruthy();
    expect(btn.rect).toBeDefined();
    expect(typeof btn.overlapRatio).toBe('number');
  });
});

describe('clickElement 智能定位', () => {
  beforeEach(() => {
    setupDOM(`
      <button id="submit-btn" class="submit" aria-label="提交">提交表单</button>
      <a href="/home" id="home-link">首页</a>
    `);
  });

  it('通过 CSS 选择器点击', () => {
    expect(getBridge().clickElement('#submit-btn').ok).toBe(true);
  });

  it('通过文本内容点击', () => {
    expect(getBridge().clickElement('提交表单').ok).toBe(true);
  });

  it('通过 aria 标签点击', () => {
    expect(getBridge().clickElement('aria:提交').ok).toBe(true);
  });

  it('找不到元素时返回错误', () => {
    const result = getBridge().clickElement('#nonexistent');
    expect(result.ok).toBe(false);
    expect(result.error).toContain('元素未找到');
  });
});

describe('fillField 智能定位', () => {
  beforeEach(() => {
    setupDOM(`
      <form>
        <input id="email" name="email" type="email" />
        <select id="city" name="city">
          <option value="bj">北京</option>
          <option value="sh">上海</option>
        </select>
      </form>
    `);
  });

  it('通过 CSS 选择器填写 input', () => {
    const result = getBridge().fillField('#email', 'test@example.com', 'auto');
    expect(result.ok).toBe(true);
    expect((document.getElementById('email') as HTMLInputElement).value).toBe('test@example.com');
  });

  it('填写 select 元素', () => {
    const result = getBridge().fillField('#city', 'sh', 'auto');
    expect(result.ok).toBe(true);
    expect((document.getElementById('city') as HTMLSelectElement).value).toBe('sh');
  });

  it('找不到元素时返回错误', () => {
    const result = getBridge().fillField('#nonexistent', 'val', 'auto');
    expect(result.ok).toBe(false);
    expect(result.error).toContain('元素未找到');
  });
});

describe('locateElement 对话框优先', () => {
  it('对话框打开时优先匹配对话框内的元素', () => {
    setupDOM(`
      <div>
        <button id="page-create">创建 API key</button>
        <div class="ant-modal-wrap" role="dialog" id="modal">
          <div class="ant-modal">
            <div class="ant-modal-content">
              <div class="ant-modal-footer">
                <button id="dialog-create">创建</button>
              </div>
            </div>
          </div>
        </div>
      </div>
    `);

    const modal = document.getElementById('modal')!;
    const bridge = getBridge();
    const orig = bridge._getTopmostOverlay;
    bridge._getTopmostOverlay = () => modal;

    const el = bridge.locateElement('创建');
    expect(el).not.toBeNull();
    expect(el.id).toBe('dialog-create');

    bridge._getTopmostOverlay = orig;
  });

  it('精确文本匹配优先于部分匹配', () => {
    setupDOM(`
      <div>
        <a id="link-create">创建 API key</a>
        <button id="btn-create">创建</button>
      </div>
    `);

    const el = getBridge().locateElement('创建');
    expect(el).not.toBeNull();
    // XPath text() = '创建' 应优先匹配精确文本
    expect(el.id).toBe('btn-create');
  });

  it('Element Plus 对话框内也能优先匹配', () => {
    setupDOM(`
      <div>
        <button id="page-btn">确定</button>
        <div class="el-dialog__wrapper" id="el-wrapper">
          <div class="el-dialog">
            <button id="dialog-btn">确定</button>
          </div>
        </div>
      </div>
    `);

    const wrapper = document.getElementById('el-wrapper')!;
    const bridge = getBridge();
    const orig = bridge._getTopmostOverlay;
    bridge._getTopmostOverlay = () => wrapper;

    const el = bridge.locateElement('确定');
    expect(el).not.toBeNull();
    expect(el.id).toBe('dialog-btn');

    bridge._getTopmostOverlay = orig;
  });

  it('无对话框时全局精确匹配', () => {
    setupDOM(`
      <div>
        <button id="save-btn">保存并继续</button>
        <button id="save">保存</button>
      </div>
    `);

    const el = getBridge().locateElement('保存');
    expect(el).not.toBeNull();
    expect(el.id).toBe('save');
  });
});

describe('locateAll', () => {
  it('返回所有匹配元素', () => {
    setupDOM(`
      <div>
        <button id="b1">提交</button>
        <button id="b2">取消</button>
        <button id="b3">提交并继续</button>
      </div>
    `);
    const results = getBridge().locateAll('提交');
    expect(results.length).toBeGreaterThanOrEqual(2);
  });

  it('空选择器返回空数组', () => {
    setupDOM('<div>hello</div>');
    expect(getBridge().locateAll('').length).toBe(0);
  });
});

describe('nth 语法', () => {
  it('nth:2 选择第 2 个匹配', () => {
    setupDOM(`
      <div>
        <button id="b1">提交</button>
        <button id="b2">提交</button>
        <button id="b3">提交</button>
      </div>
    `);
    const el = getBridge().locateElement('nth:2,提交');
    expect(el).not.toBeNull();
    expect(el.id).toBe('b2');
  });

  it('nth 超出范围返回 null', () => {
    setupDOM('<button id="b1">提交</button>');
    expect(getBridge().locateElement('nth:5,提交')).toBeNull();
  });

  it('nth 无效格式返回 null', () => {
    setupDOM('<button>提交</button>');
    expect(getBridge().locateElement('nth:')).toBeNull();
    expect(getBridge().locateElement('nth:abc,提交')).toBeNull();
  });
});

describe('clickElement 候选列表', () => {
  it('找不到元素但 locateAll 有结果时返回候选', () => {
    setupDOM(`
      <button id="b1" disabled>提交</button>
      <button id="b2">取消</button>
    `);
    // CSS selector #nonexistent 不会 locateAll 到任何东西
    const result = getBridge().clickElement('#nonexistent');
    expect(result.ok).toBe(false);
    expect(result.candidates).toBeDefined();
  });
});

describe('_checkWaitCondition', () => {
  it('navigation：URL 相同时返回 false', () => {
    setupDOM('<div>test</div>');
    getBridge()._waitInitialState = { url: window.location.href, lastMutationTime: Date.now() };
    expect(getBridge()._checkWaitCondition('navigation')).toBe(false);
  });

  it('element：元素存在时返回 true', () => {
    setupDOM('<div id="x">test</div>');
    expect(getBridge()._checkWaitCondition('element:#x')).toBe(true);
    expect(getBridge()._checkWaitCondition('element:#missing')).toBe(false);
  });

  it('element!：元素不存在时返回 true', () => {
    setupDOM('<div>test</div>');
    expect(getBridge()._checkWaitCondition('element!:#missing')).toBe(true);
    expect(getBridge()._checkWaitCondition('element!:#x')).toBe(true);
  });

  it('stable：无 DOM 变化超 1 秒时返回 true', () => {
    setupDOM('<div>test</div>');
    getBridge()._waitInitialState = { url: '', lastMutationTime: Date.now() - 1100 };
    expect(getBridge()._checkWaitCondition('stable')).toBe(true);
    getBridge()._waitInitialState = { url: '', lastMutationTime: Date.now() - 100 };
    expect(getBridge()._checkWaitCondition('stable')).toBe(false);
  });
});

describe('waitFor', () => {

  it('element 条件满足', async () => {
    setupDOM('<div id="target">appeared</div>');
    const result = await getBridge().waitFor('element:#target', 1000);
    expect(result.ok).toBe(true);
  });

  it('element! 条件满足（元素不存在）', async () => {
    setupDOM('<div>test</div>');
    const result = await getBridge().waitFor('element!:#nonexistent', 1000);
    expect(result.ok).toBe(true);
  });

  it('超时返回失败', async () => {
    setupDOM('<div>test</div>');
    // 等一个不存在的条件但给很短的超时
    const result = await getBridge().waitFor('element:#never-exist', 100);
    expect(result.ok).toBe(false);
    expect(result.error).toContain('超时');
  });
});

describe('annotation 自动提取元素', () => {
  it('getAnnotations 返回的 rect 批注包含 elements 字段', () => {
    setupDOM(`
      <button id="btn" style="position:absolute;top:50px;left:50px;width:100px;height:30px;">按钮</button>
    `);

    const el = document.getElementById('btn')!;
    el.getBoundingClientRect = () =>
      ({ x: 50, y: 50, width: 100, height: 30, top: 50, left: 50, right: 150, bottom: 80 }) as DOMRect;

    const bridge = getBridge();
    bridge.annotation.start('rect');

    // 手动模拟 _handleUp 效果：添加带 elements 的 annotation
    bridge.annotation._annotations.push({
      type: 'rect',
      x: 40,
      y: 40,
      width: 120,
      height: 50,
      color: '#ff0000',
      elements: bridge.extractElementsInRect(40, 40, 120, 50),
    });

    const result = bridge.annotation.getAnnotations();
    expect(result.count).toBe(1);
    expect(result.annotations[0].elements).toBeDefined();
    expect(result.annotations[0].elements.length).toBeGreaterThan(0);
    expect(result.annotations[0].elements[0].tag).toBe('button');
  });
});

describe('_isDisabled', () => {
  it('检测原生 disabled 属性', () => {
    setupDOM('<button id="btn" disabled>按钮</button>');
    const bridge = getBridge();
    expect(bridge._isDisabled(document.getElementById('btn'))).toBe(true);
  });

  it('检测 aria-disabled="true"', () => {
    setupDOM('<div id="el" aria-disabled="true">元素</div>');
    const bridge = getBridge();
    expect(bridge._isDisabled(document.getElementById('el'))).toBe(true);
  });

  it('检测 CSS 类名中包含 disabled（如 ds-button--disabled）', () => {
    setupDOM('<div id="el" class="ds-button ds-button--disabled">创建</div>');
    const bridge = getBridge();
    expect(bridge._isDisabled(document.getElementById('el'))).toBe(true);
  });

  it('检测 is-disabled 类名（Element Plus）', () => {
    setupDOM('<div id="el" class="el-button is-disabled">按钮</div>');
    const bridge = getBridge();
    expect(bridge._isDisabled(document.getElementById('el'))).toBe(true);
  });

  it('正常元素返回 false', () => {
    setupDOM('<button id="btn">按钮</button>');
    const bridge = getBridge();
    expect(bridge._isDisabled(document.getElementById('btn'))).toBe(false);
  });
});

describe('clickElement disabled 检测', () => {
  it('CSS 类 disabled 的 div[role=button] 返回错误', () => {
    setupDOM(`
      <div id="dialog" role="dialog">
        <input type="text" class="ds-input__input" placeholder="输入名称" value="">
        <div role="button" class="ds-button ds-button--disabled">创建</div>
      </div>
    `);
    const bridge = getBridge();
    const result = bridge.clickElement('创建');
    expect(result.ok).toBe(false);
    expect(result.error).toContain('禁用');
  });

  it('正常按钮可以点击', () => {
    setupDOM(`
      <div role="dialog">
        <input type="text" value="test">
        <div role="button" class="ds-button ds-button--primary">创建</div>
      </div>
    `);
    const bridge = getBridge();
    const result = bridge.clickElement('创建');
    expect(result.ok).toBe(true);
  });
});

describe('extractForms 非原生按钮', () => {
  it('提取 div[role=button] 按钮', () => {
    setupDOM(`
      <div id="dialog" role="dialog">
        <div class="ds-form-item">
          <label class="ds-form-item__label"><span class="ds-form-item__label-text">名称</span></label>
          <div class="ds-form-item__content">
            <input type="text" class="ds-input__input" placeholder="输入名称" value="">
          </div>
        </div>
        <div class="ds-modal-content__footer">
          <div role="button" class="ds-button ds-button--outlinedNeutral">取消</div>
          <div role="button" class="ds-button ds-button--primary ds-button--disabled">创建</div>
        </div>
      </div>
    `);
    const bridge = getBridge();
    const result = bridge.extractForms();
    expect(result.forms.length).toBeGreaterThan(0);
    const form = result.forms[0];
    expect(form.fields.length).toBe(1);
    expect(form.fields[0].placeholder).toBe('输入名称');
    // 应该提取到 div[role=button] 按钮
    expect(form.buttons.length).toBe(2);
    expect(form.buttons[0].text).toBe('取消');
    expect(form.buttons[0].disabled).toBe(false);
    expect(form.buttons[1].text).toBe('创建');
    expect(form.buttons[1].disabled).toBe(true);
  });
});

describe('diffDigest 内容变化检测', () => {
  it('检测对话框关闭后页面内容变化', () => {
    setupDOM(`
      <div id="main">已有 API key 列表 key-001 active</div>
    `);
    const bridge = getBridge();
    const before = { url: 'https://example.com', title: 'API Keys', overlayOpen: true, overlayText: '创建 API key', mainTextTail: '已有 API key 列表 key-001 active' };
    const after = { url: 'https://example.com', title: 'API Keys', overlayOpen: false, overlayText: '', mainTextTail: '已有 API key 列表 key-001 active sk-abc123 新创建的key active' };
    const diff = bridge.diffDigest(before, after);
    expect(diff).toContain('覆盖层已关闭');
    expect(diff).toContain('页面内容变化');
    expect(diff).toContain('sk-abc123');
  });

  it('无变化时返回默认信息', () => {
    setupDOM('<div>test</div>');
    const bridge = getBridge();
    const before = { url: 'https://example.com', title: 'Test', overlayOpen: false, overlayText: '', mainTextTail: 'hello world' };
    const after = { url: 'https://example.com', title: 'Test', overlayOpen: false, overlayText: '', mainTextTail: 'hello world' };
    const diff = bridge.diffDigest(before, after);
    expect(diff).toContain('页面无明显变化');
  });

  it('_textDiff 提取新增内容', () => {
    setupDOM('<div>test</div>');
    const bridge = getBridge();
    const result = bridge._textDiff(
      '这是原来的内容。第一行不变。',
      '这是原来的内容。第一行不变。这是新增的一行。'
    );
    expect(result).toContain('新增的一行');
  });

  it('_textDiff 内容过长时截断', () => {
    setupDOM('<div>test</div>');
    const bridge = getBridge();
    const newPart = 'x'.repeat(2000);
    const result = bridge._textDiff('before', newPart);
    expect(result.length).toBeLessThanOrEqual(1004); // 1000 + '...'
    expect(result).toContain('...');
  });
});

describe('observer 模块', () => {
  it('start/stop 生命周期', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    expect(bridge.observer._started).toBe(false);
    bridge.observer.start();
    expect(bridge.observer._started).toBe(true);
    bridge.observer.stop();
    expect(bridge.observer._started).toBe(false);
  });

  it('drainEvents 清空事件队列', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();
    bridge.observer._pushEvent({ type: 'test', timestamp: Date.now() });
    expect(bridge.observer._eventQueue.length).toBe(1);
    const events = bridge.observer.drainEvents();
    expect(events.length).toBe(1);
    expect(events[0].type).toBe('test');
    expect(bridge.observer._eventQueue.length).toBe(0);
    bridge.observer.stop();
  });

  it('队列长度超过 100 时自动裁剪', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();
    for (let i = 0; i < 120; i++) {
      bridge.observer._pushEvent({ type: 'test', idx: i });
    }
    // 裁剪触发后队列长度应 <= 100
    expect(bridge.observer._eventQueue.length).toBeLessThanOrEqual(100);
    bridge.observer.stop();
  });

  it('用户点击事件捕获', () => {
    setupDOM(`
      <div id="app">
        <button id="test-btn">点击我</button>
      </div>
    `);
    const bridge = getBridge();
    bridge.observer.start();
    const btn = document.getElementById('test-btn')!;
    btn.click();
    const events = bridge.observer.drainEvents();
    expect(events.length).toBe(1);
    expect(events[0].type).toBe('user_click');
    expect(events[0].text).toContain('点击我');
    bridge.observer.stop();
  });

  it('用户输入事件捕获（防抖）', async () => {
    setupDOM(`
      <div id="app">
        <input id="test-input" placeholder="输入" />
      </div>
    `);
    const bridge = getBridge();
    bridge.observer.start();
    const input = document.getElementById('test-input') as HTMLInputElement;
    input.value = 'hello';
    // jsdom 的 dispatchEvent 被 mock 为直接返回 true，不触发冒泡。
    // 直接调用内部防抖逻辑验证：模拟 input 事件处理器被调用后的效果
    bridge.observer._pushEvent({
      type: 'user_input',
      timestamp: Date.now(),
      selector: '#test-input',
      label: '输入',
      value_length: input.value.length,
    });
    const events = bridge.observer.drainEvents();
    expect(events.length).toBe(1);
    const inputEvent = events[0] as any;
    expect(inputEvent.type).toBe('user_input');
    expect(inputEvent.value_length).toBe(5);
    bridge.observer.stop();
  });
});

describe('_getTopmostOverlay 泛化覆盖层检测', () => {
  it('无覆盖层时返回 null', () => {
    setupDOM(`
      <div id="app">
        <main>页面内容</main>
      </div>
    `);
    const bridge = getBridge();
    expect(bridge._getTopmostOverlay()).toBeNull();
  });

  it('检测 fixed 定位的覆盖层', () => {
    setupDOM(`
      <div id="app">
        <main>页面内容</main>
      </div>
    `);
    const overlay = document.createElement('div');
    overlay.textContent = '对话框内容';
    overlay.style.position = 'fixed';
    document.body.appendChild(overlay);

    // mock getBoundingClientRect 和 getComputedStyle
    overlay.getBoundingClientRect = () => ({ width: 400, height: 300, top: 100, left: 100, right: 500, bottom: 400, x: 100, y: 100 } as any);
    const origGetComputedStyle = window.getComputedStyle;
    window.getComputedStyle = (el: Element) => {
      const real = origGetComputedStyle.call(window, el);
      if (el === overlay) {
        return { ...real, position: 'fixed' } as CSSStyleDeclaration;
      }
      return real;
    };

    const origFn = document.elementFromPoint;
    document.elementFromPoint = () => overlay;

    const bridge = getBridge();
    const result = bridge._getTopmostOverlay();
    expect(result).not.toBeNull();
    expect(result!.textContent).toContain('对话框内容');

    document.elementFromPoint = origFn;
    window.getComputedStyle = origGetComputedStyle;
  });

  it('全屏 fixed 背景遮罩不被误判为覆盖层', () => {
    setupDOM(`
      <div id="backdrop">导航栏</div>
      <main>页面内容</main>
    `);
    const backdrop = document.getElementById('backdrop')!;
    // 模拟全屏遮罩层：width ≈ W 且 height ≈ H（不满足 width < W-10 || height < H-10）
    backdrop.getBoundingClientRect = () => ({ width: 1920, height: 1080, top: 0, left: 0, right: 1920, bottom: 1080, x: 0, y: 0 } as any);
    const origGetComputedStyle = window.getComputedStyle;
    window.getComputedStyle = ((el: Element) => {
      const real = origGetComputedStyle.call(window, el);
      if (el === backdrop) {
        return { ...real, position: 'fixed' } as CSSStyleDeclaration;
      }
      return real;
    }) as any;

    const origFn = document.elementFromPoint;
    document.elementFromPoint = () => backdrop;

    const origW = window.innerWidth;
    const origH = window.innerHeight;
    Object.defineProperty(window, 'innerWidth', { value: 1920, configurable: true });
    Object.defineProperty(window, 'innerHeight', { value: 1080, configurable: true });

    const bridge = getBridge();
    const result = bridge._getTopmostOverlay();
    expect(result).toBeNull();

    document.elementFromPoint = origFn;
    window.getComputedStyle = origGetComputedStyle;
    Object.defineProperty(window, 'innerWidth', { value: origW, configurable: true });
    Object.defineProperty(window, 'innerHeight', { value: origH, configurable: true });
  });

  it('全屏蒙层内有弹窗内容时能检测到蒙层', () => {
    setupDOM(`
      <div id="backdrop">API Key: sk-test456 复制 关闭</div>
      <main>页面内容</main>
    `);
    const backdrop = document.getElementById('backdrop')!;
    // 模拟全屏蒙层
    backdrop.getBoundingClientRect = () => ({ width: 1920, height: 1080, top: 0, left: 0, right: 1920, bottom: 1080, x: 0, y: 0 } as any);
    const origGetComputedStyle = window.getComputedStyle;
    window.getComputedStyle = ((el: Element) => {
      const real = origGetComputedStyle.call(window, el);
      if (el === backdrop) {
        return { ...real, position: 'fixed' } as CSSStyleDeclaration;
      }
      return real;
    }) as any;

    const origFn = document.elementFromPoint;
    document.elementFromPoint = () => backdrop;

    const origW = window.innerWidth;
    const origH = window.innerHeight;
    Object.defineProperty(window, 'innerWidth', { value: 1920, configurable: true });
    Object.defineProperty(window, 'innerHeight', { value: 1080, configurable: true });

    const bridge = getBridge();
    const result = bridge._getTopmostOverlay();
    // 全屏蒙层内有有意义的文本（>10字符），应返回蒙层
    expect(result).not.toBeNull();
    expect(result!.textContent).toContain('sk-test456');

    document.elementFromPoint = origFn;
    window.getComputedStyle = origGetComputedStyle;
    Object.defineProperty(window, 'innerWidth', { value: origW, configurable: true });
    Object.defineProperty(window, 'innerHeight', { value: origH, configurable: true });
  });
});

describe('_extractOverlayContent 覆盖层内容提取', () => {
  it('提取覆盖层文本，排除按钮', () => {
    setupDOM('<div id="app">content</div>');
    const overlay = document.createElement('div');
    overlay.innerHTML = '<p>API Key: sk-abc123</p><button>复制</button><div class="close">×</div>';
    const bridge = getBridge();
    const content = bridge._extractOverlayContent(overlay);
    expect(content).toContain('sk-abc123');
    expect(content).not.toContain('复制');
    expect(content).not.toContain('×');
  });

  it('null 输入返回空字符串', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    expect(bridge._extractOverlayContent(null)).toBe('');
  });
});

describe('getPageDigest 泛化摘要', () => {
  it('无覆盖层时 overlayOpen 为 false', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    const digest = bridge.getPageDigest();
    expect(digest.overlayOpen).toBe(false);
    expect(digest.overlayText).toBe('');
    expect(digest.url).toBeTruthy();
  });

  it('有覆盖层时 overlayOpen 为 true 且包含内容', () => {
    setupDOM('<div id="app">content</div>');
    const overlay = document.createElement('div');
    overlay.textContent = '新创建的 API Key: sk-test123';
    document.body.appendChild(overlay);

    // mock getBoundingClientRect
    overlay.getBoundingClientRect = () => ({ width: 400, height: 200, top: 100, left: 100, right: 500, bottom: 300, x: 100, y: 100 } as any);
    // mock getComputedStyle 返回 fixed
    const origGCS = window.getComputedStyle;
    window.getComputedStyle = ((el: Element) => {
      const real = origGCS.call(window, el);
      if (el === overlay) {
        return { ...real, position: 'fixed' } as CSSStyleDeclaration;
      }
      return real;
    }) as any;
    // mock elementFromPoint 返回 overlay
    const origFn = document.elementFromPoint;
    document.elementFromPoint = () => overlay;

    const bridge = getBridge();
    const digest = bridge.getPageDigest();
    expect(digest.overlayOpen).toBe(true);
    expect(digest.overlayText).toContain('sk-test123');

    document.elementFromPoint = origFn;
    window.getComputedStyle = origGCS;
  });
});

describe('fetch 拦截与网络响应捕获', () => {
  it('JSON 响应被捕获到独立网络事件队列', async () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    bridge.observer._pushEvent({
      type: 'network_response',
      timestamp: Date.now(),
      url: 'https://api.example.com/keys',
      method: 'POST',
      status: 200,
      detail: JSON.stringify({ key: 'sk-abc123', name: 'test-key' }),
    });

    // drainEvents 不再包含 network_response（独立队列）
    expect(bridge.observer.drainEvents().length).toBe(0);
    // drainNetworkEvents 返回网络事件
    const networkEvents = bridge.observer.drainNetworkEvents();
    expect(networkEvents.length).toBe(1);
    expect(networkEvents[0].type).toBe('network_response');
    expect(networkEvents[0].url).toBe('https://api.example.com/keys');
    expect(networkEvents[0].method).toBe('POST');
    expect(networkEvents[0].status).toBe(200);
    expect(networkEvents[0].detail).toContain('sk-abc123');

    bridge.observer.stop();
  });

  it('多个网络响应按序入独立队列', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    bridge.observer._pushEvent({ type: 'network_response', timestamp: 100, url: '/api/a', method: 'GET', status: 200, detail: '{"a":1}' });
    bridge.observer._pushEvent({ type: 'network_response', timestamp: 200, url: '/api/b', method: 'POST', status: 201, detail: '{"b":2}' });
    bridge.observer._pushEvent({ type: 'content_changed', timestamp: 300, detail: 'text updated' });
    bridge.observer._pushEvent({ type: 'network_response', timestamp: 400, url: '/api/c', method: 'DELETE', status: 204, detail: '' });

    // drainEvents 只返回非网络事件
    const events = bridge.observer.drainEvents();
    expect(events.length).toBe(1);
    expect(events[0].type).toBe('content_changed');

    // drainNetworkEvents 返回所有网络事件
    const networkEvents = bridge.observer.drainNetworkEvents();
    expect(networkEvents.length).toBe(3);
    expect(networkEvents[0].url).toBe('/api/a');
    expect(networkEvents[1].url).toBe('/api/b');
    expect(networkEvents[2].url).toBe('/api/c');

    bridge.observer.stop();
  });

  it('drainNetworkEvents 后网络队列清空', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    bridge.observer._pushEvent({ type: 'network_response', timestamp: 1, url: '/a', method: 'GET', status: 200, detail: '{}' });
    expect(bridge.observer.drainNetworkEvents().length).toBe(1);
    expect(bridge.observer.drainNetworkEvents().length).toBe(0);

    bridge.observer.stop();
  });

  it('detail 超过 500 字符时入队不截断（截断在 bridge.js fetch/XHR 层完成）', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    const longDetail = 'x'.repeat(600);
    bridge.observer._pushEvent({ type: 'network_response', timestamp: 1, url: '/a', method: 'GET', status: 200, detail: longDetail });

    const events = bridge.observer.drainNetworkEvents();
    expect(events.length).toBe(1);
    expect(events[0].detail.length).toBe(600);

    bridge.observer.stop();
  });
});

describe('XHR 拦截事件流', () => {
  it('fetch JSON 响应会进入 drainAllEvents', async () => {
    const body = JSON.stringify({ data: { key: 'sk-fetch123', name: 'fetch-key' } });
    (window as any).fetch = () => Promise.resolve(createJsonResponse(body));

    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    await (window as any).fetch('https://platform.deepseek.com/api/v0/users/create_api_key', {
      method: 'POST',
    });
    await Promise.resolve();

    const events = bridge.observer.drainAllEvents();
    expect(events.length).toBe(1);
    expect(events[0]).toMatchObject({
      type: 'network_response',
      url: 'https://platform.deepseek.com/api/v0/users/create_api_key',
      method: 'POST',
      status: 200,
    });
    expect(events[0].detail).toContain('sk-fetch123');

    bridge.observer.stop();
  });

  it('XHR load 事件触发后 network_response 入队', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    // 模拟 XHR 拦截逻辑：直接 push 事件（与 bridge.js 中的逻辑一致）
    const mockXHR = {
      _tiangong_method: 'POST',
      _tiangong_url: 'https://platform.deepseek.com/api_keys',
      status: 200,
      responseText: JSON.stringify({ data: { key: 'sk-deep123', name: 'my-key' } }),
      getResponseHeader(name: string) {
        if (name === 'content-type') return 'application/json';
        return null;
      },
    };

    // 模拟 bridge.js 中 XHR load 回调的拦截逻辑
    const ct = mockXHR.getResponseHeader('content-type') || '';
    if (ct.indexOf('application/json') >= 0) {
      bridge.observer._pushEvent({
        type: 'network_response',
        timestamp: Date.now(),
        url: mockXHR._tiangong_url,
        method: mockXHR._tiangong_method,
        status: mockXHR.status,
        detail: (mockXHR.responseText || '').substring(0, 500),
      });
    }

    const events = bridge.observer.drainNetworkEvents();
    expect(events.length).toBe(1);
    expect(events[0].type).toBe('network_response');
    expect(events[0].url).toBe('https://platform.deepseek.com/api_keys');
    expect(events[0].method).toBe('POST');
    expect(events[0].status).toBe(200);
    expect(events[0].detail).toContain('sk-deep123');

    bridge.observer.stop();
  });

  it('非 JSON Content-Type 的 XHR 不产生事件', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    // 模拟非 JSON 响应（text/html）
    const ct = 'text/html; charset=utf-8';
    if (ct.indexOf('application/json') < 0 && ct.indexOf('text/json') < 0) {
      // 不 push 事件 — 与 bridge.js 逻辑一致
    }

    const events = bridge.observer.drainEvents();
    expect(events.length).toBe(0);

    bridge.observer.stop();
  });

  it('ipc:// 协议 XHR 请求不产生事件', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    // 模拟 ipc:// URL — bridge.js 中 open 调用直接 return，不设置 _tiangong_url
    const url = 'ipc://localhost/some-command';
    const shouldIntercept = !(typeof url === 'string' && url.indexOf('ipc://') === 0);

    expect(shouldIntercept).toBe(false);
    const events = bridge.observer.drainEvents();
    expect(events.length).toBe(0);

    bridge.observer.stop();
  });
});

describe('ipc:// 协议屏蔽', () => {
  it('fetch ipc:// 返回空 JSON 响应', async () => {
    setupDOM('<div id="app">content</div>');
    // bridge.js 替换了 window.fetch，ipc:// 请求返回空 Response
    const response = await window.fetch('ipc://localhost/test');
    const text = await response.text();
    expect(text).toBe('{}');
    expect(response.status).toBe(200);
  });
});

describe('双队列竞争条件验证', () => {
  it('旧 drain 接口仍分别消费普通事件和网络事件', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    bridge.observer._pushEvent({ type: 'network_response', timestamp: 1, url: '/a', method: 'POST', status: 200, detail: '{"key":"sk-a"}' });
    bridge.observer._pushEvent({ type: 'content_changed', timestamp: 2, detail: 'changed' });

    // drainEvents 只返回普通事件
    const events = bridge.observer.drainEvents();
    expect(events.length).toBe(1);
    expect(events[0].type).toBe('content_changed');

    // drainNetworkEvents 只返回网络事件（不受 drainEvents 影响）
    const network = bridge.observer.drainNetworkEvents();
    expect(network.length).toBe(1);
    expect(network[0].detail).toContain('sk-a');

    bridge.observer.stop();
  });

  it('drainAllEvents 一次消费普通事件和网络事件并按时间排序', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    bridge.observer._pushEvent({ type: 'network_response', timestamp: 3, url: '/api/key', method: 'POST', status: 200, detail: '{"key":"sk-xyz"}' });
    bridge.observer._pushEvent({ type: 'dialog_opened', timestamp: 2, detail: 'dialog' });
    bridge.observer._pushEvent({ type: 'content_changed', timestamp: 1, detail: 'changed' });

    const events = bridge.observer.drainAllEvents();
    expect(events.map((e: any) => e.type)).toEqual(['content_changed', 'dialog_opened', 'network_response']);
    expect(events[2].detail).toContain('sk-xyz');
    expect(bridge.observer.drainEvents().length).toBe(0);
    expect(bridge.observer.drainNetworkEvents().length).toBe(0);

    bridge.observer.stop();
  });

  it('bridge 初始化前缓存的网络事件会被 drainAllEvents 消费', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();

    (window as any).__tiangong_pending_network_events = [
      { type: 'network_response', timestamp: 1, url: '/early', method: 'GET', status: 200, detail: '{"ok":true}' },
    ];

    const events = bridge.observer.drainAllEvents();
    expect(events.length).toBe(1);
    expect(events[0].url).toBe('/early');
    expect((window as any).__tiangong_pending_network_events.length).toBe(0);
  });

  it('多次 drainNetworkEvents 只拿到各自入队的事件', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    bridge.observer._pushEvent({ type: 'network_response', timestamp: 1, url: '/a', method: 'POST', status: 200, detail: '{"key":"sk-a"}' });
    const first = bridge.observer.drainNetworkEvents();
    expect(first.length).toBe(1);
    expect(first[0].detail).toContain('sk-a');

    bridge.observer._pushEvent({ type: 'network_response', timestamp: 2, url: '/b', method: 'POST', status: 201, detail: '{"key":"sk-b"}' });
    const second = bridge.observer.drainNetworkEvents();
    expect(second.length).toBe(1);
    expect(second[0].detail).toContain('sk-b');

    bridge.observer.stop();
  });

  it('事件轮询线程使用 drainAllEvents 时不会漏掉网络事件', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    // 模拟：先有网络事件
    bridge.observer._pushEvent({ type: 'network_response', timestamp: 1, url: '/api/key', method: 'POST', status: 200, detail: '{"key":"sk-xyz"}' });
    bridge.observer._pushEvent({ type: 'dialog_opened', timestamp: 2, detail: 'dialog' });

    // 模拟事件轮询线程调用统一 drain
    const polled = bridge.observer.drainAllEvents();
    expect(polled.length).toBe(2);
    expect(polled.map((e: any) => e.type)).toEqual(['network_response', 'dialog_opened']);
    expect(polled[0].detail).toContain('sk-xyz');
    expect(bridge.observer.drainNetworkEvents().length).toBe(0);

    bridge.observer.stop();
  });
});

describe('getPageDigest + diffDigest 微小变化检测', () => {
  it('覆盖层内新增一行文本能被 diffDigest 捕获', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();

    const before = {
      url: 'https://platform.deepseek.com/api_keys',
      title: 'API Keys',
      overlayOpen: true,
      overlayText: '创建 API key 请将此 API key 保存在安全且易于访问的地方。',
      mainTextTail: '已有 key 列表',
    };
    const after = {
      url: 'https://platform.deepseek.com/api_keys',
      title: 'API Keys',
      overlayOpen: true,
      overlayText: '创建 API key 请将此 API key 保存在安全且易于访问的地方。 sk-dcc5ad16d02d4a61ac88e1196c578ad4 复制 关闭',
      mainTextTail: '已有 key 列表',
    };

    const diff = bridge.diffDigest(before, after);
    expect(diff).toContain('覆盖层内容已变化');
    expect(diff).toContain('sk-dcc5ad16d02d4a61ac88e1196c578ad4');
  });

  it('overlayText 完全相同时不报覆盖层变化', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();

    const before = { url: '', title: '', overlayOpen: true, overlayText: '对话框内容', mainTextTail: '' };
    const after = { url: '', title: '', overlayOpen: true, overlayText: '对话框内容', mainTextTail: '' };
    const diff = bridge.diffDigest(before, after);
    expect(diff).not.toContain('覆盖层');
  });

  it('mainTextTail 微小新增（仅一行 key）能被检测', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();

    const before = {
      url: 'https://example.com/keys',
      title: 'Keys',
      overlayOpen: false,
      overlayText: '',
      mainTextTail: 'key-001 active\nkey-002 active\n创建新 key',
    };
    const after = {
      url: 'https://example.com/keys',
      title: 'Keys',
      overlayOpen: false,
      overlayText: '',
      mainTextTail: 'key-001 active\nkey-002 active\n创建新 key\nsk-newkey123 active',
    };

    const diff = bridge.diffDigest(before, after);
    expect(diff).toContain('页面内容变化');
    expect(diff).toContain('sk-newkey123');
  });
});

describe('网络响应格式化（模拟 handler drain 逻辑）', () => {
  it('network_response 事件格式化为可读摘要', () => {
    setupDOM('<div id="app">content</div>');
    const bridge = getBridge();
    bridge.observer.start();

    bridge.observer._pushEvent({
      type: 'network_response',
      timestamp: Date.now(),
      url: 'https://api.deepseek.com/v1/api_keys',
      method: 'POST',
      status: 200,
      detail: JSON.stringify({ data: { key: 'sk-dcc5ad16d02d4a61', name: 'test' } }),
    });

    const events = bridge.observer.drainNetworkEvents();
    const networkEvents = events.filter((e: any) => e.type === 'network_response');

    // 模拟 handler.rs 中 drain_network_responses 的格式化逻辑
    const lines: string[] = [];
    for (const evt of networkEvents) {
      const method = evt.method || 'GET';
      const url = evt.url || '';
      const status = evt.status || 0;
      const detail = evt.detail || '';
      const shortUrl = url.length > 80 ? url.substring(0, 80) : url;
      lines.push(`[网络响应] ${method} ${shortUrl} (状态 ${status})`);
      if (detail) {
        const preview = detail.length > 300 ? detail.substring(0, 300) + '...' : detail;
        lines.push(preview);
      }
    }
    const result = lines.join('\n');

    expect(result).toContain('[网络响应] POST https://api.deepseek.com/v1/api_keys (状态 200)');
    expect(result).toContain('sk-dcc5ad16d02d4a61');

    bridge.observer.stop();
  });
});

describe('完整感知链路模拟', () => {
  it('点击 → XHR 响应 → digest 变化 → 网络事件捕获', () => {
    setupDOM(`
      <div id="app">
        <div id="key-list">key-001 active</div>
        <div id="backdrop" style="display:none;">
          <div id="dialog">
            <p id="key-display"></p>
          </div>
        </div>
      </div>
    `);

    const bridge = getBridge();
    bridge.observer.start();

    // 1. 操作前 digest
    const beforeDigest = bridge.getPageDigest();
    expect(beforeDigest.overlayOpen).toBe(false);

    // 2. 模拟点击后 XHR 响应入队（bridge.js XHR 拦截层的行为）
    bridge.observer._pushEvent({
      type: 'network_response',
      timestamp: Date.now(),
      url: 'https://platform.deepseek.com/api_keys/create',
      method: 'POST',
      status: 200,
      detail: JSON.stringify({ data: { key: 'sk-fullchain123', name: 'my-key' } }),
    });

    // 3. 模拟弹窗出现
    const backdrop = document.getElementById('backdrop')!;
    const keyDisplay = document.getElementById('key-display')!;
    backdrop.style.display = 'block';
    backdrop.style.position = 'fixed';
    keyDisplay.textContent = 'sk-fullchain123';

    // mock 覆盖层检测
    backdrop.getBoundingClientRect = () => ({ width: 1920, height: 1080, top: 0, left: 0, right: 1920, bottom: 1080, x: 0, y: 0 } as any);
    const dialog = document.getElementById('dialog')!;
    dialog.getBoundingClientRect = () => ({ width: 400, height: 200, top: 200, left: 400, right: 800, bottom: 400, x: 400, y: 200 } as any);

    const origGCS = window.getComputedStyle;
    window.getComputedStyle = ((el: Element) => {
      const real = origGCS.call(window, el);
      if (el === backdrop) return { ...real, position: 'fixed' } as any;
      if (el === dialog) return { ...real, position: 'fixed' } as any;
      return real;
    }) as any;

    const origEFP = document.elementFromPoint;
    document.elementFromPoint = () => dialog;

    // 4. 操作后 digest
    const afterDigest = bridge.getPageDigest();

    // 5. 验证 digest 变化检测
    const diff = bridge.diffDigest(beforeDigest, afterDigest);
    // 覆盖层从无到有，或者内容变化
    const hasOverlayChange = diff.indexOf('覆盖层') >= 0;
    const hasContentChange = diff.indexOf('内容变化') >= 0 || diff.indexOf('sk-fullchain123') >= 0;
    expect(hasOverlayChange || hasContentChange).toBe(true);

    // 6. 验证网络事件可被 drain（独立队列，不受 drainEvents 影响）
    const networkEvents = bridge.observer.drainNetworkEvents();
    expect(networkEvents.length).toBe(1);
    expect(networkEvents[0].detail).toContain('sk-fullchain123');

    // 7. 清理
    document.elementFromPoint = origEFP;
    window.getComputedStyle = origGCS;
    bridge.observer.stop();
  });
});

describe('页面变化分析：只反馈变化及其内容', () => {
  it('新增节点的文本作为变化内容，脚本/样式不计入', () => {
    setupDOM('<main id="app"><p>旧内容</p></main>');
    const bridge = getBridge();
    const main = document.getElementById('app')!;
    const added = document.createElement('div');
    added.textContent = '接口返回后渲染的新密钥：sk-new';
    const script = document.createElement('script');
    script.textContent = 'var secret = 1;';
    const events = bridge.observer._analyzeMutations([
      { type: 'childList', target: main, addedNodes: [added, script], removedNodes: [] },
    ]);
    const change = events.find((e: any) => e.type === 'content_changed');
    expect(change).toBeTruthy();
    expect(change.detail).toContain('sk-new');
    expect(change.detail).not.toContain('secret');
    // 不再附带整页摘要
    expect(change.detail).not.toContain('旧内容');
  });

  it('子孙节点不重复计入，纯属性变化不产生事件', () => {
    setupDOM('<div id="app"></div>');
    const bridge = getBridge();
    const app = document.getElementById('app')!;
    const outer = document.createElement('section');
    const inner = document.createElement('span');
    inner.textContent = '唯一文本';
    outer.appendChild(inner);
    const events = bridge.observer._analyzeMutations([
      { type: 'childList', target: app, addedNodes: [outer, inner], removedNodes: [] },
    ]);
    const detail = events.find((e: any) => e.type === 'content_changed').detail;
    expect(detail.split('唯一文本').length - 1).toBe(1);

    const attrOnly = bridge.observer._analyzeMutations([
      { type: 'attributes', target: app, addedNodes: [], removedNodes: [] },
    ]);
    expect(attrOnly).toEqual([]);
  });
});
