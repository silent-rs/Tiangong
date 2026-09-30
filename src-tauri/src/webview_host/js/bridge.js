(function() {
    if (!window.__tiangong_document_id) {
        var documentId = Date.now().toString(36) + '-' + Math.random().toString(36).slice(2);
        try {
            Object.defineProperty(window, '__tiangong_document_id', {
                configurable: false,
                enumerable: false,
                writable: false,
                value: documentId
            });
        } catch(e) {
            window.__tiangong_document_id = documentId;
        }
    }
    if (window.__tiangong_bridge_loaded) return;
    window.__tiangong_bridge_loaded = true;

    // 屏蔽 Tauri IPC — 浏览器 WebView 加载外部 URL，不应暴露 Tauri API
    //
    // 策略：
    // 1. 用 setter 陷阱拦截 __TAURI_INTERNALS__（Tauri 后续注入时会触发我们的 setter）
    // 2. 让所有 IPC 调用静默返回成功空结果，避免页面 JS 报错
    // 3. 拦截 fetch/XHR 屏蔽 ipc:// 协议
    try {
        // --- 1. 陷阱 __TAURI_INTERNALS__ ---
        // 我们的 initialization_script 在 Tauri 的 runtime 脚本之前运行，
        // 所以可以抢先定义 __TAURI_INTERNALS__ 为带 setter 陷阱的属性。
        // 当 Tauri 后续注入真实的 __TAURI_INTERNALS__ 时，setter 触发，
        // 我们保留一个 noop 版本。
        var _noopInternals = {
            invoke: function() { return Promise.resolve('{}'); },
            ipc: { postMessage: function() {} },
            postMessage: function() {},
            convertCallback: function(cb) {
                var id = Math.floor(Math.random() * 1e9);
                return cb ? id : id;
            },
            transformCallback: function(cb, once) {
                var id = Math.floor(Math.random() * 1e9);
                return cb ? id : id;
            },
            metadata: {
                currentWebview: { label: 'browser-webview' },
                currentWindow: { label: 'main' }
            }
        };
        try {
            Object.defineProperty(window, '__TAURI_INTERNALS__', {
                configurable: true,
                enumerable: false,
                get: function() { return _noopInternals; },
                set: function(val) {
                    // Tauri 尝试设置真实 internals 时，复制 metadata 但保持 noop
                    if (val && val.metadata) {
                        _noopInternals.metadata = val.metadata;
                    }
                }
            });
        } catch(e) {
            // __TAURI_INTERNALS__ 已被 Tauri 定义（不可配置），直接覆盖方法
            if (window.__TAURI_INTERNALS__) {
                try { window.__TAURI_INTERNALS__.invoke = function() { return Promise.resolve('{}'); }; } catch(e2) {}
                try { window.__TAURI_INTERNALS__.ipc = { postMessage: function() {} }; } catch(e2) {}
                try { window.__TAURI_INTERNALS__.postMessage = function() {}; } catch(e2) {}
            }
        }

        // --- 2. 屏蔽 window.ipc ---
        // window.ipc 由 WRY 在我们的脚本之前注入，使用 Object.defineProperty + Object.freeze，
        // 不可覆盖。但我们可以在其 postMessage 被调用前拦截。
        try {
            Object.defineProperty(window, 'ipc', {
                configurable: true,
                enumerable: false,
                value: Object.freeze({ postMessage: function() {} })
            });
        } catch(e) {
            // window.ipc 不可重新配置，忽略
        }

        // --- 3. 拦截 fetch ipc:// 协议 + 捕获 JSON 响应 ---
        var _pendingNetworkEvents = window.__tiangong_pending_network_events || [];
        window.__tiangong_pending_network_events = _pendingNetworkEvents;
        var _isJsonContentType = function(contentType) {
            contentType = (contentType || '').toLowerCase();
            return contentType.indexOf('application/json') >= 0 ||
                contentType.indexOf('text/json') >= 0 ||
                contentType.indexOf('+json') >= 0;
        };
        var _pushNetworkEvent = function(event) {
            try {
                if (window.__tiangong_bridge && window.__tiangong_bridge.observer) {
                    window.__tiangong_bridge.observer._pushEvent(event);
                    return;
                }
            } catch(e) {
            }
            _pendingNetworkEvents.push(event);
            if (_pendingNetworkEvents.length > 100) {
                _pendingNetworkEvents = _pendingNetworkEvents.slice(-50);
                window.__tiangong_pending_network_events = _pendingNetworkEvents;
            }
        };
        var _responsePreview = function(text) {
            text = text || '';
            return text.length > 500 ? text.substring(0, 500) : text;
        };
        var _origFetch = window.fetch;
        window.fetch = function(input, init) {
            var url = (typeof input === 'string') ? input : (input && input.url ? input.url : '');
            if (url.indexOf('ipc://') === 0) {
                return Promise.resolve(new Response('{}', {
                    status: 200,
                    headers: {
                        'Content-Type': 'application/json',
                        'Tauri-Response': 'ok',
                        'Access-Control-Allow-Origin': '*',
                        'Access-Control-Expose-Headers': 'Tauri-Response'
                    }
                }));
            }
            return _origFetch.call(this, input, init).then(function(response) {
                try {
                    var ct = response.headers.get('content-type') || '';
                    var method = (init && init.method) || (input && input.method) || 'GET';
                    // 所有请求都记录元信息（URL、方法、状态码）
                    var meta = {
                        type: 'network_response',
                        timestamp: Date.now(),
                        url: url,
                        method: method,
                        status: response.status,
                        detail: ''
                    };
                    if (_isJsonContentType(ct)) {
                        var cloned = response.clone();
                        cloned.text().then(function(body) {
                            meta.detail = _responsePreview(body);
                            _pushNetworkEvent(meta);
                        }).catch(function() { _pushNetworkEvent(meta); });
                    } else {
                        _pushNetworkEvent(meta);
                    }
                } catch(e) {}
                return response;
            });
        };

        // --- 4. 拦截 XHR ipc:// 协议 + 捕获 JSON 响应 ---
        var _origXHROpen = window.XMLHttpRequest.prototype.open;
        var _origXHRSend = window.XMLHttpRequest.prototype.send;
        window.XMLHttpRequest.prototype.open = function(method, url, async, user, password) {
            if (typeof url === 'string' && url.indexOf('ipc://') === 0) {
                this._tiangong_blocked = true;
                return;
            }
            this._tiangong_blocked = false;
            this._tiangong_method = method;
            this._tiangong_url = url;
            return _origXHROpen.call(this, method, url, async, user, password);
        };
        window.XMLHttpRequest.prototype.send = function(body) {
            var xhr = this;
            if (xhr._tiangong_blocked) {
                return;
            }
            if (xhr._tiangong_url && xhr._tiangong_url.indexOf('ipc://') !== 0) {
                xhr.addEventListener('load', function() {
                    try {
                        var ct = xhr.getResponseHeader('content-type') || '';
                        var detail = '';
                        if (_isJsonContentType(ct)) {
                            try {
                                detail = xhr.responseText || '';
                            } catch(e2) {
                                try {
                                    detail = JSON.stringify(xhr.response || '');
                                } catch(e3) {
                                    detail = '';
                                }
                            }
                        }
                        _pushNetworkEvent({
                            type: 'network_response',
                            timestamp: Date.now(),
                            url: xhr._tiangong_url || '',
                            method: xhr._tiangong_method || 'GET',
                            status: xhr.status,
                            detail: _responsePreview(detail)
                        });
                    } catch(e) {}
                });
            }
            return _origXHRSend.call(this, body);
        };
    } catch(e) {}

    window.__tiangong_bridge = {
        version: '0.9.0',

        detectFramework: function() {
            var r = { frameworks: [], uiLibraries: [] };
            // React
            if (document.querySelector('[data-reactroot]') ||
                document.querySelector('[data-reactid]') ||
                window.__REACT_DEVTOOLS_GLOBAL_HOOK__) {
                r.frameworks.push('react');
            }
            // Vue 2/3
            if (document.querySelector('[data-server-rendered]') ||
                document.querySelector('[data-v-]') ||
                window.__VUE__) {
                r.frameworks.push('vue');
            }
            // Vue 3 组件带 __vue_app__
            if (document.querySelector('[data-v-inspector]') ||
                (document.querySelector && Array.from(document.querySelectorAll('*')).some(function(el) {
                    return el.__vue_app__;
                }))) {
                if (r.frameworks.indexOf('vue') === -1) r.frameworks.push('vue');
            }
            // Ant Design
            if (document.querySelector('.ant-select') || document.querySelector('.ant-picker') ||
                document.querySelector('.ant-form-item') || document.querySelector('.ant-btn')) {
                r.uiLibraries.push('antd');
            }
            // Element Plus / Element UI
            if (document.querySelector('.el-select') || document.querySelector('.el-date-editor') ||
                document.querySelector('.el-form-item') || document.querySelector('.el-button')) {
                r.uiLibraries.push('element-plus');
            }
            return r;
        },

        _extractFullText: function() {
            var text = '';
            if (document.body) {
                var clone = document.body.cloneNode(true);
                var removes = clone.querySelectorAll('script,style,noscript');
                for (var i = 0; i < removes.length; i++) {
                    removes[i].parentNode.removeChild(removes[i]);
                }
                text = (clone.textContent || '').replace(/\s+/g, ' ').trim();
                if (text.length < 50) {
                    text = (document.body.innerText || '').trim();
                }
            }
            return text;
        },

        // 头尾截取：限制内完整返回；超出时保留头部约 2/3、尾部约 1/3，
        // 中间以标记说明被省略区间（offset 与 pageText 一致），总长不超过 maxChars。
        _clipHeadTail: function(text, maxChars) {
            var total = text.length;
            if (total <= maxChars) return text;
            var markerFor = function(start, end) {
                return '\n\n…[页面内容过长，已省略第 ' + start + '–' + end + ' 字（全文 ' + total
                    + ' 字）；如需查看，请调用 web_page_text 按 offset 读取或按 keyword 搜索]…\n\n';
            };
            var budget = maxChars - markerFor(total, total).length;
            if (budget <= 0) return text.substring(0, maxChars);
            var headLen = Math.floor(budget * 2 / 3);
            var tailStart = total - (budget - headLen);
            return text.substring(0, headLen) + markerFor(headLen, tailStart) + text.substring(tailStart);
        },

        getFullText: function(maxChars) {
            maxChars = maxChars || 12000;
            var text = this._extractFullText();
            return {
                title: document.title,
                url: window.location.href,
                text: this._clipHeadTail(text, maxChars),
                total_chars: text.length,
            };
        },

        // Agent 主动查询页面正文：按 offset/max_chars 读取区间，或按 keyword
        // 搜索（返回命中位置与上下文片段）。offset 与推送中的省略标记一致。
        pageText: function(offset, maxChars, keyword) {
            var text = this._extractFullText();
            var total = text.length;
            var base = { title: document.title, url: window.location.href, total_chars: total };
            if (keyword) {
                var lower = text.toLowerCase();
                var kw = String(keyword).toLowerCase();
                var matches = [];
                var count = 0;
                var from = 0;
                var ctx = 150;
                while (true) {
                    var idx = lower.indexOf(kw, from);
                    if (idx < 0) break;
                    count++;
                    if (matches.length < 20) {
                        matches.push({
                            offset: idx,
                            snippet: text.substring(Math.max(0, idx - ctx), Math.min(total, idx + kw.length + ctx)),
                        });
                    }
                    from = idx + Math.max(kw.length, 1);
                }
                base.keyword = keyword;
                base.match_count = count;
                base.matches = matches;
                return base;
            }
            maxChars = Math.max(1, Math.min(maxChars || 8000, 20000));
            offset = Math.max(0, Math.min(offset || 0, total));
            var end = Math.min(total, offset + maxChars);
            base.offset = offset;
            base.end = end;
            base.has_more = end < total;
            base.text = text.substring(offset, end);
            return base;
        },

        _normalizeText: function(text) {
            return (text || '').replace(/\s+/g, ' ').trim().toLowerCase();
        },

        _shortText: function(text, maxLen) {
            maxLen = maxLen || 80;
            text = (text || '').replace(/\s+/g, ' ').trim();
            if (text.length > maxLen) {
                return text.substring(0, maxLen - 1) + '…';
            }
            return text;
        },

        _escapeCssIdent: function(value) {
            value = String(value || '');
            if (window.CSS && window.CSS.escape) {
                return window.CSS.escape(value);
            }
            return value.replace(/([^\w-])/g, '\\$1');
        },

        _escapeCssString: function(value) {
            return String(value || '').replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\a ');
        },

        _isVisible: function(el) {
            if (!el || el.nodeType !== 1) return false;
            var style = window.getComputedStyle ? window.getComputedStyle(el) : null;
            if (style && (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0')) {
                return false;
            }
            var rect = el.getBoundingClientRect ? el.getBoundingClientRect() : null;
            if (rect && rect.width <= 0 && rect.height <= 0) return false;
            return true;
        },

        _isDisabled: function(el) {
            return !!(el && (el.disabled || el.getAttribute('aria-disabled') === 'true' || el.classList.contains('disabled') || el.classList.contains('is-disabled')));
        },

        _queryAll: function(selector) {
            try {
                return Array.from(document.querySelectorAll(selector));
            } catch(e) {
                return null;
            }
        },

        _matches: function(el, selector) {
            if (!el || !el.matches) return false;
            try {
                return el.matches(selector);
            } catch(e) {
                return false;
            }
        },

        _selectorMatchesOnly: function(selector, el) {
            var matches = this._queryAll(selector);
            return !!(matches && matches.length === 1 && matches[0] === el);
        },

        _cssPath: function(el) {
            if (!el || !el.tagName) return '';
            var parts = [];
            var node = el;
            while (node && node.nodeType === 1 && node !== document) {
                var tag = node.tagName.toLowerCase();
                if (node.id) {
                    var idSelector = '#' + this._escapeCssIdent(node.id);
                    parts.unshift(idSelector);
                    break;
                }
                var parent = node.parentElement;
                if (!parent) {
                    parts.unshift(tag);
                    break;
                }
                var siblings = Array.from(parent.children).filter(function(child) {
                    return child.tagName === node.tagName;
                });
                if (siblings.length > 1) {
                    tag += ':nth-of-type(' + (siblings.indexOf(node) + 1) + ')';
                }
                parts.unshift(tag);
                node = parent;
            }
            return parts.join(' > ');
        },

        generateSelector: function(el) {
            if (!el || !el.tagName) return '';
            var tag = el.tagName.toLowerCase();
            var selector;

            if (el.id) {
                selector = '#' + this._escapeCssIdent(el.id);
                if (this._selectorMatchesOnly(selector, el)) return selector;
            }

            var testAttr = '';
            var testId = '';
            if (el.getAttribute('data-testid')) {
                testAttr = 'data-testid';
                testId = el.getAttribute('data-testid');
            } else if (el.getAttribute('data-test')) {
                testAttr = 'data-test';
                testId = el.getAttribute('data-test');
            } else if (el.getAttribute('data-cy')) {
                testAttr = 'data-cy';
                testId = el.getAttribute('data-cy');
            }
            if (testId) {
                selector = tag + '[' + testAttr + '="' + this._escapeCssString(testId) + '"]';
                if (this._selectorMatchesOnly(selector, el)) return selector;
                selector = '[' + testAttr + '="' + this._escapeCssString(testId) + '"]';
                if (this._selectorMatchesOnly(selector, el)) return selector;
            }

            if (el.name) {
                selector = tag + '[name="' + this._escapeCssString(el.name) + '"]';
                if (this._selectorMatchesOnly(selector, el)) return selector;
                selector = '[name="' + this._escapeCssString(el.name) + '"]';
                if (this._selectorMatchesOnly(selector, el)) return selector;
            }

            var aria = el.getAttribute('aria-label');
            if (aria) {
                selector = tag + '[aria-label="' + this._escapeCssString(aria) + '"]';
                if (this._selectorMatchesOnly(selector, el)) return selector;
            }

            var placeholder = el.getAttribute('placeholder');
            if (placeholder) {
                selector = tag + '[placeholder="' + this._escapeCssString(placeholder) + '"]';
                if (this._selectorMatchesOnly(selector, el)) return selector;
            }

            return this._cssPath(el);
        },

        _formLabelFor: function(el) {
            if (!el) return '';
            if (el.id) {
                var byFor = document.querySelector('label[for="' + this._escapeCssString(el.id) + '"]');
                if (byFor) return this._shortText(byFor.textContent || '');
            }
            if (el.labels && el.labels.length > 0) {
                return this._shortText(el.labels[0].textContent || '');
            }
            var closestLabel = el.closest ? el.closest('label') : null;
            if (closestLabel) return this._shortText(closestLabel.textContent || '');
            var antLabel = this._getAntFormItemLabel ? this._getAntFormItemLabel(el) : '';
            if (antLabel) return this._shortText(antLabel);
            var elLabel = this._getElFormItemLabel ? this._getElFormItemLabel(el) : '';
            if (elLabel) return this._shortText(elLabel);
            var formItem = el.closest ? el.closest('.form-item,.field,.form-group,[class*="form-item"],[class*="form-group"]') : null;
            if (formItem) {
                var label = formItem.querySelector('label');
                if (label) return this._shortText(label.textContent || '');
            }
            return '';
        },

        _implicitRole: function(el) {
            if (!el || !el.tagName) return '';
            var tag = el.tagName.toLowerCase();
            var type = (el.type || '').toLowerCase();
            if (tag === 'button' || (tag === 'input' && ['button', 'submit', 'reset'].indexOf(type) >= 0)) return 'button';
            if (tag === 'a' && el.getAttribute('href')) return 'link';
            if (tag === 'select') return 'combobox';
            if (tag === 'textarea') return 'textbox';
            if (tag === 'input') {
                if (type === 'checkbox') return 'checkbox';
                if (type === 'radio') return 'radio';
                if (type === 'range') return 'slider';
                return 'textbox';
            }
            if (tag === 'summary') return 'button';
            return '';
        },

        _accessibleName: function(el) {
            if (!el) return '';
            var aria = el.getAttribute('aria-label');
            if (aria) return this._shortText(aria);
            var labelledBy = el.getAttribute('aria-labelledby');
            if (labelledBy) {
                var texts = labelledBy.split(/\s+/).map(function(id) {
                    var ref = document.getElementById(id);
                    return ref ? (ref.textContent || '') : '';
                }).filter(Boolean);
                if (texts.length > 0) return this._shortText(texts.join(' '));
            }
            var label = this._formLabelFor(el);
            if (label) return label;
            var placeholder = el.getAttribute('placeholder');
            if (placeholder) return this._shortText(placeholder);
            var title = el.getAttribute('title');
            if (title) return this._shortText(title);
            var alt = el.getAttribute('alt');
            if (alt) return this._shortText(alt);
            if (el.value && this._matches(el, 'button,input[type="button"],input[type="submit"],input[type="reset"]')) {
                return this._shortText(el.value);
            }
            return this._shortText(el.textContent || '');
        },

        _visibleText: function(el) {
            if (!el) return '';
            return this._shortText((el.innerText || el.textContent || '').replace(/\s+/g, ' '));
        },

        _candidateSelector: function(action, includeComponents) {
            if (action === 'fill') {
                if (includeComponents === 'only') {
                    return '.ant-select,.ant-picker,.el-select,.el-date-editor';
                }
                var base = 'input:not([type="hidden"]):not([type="submit"]):not([type="button"]):not([type="reset"]),textarea,select,[contenteditable="true"]';
                if (includeComponents) {
                    base += ',.ant-select,.ant-picker,.el-select,.el-date-editor';
                }
                return base;
            }
            return 'button,a,summary,[role="button"],[role="link"],[role="tab"],[role="menuitem"],[onclick],[tabindex],input[type="button"],input[type="submit"],input[type="reset"]';
        },

        _candidateElements: function(options) {
            options = options || {};
            var selector = this._candidateSelector(
                options.action || 'click',
                options.componentOnly ? 'only' : !!options.components
            );
            var elements = this._queryAll(selector) || [];
            if ((options.action || 'click') === 'fill' && !options.components && !options.componentOnly) {
                elements = elements.filter(function(el) {
                    return !(el.closest && el.closest('.ant-select,.ant-picker,.el-select,.el-date-editor'));
                });
            }
            if ((options.action || 'click') === 'click') {
                var hasFallbackText = elements.length === 0;
                if (!hasFallbackText) return elements;
                elements = this._queryAll('body *') || [];
                elements = elements.filter(function(el) {
                    var tag = el.tagName ? el.tagName.toLowerCase() : '';
                    if (['script', 'style', 'noscript', 'meta', 'link', 'svg', 'path'].indexOf(tag) >= 0) return false;
                    var text = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
                    return text.length > 0 && text.length <= 120;
                });
            }
            return elements;
        },

        _primaryQueryText: function(query) {
            query = (query || '').trim();
            var quoted = query.match(/[“‘"']([^“”‘’"']+)[”’"']/);
            if (quoted && quoted[1]) return quoted[1].trim();
            var text = query
                .replace(/^(请|帮我|帮忙)?\s*(点击|点一下|按下|打开|选择|填写|输入|填入|设置)\s*/g, '')
                .replace(/\s*(按钮|按键|链接|入口|输入框|文本框|字段|下拉框|选择框|复选框|单选框|表单项|控件|元素)$/g, '')
                .replace(/(包含|含有|文字|文本|名称|名为|叫做|为|的)/g, ' ')
                .replace(/\s+/g, ' ')
                .trim();
            return text || query;
        },

        _textScore: function(queryText, candidateText) {
            var q = this._normalizeText(queryText);
            var c = this._normalizeText(candidateText);
            if (!q || !c) return 0;
            if (q === c) return 100;
            if (c.indexOf(q) >= 0) return 82;
            if (q.indexOf(c) >= 0 && c.length >= 2) return 72;
            return 0;
        },

        _typeBoost: function(query, el, action) {
            query = query || '';
            var role = (el.getAttribute('role') || this._implicitRole(el) || '').toLowerCase();
            var tag = el.tagName ? el.tagName.toLowerCase() : '';
            var type = (el.type || '').toLowerCase();
            var boost = 0;
            if (/按钮|按键|提交|登录|确认/.test(query) && (role === 'button' || tag === 'button' || ['button', 'submit', 'reset'].indexOf(type) >= 0)) boost += 12;
            if (/链接|入口|打开/.test(query) && (role === 'link' || tag === 'a')) boost += 12;
            if (/输入框|文本框|字段|填写|输入|邮箱|账号|密码|电话|手机/.test(query) && action === 'fill') boost += 10;
            if (/下拉|选择/.test(query) && (tag === 'select' || role === 'combobox' || el.classList.contains('ant-select') || el.classList.contains('el-select'))) boost += 10;
            return boost;
        },

        _describeCandidate: function(el, score, reason) {
            var rect = el && el.getBoundingClientRect ? el.getBoundingClientRect() : null;
            return {
                selector: this.generateSelector(el),
                text: this._visibleText(el),
                tag: el && el.tagName ? el.tagName.toLowerCase() : '',
                role: (el && (el.getAttribute('role') || this._implicitRole(el))) || '',
                label: this._accessibleName(el),
                score: Math.round(score || 0),
                reason: reason || '',
                x: rect ? Math.round(rect.left + rect.width / 2) : null,
                y: rect ? Math.round(rect.top + rect.height / 2) : null
            };
        },

        _pushCandidate: function(list, seen, el, score, reason) {
            if (!el || seen.indexOf(el) >= 0 || !this._isVisible(el) || this._isDisabled(el)) return;
            seen.push(el);
            list.push({ el: el, score: score, reason: reason });
        },

        _rankElement: function(el, query, options, reason) {
            options = options || {};
            var queryText = this._primaryQueryText(query);
            var action = options.action || 'click';
            var score = 0;
            var visible = this._visibleText(el);
            var name = this._accessibleName(el);
            var label = this._formLabelFor(el);
            var placeholder = el.getAttribute('placeholder') || '';
            var attrName = el.getAttribute('name') || '';
            var id = el.id || '';

            score = Math.max(score, this._textScore(queryText, visible));
            score = Math.max(score, this._textScore(queryText, name) + (name ? 8 : 0));
            score = Math.max(score, this._textScore(queryText, label) + (label ? 12 : 0));
            score = Math.max(score, this._textScore(queryText, placeholder) + (placeholder ? 8 : 0));
            score = Math.max(score, this._textScore(queryText, attrName) + (attrName ? 4 : 0));
            score = Math.max(score, this._textScore(queryText, id) + (id ? 2 : 0));
            if (score > 0) score += this._typeBoost(query, el, action);
            if (reason === 'css selector') score = 120;
            return score;
        },

        _parseRoleQuery: function(query) {
            var m = (query || '').match(/^role\s*=\s*([^\[\]]+)(?:\[\s*name\s*=\s*['"]?([^'"\]]+)['"]?\s*\])?$/i);
            if (!m) return null;
            return { role: (m[1] || '').trim().toLowerCase(), name: (m[2] || '').trim() };
        },

        _findExplicitCandidates: function(query, options, candidates, seen) {
            var q = (query || '').trim();
            var action = (options && options.action) || 'click';
            var m = q.match(/^(text|aria|aria-label|label|placeholder|name)\s*=\s*(.+)$/i);
            if (m) {
                var kind = m[1].toLowerCase();
                if (kind === 'aria-label') kind = 'aria';
                var value = m[2].replace(/^[‘’“”'"]|[‘’“”'"]$/g, '').trim();
                var elements = this._candidateElements(options);
                for (var i = 0; i < elements.length; i++) {
                    var el = elements[i];
                    var field = '';
                    if (kind === 'text') field = this._visibleText(el) || this._accessibleName(el);
                    if (kind === 'aria') field = el.getAttribute('aria-label') || '';
                    if (kind === 'label') field = this._formLabelFor(el) || this._accessibleName(el);
                    if (kind === 'placeholder') field = el.getAttribute('placeholder') || '';
                    if (kind === 'name') field = el.getAttribute('name') || '';
                    var score = this._textScore(value, field);
                    if (score > 0) {
                        this._pushCandidate(candidates, seen, el, score + 18 + this._typeBoost(value, el, action), kind + ' match');
                    }
                }
                return;
            }

            var roleQuery = this._parseRoleQuery(q);
            if (roleQuery) {
                var all = this._queryAll('[role],button,a,input,textarea,select,summary') || [];
                for (var r = 0; r < all.length; r++) {
                    var role = (all[r].getAttribute('role') || this._implicitRole(all[r]) || '').toLowerCase();
                    if (role !== roleQuery.role) continue;
                    var score = 92;
                    if (roleQuery.name) {
                        score = this._textScore(roleQuery.name, this._accessibleName(all[r]) || this._visibleText(all[r]));
                        if (score === 0) continue;
                        score += 18;
                    }
                    this._pushCandidate(candidates, seen, all[r], score, 'role match');
                }
            }
        },

        _parseChineseNumber: function(text) {
            text = String(text || '').trim();
            if (/^\d+$/.test(text)) return parseInt(text, 10);
            var map = { '零': 0, '一': 1, '二': 2, '两': 2, '三': 3, '四': 4, '五': 5, '六': 6, '七': 7, '八': 8, '九': 9 };
            if (text === '十') return 10;
            var ten = text.indexOf('十');
            if (ten >= 0) {
                var left = text.substring(0, ten);
                var right = text.substring(ten + 1);
                return (left ? (map[left] || 0) : 1) * 10 + (right ? (map[right] || 0) : 0);
            }
            return map[text] || 0;
        },

        _findTableCellTarget: function(query, options, candidates, seen) {
            var m = (query || '').match(/(?:表格|table).*?第?\s*([一二两三四五六七八九十\d]+)\s*行.*?第?\s*([一二两三四五六七八九十\d]+)\s*列/);
            if (!m) return;
            var rowIndex = this._parseChineseNumber(m[1]) - 1;
            var colIndex = this._parseChineseNumber(m[2]) - 1;
            if (rowIndex < 0 || colIndex < 0) return;
            var wantLink = /链接|link/.test(query);
            var wantButton = /按钮|按键|button/.test(query);
            var tables = this._queryAll('table') || [];
            for (var t = 0; t < tables.length; t++) {
                if (!this._isVisible(tables[t])) continue;
                var rows = Array.from(tables[t].querySelectorAll('tr')).filter(this._isVisible.bind(this));
                if (rowIndex >= rows.length) continue;
                var cells = Array.from(rows[rowIndex].querySelectorAll('th,td')).filter(this._isVisible.bind(this));
                if (colIndex >= cells.length) continue;
                var cell = cells[colIndex];
                var target = cell;
                if (wantLink) target = cell.querySelector('a,[role="link"]') || cell;
                if (wantButton) target = cell.querySelector('button,[role="button"],input[type="button"],input[type="submit"]') || cell;
                this._pushCandidate(candidates, seen, target, 118, 'table cell');
            }
        },

        _findNaturalCandidates: function(query, options, candidates, seen) {
            var elements = this._candidateElements(options);
            for (var i = 0; i < elements.length; i++) {
                var score = this._rankElement(elements[i], query, options, 'natural');
                if (score > 0) {
                    this._pushCandidate(candidates, seen, elements[i], score, 'smart match');
                }
            }
            // 标准候选未命中时，扩大范围到所有可见文本元素（排除祖先容器，只保留最内层匹配）
            if (candidates.length === 0) {
                var all = this._queryAll('body *') || [];
                var rawMatches = [];
                for (var j = 0; j < all.length; j++) {
                    var tag = all[j].tagName ? all[j].tagName.toLowerCase() : '';
                    if (['script', 'style', 'noscript', 'meta', 'link', 'svg', 'path'].indexOf(tag) >= 0) continue;
                    var score2 = this._rankElement(all[j], query, options, 'natural');
                    if (score2 > 0) rawMatches.push({ el: all[j], score: score2 });
                }
                // 去除祖先：如果 A 包含 B 且两者都匹配，保留 B（更精确的后代）
                var filtered = [];
                for (var m = 0; m < rawMatches.length; m++) {
                    var isAncestor = false;
                    for (var n = 0; n < rawMatches.length; n++) {
                        if (m === n) continue;
                        if (rawMatches[m].el.contains(rawMatches[n].el)) {
                            isAncestor = true;
                            break;
                        }
                    }
                    if (!isAncestor) filtered.push(rawMatches[m]);
                }
                for (var k = 0; k < filtered.length; k++) {
                    this._pushCandidate(candidates, seen, filtered[k].el, filtered[k].score, 'fallback match');
                }
            }
        },

        _resolveCandidates: function(query, candidates, options) {
            options = options || {};
            candidates.sort(function(a, b) { return b.score - a.score; });
            var minScore = options.minScore || 55;
            candidates = candidates.filter(function(c) { return c.score >= minScore; }).slice(0, 12);
            var described = [];
            for (var i = 0; i < candidates.length; i++) {
                described.push(this._describeCandidate(candidates[i].el, candidates[i].score, candidates[i].reason));
            }
            if (candidates.length === 0) {
                return { ok: false, error: '元素未找到: ' + query, candidates: [] };
            }
            if (candidates.length > 1) {
                var gap = candidates[0].score - candidates[1].score;
                if (options.strictMultiple || gap < 25) {
                    return {
                        ok: false,
                        ambiguous: true,
                        error: '找到多个候选元素，请选择更精确目标',
                        candidates: described
                    };
                }
            }
            return {
                ok: true,
                element: candidates[0].el,
                selector: described[0].selector,
                target: described[0],
                candidates: []
            };
        },

        _locateElement: function(query, options) {
            options = options || {};
            query = (query || '').trim();
            if (!query) return { ok: false, error: '定位描述不能为空', candidates: [] };
            var candidates = [];
            var seen = [];

            var cssMatches = this._queryAll(query);
            if (cssMatches && cssMatches.length > 0) {
                for (var i = 0; i < cssMatches.length; i++) {
                    this._pushCandidate(candidates, seen, cssMatches[i], 120, 'css selector');
                }
                var result = this._resolveCandidates(query, candidates, { strictMultiple: candidates.length > 1, minScore: 1 });
                if (!result.ok) {
                    var disabledCheck = this._checkDisabledFallback(query, cssMatches);
                    if (disabledCheck) return disabledCheck;
                }
                return result;
            }

            this._findExplicitCandidates(query, options, candidates, seen);
            this._findTableCellTarget(query, options, candidates, seen);
            this._findNaturalCandidates(query, options, candidates, seen);
            var result = this._resolveCandidates(query, candidates, {});
            if (!result.ok) {
                var elements = this._candidateElements(options);
                var disabledCheck = this._checkDisabledFallback(query, elements);
                if (disabledCheck) return disabledCheck;
            }
            return result;
        },

        _checkDisabledFallback: function(query, elements) {
            for (var i = 0; i < elements.length; i++) {
                var el = elements[i];
                if (!this._isVisible(el)) continue;
                var text = (this._visibleText(el) || '').trim();
                if (text === query && this._isDisabled(el)) {
                    return { ok: false, error: '元素已禁用: ' + query, candidates: [] };
                }
            }
            return null;
        },

        locateElement: function(query, options) {
            var result = this._locateElement(query, options || {});
            if (!result.ok) {
                return {
                    ok: false,
                    ambiguous: !!result.ambiguous,
                    error: result.error,
                    candidates: result.candidates || []
                };
            }
            return {
                ok: true,
                selector: result.selector,
                target: result.target,
                candidates: []
            };
        },

        click: function(selector) {
            var el = document.querySelector(selector);
            if (el) { el.click(); return true; }
            return false;
        },

        type: function(selector, text) {
            var el = document.querySelector(selector);
            if (!el) return false;
            el.focus();
            var nativeSetter = Object.getOwnPropertyDescriptor(
                HTMLInputElement.prototype, 'value'
            );
            if (nativeSetter && nativeSetter.set) {
                nativeSetter.set.call(el, text);
            } else {
                el.value = text;
            }
            el.dispatchEvent(new Event('input', { bubbles: true }));
            el.dispatchEvent(new Event('change', { bubbles: true }));
            return true;
        },

        extractForms: function() {
            var forms = [];
            var containers = document.querySelectorAll('form');
            // 无 <form> 时，尝试在 dialog 或 body 中查找
            if (containers.length === 0) {
                var dialog = this._getTopmostOverlay();
                containers = dialog ? [dialog] : [document.body];
            }
            for (var ci = 0; ci < containers.length; ci++) {
                var container = containers[ci];
                var fields = [];
                var inputs = container.querySelectorAll(
                    'input:not([type="hidden"]):not([type="submit"]):not([type="button"]):not([type="reset"]),textarea,select'
                );
                for (var idx = 0; idx < inputs.length; idx++) {
                    var el = inputs[idx];
                    var field = {
                        index: idx,
                        tag: el.tagName.toLowerCase(),
                        type: el.type || '',
                        name: el.name || '',
                        id: el.id || '',
                        label: '',
                        placeholder: el.placeholder || '',
                        value: el.value || '',
                        required: el.required || false,
                        readonly: el.readOnly || false,
                        disabled: el.disabled || false,
                        selector: ''
                    };
                    // 尝试关联 label
                    if (el.id) {
                        var lbl = container.querySelector('label[for="' + this._escapeCssString(el.id) + '"]');
                        if (lbl) field.label = (lbl.textContent || '').trim();
                    }
                    if (!field.label) {
                        // 向上查找 ds-form-item__label-text、ant-form-item-label、el-form-item__label
                        var labelEl = el.closest('[class*="form-item"]') || el.closest('.ant-form-item') || el.closest('.el-form-item');
                        if (labelEl) {
                            var labelText = labelEl.querySelector('[class*="label-text"]') ||
                                           labelEl.querySelector('.ant-form-item-label') ||
                                           labelEl.querySelector('.el-form-item__label') ||
                                           labelEl.querySelector('label');
                            if (labelText) field.label = (labelText.textContent || '').trim();
                        }
                    }
                    if (!field.label) {
                        var parent = el.parentElement;
                        if (parent && parent.tagName === 'LABEL') {
                            field.label = (parent.textContent || '').trim();
                        }
                    }
                    // select 的 options
                    if (el.tagName === 'SELECT') {
                        field.options = [];
                        var opts = el.querySelectorAll('option');
                        for (var oi = 0; oi < opts.length; oi++) {
                            field.options.push({
                                value: opts[oi].value,
                                text: (opts[oi].textContent || '').trim()
                            });
                        }
                    }
                    // 构造 selector 和 description（智能选择器优先）
                    field.selector = this.generateSelector(el);
                    field.description = this._fieldDescription(field);
                    fields.push(field);
                }
                // 提取表单内/容器内的按钮（包含原生和 div[role="button"] 等）
                var buttons = [];
                var btns = container.querySelectorAll('button, input[type="submit"], input[type="reset"], [role="button"]');
                for (var bci = 0; bci < btns.length; bci++) {
                    var btn = btns[bci];
                    var btnText = (btn.textContent || '').trim();
                    if (!btnText) continue; // 跳过无文本的按钮（如图标按钮）
                    buttons.push({
                        tag: btn.tagName.toLowerCase(),
                        type: btn.type || 'button',
                        text: btnText,
                        selector: this.generateSelector(btn),
                        description: btnText ? 'text=' + btnText : this.generateSelector(btn),
                        disabled: this._isDisabled(btn)
                    });
                }
                if (fields.length > 0 || buttons.length > 0) {
                    forms.push({ fields: fields, buttons: buttons });
                }
            }
            return { forms: forms, framework: this.detectFramework(), uiComponents: this._extractUIComponents() };
        },

        _fieldDescription: function(field) {
            if (field.label) return 'label=' + field.label;
            if (field.placeholder) return 'placeholder=' + field.placeholder;
            if (field.name) return 'name=' + field.name;
            return field.selector || '';
        },

        extractInteractiveElements: function() {
            var result = [];
            var sel = 'button,a,input[type="button"],input[type="submit"],input[type="reset"],[role="button"],[role="link"],[role="tab"],[role="menuitem"],[role="treeitem"],[role="listitem"],summary,details';
            var els = this._queryAll(sel) || [];
            for (var i = 0; i < els.length && result.length < 50; i++) {
                var el = els[i];
                if (!this._isVisible(el)) continue;
                var text = (el.textContent || el.value || '').trim();
                if (text.length > 80) text = text.substring(0, 80) + '...';
                var tag = el.tagName.toLowerCase();
                var role = el.getAttribute('role') || '';
                var href = tag === 'a' ? (el.href || '') : '';
                var item = {
                    tag: tag,
                    text: text,
                    role: role,
                    selector: this.generateSelector(el),
                    description: text ? 'text=' + text : this.generateSelector(el),
                    disabled: el.disabled || false
                };
                if (href) item.href = href;
                result.push(item);
            }
            return { elements: result, count: result.length };
        },

        _extractUIComponents: function() {
            var components = [];
            // Ant Design Select
            var antSelects = document.querySelectorAll('.ant-select');
            for (var i = 0; i < antSelects.length; i++) {
                var sel = antSelects[i];
                var selItem = sel.querySelector('.ant-select-selection-item');
                var placeholder = sel.querySelector('.ant-select-selection-placeholder');
                var input = sel.querySelector('input');
                components.push({
                    componentType: 'antd-select',
                    selector: this.generateSelector(sel),
                    label: this._getAntFormItemLabel(sel),
                    placeholder: placeholder ? (placeholder.textContent || '').trim() : '',
                    value: selItem ? (selItem.textContent || '').trim() : '',
                    name: input ? (input.name || '') : '',
                    id: sel.id || '',
                    disabled: sel.classList.contains('ant-select-disabled'),
                    required: false,
                    readonly: false,
                    tag: 'div',
                    type: 'antd-select',
                    index: components.length
                });
            }
            // Ant Design DatePicker
            var antPickers = document.querySelectorAll('.ant-picker');
            for (var j = 0; j < antPickers.length; j++) {
                var picker = antPickers[j];
                var pickerInput = picker.querySelector('input');
                components.push({
                    componentType: 'antd-datepicker',
                    selector: this.generateSelector(picker),
                    label: this._getAntFormItemLabel(picker),
                    placeholder: pickerInput ? (pickerInput.placeholder || '') : '',
                    value: pickerInput ? (pickerInput.value || '') : '',
                    name: pickerInput ? (pickerInput.name || '') : '',
                    id: picker.id || '',
                    disabled: picker.classList.contains('ant-picker-disabled'),
                    required: false,
                    readonly: false,
                    tag: 'div',
                    type: 'antd-datepicker',
                    index: components.length
                });
            }
            // Element Plus Select
            var elSelects = document.querySelectorAll('.el-select');
            for (var k = 0; k < elSelects.length; k++) {
                var es = elSelects[k];
                var esInput = es.querySelector('.el-input__inner') || es.querySelector('input');
                var esPlaceholder = esInput ? (esInput.placeholder || '') : '';
                var esSuffix = es.querySelector('.el-select__suffix');
                // 检查是否有已选值显示
                var esSelected = es.querySelector('.el-select__selected-item') ||
                                 es.querySelector('.el-input__inner');
                components.push({
                    componentType: 'el-select',
                    selector: this.generateSelector(es),
                    label: this._getElFormItemLabel(es),
                    placeholder: esPlaceholder,
                    value: esSelected ? (esSelected.textContent || '').trim() : '',
                    name: esInput ? (esInput.name || '') : '',
                    id: es.id || '',
                    disabled: es.classList.contains('is-disabled'),
                    required: false,
                    readonly: false,
                    tag: 'div',
                    type: 'el-select',
                    index: components.length
                });
            }
            // Element Plus DatePicker
            var elDates = document.querySelectorAll('.el-date-editor');
            for (var m = 0; m < elDates.length; m++) {
                var ed = elDates[m];
                var edInput = ed.querySelector('input');
                components.push({
                    componentType: 'el-datepicker',
                    selector: this.generateSelector(ed),
                    label: this._getElFormItemLabel(ed),
                    placeholder: edInput ? (edInput.placeholder || '') : '',
                    value: edInput ? (edInput.value || '') : '',
                    name: edInput ? (edInput.name || '') : '',
                    id: ed.id || '',
                    disabled: ed.classList.contains('is-disabled'),
                    required: false,
                    readonly: false,
                    tag: 'div',
                    type: 'el-datepicker',
                    index: components.length
                });
            }
            return components;
        },

        _getAntFormItemLabel: function(el) {
            var formItem = el.closest('.ant-form-item');
            if (formItem) {
                var lbl = formItem.querySelector('.ant-form-item-label');
                if (lbl) return (lbl.textContent || '').trim();
            }
            return '';
        },

        _getElFormItemLabel: function(el) {
            var formItem = el.closest('.el-form-item');
            if (formItem) {
                var lbl = formItem.querySelector('.el-form-item__label');
                if (lbl) return (lbl.textContent || '').trim();
            }
            return '';
        },

        fillField: function(selector, value, strategy) {
            var located = this._locateElement(selector, { action: 'fill', components: false });
            if (!located.ok) {
                return {
                    ok: false,
                    error: located.error || ('元素未找到: ' + selector),
                    candidates: located.candidates || []
                };
            }
            var el = located.element;
            var locatedSelector = located.selector;
            var target = located.target;

            // Agent 鼠标：填写前在目标字段展示专属光标（无按压，标识
            // Agent 正在操作此处）
            try {
                el.scrollIntoView({ block: 'center', behavior: 'instant' });
                var fr = el.getBoundingClientRect();
                this.showAgentCursor(fr.left + fr.width / 2, fr.top + fr.height / 2, {});
            } catch (e) { /* ignore */ }

            if (el.getAttribute && el.getAttribute('contenteditable') === 'true') {
                el.focus();
                el.textContent = value;
                el.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: value }));
                el.dispatchEvent(new Event('change', { bubbles: true }));
                return { ok: true, strategy: 'contenteditable', selector: locatedSelector, target: target, currentValue: el.textContent || '' };
            }

            // select 特殊处理
            if (el.tagName === 'SELECT') {
                el.focus();
                el.value = value;
                el.dispatchEvent(new Event('change', { bubbles: true }));
                el.dispatchEvent(new FocusEvent('blur', { bubbles: true }));
                return { ok: true, strategy: 'select-change', selector: locatedSelector, target: target, currentValue: el.value };
            }

            // checkbox / radio 特殊处理
            if (el.type === 'checkbox' || el.type === 'radio') {
                var shouldCheck = (value === 'true' || value === '1');
                if (el.checked !== shouldCheck) {
                    el.click();
                }
                el.dispatchEvent(new FocusEvent('blur', { bubbles: true }));
                return { ok: true, strategy: 'click-toggle', selector: locatedSelector, target: target, currentValue: String(el.checked) };
            }

            strategy = strategy || 'auto';
            var result = null;

            // 策略 1: execCommand insertText（走浏览器原生编辑管线，产生 trusted 事件）
            if (strategy === 'auto' || strategy === 'insertText') {
                el.dispatchEvent(new FocusEvent('focus', { bubbles: true }));
                el.focus();
                // 策略 1a: execCommand insertText（走浏览器原生编辑管线，产生 trusted 事件）
                el.select();
                try {
                    if (document.execCommand('insertText', false, value)) {
                        if (el.value === value) {
                            result = { ok: true, strategy: 'execCommand-insertText', selector: locatedSelector, target: target, currentValue: el.value };
                        }
                    }
                } catch(e) {}
                // 策略 1b: 逐字符键盘模拟（兼容 execCommand 不生效的场景）
                if (!result) {
                    el.dispatchEvent(new KeyboardEvent('keydown', { key: '', bubbles: true }));
                    el.value = '';
                    el.dispatchEvent(new Event('input', { bubbles: true }));
                    for (var ci = 0; ci < value.length; ci++) {
                        var ch = value[ci];
                        el.value = el.value + ch;
                        var keyInit = { key: ch, code: 'Key' + ch.toUpperCase(), bubbles: true };
                        el.dispatchEvent(new KeyboardEvent('keydown', keyInit));
                        el.dispatchEvent(new KeyboardEvent('keypress', keyInit));
                        el.dispatchEvent(new Event('input', { bubbles: true }));
                        el.dispatchEvent(new KeyboardEvent('keyup', keyInit));
                    }
                    el.dispatchEvent(new Event('change', { bubbles: true }));
                    if (el.value === value) {
                        result = { ok: true, strategy: 'keyboard', selector: locatedSelector, target: target, currentValue: el.value };
                    }
                }
            }

            // 策略 2: native setter（适用于 React 受控组件）
            if (!result && (strategy === 'auto' || strategy === 'native')) {
                el.dispatchEvent(new FocusEvent('focus', { bubbles: true }));
                el.focus();
                var proto = Object.getPrototypeOf(el);
                var descriptor = Object.getOwnPropertyDescriptor(proto, 'value') ||
                                 Object.getOwnPropertyDescriptor(
                                     el.tagName === 'TEXTAREA' ?
                                         HTMLTextAreaElement.prototype :
                                         HTMLInputElement.prototype,
                                     'value'
                                 );
                if (descriptor && descriptor.set) {
                    descriptor.set.call(el, '');
                    el.dispatchEvent(new Event('input', { bubbles: true }));
                    descriptor.set.call(el, value);
                    el.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: value }));
                    el.dispatchEvent(new Event('change', { bubbles: true }));
                    // 额外触发键盘事件，确保框架感知到用户交互
                    el.dispatchEvent(new KeyboardEvent('keyup', { key: ' ', code: 'Space', keyCode: 32, bubbles: true }));
                    if (el.value === value) {
                        result = { ok: true, strategy: 'native-setter', selector: locatedSelector, target: target, currentValue: el.value };
                    }
                }
            }

            // 策略 3: 直接赋值 + 事件（兜底）
            if (!result && (strategy === 'auto' || strategy === 'paste')) {
                el.dispatchEvent(new FocusEvent('focus', { bubbles: true }));
                el.focus();
                el.value = value;
                el.dispatchEvent(new Event('input', { bubbles: true }));
                el.dispatchEvent(new Event('change', { bubbles: true }));
                result = { ok: true, strategy: 'direct-assign', selector: locatedSelector, target: target, currentValue: el.value };
            }

            if (!result) {
                return { ok: false, error: '所有填写策略均未成功', currentValue: el.value, selector: locatedSelector, target: target };
            }

            // 填写完成后触发 blur 激活表单校验
            el.dispatchEvent(new FocusEvent('blur', { bubbles: true }));

            // 检查填写后是否有 disabled 按钮变为 enabled（返回提示信息）
            result.currentValue = el.value;
            if (!result.selector) result.selector = locatedSelector;
            if (!result.target) result.target = target;
            var dialog = this._getTopmostOverlay();
            if (dialog) {
                var disabledBtns = dialog.querySelectorAll('[role="button"]');
                for (var di = 0; di < disabledBtns.length; di++) {
                    var btnText = (disabledBtns[di].textContent || '').trim();
                    if (btnText && this._isDisabled(disabledBtns[di])) {
                        result.note = '按钮 "' + btnText + '" 仍处于禁用状态，可能需要填写更多必填字段';
                        break;
                    }
                }
            }
            return result;
        },

        // UI 库组件填写（多步交互，异步返回）
        fillComponent: function(selector, value) {
            var located = this._locateElement(selector, { action: 'fill', components: true, componentOnly: true });
            if (!located.ok) {
                return {
                    ok: false,
                    error: located.error || ('组件未找到: ' + selector),
                    candidates: located.candidates || []
                };
            }
            var el = located.element;
            var result;

            // Ant Design Select
            if (el.classList.contains('ant-select')) {
                result = this._fillAntSelect(el, value);
            }
            // Ant Design DatePicker
            else if (el.classList.contains('ant-picker')) {
                result = this._fillAntDatePicker(el, value);
            }
            // Element Plus Select
            else if (el.classList.contains('el-select')) {
                result = this._fillElSelect(el, value);
            }
            // Element Plus DatePicker
            else if (el.classList.contains('el-date-editor')) {
                result = this._fillElDatePicker(el, value);
            } else {
                result = { ok: false, error: '未知的 UI 库组件类型' };
            }
            if (result) {
                result.selector = located.selector;
                result.target = located.target;
            }
            return result;
        },

        _fillAntSelect: function(el, value) {
            // 点击打开下拉框
            var trigger = el.querySelector('.ant-select-selector');
            if (!trigger) return { ok: false, error: 'Ant Select 触发器未找到' };
            trigger.click();
            // 异步选择：用 setTimeout 等待下拉框渲染
            var self = this;
            setTimeout(function() {
                var dropdown = document.querySelector('.ant-select-dropdown:not(.ant-select-dropdown-hidden)');
                if (!dropdown) {
                    // 下拉框可能挂在 body 上
                    var allDropdowns = document.querySelectorAll('.ant-select-dropdown');
                    for (var d = 0; d < allDropdowns.length; d++) {
                        if (allDropdowns[d].offsetParent !== null) {
                            dropdown = allDropdowns[d];
                            break;
                        }
                    }
                }
                if (!dropdown) return;
                var items = dropdown.querySelectorAll('.ant-select-item-option');
                for (var i = 0; i < items.length; i++) {
                    var text = (items[i].textContent || '').trim();
                    if (text === value || text.indexOf(value) >= 0) {
                        items[i].click();
                        return;
                    }
                }
            }, 300);
            return { ok: true, strategy: 'antd-select', note: '异步执行中，稍后检查结果' };
        },

        _fillAntDatePicker: function(el, value) {
            var input = el.querySelector('input');
            if (!input) return { ok: false, error: 'DatePicker 输入框未找到' };
            input.focus();
            // 清空并输入
            var nativeSetter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
            if (nativeSetter && nativeSetter.set) {
                nativeSetter.set.call(input, value);
            } else {
                input.value = value;
            }
            input.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: value }));
            input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true }));
            input.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true }));
            return { ok: true, strategy: 'antd-datepicker' };
        },

        _fillElSelect: function(el, value) {
            var input = el.querySelector('.el-input') || el.querySelector('input');
            if (!input) return { ok: false, error: 'El Select 输入框未找到' };
            input.click();
            var self = this;
            setTimeout(function() {
                var dropdown = document.querySelector('.el-select-dropdown:not([style*="display: none"])');
                if (!dropdown) {
                    var allDropdowns = document.querySelectorAll('.el-select-dropdown');
                    for (var d = 0; d < allDropdowns.length; d++) {
                        if (allDropdowns[d].offsetParent !== null) {
                            dropdown = allDropdowns[d];
                            break;
                        }
                    }
                }
                if (!dropdown) return;
                var items = dropdown.querySelectorAll('.el-select-dropdown__item');
                for (var i = 0; i < items.length; i++) {
                    var text = (items[i].textContent || '').trim();
                    if (text === value || text.indexOf(value) >= 0) {
                        items[i].click();
                        return;
                    }
                }
            }, 300);
            return { ok: true, strategy: 'el-select', note: '异步执行中，稍后检查结果' };
        },

        _fillElDatePicker: function(el, value) {
            var input = el.querySelector('input');
            if (!input) return { ok: false, error: 'El DatePicker 输入框未找到' };
            input.focus();
            var nativeSetter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
            if (nativeSetter && nativeSetter.set) {
                nativeSetter.set.call(input, value);
            } else {
                input.value = value;
            }
            input.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: value }));
            input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true }));
            input.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true }));
            return { ok: true, strategy: 'el-datepicker' };
        },

        // 智能元素定位：根据多种格式自动选择定位策略
        locateElement: function(selector) {
            if (!selector || typeof selector !== 'string') return null;
            selector = selector.trim();
            if (!selector) return null;

            // 策略 0: nth:N,selector — 选择第 N 个匹配
            if (selector.indexOf('nth:') === 0) {
                var commaIdx = selector.indexOf(',');
                if (commaIdx > 4) {
                    var idx = parseInt(selector.substring(4, commaIdx), 10);
                    var innerSelector = selector.substring(commaIdx + 1);
                    if (!isNaN(idx) && idx > 0) {
                        var all = this.locateAll(innerSelector);
                        if (all && all.length >= idx) {
                            return all[idx - 1];
                        }
                    }
                }
                return null;
            }

            // 策略 1: rect:x,y,w,h — 批注矩形区域定位
            if (selector.indexOf('rect:') === 0) {
                var parts = selector.substring(5).split(',').map(Number);
                if (parts.length >= 4 && parts.every(function(n) { return !isNaN(n); })) {
                    return this._findElementInRect(parts[0], parts[1], parts[2], parts[3]);
                }
                return null;
            }

            // 策略 2: aria:label — ARIA 属性定位
            if (selector.indexOf('aria:') === 0) {
                var ariaVal = selector.substring(5);
                var el = document.querySelector('[aria-label*="' + ariaVal + '"]');
                if (el) return el;
                el = document.querySelector('[role="' + ariaVal + '"]');
                if (el) return el;
                el = document.querySelector('[aria-roledescription*="' + ariaVal + '"]');
                return el || null;
            }

            // 策略 3: CSS selector 尝试（#id, .class, tag, [attr], 组合选择器）
            try {
                var cssEl = document.querySelector(selector);
                if (cssEl) return cssEl;
            } catch(e) {
                // 不是有效 CSS selector，继续其他策略
            }

            // 检测当前打开的对话框，优先在其中查找
            var dialog = this._getTopmostOverlay();
            var contexts = dialog ? [dialog, document.body] : [document.body];
            var lowerSelector = selector.toLowerCase();
            var xpathLit = this._xpathLiteral(selector);

            // 策略 4: 精确文本匹配（对话框内 → 全局）
            for (var ci = 0; ci < contexts.length; ci++) {
                var exactResult = document.evaluate(
                    './/*[text() = ' + xpathLit + ']',
                    contexts[ci], null, XPathResult.FIRST_ORDERED_NODE_TYPE, null
                );
                if (exactResult.singleNodeValue) return exactResult.singleNodeValue;
            }

            // 策略 5: 按钮精确文本（对话框内 → 全局）
            var allButtons = document.querySelectorAll('button, a, [role="button"], input[type="submit"]');
            for (var ci2 = 0; ci2 < contexts.length; ci2++) {
                for (var bi = 0; bi < allButtons.length; bi++) {
                    if (!contexts[ci2].contains(allButtons[bi])) continue;
                    if ((allButtons[bi].textContent || '').trim().toLowerCase() === lowerSelector) {
                        return allButtons[bi];
                    }
                }
            }

            // 策略 6: 部分文本匹配（对话框内 → 全局）
            for (var ci3 = 0; ci3 < contexts.length; ci3++) {
                var partialResult = document.evaluate(
                    './/*[contains(text(), ' + xpathLit + ')]',
                    contexts[ci3], null, XPathResult.FIRST_ORDERED_NODE_TYPE, null
                );
                if (partialResult.singleNodeValue) return partialResult.singleNodeValue;
            }

            // 策略 7: 按钮模糊匹配（对话框内 → 全局）
            for (var ci4 = 0; ci4 < contexts.length; ci4++) {
                for (var bj = 0; bj < allButtons.length; bj++) {
                    if (!contexts[ci4].contains(allButtons[bj])) continue;
                    var btnText = (allButtons[bj].textContent || '').trim().toLowerCase();
                    var btnTitle = (allButtons[bj].getAttribute('title') || '').toLowerCase();
                    var btnAria = (allButtons[bj].getAttribute('aria-label') || '').toLowerCase();
                    if (btnText.indexOf(lowerSelector) >= 0 ||
                        btnTitle.indexOf(lowerSelector) >= 0 ||
                        btnAria.indexOf(lowerSelector) >= 0) {
                        return allButtons[bj];
                    }
                }
            }

            return null;
        },

        // 返回所有匹配的元素列表（用于候选提示和 nth 语法）
        locateAll: function(selector) {
            if (!selector || typeof selector !== 'string') return [];
            selector = selector.trim();
            if (!selector) return [];

            var results = [];
            var seen = {};
            var lowerSelector = selector.toLowerCase();

            // CSS selector
            try {
                var cssAll = document.querySelectorAll(selector);
                for (var ci = 0; ci < cssAll.length; ci++) {
                    var key = this._elementKey(cssAll[ci]);
                    if (!seen[key]) {
                        seen[key] = true;
                        results.push(cssAll[ci]);
                    }
                }
            } catch(e) {}

            // XPath exact text
            var exactIter = document.evaluate(
                './/*[text() = ' + this._xpathLiteral(selector) + ']',
                document.body, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null
            );
            for (var ei = 0; ei < exactIter.snapshotLength; ei++) {
                var key2 = this._elementKey(exactIter.snapshotItem(ei));
                if (!seen[key2]) { seen[key2] = true; results.push(exactIter.snapshotItem(ei)); }
            }

            // XPath partial text
            var partialIter = document.evaluate(
                './/*[contains(text(), ' + this._xpathLiteral(selector) + ')]',
                document.body, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null
            );
            for (var pi = 0; pi < partialIter.snapshotLength; pi++) {
                var key3 = this._elementKey(partialIter.snapshotItem(pi));
                if (!seen[key3]) { seen[key3] = true; results.push(partialIter.snapshotItem(pi)); }
            }

            // Button text match
            var buttons = document.querySelectorAll('button, a, [role="button"], input[type="submit"]');
            for (var bi = 0; bi < buttons.length; bi++) {
                var key4 = this._elementKey(buttons[bi]);
                if (seen[key4]) continue;
                var btnText = (buttons[bi].textContent || '').trim().toLowerCase();
                var btnTitle = (buttons[bi].getAttribute('title') || '').toLowerCase();
                var btnAria = (buttons[bi].getAttribute('aria-label') || '').toLowerCase();
                if (btnText.indexOf(lowerSelector) >= 0 ||
                    btnTitle.indexOf(lowerSelector) >= 0 ||
                    btnAria.indexOf(lowerSelector) >= 0) {
                    seen[key4] = true;
                    results.push(buttons[bi]);
                }
            }

            return results;
        },

        _elementKey: function(el) {
            if (!el) return '';
            return el.tagName + ':' + (el.id || '') + ':' + this.generateSelector(el);
        },

        _xpathLiteral: function(s) {
            if (s.indexOf('"') === -1) return '"' + s + '"';
            if (s.indexOf("'") === -1) return "'" + s + "'";
            var parts = s.split('"');
            return 'concat("' + parts.join('", \'"\', "') + '")';
        },

        _findElementInRect: function(x, y, w, h) {
            var candidates = [];
            var allEls = document.querySelectorAll('*');
            for (var i = 0; i < allEls.length; i++) {
                var el = allEls[i];
                var r = el.getBoundingClientRect();
                if (r.width === 0 || r.height === 0) continue;
                // 计算重叠面积
                var ox = Math.max(0, Math.min(x + w, r.left + r.width) - Math.max(x, r.left));
                var oy = Math.max(0, Math.min(y + h, r.top + r.height) - Math.max(y, r.top));
                var overlap = ox * oy;
                if (overlap > 0) {
                    candidates.push({ el: el, overlap: overlap, area: r.width * r.height });
                }
            }
            if (candidates.length === 0) return null;
            // 按重叠率排序，优先选择重叠比高且面积适中的元素
            candidates.sort(function(a, b) {
                var ratioA = a.overlap / Math.max(a.area, 1);
                var ratioB = b.overlap / Math.max(b.area, 1);
                return ratioB - ratioA;
            });
            // 跳过过大的容器（body, html 等），取第一个有意义的元素
            for (var ci = 0; ci < candidates.length; ci++) {
                var tag = candidates[ci].el.tagName.toLowerCase();
                if (tag !== 'body' && tag !== 'html' && candidates[ci].area < (w * h * 10)) {
                    return candidates[ci].el;
                }
            }
            return candidates[0].el;
        },

        // 根据元素生成 CSS selector
        generateSelector: function(el) {
            if (!el) return '';
            if (el.id) return '#' + el.id.replace(/([^\w-])/g, '\\$1');
            // 尝试 name 属性
            if (el.name) return '[name="' + el.name + '"]';
            // 向上生成路径
            var parts = [];
            var cur = el;
            while (cur && cur !== document.body) {
                var seg = cur.tagName.toLowerCase();
                if (cur.id) {
                    seg = '#' + cur.id.replace(/([^\w-])/g, '\\$1');
                    parts.unshift(seg);
                    break;
                }
                // 同级同名元素中排第几
                if (cur.parentElement) {
                    var siblings = cur.parentElement.children;
                    var sameTag = [];
                    for (var i = 0; i < siblings.length; i++) {
                        if (siblings[i].tagName === cur.tagName) sameTag.push(siblings[i]);
                    }
                    if (sameTag.length > 1) {
                        var idx = sameTag.indexOf(cur) + 1;
                        seg += ':nth-of-type(' + idx + ')';
                    }
                }
                parts.unshift(seg);
                cur = cur.parentElement;
            }
            return parts.join(' > ');
        },

        // 提取矩形区域内的 DOM 元素信息
        extractElementsInRect: function(x, y, w, h) {
            var results = [];
            var allEls = document.querySelectorAll('*');
            for (var i = 0; i < allEls.length; i++) {
                var el = allEls[i];
                var r = el.getBoundingClientRect();
                if (r.width === 0 || r.height === 0) continue;
                var ox = Math.max(0, Math.min(x + w, r.left + r.width) - Math.max(x, r.left));
                var oy = Math.max(0, Math.min(y + h, r.top + r.height) - Math.max(y, r.top));
                var overlap = ox * oy;
                if (overlap === 0) continue;
                var tag = el.tagName.toLowerCase();
                // 跳过无意义元素
                if (tag === 'html' || tag === 'body' || tag === 'head' || tag === 'script' || tag === 'style') continue;
                var overlapRatio = overlap / (r.width * r.height);
                if (overlapRatio < 0.5) continue;
                var text = (el.textContent || '').trim();
                if (text.length > 200) text = text.substring(0, 200) + '...';
                // 收集关键属性
                var attrs = {};
                var importantAttrs = ['id', 'class', 'name', 'type', 'href', 'src', 'placeholder',
                    'aria-label', 'aria-role', 'role', 'title', 'alt', 'value', 'data-testid'];
                for (var ai = 0; ai < importantAttrs.length; ai++) {
                    var val = el.getAttribute(importantAttrs[ai]);
                    if (val) attrs[importantAttrs[ai]] = val;
                }
                results.push({
                    tag: tag,
                    text: text,
                    attributes: attrs,
                    selector: this.generateSelector(el),
                    rect: { x: Math.round(r.left), y: Math.round(r.top), width: Math.round(r.width), height: Math.round(r.height) },
                    overlapRatio: Math.round(overlapRatio * 100) / 100,
                    area: Math.round(r.width * r.height)
                });
            }
            // 按面积从小到大排序（小面积 = 更精确的元素）
            results.sort(function(a, b) { return a.area - b.area; });
            // 去重：如果父元素和子元素重叠比相同，保留子元素
            var deduped = [];
            var seen = {};
            for (var ri = 0; ri < results.length; ri++) {
                var key = results[ri].selector;
                if (!seen[key]) {
                    seen[key] = true;
                    deduped.push(results[ri]);
                }
            }
            return deduped.slice(0, 20);
        },

        queryDom: function(selector, maxResults) {
            maxResults = maxResults || 20;
            var els = document.querySelectorAll(selector);
            var results = [];
            for (var i = 0; i < Math.min(els.length, maxResults); i++) {
                var el = els[i];
                var text = (el.innerText || '').trim();
                if (text.length > 500) text = text.substring(0, 500) + '...';
                var attrs = {};
                var importantAttrs = ['id','class','name','type','href','src','placeholder',
                    'aria-label','role','title','alt','value','data-testid','disabled','readonly'];
                for (var ai = 0; ai < importantAttrs.length; ai++) {
                    var val = el.getAttribute(importantAttrs[ai]);
                    if (val) attrs[importantAttrs[ai]] = val;
                }
                var r = el.getBoundingClientRect();
                results.push({
                    index: i,
                    tag: el.tagName.toLowerCase(),
                    text: text,
                    attributes: attrs,
                    selector: this.generateSelector(el),
                    rect: { x: Math.round(r.left), y: Math.round(r.top), width: Math.round(r.width), height: Math.round(r.height) }
                });
            }
            return { selector: selector, total: els.length, returned: results.length, elements: results };
        },

        // 综合检测元素是否处于禁用状态
        _isDisabled: function(el) {
            if (!el) return false;
            // 原生 disabled 属性
            if (el.disabled) return true;
            if (el.hasAttribute && el.hasAttribute('disabled')) return true;
            // aria-disabled
            if (el.getAttribute && el.getAttribute('aria-disabled') === 'true') return true;
            // CSS 类名包含 disabled 模式（如 ds-button--disabled、is-disabled、ant-btn-disabled）
            if (el.classList) {
                for (var i = 0; i < el.classList.length; i++) {
                    var cls = el.classList[i].toLowerCase();
                    if (cls.indexOf('disabled') >= 0 || cls.indexOf('-disabled') >= 0) {
                        // 排除误判：如 "not-disabled"
                        if (cls.indexOf('not-disabled') === -1) return true;
                    }
                }
            }
            return false;
        },

        clickElement: function(selector) {
            var located = this._locateElement(selector, { action: 'click' });
            if (!located.ok) {
                // 尝试 locateAll 看是否有候选
                var all = this.locateAll(selector);
                if (all.length > 0) {
                    return {
                        ok: false,
                        error: located.error || ('元素未找到: ' + selector),
                        candidates: all.slice(0, 5).map(function(c) {
                            return {
                                tag: (c.tagName || '').toLowerCase(),
                                text: (c.textContent || '').trim().substring(0, 50),
                                selector: window.__tiangong_bridge.generateSelector(c)
                            };
                        })
                    };
                }
                return {
                    ok: false,
                    error: located.error || ('元素未找到: ' + selector),
                    candidates: located.candidates || []
                };
            }
            var el = located.element;
            var locatedSelector = located.selector;
            var target = located.target;

            // 如果找到的是内联元素（如 span），尝试点击其父级按钮/链接
            var clickTarget = el;
            var interactiveTags = ['BUTTON', 'A', 'INPUT', 'SELECT', 'TEXTAREA', 'SUMMARY', 'LABEL'];
            if (interactiveTags.indexOf(el.tagName) === -1) {
                var parent = el.closest('button, a, [role="button"], label, summary');
                if (parent) clickTarget = parent;
            }

            // 检测 disabled 状态
            if (this._isDisabled(clickTarget)) {
                return { ok: false, error: '元素已禁用: ' + selector, candidates: [] };
            }

            clickTarget.scrollIntoView({ block: 'center', behavior: 'instant' });
            clickTarget.focus();

            // Agent 鼠标：点击前在目标位置展示专属光标与按压波纹（与
            // 用户系统鼠标区分，操作可见）；纯视觉增强，失败不影响点击
            try {
                var cr = clickTarget.getBoundingClientRect();
                this.showAgentCursor(cr.left + cr.width / 2, cr.top + cr.height / 2, { press: true });
            } catch (e) { /* ignore */ }

            // 非原生按钮（如 div[role="button"]）使用模拟鼠标事件，
            // 确保 React/Vue 等框架的事件委托系统能正确捕获点击。
            // 原生按钮直接用 .click() 产生 trusted 事件。
            if (clickTarget.tagName === 'BUTTON' || clickTarget.tagName === 'A' || clickTarget.tagName === 'INPUT') {
                clickTarget.click();
            } else {
                this._simulateClick(clickTarget);
            }
            return {
                ok: true,
                selector: locatedSelector,
                target: target,
                candidates: []
            };
        },

        // 模拟完整鼠标点击序列（mousedown → mouseup → click）
        _simulateClick: function(el) {
            var rect = el.getBoundingClientRect();
            var x = rect.left + rect.width / 2;
            var y = rect.top + rect.height / 2;
            var opts = { bubbles: true, cancelable: true, view: window, clientX: x, clientY: y };
            el.dispatchEvent(new MouseEvent('mousedown', opts));
            el.dispatchEvent(new MouseEvent('mouseup', opts));
            el.dispatchEvent(new MouseEvent('click', opts));
        },

        // Agent 鼠标：在页面固定层展示专属光标（紫色箭头 + Agent 标签 +
        // 可选按压波纹），与用户系统鼠标区分。Shadow DOM 隔离页面样式，
        // 幂等创建复用；停留 hold 后淡出移出视区。纯视觉增强，任何
        // 异常静默吞掉，不影响操作本身。
        showAgentCursor: function(x, y, opts) {
            opts = opts || {};
            try {
                var host = document.getElementById('__tiangong_agent_cursor');
                if (!host) {
                    host = document.createElement('div');
                    host.id = '__tiangong_agent_cursor';
                    host.style.cssText = 'position:fixed;top:0;left:0;z-index:2147483647;pointer-events:none;';
                    var shadow = host.attachShadow ? host.attachShadow({ mode: 'closed' }) : host;
                    var style = document.createElement('style');
                    style.textContent = ''
                        + '.cursor{position:fixed;left:-100px;top:-100px;transition:left .18s ease-out,top .18s ease-out,opacity .3s;will-change:left,top;opacity:1;filter:drop-shadow(0 1px 3px rgba(0,0,0,.5));}'
                        + '.cursor.fade{opacity:0;}'
                        + '.arrow{display:block;width:22px;height:22px;}'
                        + '.tag{position:absolute;left:17px;top:15px;padding:1px 5px;border-radius:4px;background:#7c3aed;color:#fff;font:600 10px/1.5 ui-sans-serif,system-ui,-apple-system,sans-serif;white-space:nowrap;letter-spacing:.02em;}'
                        + '.ripple{position:absolute;left:-9px;top:-9px;width:40px;height:40px;border:2px solid #7c3aed;border-radius:50%;opacity:0;transform:scale(.3);}'
                        + '.cursor.press .ripple{animation:tgAgentPress .5s ease-out;}'
                        + '@keyframes tgAgentPress{0%{opacity:.9;transform:scale(.3)}100%{opacity:0;transform:scale(1.2)}}';
                    shadow.appendChild(style);
                    var wrap = document.createElement('div');
                    wrap.className = 'cursor';
                    wrap.innerHTML = '<svg class="arrow" viewBox="0 0 24 24"><path d="M4 2l14 8.5-6.1 1.2L16 18l-3 1.5-4-6.3L4 16z" fill="#7c3aed" stroke="#ffffff" stroke-width="1.2" stroke-linejoin="round"/></svg><span class="tag">Agent</span><span class="ripple"></span>';
                    shadow.appendChild(wrap);
                    document.documentElement.appendChild(host);
                    host.__tgWrap = wrap;
                    host.__tgHideTimer = 0;
                    host.__tgMoveTimer = 0;
                }
                var wrap = host.__tgWrap;
                window.clearTimeout(host.__tgHideTimer);
                window.clearTimeout(host.__tgMoveTimer);
                wrap.classList.remove('fade');
                // 下一帧再定位：先复位再移动，保证从上一位置滑入的过渡
                window.requestAnimationFrame(function() {
                    wrap.style.left = x + 'px';
                    wrap.style.top = y + 'px';
                });
                if (opts.press) {
                    wrap.classList.remove('press');
                    void wrap.offsetWidth;
                    wrap.classList.add('press');
                }
                host.__tgHideTimer = window.setTimeout(function() {
                    wrap.classList.add('fade');
                    host.__tgMoveTimer = window.setTimeout(function() {
                        if (wrap.classList.contains('fade')) {
                            wrap.style.left = '-100px';
                            wrap.style.top = '-100px';
                        }
                    }, 400);
                }, opts.hold || 1400);
            } catch (e) { /* 纯增强，静默 */ }
        },

        // 等待页面条件满足（异步，返回 Promise）
        // condition: 'navigation' | 'element:selector' | 'element!:selector' | 'stable'
        waitFor: function(condition, timeoutMs) {
            var self = this;
            var startTime = Date.now();
            var timeout = timeoutMs || 5000;
            // 记录初始状态
            this._waitInitialState = {
                url: window.location.href,
                lastMutationTime: Date.now()
            };
            // 启动 MutationObserver 追踪 DOM 变化（用于 stable 条件）
            if (condition === 'stable' && !this._waitObserver) {
                this._startMutationObserver();
            }
            return new Promise(function(resolve) {
                var check = function() {
                    if (self._checkWaitCondition(condition)) {
                        self._stopMutationObserver();
                        resolve({ ok: true, condition: condition, elapsed: Date.now() - startTime });
                        return;
                    }
                    if (Date.now() - startTime > timeout) {
                        self._stopMutationObserver();
                        resolve({ ok: false, error: '等待超时', condition: condition, elapsed: Date.now() - startTime });
                        return;
                    }
                    setTimeout(check, 200);
                };
                check();
            });
        },

        _checkWaitCondition: function(condition) {
            if (condition === 'navigation') {
                return window.location.href !== this._waitInitialState.url;
            }
            if (condition === 'stable') {
                return Date.now() - this._waitInitialState.lastMutationTime > 1000;
            }
            if (condition.indexOf('element!:') === 0) {
                var sel = condition.substring(9);
                try { return !document.querySelector(sel); } catch(e) { return true; }
            }
            if (condition.indexOf('element:') === 0) {
                var sel2 = condition.substring(8);
                try { return !!document.querySelector(sel2); } catch(e) { return false; }
            }
            return false;
        },

        // 获取页面摘要（用于操作前后对比）
        getPageDigest: function() {
            var overlay = this._getTopmostOverlay();
            var fullText = (document.body.innerText || '').replace(/\s+/g, ' ').trim();
            return {
                url: window.location.href,
                title: document.title,
                overlayOpen: !!overlay,
                overlayText: overlay ? this._extractOverlayContent(overlay) : '',
                mainTextTail: fullText.length > 3000 ? fullText.substring(fullText.length - 3000) : fullText
            };
        },

        // 对比前后摘要，返回差异描述
        diffDigest: function(before, after) {
            if (!before || !after) return '';
            var changes = [];
            if (before.url !== after.url) {
                changes.push('页面已导航到 ' + after.url);
            }
            if (!before.overlayOpen && after.overlayOpen) {
                changes.push('覆盖层已出现：' + after.overlayText.substring(0, 2000));
            }
            if (before.overlayOpen && !after.overlayOpen) {
                changes.push('覆盖层已关闭');
            }
            if (before.overlayOpen && after.overlayOpen && before.overlayText !== after.overlayText) {
                changes.push('覆盖层内容已变化：' + after.overlayText.substring(0, 2000));
            }
            // 页面尾部内容变化
            if (before.mainTextTail !== after.mainTextTail && after.mainTextTail) {
                var newContent = this._textDiff(before.mainTextTail, after.mainTextTail);
                if (newContent) {
                    changes.push('页面内容变化：' + newContent);
                }
            }
            if (changes.length === 0) {
                changes.push('页面无明显变化');
            }
            return changes.join('\n');
        },

        // 简单文本差异：提取 after 中新增的内容
        _textDiff: function(before, after) {
            if (!before || !after) return after ? after.substring(0, 500) : '';
            // 按句子/片段分割
            var beforeParts = before.split(/(?<=[。，！？；\n.!?;])|(?<=\s{2,})/);
            var afterParts = after.split(/(?<=[。，！？；\n.!?;])|(?<=\s{2,})/);
            var beforeSet = {};
            for (var i = 0; i < beforeParts.length; i++) {
                var p = beforeParts[i].trim();
                if (p.length > 3) beforeSet[p] = true;
            }
            var newParts = [];
            for (var j = 0; j < afterParts.length; j++) {
                var ap = afterParts[j].trim();
                if (ap.length > 3 && !beforeSet[ap]) {
                    newParts.push(ap);
                }
            }
            if (newParts.length === 0) return '';
            var result = newParts.join(' ').trim();
            return result.length > 1000 ? result.substring(0, 1000) + '...' : result;
        },

        _startMutationObserver: function() {
            var self = this;
            this._waitObserver = new MutationObserver(function() {
                if (self._waitInitialState) {
                    self._waitInitialState.lastMutationTime = Date.now();
                }
            });
            this._waitObserver.observe(document.body, {
                childList: true, subtree: true, characterData: true, attributes: true
            });
        },

        _stopMutationObserver: function() {
            if (this._waitObserver) {
                this._waitObserver.disconnect();
                this._waitObserver = null;
            }
        },

        annotation: {
            _active: false,
            _canvas: null,
            _ctx: null,
            _annotations: [],
            _currentTool: 'rect',
            _startX: 0,
            _startY: 0,
            _color: '#ff0000',
            _strokeWidth: 2,
            _selectedIndex: -1,
            _dragging: false,
            _dragOffsetX: 0,
            _dragOffsetY: 0,
            _drawing: false,
            _resizing: false,
            _resizeHandle: -1,
            _hoveredHandle: -1,

            start: function(tool) {
                if (this._active) return { ok: true, note: 'already active' };
                this._currentTool = tool || 'rect';
                this._active = true;
                this._annotations = [];
                this._selectedIndex = -1;
                this._ensureCanvas();
                this._bindEvents();
                this._showToolbar();
                return { ok: true };
            },

            stop: function() {
                if (!this._active) return { ok: true, note: 'not active' };
                this._active = false;
                this._unbindEvents();
                if (this._canvas && this._canvas.parentNode) {
                    this._canvas.parentNode.removeChild(this._canvas);
                }
                this._canvas = null;
                this._ctx = null;
                this._hideToolbar();
                return { ok: true };
            },

            clear: function() {
                this._annotations = [];
                this._selectedIndex = -1;
                this._render();
                return { ok: true };
            },

            getAnnotations: function() {
                var self = this;
                var annotations = this._annotations.map(function(annotation, index) {
                    return self._enrichAnnotation(annotation, index);
                });
                return {
                    annotations: annotations,
                    count: annotations.length,
                    summary: this._formatAnnotationsForAgent(annotations)
                };
            },

            _bridge: function() {
                return window.__tiangong_bridge || {};
            },

            _annotationRect: function(annotation) {
                if (!annotation) return null;
                if (annotation.type === 'rect') {
                    return {
                        x: annotation.x,
                        y: annotation.y,
                        width: annotation.width,
                        height: annotation.height
                    };
                }
                if (annotation.type === 'arrow') {
                    var minX = Math.min(annotation.x1, annotation.x2);
                    var minY = Math.min(annotation.y1, annotation.y2);
                    var maxX = Math.max(annotation.x1, annotation.x2);
                    var maxY = Math.max(annotation.y1, annotation.y2);
                    var padding = 16;
                    return {
                        x: Math.max(0, minX - padding),
                        y: Math.max(0, minY - padding),
                        width: Math.max(1, maxX - minX + padding * 2),
                        height: Math.max(1, maxY - minY + padding * 2)
                    };
                }
                return null;
            },

            _intersectArea: function(a, b) {
                if (!a || !b) return 0;
                var left = Math.max(a.x, b.x);
                var top = Math.max(a.y, b.y);
                var right = Math.min(a.x + a.width, b.x + b.width);
                var bottom = Math.min(a.y + a.height, b.y + b.height);
                if (right <= left || bottom <= top) return 0;
                return (right - left) * (bottom - top);
            },

            _elementRect: function(el) {
                if (!el || !el.getBoundingClientRect) return null;
                var rect = el.getBoundingClientRect();
                if (!rect || rect.width <= 0 || rect.height <= 0) return null;
                return {
                    x: rect.left,
                    y: rect.top,
                    width: rect.width,
                    height: rect.height
                };
            },

            _rectKey: function(rect) {
                if (!rect) return '';
                return [
                    Math.round(rect.x),
                    Math.round(rect.y),
                    Math.round(rect.width),
                    Math.round(rect.height)
                ].join(',');
            },

            _normalizeText: function(text) {
                var bridge = this._bridge();
                if (bridge._normalizeText) return bridge._normalizeText(text);
                return (text || '').replace(/\s+/g, ' ').trim().toLowerCase();
            },

            _shortText: function(text, maxLen) {
                var bridge = this._bridge();
                if (bridge._shortText) return bridge._shortText(text, maxLen || 200);
                text = (text || '').replace(/\s+/g, ' ').trim();
                if (text.length > (maxLen || 200)) return text.substring(0, (maxLen || 200) - 1) + '…';
                return text;
            },

            _isElementVisible: function(el) {
                var bridge = this._bridge();
                if (bridge._isVisible) return bridge._isVisible(el);
                if (!el || !el.getBoundingClientRect) return false;
                var rect = el.getBoundingClientRect();
                return rect.width > 0 && rect.height > 0;
            },

            _extractTextInRect: function(regionRect) {
                var texts = [];
                var seen = {};
                if (!document.body || !document.createTreeWalker || !document.createRange || !window.NodeFilter) {
                    return '';
                }
                var walker = document.createTreeWalker(document.body, window.NodeFilter.SHOW_TEXT, {
                    acceptNode: function(node) {
                        var text = (node.nodeValue || '').replace(/\s+/g, ' ').trim();
                        if (!text) return window.NodeFilter.FILTER_REJECT;
                        var parent = node.parentElement;
                        if (!parent || parent.id === '__tiangong_annotation_canvas') return window.NodeFilter.FILTER_REJECT;
                        var tag = parent.tagName ? parent.tagName.toLowerCase() : '';
                        if (['script', 'style', 'noscript', 'template'].indexOf(tag) >= 0) return window.NodeFilter.FILTER_REJECT;
                        return window.NodeFilter.FILTER_ACCEPT;
                    }
                });
                var node;
                while ((node = walker.nextNode())) {
                    var range = document.createRange();
                    try {
                        range.selectNodeContents(node);
                        var rects = range.getClientRects ? Array.from(range.getClientRects()) : [];
                        for (var i = 0; i < rects.length; i++) {
                            var rect = {
                                x: rects[i].left,
                                y: rects[i].top,
                                width: rects[i].width,
                                height: rects[i].height
                            };
                            if (this._intersectArea(regionRect, rect) <= 0) continue;
                            var value = this._shortText(node.nodeValue || '', 180);
                            var key = this._normalizeText(value);
                            if (key && !seen[key]) {
                                seen[key] = true;
                                texts.push(value);
                            }
                            break;
                        }
                    } catch(e) {
                    } finally {
                        if (range.detach) range.detach();
                    }
                    if (texts.join(' ').length > 1200) break;
                }
                return this._shortText(texts.join(' '), 1200);
            },

            _elementText: function(el) {
                var bridge = this._bridge();
                var label = bridge._accessibleName ? bridge._accessibleName(el) : '';
                var text = bridge._visibleText ? bridge._visibleText(el) : (el ? (el.innerText || el.textContent || '') : '');
                var value = '';
                if (el && (el.value || el.placeholder)) {
                    value = el.value || el.placeholder || '';
                }
                return this._shortText(label || text || value, 160);
            },

            _elementSummary: function(el, overlap) {
                var bridge = this._bridge();
                var rect = this._elementRect(el);
                return {
                    selector: bridge.generateSelector ? bridge.generateSelector(el) : '',
                    tag: el && el.tagName ? el.tagName.toLowerCase() : '',
                    role: (el && (el.getAttribute('role') || (bridge._implicitRole ? bridge._implicitRole(el) : ''))) || '',
                    text: this._elementText(el),
                    overlap: Math.round(overlap),
                    x: rect ? Math.round(rect.x + rect.width / 2) : null,
                    y: rect ? Math.round(rect.y + rect.height / 2) : null
                };
            },

            _extractElementsInRect: function(regionRect) {
                var selector = [
                    'button',
                    'a',
                    'input',
                    'textarea',
                    'select',
                    'label',
                    'h1',
                    'h2',
                    'h3',
                    'h4',
                    'h5',
                    'h6',
                    'p',
                    'li',
                    'td',
                    'th',
                    'summary',
                    '[role]',
                    '[aria-label]',
                    '[title]',
                    '[placeholder]',
                    'img'
                ].join(',');
                var elements = Array.from(document.querySelectorAll(selector));
                var matches = [];
                for (var i = 0; i < elements.length; i++) {
                    var el = elements[i];
                    if (!el || el.id === '__tiangong_annotation_canvas' || !this._isElementVisible(el)) continue;
                    var rect = this._elementRect(el);
                    if (!rect) continue;
                    var overlap = this._intersectArea(regionRect, rect);
                    if (overlap <= 0) continue;
                    var elementArea = rect.width * rect.height;
                    var regionArea = regionRect.width * regionRect.height;
                    var centerInside = rect.x + rect.width / 2 >= regionRect.x &&
                        rect.x + rect.width / 2 <= regionRect.x + regionRect.width &&
                        rect.y + rect.height / 2 >= regionRect.y &&
                        rect.y + rect.height / 2 <= regionRect.y + regionRect.height;
                    if (!centerInside && overlap / elementArea < 0.18 && overlap / regionArea < 0.08) continue;
                    var text = this._elementText(el);
                    if (!text && !el.getAttribute('aria-label') && !el.getAttribute('title') && !el.getAttribute('placeholder')) continue;
                    matches.push({ el: el, rect: rect, overlap: overlap });
                }
                matches.sort(function(a, b) {
                    if (Math.abs(a.rect.y - b.rect.y) > 4) return a.rect.y - b.rect.y;
                    if (Math.abs(a.rect.x - b.rect.x) > 4) return a.rect.x - b.rect.x;
                    return b.overlap - a.overlap;
                });

                var result = [];
                var seenText = {};
                for (var j = 0; j < matches.length; j++) {
                    var summary = this._elementSummary(matches[j].el, matches[j].overlap);
                    var key = this._normalizeText(summary.tag + ' ' + summary.text + ' ' + summary.selector);
                    if (!key || seenText[key]) continue;
                    seenText[key] = true;
                    result.push(summary);
                    if (result.length >= 10) break;
                }
                return result;
            },

            _fallbackElementAtCenter: function(regionRect) {
                if (!document.elementFromPoint) return [];
                var x = regionRect.x + regionRect.width / 2;
                var y = regionRect.y + regionRect.height / 2;
                var el = document.elementFromPoint(x, y);
                if (!el || el.id === '__tiangong_annotation_canvas') return [];
                while (el && el !== document.body && !this._elementText(el)) {
                    el = el.parentElement;
                }
                if (!el || el === document.body || !this._isElementVisible(el)) return [];
                return [this._elementSummary(el, 0)];
            },

            _extractRegion: function(annotation) {
                var rect = this._annotationRect(annotation);
                if (!rect || rect.width <= 0 || rect.height <= 0) {
                    return { text: '', elements: [], elementCount: 0 };
                }
                // 将页面坐标转为视口坐标用于文本/元素提取
                var vp = this._toViewport(rect.x, rect.y);
                var viewportRect = { x: vp.x, y: vp.y, width: rect.width, height: rect.height };
                var text = this._extractTextInRect(viewportRect);
                var elements = this._extractElementsInRect(viewportRect);
                if (elements.length === 0) {
                    elements = this._fallbackElementAtCenter(viewportRect);
                }
                if (!text && elements.length > 0) {
                    text = this._shortText(elements.map(function(el) { return el.text; }).filter(Boolean).join(' '), 1200);
                }
                return {
                    x: Math.round(rect.x),
                    y: Math.round(rect.y),
                    width: Math.round(rect.width),
                    height: Math.round(rect.height),
                    text: text,
                    elements: elements,
                    elementCount: elements.length
                };
            },

            _enrichAnnotation: function(annotation, index) {
                var enriched = {};
                for (var key in annotation) {
                    if (Object.prototype.hasOwnProperty.call(annotation, key)) {
                        enriched[key] = annotation[key];
                    }
                }
                enriched.index = index + 1;
                enriched.region = this._extractRegion(annotation);
                return enriched;
            },

            _formatAnnotationsForAgent: function(annotations) {
                if (!annotations || annotations.length === 0) return '';
                var lines = ['[页面批注]'];
                for (var i = 0; i < annotations.length; i++) {
                    var annotation = annotations[i];
                    var region = annotation.region || {};
                    lines.push((i + 1) + '. ' + (annotation.type === 'arrow' ? '箭头批注' : '矩形批注') +
                        ' 区域：x=' + (region.x || 0) +
                        ', y=' + (region.y || 0) +
                        ', w=' + (region.width || 0) +
                        ', h=' + (region.height || 0));
                    if (region.text) {
                        lines.push('   区域文本：' + this._shortText(region.text, 600));
                    }
                    if (region.elements && region.elements.length > 0) {
                        lines.push('   覆盖元素：');
                        for (var j = 0; j < region.elements.length && j < 5; j++) {
                            var el = region.elements[j];
                            var identity = [el.tag, el.role ? 'role=' + el.role : '', el.text ? 'text="' + this._shortText(el.text, 80) + '"' : '']
                                .filter(Boolean)
                                .join(' ');
                            lines.push('   - ' + identity + (el.selector ? ' | selector: ' + el.selector : ''));
                        }
                    }
                    if (!region.text && (!region.elements || region.elements.length === 0)) {
                        lines.push('   未提取到明显文本或元素');
                    }
                }
                return lines.join('\n');
            },

            extractAnnotatedElements: function() {
                var bridge = window.__tiangong_bridge;
                if (!bridge) return { elements: [], count: 0 };
                var allElements = [];
                for (var i = 0; i < this._annotations.length; i++) {
                    var a = this._annotations[i];
                    if (a.type !== 'rect') continue;
                    var elements = bridge.extractElementsInRect(a.x, a.y, a.width, a.height);
                    allElements.push({
                        annotationIndex: i,
                        rect: { x: a.x, y: a.y, width: a.width, height: a.height },
                        elements: elements
                    });
                }
                return { elements: allElements, count: allElements.length };
            },

            _ensureCanvas: function() {
                if (this._canvas) return;
                var c = document.createElement('canvas');
                c.id = '__tiangong_annotation_canvas';
                c.style.cssText = 'position:fixed;top:0;left:0;width:100%;height:100%;z-index:2147483647;pointer-events:auto;cursor:crosshair;';
                document.body.appendChild(c);
                this._canvas = c;
                this._ctx = c.getContext('2d');
                var self = this;
                this._resizeHandler = function() { self._resize(); };
                this._scrollHandler = function() { self._render(); };
                window.addEventListener('resize', this._resizeHandler);
                window.addEventListener('scroll', this._scrollHandler, true);
                this._resize();
            },

            _resize: function() {
                if (!this._canvas) return;
                this._canvas.width = window.innerWidth;
                this._canvas.height = window.innerHeight;
                this._render();
            },

            // 将页面坐标转换为视口坐标
            _toViewport: function(px, py) {
                return { x: px - window.scrollX, y: py - window.scrollY };
            },

            // 将视口坐标转换为页面坐标
            _toPage: function(vx, vy) {
                return { x: vx + window.scrollX, y: vy + window.scrollY };
            },

            // 判断视口坐标是否命中某个批注
            _hitTest: function(vx, vy, annotation) {
                if (annotation.type === 'rect') {
                    var vp = this._toViewport(annotation.x, annotation.y);
                    // 矩形内部全部可选中，外围扩展 6px
                    return vx >= vp.x - 6 && vx <= vp.x + annotation.width + 6 &&
                           vy >= vp.y - 6 && vy <= vp.y + annotation.height + 6;
                }
                if (annotation.type === 'arrow') {
                    var a = this._toViewport(annotation.x1, annotation.y1);
                    var b = this._toViewport(annotation.x2, annotation.y2);
                    // 箭头线段容差 14px，端点单独检测（半径 18px）
                    if (this._pointDist(vx, vy, a.x, a.y) < 18) return true;
                    if (this._pointDist(vx, vy, b.x, b.y) < 18) return true;
                    return this._pointToSegmentDist(vx, vy, a.x, a.y, b.x, b.y) < 14;
                }
                return false;
            },

            _pointDist: function(px, py, x, y) {
                var dx = px - x, dy = py - y;
                return Math.sqrt(dx * dx + dy * dy);
            },

            _pointToSegmentDist: function(px, py, x1, y1, x2, y2) {
                var dx = x2 - x1, dy = y2 - y1;
                var len2 = dx * dx + dy * dy;
                if (len2 === 0) return Math.sqrt((px - x1) * (px - x1) + (py - y1) * (py - y1));
                var t = Math.max(0, Math.min(1, ((px - x1) * dx + (py - y1) * dy) / len2));
                var cx = x1 + t * dx, cy = y1 + t * dy;
                return Math.sqrt((px - cx) * (px - cx) + (py - cy) * (py - cy));
            },

            _bindEvents: function() {
                var self = this;
                this._onDown = function(e) { self._handleDown(e); };
                this._onMove = function(e) { self._handleMove(e); };
                this._onUp = function(e) { self._handleUp(e); };
                this._onKeyDown = function(e) { self._handleKeyDown(e); };
                this._canvas.addEventListener('mousedown', this._onDown);
                this._canvas.addEventListener('mousemove', this._onMove);
                this._canvas.addEventListener('mouseup', this._onUp);
                document.addEventListener('keydown', this._onKeyDown);
            },

            _unbindEvents: function() {
                if (this._canvas) {
                    this._canvas.removeEventListener('mousedown', this._onDown);
                    this._canvas.removeEventListener('mousemove', this._onMove);
                    this._canvas.removeEventListener('mouseup', this._onUp);
                }
                if (this._resizeHandler) {
                    window.removeEventListener('resize', this._resizeHandler);
                }
                if (this._scrollHandler) {
                    window.removeEventListener('scroll', this._scrollHandler, true);
                }
                if (this._onKeyDown) {
                    document.removeEventListener('keydown', this._onKeyDown);
                }
            },

            _handleKeyDown: function(e) {
                if (!this._active) return;
                if (e.key === 'Delete' || e.key === 'Backspace') {
                    e.preventDefault();
                    e.stopPropagation();
                    if (this._selectedIndex >= 0) {
                        this._annotations.splice(this._selectedIndex, 1);
                        this._selectedIndex = -1;
                        this._render();
                    }
                } else if (e.key === 'Escape') {
                    e.preventDefault();
                    this._selectedIndex = -1;
                    this._render();
                }
            },

            _handleDown: function(e) {
                var vx = e.clientX, vy = e.clientY;

                // 1. 如果已有选中批注，先检测是否点击了控制点
                if (this._selectedIndex >= 0 && this._selectedIndex < this._annotations.length) {
                    var sel = this._annotations[this._selectedIndex];
                    var handles = sel.type === 'rect' ? this._getRectHandles(sel) : this._getArrowHandles(sel);
                    for (var h = 0; h < handles.length; h++) {
                        if (Math.abs(vx - handles[h].x) < 10 && Math.abs(vy - handles[h].y) < 10) {
                            this._resizing = true;
                            this._resizeHandle = h;
                            this._dragging = false;
                            this._drawing = false;
                            return;
                        }
                    }
                }

                // 2. 检测是否点击了已有批注（不含控制点）
                for (var i = this._annotations.length - 1; i >= 0; i--) {
                    if (this._hitTest(vx, vy, this._annotations[i])) {
                        this._selectedIndex = i;
                        this._dragging = true;
                        this._drawing = false;
                        this._resizing = false;
                        var a = this._annotations[i];
                        if (a.type === 'rect') {
                            var vp = this._toViewport(a.x, a.y);
                            this._dragOffsetX = vx - vp.x;
                            this._dragOffsetY = vy - vp.y;
                        } else {
                            this._dragOffsetX = vx;
                            this._dragOffsetY = vy;
                        }
                        this._render();
                        return;
                    }
                }

                // 3. 未命中任何批注，开始新绘制
                this._selectedIndex = -1;
                this._startX = vx;
                this._startY = vy;
                this._drawing = true;
                this._dragging = false;
                this._resizing = false;
            },

            _handleMove: function(e) {
                var vx = e.clientX, vy = e.clientY;

                // 更新光标样式
                this._updateCursor(vx, vy);

                // 控制点拖拽调整大小
                if (this._resizing && this._selectedIndex >= 0) {
                    var a = this._annotations[this._selectedIndex];
                    var pg = this._toPage(vx, vy);
                    if (a.type === 'rect') {
                        this._resizeRect(a, this._resizeHandle, pg.x, pg.y);
                    } else if (a.type === 'arrow') {
                        if (this._resizeHandle === 0) {
                            a.x1 = pg.x; a.y1 = pg.y;
                        } else {
                            a.x2 = pg.x; a.y2 = pg.y;
                        }
                    }
                    this._render();
                    return;
                }

                // 拖拽移动已选中的批注
                if (this._dragging && this._selectedIndex >= 0) {
                    var a = this._annotations[this._selectedIndex];
                    var pg;
                    if (a.type === 'rect') {
                        pg = this._toPage(vx - this._dragOffsetX, vy - this._dragOffsetY);
                        a.x = pg.x;
                        a.y = pg.y;
                    } else {
                        var dx = vx - this._dragOffsetX;
                        var dy = vy - this._dragOffsetY;
                        pg = this._toPage(a.x1 + dx, a.y1 + dy);
                        var pg2 = this._toPage(a.x2 + dx, a.y2 + dy);
                        a.x1 = pg.x; a.y1 = pg.y;
                        a.x2 = pg2.x; a.y2 = pg2.y;
                        this._dragOffsetX = vx;
                        this._dragOffsetY = vy;
                    }
                    this._render();
                    return;
                }

                // 绘制新批注预览
                if (!this._drawing) return;
                this._render();
                var ctx = this._ctx;
                ctx.strokeStyle = this._color;
                ctx.lineWidth = this._strokeWidth;
                ctx.setLineDash([6, 4]);
                if (this._currentTool === 'rect') {
                    ctx.strokeRect(this._startX, this._startY, vx - this._startX, vy - this._startY);
                } else if (this._currentTool === 'arrow') {
                    this._drawArrow(ctx, this._startX, this._startY, vx, vy);
                }
                ctx.setLineDash([]);
            },

            _updateCursor: function(vx, vy) {
                if (!this._canvas) return;
                var prevHover = this._hoveredHandle;
                this._hoveredHandle = -1;
                // 1. 检测控制点悬停
                if (this._selectedIndex >= 0 && this._selectedIndex < this._annotations.length) {
                    var sel = this._annotations[this._selectedIndex];
                    var handles = sel.type === 'rect' ? this._getRectHandles(sel) : this._getArrowHandles(sel);
                    for (var h = 0; h < handles.length; h++) {
                        if (Math.abs(vx - handles[h].x) < 10 && Math.abs(vy - handles[h].y) < 10) {
                            this._hoveredHandle = h;
                            if (sel.type === 'rect') {
                                var cursors = ['nwse-resize','ns-resize','nesw-resize','ew-resize','nwse-resize','ns-resize','nesw-resize','ew-resize'];
                                this._canvas.style.cursor = cursors[h];
                            } else {
                                this._canvas.style.cursor = 'grab';
                            }
                            if (prevHover !== h) this._render();
                            return;
                        }
                    }
                }
                // 2. 检测批注悬停
                for (var i = this._annotations.length - 1; i >= 0; i--) {
                    if (this._hitTest(vx, vy, this._annotations[i])) {
                        this._canvas.style.cursor = 'move';
                        if (prevHover !== -1) this._render();
                        return;
                    }
                }
                this._canvas.style.cursor = 'crosshair';
                if (prevHover !== -1) this._render();
            },

            _resizeRect: function(a, handle, px, py) {
                var right = a.x + a.width;
                var bottom = a.y + a.height;
                switch (handle) {
                    case 0: a.x = px; a.y = py; a.width = right - px; a.height = bottom - py; break;
                    case 1: a.y = py; a.height = bottom - py; break;
                    case 2: a.width = px - a.x; a.y = py; a.height = bottom - py; break;
                    case 3: a.width = px - a.x; break;
                    case 4: a.width = px - a.x; a.height = py - a.y; break;
                    case 5: a.height = py - a.y; break;
                    case 6: a.x = px; a.width = right - px; a.height = py - a.y; break;
                    case 7: a.x = px; a.width = right - px; break;
                }
                // 防止翻转（宽高为负时修正）
                if (a.width < 0) { a.x += a.width; a.width = -a.width; }
                if (a.height < 0) { a.y += a.height; a.height = -a.height; }
            },

            _handleUp: function(e) {
                // 完成控制点拖拽
                if (this._resizing) {
                    this._resizing = false;
                    this._resizeHandle = -1;
                    return;
                }
                // 完成拖拽移动
                if (this._dragging) {
                    this._dragging = false;
                    return;
                }
                if (!this._drawing) return;
                this._drawing = false;
                var vx = e.clientX, vy = e.clientY;
                var pg = this._toPage(this._startX, this._startY);
                var pg2 = this._toPage(vx, vy);
                var annotation = {
                    type: this._currentTool,
                    color: this._color
                };
                if (this._currentTool === 'rect') {
                    annotation.x = Math.min(pg.x, pg2.x);
                    annotation.y = Math.min(pg.y, pg2.y);
                    annotation.width = Math.abs(pg2.x - pg.x);
                    annotation.height = Math.abs(pg2.y - pg.y);
                } else if (this._currentTool === 'arrow') {
                    annotation.x1 = pg.x;
                    annotation.y1 = pg.y;
                    annotation.x2 = pg2.x;
                    annotation.y2 = pg2.y;
                }
                if (annotation.width > 5 || annotation.height > 5 || annotation.x1 !== undefined) {
                    // 自动提取矩形区域内的 DOM 元素信息
                    if (annotation.type === 'rect' && window.__tiangong_bridge) {
                        try {
                            annotation.elements = window.__tiangong_bridge.extractElementsInRect(
                                annotation.x, annotation.y, annotation.width, annotation.height
                            );
                        } catch(ex) {}
                    }
                    this._annotations.push(annotation);
                }
                this._render();
            },

            _drawArrow: function(ctx, x1, y1, x2, y2) {
                var headLen = 10;
                var angle = Math.atan2(y2 - y1, x2 - x1);
                ctx.beginPath();
                ctx.moveTo(x1, y1);
                ctx.lineTo(x2, y2);
                ctx.lineTo(x2 - headLen * Math.cos(angle - Math.PI / 6), y2 - headLen * Math.sin(angle - Math.PI / 6));
                ctx.moveTo(x2, y2);
                ctx.lineTo(x2 - headLen * Math.cos(angle + Math.PI / 6), y2 - headLen * Math.sin(angle + Math.PI / 6));
                ctx.stroke();
            },

            // 获取矩形 8 个控制点的视口坐标（顺时针：TL, T, TR, R, BR, B, BL, L）
            _getRectHandles: function(a) {
                var vp = this._toViewport(a.x, a.y);
                var x = vp.x, y = vp.y, w = a.width, h = a.height;
                return [
                    { x: x, y: y },           // 0: TL
                    { x: x + w / 2, y: y },   // 1: T
                    { x: x + w, y: y },        // 2: TR
                    { x: x + w, y: y + h / 2 },// 3: R
                    { x: x + w, y: y + h },    // 4: BR
                    { x: x + w / 2, y: y + h },// 5: B
                    { x: x, y: y + h },         // 6: BL
                    { x: x, y: y + h / 2 }      // 7: L
                ];
            },

            // 获取箭头 2 个端点的视口坐标
            _getArrowHandles: function(a) {
                var va = this._toViewport(a.x1, a.y1);
                var vb = this._toViewport(a.x2, a.y2);
                return [{ x: va.x, y: va.y }, { x: vb.x, y: vb.y }];
            },

            _drawHandle: function(ctx, x, y, isHovered) {
                var r = isHovered ? 7 : 4;
                ctx.fillStyle = isHovered ? '#6366f1' : '#fff';
                ctx.strokeStyle = 'rgba(99,102,241,0.9)';
                ctx.lineWidth = isHovered ? 2 : 1.5;
                ctx.beginPath();
                ctx.arc(x, y, r, 0, Math.PI * 2);
                ctx.fill();
                ctx.stroke();
            },

            _render: function() {
                if (!this._ctx) return;
                this._ctx.clearRect(0, 0, this._canvas.width, this._canvas.height);
                for (var i = 0; i < this._annotations.length; i++) {
                    var a = this._annotations[i];
                    var isSelected = (i === this._selectedIndex);
                    this._ctx.strokeStyle = a.color;
                    this._ctx.lineWidth = 2;
                    this._ctx.setLineDash([]);
                    if (a.type === 'rect') {
                        var vp = this._toViewport(a.x, a.y);
                        this._ctx.strokeRect(vp.x, vp.y, a.width, a.height);
                        if (isSelected) {
                            var handles = this._getRectHandles(a);
                            for (var h = 0; h < handles.length; h++) {
                                this._drawHandle(this._ctx, handles[h].x, handles[h].y, h === this._hoveredHandle);
                            }
                        }
                    } else if (a.type === 'arrow') {
                        var va = this._toViewport(a.x1, a.y1);
                        var vb = this._toViewport(a.x2, a.y2);
                        this._drawArrow(this._ctx, va.x, va.y, vb.x, vb.y);
                        if (isSelected) {
                            var handles = this._getArrowHandles(a);
                            for (var h = 0; h < handles.length; h++) {
                                this._drawHandle(this._ctx, handles[h].x, handles[h].y, h === this._hoveredHandle);
                            }
                        }
                    }
                }
                this._updateToolbarCount();
            },

            _showToolbar: function() {
                if (this._toolbar) return;
                var self = this;
                var toolbar = document.createElement('div');
                toolbar.id = '__tiangong_annotation_toolbar';
                toolbar.style.cssText = 'position:fixed;bottom:16px;right:16px;z-index:2147483647;display:flex;align-items:center;gap:6px;padding:6px 10px;background:rgba(30,30,30,0.92);border-radius:8px;box-shadow:0 2px 12px rgba(0,0,0,0.3);font-family:system-ui,-apple-system,sans-serif;font-size:13px;color:#e5e5e5;user-select:none;backdrop-filter:blur(8px);';
                var tools = [
                    { key: 'rect', label: '矩形', icon: '▭' },
                    { key: 'arrow', label: '箭头', icon: '→' }
                ];
                for (var i = 0; i < tools.length; i++) {
                    (function(t) {
                        var btn = document.createElement('button');
                        btn.setAttribute('data-tool', t.key);
                        btn.textContent = t.icon + ' ' + t.label;
                        btn.title = t.label + '工具';
                        btn.style.cssText = 'padding:4px 10px;border:1px solid rgba(255,255,255,0.15);border-radius:5px;background:transparent;color:#e5e5e5;cursor:pointer;font-size:13px;line-height:1.2;transition:all 0.15s;';
                        if (self._currentTool === t.key) {
                            btn.style.background = 'rgba(99,102,241,0.8)';
                            btn.style.borderColor = 'rgba(99,102,241,0.9)';
                        }
                        btn.onmouseenter = function() { if (self._currentTool !== t.key) btn.style.background = 'rgba(255,255,255,0.1)'; };
                        btn.onmouseleave = function() { if (self._currentTool !== t.key) btn.style.background = 'transparent'; };
                        btn.onclick = function() { self._switchTool(t.key); };
                        toolbar.appendChild(btn);
                    })(tools[i]);
                }
                var sep = document.createElement('span');
                sep.style.cssText = 'width:1px;height:18px;background:rgba(255,255,255,0.15);margin:0 2px;';
                toolbar.appendChild(sep);
                var countSpan = document.createElement('span');
                countSpan.setAttribute('data-role', 'count');
                countSpan.style.cssText = 'font-size:12px;color:#9ca3af;min-width:32px;text-align:center;';
                countSpan.textContent = '0 个';
                toolbar.appendChild(countSpan);
                var clearBtn = document.createElement('button');
                clearBtn.setAttribute('data-role', 'clear');
                clearBtn.textContent = '清除';
                clearBtn.title = '清除所有批注';
                clearBtn.style.cssText = 'padding:4px 10px;border:1px solid rgba(239,68,68,0.3);border-radius:5px;background:transparent;color:#f87171;cursor:pointer;font-size:13px;line-height:1.2;transition:all 0.15s;';
                clearBtn.onmouseenter = function() { clearBtn.style.background = 'rgba(239,68,68,0.15)'; };
                clearBtn.onmouseleave = function() { clearBtn.style.background = 'transparent'; };
                clearBtn.onclick = function() { self.clear(); };
                toolbar.appendChild(clearBtn);
                document.body.appendChild(toolbar);
                this._toolbar = toolbar;
            },

            _hideToolbar: function() {
                if (this._toolbar && this._toolbar.parentNode) {
                    this._toolbar.parentNode.removeChild(this._toolbar);
                }
                this._toolbar = null;
            },

            _switchTool: function(tool) {
                this._currentTool = tool;
                this._selectedIndex = -1;
                var buttons = this._toolbar.querySelectorAll('[data-tool]');
                for (var i = 0; i < buttons.length; i++) {
                    var btn = buttons[i];
                    if (btn.getAttribute('data-tool') === tool) {
                        btn.style.background = 'rgba(99,102,241,0.8)';
                        btn.style.borderColor = 'rgba(99,102,241,0.9)';
                    } else {
                        btn.style.background = 'transparent';
                        btn.style.borderColor = 'rgba(255,255,255,0.15)';
                    }
                }
                this._render();
            },

            _updateToolbarCount: function() {
                if (!this._toolbar) return;
                var span = this._toolbar.querySelector('[data-role="count"]');
                if (span) {
                    span.textContent = this._annotations.length + ' 个';
                }
            },
        },

        // ── 持久观测层 ─────────────────────────────────────
        observer: {
            _eventQueue: [],
            _networkQueue: [],
            _mutationObserver: null,
            _debounceTimer: null,
            _pendingMutations: [],
            _started: false,
            _userEventBound: false,

            start: function() {
                if (this._started) return;
                this._started = true;
                this._flushPendingNetworkEvents();
                this._startMutationObserver();
                this._bindUserEvents();
            },

            stop: function() {
                this._started = false;
                if (this._mutationObserver) {
                    this._mutationObserver.disconnect();
                    this._mutationObserver = null;
                }
                if (this._debounceTimer) {
                    clearTimeout(this._debounceTimer);
                    this._debounceTimer = null;
                }
                this._pendingMutations = [];
            },

            drainEvents: function() {
                var events = this._eventQueue;
                this._eventQueue = [];
                return events;
            },

            drainNetworkEvents: function() {
                this._flushPendingNetworkEvents();
                var events = this._networkQueue;
                this._networkQueue = [];
                return events;
            },

            drainAllEvents: function() {
                this._flushPendingNetworkEvents();
                var events = this._eventQueue.concat(this._networkQueue);
                this._eventQueue = [];
                this._networkQueue = [];
                events.sort(function(a, b) {
                    return (a.timestamp || 0) - (b.timestamp || 0);
                });
                return events;
            },

            hasPendingEvents: function() {
                this._flushPendingNetworkEvents();
                return this._eventQueue.length > 0 || this._networkQueue.length > 0;
            },

            _flushPendingNetworkEvents: function() {
                var pending = window.__tiangong_pending_network_events || [];
                if (!pending.length) return;
                window.__tiangong_pending_network_events = [];
                for (var i = 0; i < pending.length; i++) {
                    this._pushEvent(pending[i]);
                }
            },

            // ── MutationObserver ──

            _startMutationObserver: function() {
                var self = this;
                this._mutationObserver = new MutationObserver(function(mutations) {
                    if (!self._started) return;
                    self._pendingMutations = self._pendingMutations.concat(mutations);
                    if (self._debounceTimer) clearTimeout(self._debounceTimer);
                    self._debounceTimer = setTimeout(function() {
                        self._flushMutations();
                    }, 500);
                });
                this._mutationObserver.observe(document.body, {
                    childList: true,
                    subtree: true,
                    characterData: true,
                    attributes: true,
                    attributeFilter: [
                        'class', 'disabled', 'readonly',
                        'aria-busy', 'aria-hidden', 'aria-disabled',
                        'aria-expanded', 'aria-selected'
                    ]
                });
            },

            _flushMutations: function() {
                var mutations = this._pendingMutations;
                this._pendingMutations = [];
                var events = this._analyzeMutations(mutations);
                for (var i = 0; i < events.length; i++) {
                    this._pushEvent(events[i]);
                }
            },

            _analyzeMutations: function(mutations) {
                var events = [];
                var dialogAdded = false;
                var dialogRemoved = false;
                var contentChanged = false;
                // 本批次新增的文本（变化的具体内容），用于反馈“变了什么”
                var addedTexts = [];
                var seen = [];

                for (var i = 0; i < mutations.length; i++) {
                    var m = mutations[i];

                    if (m.type === 'attributes') {
                        continue;
                    }

                    if (m.type === 'characterData') {
                        var ct = (m.target && m.target.textContent || '').trim();
                        if (ct) { contentChanged = true; addedTexts.push(ct); }
                        continue;
                    }

                    if (m.type === 'childList') {
                        for (var j = 0; j < m.addedNodes.length; j++) {
                            var node = m.addedNodes[j];
                            if (node.nodeType === 3) {
                                var tn = (node.textContent || '').trim();
                                if (tn) { contentChanged = true; addedTexts.push(tn); }
                                continue;
                            }
                            if (node.nodeType !== 1) continue;
                            if (this._isDialog(node) || this._containsDialog(node)) {
                                dialogAdded = true;
                            }
                            if (this._isMainContent(node) || this._isMainContent(m.target)) {
                                contentChanged = true;
                            }
                            var tag = node.tagName;
                            if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT') continue;
                            // 已收集祖先节点时跳过，避免重复
                            var covered = false;
                            for (var s = 0; s < seen.length; s++) {
                                if (seen[s].contains && seen[s].contains(node)) { covered = true; break; }
                            }
                            if (covered) continue;
                            var text = (node.innerText || node.textContent || '').replace(/\s+/g, ' ').trim();
                            if (text) {
                                seen.push(node);
                                addedTexts.push(text);
                                contentChanged = true;
                            }
                        }
                        for (var k = 0; k < m.removedNodes.length; k++) {
                            var rNode = m.removedNodes[k];
                            if (rNode.nodeType !== 1) continue;
                            if (this._isDialog(rNode) || this._containsDialog(rNode)) {
                                dialogRemoved = true;
                            }
                        }
                    }
                }

                if (dialogAdded) {
                    events.push({
                        type: 'dialog_opened',
                        timestamp: Date.now(),
                        detail: this._describeActiveOverlay()
                    });
                }
                if (dialogRemoved && !dialogAdded) {
                    events.push({
                        type: 'dialog_closed',
                        timestamp: Date.now()
                    });
                }
                if (contentChanged && !dialogAdded) {
                    events.push({
                        type: 'content_changed',
                        timestamp: Date.now(),
                        detail: this._summarizeAddedText(addedTexts)
                    });
                }

                return events;
            },

            // 变化内容：新增文本拼接（宿主按推送上限头尾截取）；本地先做一层
            // 上限防止极端页面一次性产出超大字符串。
            _summarizeAddedText: function(texts) {
                var joined = texts.join('\n').trim();
                var cap = 40000;
                if (joined.length > cap) {
                    joined = joined.substring(0, cap * 2 / 3) + '\n…\n' + joined.substring(joined.length - cap / 3);
                }
                return joined;
            },

            // ── 用户行为监听 ──

            _bindUserEvents: function() {
                if (this._userEventBound) return;
                this._userEventBound = true;
                var self = this;

                document.addEventListener('click', function(e) {
                    if (!self._started) return;
                    var target = e.target;
                    var interactive = target.closest(
                        'button, a, input, select, textarea, [role="button"], [role="link"], [role="tab"], summary'
                    );
                    if (!interactive) return;
                    var desc = self._describeElement(interactive);
                    self._pushEvent({
                        type: 'user_click',
                        timestamp: Date.now(),
                        element: desc.tag,
                        text: desc.text,
                        selector: desc.selector
                    });
                }, true);

                var inputTimer = null;
                var inputTarget = null;
                document.addEventListener('input', function(e) {
                    if (!self._started) return;
                    inputTarget = e.target;
                    if (inputTimer) clearTimeout(inputTimer);
                    inputTimer = setTimeout(function() {
                        if (!inputTarget) return;
                        var desc = self._describeElement(inputTarget);
                        self._pushEvent({
                            type: 'user_input',
                            timestamp: Date.now(),
                            selector: desc.selector,
                            label: desc.label || desc.placeholder,
                            value_length: (inputTarget.value || '').length
                        });
                        inputTarget = null;
                    }, 1000);
                }, true);

                window.addEventListener('popstate', function() {
                    if (!self._started) return;
                    self._pushEvent({
                        type: 'user_navigation',
                        timestamp: Date.now(),
                        url: window.location.href
                    });
                });
            },

            // ── 辅助方法 ──

            _isDialog: function(el) {
                if (el.nodeType !== 1) return false;
                if (el.getAttribute && el.getAttribute('role') === 'dialog') return true;
                // 通过几何特征检测覆盖层
                if (el.tagName) {
                    var style = window.getComputedStyle(el);
                    var pos = style.position;
                    if (pos === 'fixed' || pos === 'absolute') {
                        var rect = el.getBoundingClientRect();
                        var W = window.innerWidth;
                        var H = window.innerHeight;
                        if (rect.width > 100 && rect.height > 50 &&
                            rect.width < W - 10 && rect.height < H - 10) {
                            return true;
                        }
                    }
                }
                return false;
            },

            _containsDialog: function(el) {
                if (el.nodeType !== 1) return false;
                try {
                    var dialogs = el.querySelectorAll('[role="dialog"]');
                    if (dialogs.length > 0) return true;
                    // 检查 fixed/absolute 子元素
                    var fixed = el.querySelectorAll('*');
                    for (var i = 0; i < Math.min(fixed.length, 50); i++) {
                        var s = window.getComputedStyle(fixed[i]);
                        if ((s.position === 'fixed' || s.position === 'absolute')) {
                            var r = fixed[i].getBoundingClientRect();
                            if (r.width > 100 && r.height > 50 &&
                                r.width < window.innerWidth - 10 && r.height < window.innerHeight - 10) {
                                return true;
                            }
                        }
                    }
                } catch(e) {}
                return false;
            },

            _isMainContent: function(el) {
                if (el.nodeType !== 1) return false;
                var tag = el.tagName;
                if (tag === 'MAIN' || tag === 'ARTICLE') return true;
                if (el.id === 'app' || el.id === 'root' || el.id === '__next') return true;
                return false;
            },

            _describeElement: function(el) {
                var text = (el.textContent || '').trim().substring(0, 100);
                var tag = (el.tagName || '').toLowerCase();
                var selector = '';
                if (el.id) {
                    selector = '#' + el.id;
                } else {
                    selector = window.__tiangong_bridge.generateSelector(el);
                }
                var label = '';
                var labelEl = el.closest('[class*="form-item"]');
                if (labelEl) {
                    var lbl = labelEl.querySelector('[class*="label"]');
                    if (lbl) label = (lbl.textContent || '').trim();
                }
                var placeholder = el.placeholder || el.getAttribute('placeholder') || '';
                return { tag: tag, text: text, selector: selector, label: label, placeholder: placeholder };
            },

            _describeActiveOverlay: function() {
                var overlay = window.__tiangong_bridge._getTopmostOverlay();
                if (!overlay) return '';
                // 克隆后移除交互元素
                var clone = overlay.cloneNode(true);
                var interactive = clone.querySelectorAll('button, [role="button"], [class*="close"], [class*="Close"], [aria-label="Close"]');
                for (var i = 0; i < interactive.length; i++) interactive[i].remove();
                return (clone.innerText || '').trim();
            },

            _pushEvent: function(event) {
                if (event.type === 'network_response') {
                    this._networkQueue.push(event);
                    if (this._networkQueue.length > 100) {
                        this._networkQueue = this._networkQueue.slice(-50);
                    }
                } else {
                    this._eventQueue.push(event);
                    if (this._eventQueue.length > 100) {
                        this._eventQueue = this._eventQueue.slice(-50);
                    }
                }
            }
        },

        // ── 泛化覆盖层检测（替代 _getActiveDialog）───────────
        _getTopmostOverlay: function() {
            var W = window.innerWidth;
            var H = window.innerHeight;
            var points = [
                [Math.round(W / 2), Math.round(H / 3)],
                [Math.round(W / 2), Math.round(H / 2)]
            ];
            // 方法 1：elementFromPoint + 向上遍历找非全屏 fixed/absolute 容器
            for (var pi = 0; pi < points.length; pi++) {
                var el = document.elementFromPoint(points[pi][0], points[pi][1]);
                if (!el || el === document.body || el === document.documentElement) continue;
                var overlay = this._walkUpToOverlay(el, W, H);
                if (overlay) return overlay;
            }
            return null;
        },

        // 从命中元素向上找第一个 fixed/absolute 定位容器。
        // 如果遇到全屏蒙层，尝试在蒙层内查找非全屏子覆盖层。
        _walkUpToOverlay: function(el, W, H) {
            var current = el;
            while (current && current !== document.body && current !== document.documentElement) {
                var style = window.getComputedStyle(current);
                var pos = style.position;
                if (pos === 'fixed' || pos === 'absolute') {
                    var rect = current.getBoundingClientRect();
                    if (rect.width > 50 && rect.height > 50 &&
                        (rect.width < W - 10 || rect.height < H - 10)) {
                        return current;
                    }
                    // 全屏 fixed/absolute 容器（蒙层）：
                    // 在其内部查找非全屏的 fixed/absolute 子元素
                    var inner = this._findInnerOverlay(current, W, H);
                    if (inner) return inner;
                    // 蒙层内没有非全屏子覆盖层，检查蒙层自身是否有有意义的文本内容
                    var text = (current.innerText || '').replace(/\s+/g, ' ').trim();
                    if (text.length > 10) return current;
                    return null;
                }
                current = current.parentElement;
            }
            return null;
        },

        // 在全屏蒙层内查找面积适中的 fixed/absolute 子元素
        _findInnerOverlay: function(backdrop, W, H) {
            var candidates = backdrop.querySelectorAll('*');
            var best = null;
            var bestArea = 0;
            for (var i = 0; i < candidates.length; i++) {
                var child = candidates[i];
                var style = window.getComputedStyle(child);
                var pos = style.position;
                if (pos === 'fixed' || pos === 'absolute') {
                    var rect = child.getBoundingClientRect();
                    if (rect.width > 50 && rect.height > 50 &&
                        (rect.width < W - 10 || rect.height < H - 10)) {
                        var area = rect.width * rect.height;
                        if (area > bestArea) {
                            bestArea = area;
                            best = child;
                        }
                    }
                }
            }
            return best;
        },

        _extractOverlayContent: function(overlay) {
            if (!overlay) return '';
            var clone = overlay.cloneNode(true);
            var interactive = clone.querySelectorAll('button, [role="button"], [class*="close"], [class*="Close"], [aria-label="Close"]');
            for (var i = 0; i < interactive.length; i++) interactive[i].remove();
            return (clone.innerText || '').trim().substring(0, 3000);
        },
    };

    // --- 5. 拦截 target="_blank" 链接和 window.open ---
    // 嵌入浏览器只有一个 WebView，所有导航都在同一视图内完成。
    // window.open 重写为 location.href 导航
    window.open = function(url) {
        if (url) window.location.href = url;
        return null;
    };
    // 点击事件委托：拦截 target="_blank" 的 <a> 标签
    document.addEventListener('click', function(e) {
        var el = e.target;
        // 向上查找最近的 <a> 元素
        while (el && el.tagName !== 'A') {
            el = el.parentElement;
        }
        if (!el) return;
        var target = el.getAttribute('target');
        if (target === '_blank') {
            e.preventDefault();
            e.stopPropagation();
            var href = el.getAttribute('href');
            if (href && href.indexOf('javascript:') !== 0) {
                window.location.href = href;
            }
        }
    }, true);

    console.log('[Tiangong Bridge] loaded v0.9.0');
})();
