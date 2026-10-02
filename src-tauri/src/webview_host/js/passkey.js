// 内置浏览器通行密钥（WebAuthn）桥接：仅在宿主确认具备浏览器通行密钥权限时注入。
// 接管 navigator.credentials.create/get 中的 publicKey 请求，转交原生
// AuthenticationServices 处理；其余请求（密码、联合登录、条件式自动填充等）
// 以及处理器尚未就绪时，一律回退到 WebKit 原生实现。
(function () {
    'use strict';
    if (window.__tiangong_passkey_loaded) return;
    window.__tiangong_passkey_loaded = true;

    var HANDLER_NAME = 'tiangongPasskey';
    var container = navigator.credentials;
    if (!container || typeof window.PublicKeyCredential === 'undefined') return;

    var originalCreate = container.create.bind(container);
    var originalGet = container.get.bind(container);
    var sequence = 0;

    function nativeHandler() {
        try {
            return (window.webkit && window.webkit.messageHandlers &&
                window.webkit.messageHandlers[HANDLER_NAME]) || null;
        } catch (e) {
            return null;
        }
    }

    function toBytes(source) {
        if (source instanceof ArrayBuffer) return new Uint8Array(source);
        if (ArrayBuffer.isView(source)) {
            return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
        }
        throw new TypeError('Expected a BufferSource');
    }

    function encode(source) {
        var bytes = toBytes(source);
        var binary = '';
        for (var i = 0; i < bytes.length; i += 0x8000) {
            binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
        }
        return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    }

    function decode(text) {
        var base64 = text.replace(/-/g, '+').replace(/_/g, '/');
        while (base64.length % 4) base64 += '=';
        var binary = atob(base64);
        var out = new Uint8Array(binary.length);
        for (var i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
        return out.buffer;
    }

    function descriptors(list) {
        return Array.prototype.map.call(list || [], function (item) {
            return { type: item.type || 'public-key', id: encode(item.id) };
        });
    }

    function encodeCreate(options) {
        var selection = options.authenticatorSelection;
        return {
            rp: { id: options.rp && options.rp.id, name: (options.rp && options.rp.name) || '' },
            user: {
                id: encode(options.user.id),
                name: options.user.name || '',
                displayName: options.user.displayName || ''
            },
            challenge: encode(options.challenge),
            pubKeyCredParams: Array.prototype.map.call(options.pubKeyCredParams || [], function (p) {
                return { type: p.type || 'public-key', alg: p.alg };
            }),
            excludeCredentials: descriptors(options.excludeCredentials),
            authenticatorSelection: selection ? {
                authenticatorAttachment: selection.authenticatorAttachment || null,
                userVerification: selection.userVerification || null
            } : null,
            attestation: options.attestation || null
        };
    }

    function encodeGet(options) {
        return {
            challenge: encode(options.challenge),
            rpId: options.rpId,
            allowCredentials: descriptors(options.allowCredentials),
            userVerification: options.userVerification || null
        };
    }

    // 原生以 "Name: message" 回传错误，按名称还原为 DOMException / TypeError。
    function toError(error) {
        var text = String((error && error.message) || error || '');
        var match = /^(\w+): ([\s\S]*)$/.exec(text);
        if (!match) return new DOMException(text || 'The operation failed.', 'NotAllowedError');
        if (match[1] === 'TypeError') return new TypeError(match[2]);
        return new DOMException(match[2], match[1]);
    }

    function abortError(signal) {
        return (signal && signal.reason) || new DOMException('The operation was aborted.', 'AbortError');
    }

    function define(target, values) {
        Object.keys(values).forEach(function (key) {
            Object.defineProperty(target, key, {
                value: values[key], enumerable: true, configurable: true, writable: false
            });
        });
    }

    // 让结果通过 instanceof 检查；自有属性会遮蔽原型上的访问器。
    function adopt(target, ctor) {
        try {
            if (ctor && ctor.prototype) Object.setPrototypeOf(target, ctor.prototype);
        } catch (e) { /* 保持普通对象 */ }
    }

    function clone(value) {
        return JSON.parse(JSON.stringify(value));
    }

    function buildCredential(json) {
        var raw = json.response;
        var response = {};
        define(response, { clientDataJSON: decode(raw.clientDataJSON) });
        if (raw.attestationObject) {
            var authenticatorData = decode(raw.authenticatorData);
            var publicKey = raw.publicKey ? decode(raw.publicKey) : null;
            define(response, {
                attestationObject: decode(raw.attestationObject),
                getTransports: function () { return (raw.transports || []).slice(); },
                getAuthenticatorData: function () { return authenticatorData; },
                getPublicKey: function () { return publicKey; },
                getPublicKeyAlgorithm: function () { return raw.publicKeyAlgorithm; },
                toJSON: function () { return clone(raw); }
            });
            adopt(response, window.AuthenticatorAttestationResponse);
        } else {
            define(response, {
                authenticatorData: decode(raw.authenticatorData),
                signature: decode(raw.signature),
                userHandle: raw.userHandle ? decode(raw.userHandle) : null,
                toJSON: function () { return clone(raw); }
            });
            adopt(response, window.AuthenticatorAssertionResponse);
        }
        var credential = {};
        define(credential, {
            id: json.id,
            rawId: decode(json.rawId),
            type: 'public-key',
            authenticatorAttachment: json.authenticatorAttachment || null,
            response: response,
            getClientExtensionResults: function () { return {}; },
            toJSON: function () { return clone(json); }
        });
        adopt(credential, window.PublicKeyCredential);
        return credential;
    }

    // 处理器未就绪时返回 null，由调用方回退到原生实现。
    function run(op, payload, signal) {
        var handler = nativeHandler();
        if (!handler) return null;
        return new Promise(function (resolve, reject) {
            if (signal && signal.aborted) {
                reject(abortError(signal));
                return;
            }
            var requestId = String(++sequence);
            var settled = false;
            function onAbort() {
                if (settled) return;
                settled = true;
                try {
                    handler.postMessage(JSON.stringify({ op: 'cancel', requestId: requestId }));
                } catch (e) { /* 原生侧会随页面销毁释放 */ }
                reject(abortError(signal));
            }
            function cleanup() {
                if (signal) signal.removeEventListener('abort', onAbort);
            }
            if (signal) signal.addEventListener('abort', onAbort, { once: true });
            handler.postMessage(JSON.stringify({ op: op, requestId: requestId, options: payload }))
                .then(function (text) {
                    if (settled) return;
                    settled = true;
                    cleanup();
                    try {
                        resolve(buildCredential(JSON.parse(text)));
                    } catch (e) {
                        reject(new DOMException('Invalid credential response.', 'UnknownError'));
                    }
                }, function (error) {
                    if (settled) return;
                    settled = true;
                    cleanup();
                    reject(toError(error));
                });
        });
    }

    function override(target, name, fn) {
        try {
            Object.defineProperty(target, name, { value: fn, configurable: true, writable: true });
        } catch (e) {
            try { target[name] = fn; } catch (e2) { /* 保持原实现 */ }
        }
    }

    override(container, 'create', function create(options) {
        if (!options || !options.publicKey || !nativeHandler()) return originalCreate(options);
        var payload;
        try {
            payload = encodeCreate(options.publicKey);
        } catch (e) {
            return Promise.reject(e instanceof TypeError ? e : new TypeError(String(e)));
        }
        return run('create', payload, options.signal) || originalCreate(options);
    });

    override(container, 'get', function get(options) {
        // 条件式（自动填充）请求暂不接管
        if (!options || !options.publicKey || options.mediation === 'conditional' ||
            !nativeHandler()) {
            return originalGet(options);
        }
        var payload;
        try {
            payload = encodeGet(options.publicKey);
        } catch (e) {
            return Promise.reject(e instanceof TypeError ? e : new TypeError(String(e)));
        }
        return run('get', payload, options.signal) || originalGet(options);
    });

    var PKC = window.PublicKeyCredential;
    var originalUvpaa = PKC.isUserVerifyingPlatformAuthenticatorAvailable;
    override(PKC, 'isUserVerifyingPlatformAuthenticatorAvailable', function () {
        if (nativeHandler()) return Promise.resolve(true);
        return originalUvpaa ? originalUvpaa.call(PKC) : Promise.resolve(false);
    });
    override(PKC, 'isConditionalMediationAvailable', function () {
        return Promise.resolve(false);
    });
})();
