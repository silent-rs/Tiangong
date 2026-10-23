// 索引管理设置页脚本。
//
// 与 memory.js 同构的 host context + callHost 桥接框架：
// - 启动时发 plugin_host_ready，等待宿主回传 host context（主题 + CSS token + channel）
// - callHost(method, payload) 经 postMessage 把请求发回宿主，宿主调 pluginCall 转发到 WASM
//   的 handle_view_message，结果回传（天工不解析消息内容，只做透传）

// 与 sidecar request_timeout_ms（60s）对齐，留出 IPC 通信余量。
// 大工作区重建耗时较长，30s 会先于 sidecar 失败导致前端误报。
const HOST_TIMEOUT_MS = 90000;
let hostChannel = null;
let hostReadyResolve = null;
const hostReady = new Promise((resolve) => {
  hostReadyResolve = resolve;
});
let requestSequence = 0;

function applyHostContext(context) {
  if (hostChannel && context.channel !== hostChannel) return;
  hostChannel = context.channel;
  const root = document.documentElement;
  root.dataset.theme = context.theme === "dark" ? "dark" : "light";
  Object.entries(context.tokens || {}).forEach(([name, value]) => {
    if (typeof value === "string" && value) {
      root.style.setProperty(`--host-${name}`, value);
    }
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
    const id = `index-${Date.now()}-${++requestSequence}`;
    const channel = hostChannel;
    const timeout = window.setTimeout(() => {
      window.removeEventListener("message", handler);
      reject(new Error("插件请求超时"));
    }, HOST_TIMEOUT_MS);
    const handler = (event) => {
      if (
        event.source !== window.parent ||
        !event.data ||
        event.data.id !== id ||
        event.data.channel !== channel
      )
        return;
      window.clearTimeout(timeout);
      window.removeEventListener("message", handler);
      if (event.data.error) {
        reject(new Error(String(event.data.error)));
      } else {
        resolve(event.data.result ?? "");
      }
    };
    window.addEventListener("message", handler);
    // v2 无 wasm 桥接：宿主方法经宿主桥 sidecar.* 直达插件 sidecar。
    window.parent.postMessage(
      { type: "plugin_call", channel, id, method: `sidecar.${method}`, payload: payload === "" ? "{}" : payload },
      "*",
    );
  });
}

// ── 索引管理业务（对齐原版 IndexManagementSettings）──

const listEl = document.getElementById("list");
const statusEl = document.getElementById("status");
const loadingState = document.getElementById("loading-state");
const emptyState = document.getElementById("empty-state");
const refreshBtn = document.getElementById("refresh-btn");
const refreshIcon = document.getElementById("refresh-icon");
const rowTemplate = document.getElementById("row-template");

function setStatus(text, isError = false) {
  statusEl.textContent = text;
  statusEl.classList.toggle("error", isError);
  statusEl.hidden = !text;
}

function showLoading() {
  loadingState.hidden = false;
  emptyState.hidden = true;
  listEl.innerHTML = "";
}

function renderList(items) {
  loadingState.hidden = true;
  emptyState.hidden = items.length !== 0;
  listEl.innerHTML = "";
  if (items.length === 0) return;

  for (const item of items) {
    const node = rowTemplate.content.firstElementChild.cloneNode(true);
    const root = item.root || `未知来源 (${String(item.id).slice(0, 12)}…)`;
    node.querySelector(".row-title").textContent = root;
    node.querySelector(".row-title").title = root;

    const countEl = node.querySelector(".row-count");
    countEl.textContent = `${item.entry_count ?? 0} 个文件`;

    const updatedEl = node.querySelector(".row-updated");
    if (item.updated_at) {
      updatedEl.textContent = `更新于 ${String(item.updated_at).replace("T", " ").slice(0, 19)}`;
    } else {
      updatedEl.textContent = "未记录时间";
      updatedEl.classList.add("warning");
    }

    const rebuildBtn = node.querySelector(".rebuild-btn");
    const deleteBtn = node.querySelector(".delete-btn");
    if (item.root) {
      rebuildBtn.addEventListener("click", () => rebuild(item, rebuildBtn));
    } else {
      rebuildBtn.hidden = true;
    }
    deleteBtn.addEventListener("click", () => remove(item, deleteBtn));

    listEl.appendChild(node);
  }
}

async function loadIndexes() {
  showLoading();
  refreshBtn.disabled = true;
  refreshIcon.classList.add("spinning");
  setStatus("");
  try {
    const raw = await callHost("index.list_workspace_indexes", "{}");
    const items = raw ? JSON.parse(raw) : [];
    renderList(Array.isArray(items) ? items : []);
    setStatus("");
  } catch (e) {
    renderList([]);
    setStatus(`加载索引列表失败：${e.message || e}`, true);
  } finally {
    refreshBtn.disabled = false;
    refreshIcon.classList.remove("spinning");
  }
}

async function rebuild(item, btn) {
  btn.disabled = true;
  btn.querySelector("svg").classList.add("spinning");
  setStatus(`重建中：${item.root || item.id} …`);
  try {
    const raw = await callHost("index.rebuild_workspace_index", JSON.stringify({ root: item.root }));
    await loadIndexes();
    const count = Number(raw ? JSON.parse(raw) : NaN);
    setStatus(Number.isFinite(count) ? `索引重建完成，共 ${count} 个文件` : "索引重建完成");
  } catch (e) {
    setStatus(`重建索引失败：${e.message || e}`, true);
  } finally {
    btn.disabled = false;
    btn.querySelector("svg").classList.remove("spinning");
  }
}

async function remove(item, btn) {
  btn.disabled = true;
  btn.querySelector("svg").classList.add("spinning");
  setStatus(`删除中：${item.root || item.id} …`);
  try {
    await callHost(
      "delete",
      JSON.stringify({ root: item.root, workspace_id: item.id }),
    );
    await loadIndexes();
    setStatus("索引已删除");
  } catch (e) {
    setStatus(`删除索引失败：${e.message || e}`, true);
  } finally {
    btn.disabled = false;
    btn.querySelector("svg").classList.remove("spinning");
  }
}

refreshBtn.addEventListener("click", loadIndexes);

// 挂载即加载列表。
loadIndexes();
