// 火山引擎设置页脚本（与其他插件设置页同构的 postMessage 桥接框架）。

const HOST_TIMEOUT_MS = 60000;
const DEFAULT_BASE_URL = "https://ark.cn-beijing.volces.com/api/v3";
const DEFAULT_VIDEO_TIMEOUT = 300;
const DEFAULT_SPEECH = {
  base_url: "https://openspeech.bytedance.com",
  tts_resource_id: "seed-tts-2.0",
  tts_speaker: "zh_female_vv_uranus_bigtts",
  asr_resource_id: "volc.bigasr.auc_turbo",
};
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
    const id = `volc-${Date.now()}-${++requestSequence}`;
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

const apiKey = document.getElementById("api-key");
const baseUrl = document.getElementById("base-url");
const imageModel = document.getElementById("image-model");
const videoModel = document.getElementById("video-model");
const videoTimeout = document.getElementById("video-timeout");
const watermark = document.getElementById("watermark");
const speechApiKey = document.getElementById("speech-api-key");
const speechAppId = document.getElementById("speech-app-id");
const speechAccessToken = document.getElementById("speech-access-token");
const ttsResourceId = document.getElementById("tts-resource-id");
const ttsSpeaker = document.getElementById("tts-speaker");
const ttsSpeakerList = document.getElementById("tts-speaker-list");
const asrResourceId = document.getElementById("asr-resource-id");
const speechBaseUrl = document.getElementById("speech-base-url");
const saveBtn = document.getElementById("save-btn");
const statusEl = document.getElementById("status");

function setStatus(message, type) {
  statusEl.textContent = message;
  statusEl.className = `status${type ? ` ${type}` : ""}`;
}

// ── 加载 ──

async function loadConfig() {
  try {
    const raw = await callHost("bootstrap", "{}");
    const config = raw ? JSON.parse(raw) : {};
    apiKey.value = config.api_key || "";
    baseUrl.value = config.base_url && config.base_url !== DEFAULT_BASE_URL ? config.base_url : "";
    imageModel.value = config.image_model || "";
    videoModel.value = config.video_model || "";
    videoTimeout.value = String(config.video_poll_timeout_secs || DEFAULT_VIDEO_TIMEOUT);
    watermark.checked = Boolean(config.watermark);
    const speech = config.speech || {};
    speechApiKey.value = speech.api_key || "";
    speechAppId.value = speech.app_id || "";
    speechAccessToken.value = speech.access_token || "";
    ttsResourceId.value = speech.tts_resource_id || DEFAULT_SPEECH.tts_resource_id;
    ttsSpeaker.value = speech.tts_speaker || DEFAULT_SPEECH.tts_speaker;
    asrResourceId.value = speech.asr_resource_id || DEFAULT_SPEECH.asr_resource_id;
    speechBaseUrl.value = speech.base_url && speech.base_url !== DEFAULT_SPEECH.base_url ? speech.base_url : "";
  } catch (error) {
    setStatus(`加载失败：${error.message || error}`, "error");
  }
}

async function loadVoices() {
  try {
    const raw = await callHost("list_voices", "{}");
    const voices = (raw ? JSON.parse(raw).voices : []) || [];
    ttsSpeakerList.replaceChildren(...voices.map((voice) => {
      const option = document.createElement("option");
      option.value = voice.id;
      option.label = voice.name || voice.id;
      return option;
    }));
  } catch {
    // 音色候选仅为输入提示，加载失败不影响手填。
  }
}

// ── 保存 ──

async function saveConfig() {
  const timeoutValue = Number.parseInt(videoTimeout.value, 10);
  const hasArk = Boolean(apiKey.value.trim());
  const hasSpeech = Boolean(speechApiKey.value.trim())
    || Boolean(speechAppId.value.trim() && speechAccessToken.value.trim());
  if (!hasArk && !hasSpeech) {
    setStatus("请填写火山方舟 API Key 或豆包语音凭据", "error");
    return;
  }
  if (hasArk && !imageModel.value.trim() && !videoModel.value.trim()) {
    setStatus("请至少填写一个图片或视频生成模型", "error");
    return;
  }
  if (!speechApiKey.value.trim() && Boolean(speechAppId.value.trim()) !== Boolean(speechAccessToken.value.trim())) {
    setStatus("旧版控制台需同时填写 App ID 与 Access Token", "error");
    return;
  }
  saveBtn.disabled = true;
  setStatus("保存中...", "");
  try {
    const payload = {
      api_key: apiKey.value.trim(),
      base_url: baseUrl.value.trim() || DEFAULT_BASE_URL,
      image_model: imageModel.value.trim(),
      video_model: videoModel.value.trim(),
      watermark: watermark.checked,
      video_poll_timeout_secs: Number.isFinite(timeoutValue) ? Math.max(30, timeoutValue) : DEFAULT_VIDEO_TIMEOUT,
      speech: {
        base_url: speechBaseUrl.value.trim() || DEFAULT_SPEECH.base_url,
        api_key: speechApiKey.value.trim(),
        app_id: speechAppId.value.trim(),
        access_token: speechAccessToken.value.trim(),
        tts_resource_id: ttsResourceId.value.trim() || DEFAULT_SPEECH.tts_resource_id,
        tts_speaker: ttsSpeaker.value.trim() || DEFAULT_SPEECH.tts_speaker,
        asr_resource_id: asrResourceId.value.trim() || DEFAULT_SPEECH.asr_resource_id,
      },
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
loadVoices();
