// OpenAI 生图设置页脚本（与 prompt/memory 同构的 postMessage 桥接框架）。

const HOST_TIMEOUT_MS = 60000;
let hostChannel = null;
let hostReadyResolve = null;
const hostReady = new Promise((resolve) => { hostReadyResolve = resolve; });
let requestSequence = 0;

function applyHostContext(context) {
  if (hostChannel && context.channel !== hostChannel) return;
  hostChannel = context.channel;
  const root = document.documentElement;
  root.dataset.theme = context.theme === "dark" ? "dark" : "light";
  Object.entries(context.tokens || {}).forEach(([name, value]) => {
    if (typeof value === "string" && value) root.style.setProperty(`--host-${name}`, value);
  });
  if (typeof context.fontFamily === "string" && context.fontFamily) {
    root.style.setProperty("--host-font-family", context.fontFamily);
  }
  hostReadyResolve?.();
  hostReadyResolve = null;
}

window.addEventListener("message", (event) => {
  if (event.source !== window.parent || !event.data) return;
  if (event.data.type === "tiangong_host_context" && typeof event.data.channel === "string") {
    applyHostContext(event.data);
  }
});
window.parent.postMessage({ type: "plugin_host_ready" }, "*");

async function callHost(method, payload = "") {
  if (!hostChannel) await hostReady;
  return new Promise((resolve, reject) => {
    const id = `img-${Date.now()}-${++requestSequence}`;
    const channel = hostChannel;
    const timeout = window.setTimeout(() => {
      window.removeEventListener("message", handler);
      reject(new Error("插件请求超时"));
    }, HOST_TIMEOUT_MS);
    const handler = (event) => {
      if (event.source !== window.parent || !event.data || event.data.id !== id || event.data.channel !== channel) return;
      window.clearTimeout(timeout);
      window.removeEventListener("message", handler);
      if (event.data.error) reject(new Error(String(event.data.error)));
      else resolve(event.data.result ?? "");
    };
    window.addEventListener("message", handler);
    window.parent.postMessage({ type: "plugin_call", channel, id, method, payload }, "*");
  });
}

// ── DOM ──

const sourceGlobal = document.getElementById("source-global");
const sourceChatgpt = document.getElementById("source-chatgpt");
const sourceManual = document.getElementById("source-manual");
const globalSection = document.getElementById("global-section");
const chatgptSection = document.getElementById("chatgpt-section");
const manualSection = document.getElementById("manual-section");
const globalModel = document.getElementById("global-model");
const globalHint = document.getElementById("global-hint");
const chatgptStatus = document.getElementById("chatgpt-status");
const chatgptModel = document.getElementById("chatgpt-model");
const chatgptModelList = document.getElementById("chatgpt-model-list");
const manualProtocol = document.getElementById("manual-protocol");
const manualBaseUrl = document.getElementById("manual-base-url");
const manualApiKey = document.getElementById("manual-api-key");
const manualModel = document.getElementById("manual-model");
const extraPrompt = document.getElementById("extra-prompt");
const saveBtn = document.getElementById("save-btn");
const statusEl = document.getElementById("status");

function currentSource() {
  if (sourceManual.checked) return "manual";
  if (sourceChatgpt.checked) return "chatgpt";
  return "global";
}

function toggleSource() {
  const source = currentSource();
  globalSection.hidden = source !== "global";
  chatgptSection.hidden = source !== "chatgpt";
  manualSection.hidden = source !== "manual";
}

sourceGlobal.addEventListener("change", toggleSource);
sourceChatgpt.addEventListener("change", toggleSource);
sourceManual.addEventListener("change", toggleSource);

function setStatus(message, type) {
  statusEl.textContent = message;
  statusEl.className = `status${type ? ` ${type}` : ""}`;
}

// ── 加载 ──

async function loadConfig() {
  try {
    const raw = await callHost("bootstrap", "{}");
    const data = raw ? JSON.parse(raw) : {};
    const config = data.config || {};
    const models = data.models || [];

    // 填充模型下拉
    globalModel.innerHTML = "";
    if (models.length === 0) {
      globalModel.innerHTML = '<option value="">（暂无已配置的 chat 模型）</option>';
    } else {
      const placeholder = document.createElement("option");
      placeholder.value = "";
      placeholder.textContent = "请选择模型";
      globalModel.appendChild(placeholder);
      models.forEach((m) => {
        const opt = document.createElement("option");
        opt.value = m.key;
        const status = m.configured ? "✓" : "未配置";
        opt.textContent = `${m.key} — ${m.model} (${status})`;
        globalModel.appendChild(opt);
      });
    }

    // ChatGPT 账号状态与可选模型
    const chatgpt = data.chatgpt || {};
    chatgptStatus.textContent = chatgpt.logged_in
      ? `已登录 ChatGPT 账号${chatgpt.email ? `：${chatgpt.email}` : ""}`
      : "尚未登录 ChatGPT 账号，请先在「设置 → 模型配置 → ChatGPT」中登录";
    chatgptStatus.className = chatgpt.logged_in ? "hint" : "hint error";
    chatgptModelList.innerHTML = "";
    (chatgpt.models || []).forEach((model) => {
      const opt = document.createElement("option");
      opt.value = model;
      chatgptModelList.appendChild(opt);
    });

    // 回显配置
    sourceManual.checked = config.source === "manual";
    sourceChatgpt.checked = config.source === "chatgpt";
    sourceGlobal.checked = !sourceManual.checked && !sourceChatgpt.checked;
    toggleSource();

    if (config.global_model_key) globalModel.value = config.global_model_key;
    chatgptModel.value = config.chatgpt_model || "";
    if (config.manual_endpoint) {
      manualProtocol.value = config.manual_endpoint.protocol === "chat_completions" ? "chat_completions" : "responses";
      manualBaseUrl.value = config.manual_endpoint.base_url || "";
      manualApiKey.value = config.manual_endpoint.api_key || "";
      manualModel.value = config.manual_endpoint.model || "";
    }
    extraPrompt.value = config.extra_prompt || "";

    globalHint.textContent = models.length > 0
      ? `共 ${models.length} 个 chat 模型可选`
      : "请先在设置中配置 chat 模型，或改用手动输入";
  } catch (error) {
    setStatus(`加载失败：${error.message || error}`, "error");
  }
}

// ── 保存 ──

async function saveConfig() {
  saveBtn.disabled = true;
  setStatus("保存中...", "");
  try {
    const source = currentSource();
    const payload = {
      source,
      global_model_key: source === "global" ? (globalModel.value || null) : null,
      chatgpt_model: chatgptModel.value.trim() || null,
      manual_endpoint: {
        protocol: manualProtocol.value,
        base_url: manualBaseUrl.value.trim(),
        api_key: manualApiKey.value,
        model: manualModel.value.trim(),
      },
      extra_prompt: extraPrompt.value.trim() || null,
    };
    await callHost("save_config", JSON.stringify(payload));
    setStatus("已保存", "success");
    setTimeout(() => setStatus("", ""), 3000);
  } catch (error) {
    setStatus(`保存失败：${error.message || error}`, "error");
  } finally {
    saveBtn.disabled = false;
  }
}

saveBtn.addEventListener("click", saveConfig);
loadConfig();
