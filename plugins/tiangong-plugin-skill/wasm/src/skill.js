// Skill 设置页脚本（与 scheduler/memory/index 同构的桥接框架）。

const HOST_TIMEOUT_MS = 60000;
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
    const id = `skill-${Date.now()}-${++requestSequence}`;
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
    window.parent.postMessage({ type: "plugin_call", channel, id, method, payload }, "*");
  });
}

function setHostMask(visible) {
  if (!hostChannel) return;
  window.parent.postMessage(
    {
      type: "plugin_host_mask",
      channel: hostChannel,
      visible,
      ...(visible ? { color: "rgba(0, 0, 0, 0.5)" } : {}),
    },
    "*",
  );
}

function syncHostMask() {
  const anyOpen = ["detail-modal", "env-modal", "remove-modal"].some(
    (id) => !document.getElementById(id).hidden,
  );
  setHostMask(anyOpen);
}

// ── 状态 ──

let skills = [];
let editingEnvId = null;

// ── DOM ──

const loadingState = document.getElementById("loading-state");
const skillContent = document.getElementById("skill-content");
const emptyState = document.getElementById("empty-state");
const listEl = document.getElementById("list");
const statusEl = document.getElementById("status");
const refreshBtn = document.getElementById("refresh-btn");
const refreshIcon = document.getElementById("refresh-icon");

// ── 加载列表 ──

async function loadSkills() {
  loadingState.hidden = false;
  skillContent.hidden = true;
  try {
    const raw = await callHost("list", "{}");
    const data = raw ? JSON.parse(raw) : { skills: [] };
    skills = Array.isArray(data.skills) ? data.skills : [];
    renderList();
  } catch (error) {
    showStatus(`加载 Skills 失败：${error.message || error}`, true);
  } finally {
    loadingState.hidden = true;
    skillContent.hidden = false;
  }
}

function renderList() {
  document.getElementById("skill-count-num").textContent = skills.length;
  listEl.innerHTML = "";
  if (skills.length === 0) {
    emptyState.hidden = false;
    listEl.hidden = true;
    return;
  }
  emptyState.hidden = true;
  listEl.hidden = false;
  for (const skill of skills) {
    listEl.appendChild(renderRow(skill));
  }
}

function renderRow(skill) {
  const row = document.createElement("div");
  row.className = "skill-row";

  const info = document.createElement("div");
  info.className = "skill-info";

  const nameRow = document.createElement("div");
  nameRow.className = "skill-name";

  const name = document.createElement("span");
  name.textContent = skill.name || skill.id;
  nameRow.appendChild(name);

  const enabledBadge = document.createElement("span");
  enabledBadge.className = `badge ${skill.enabled ? "badge-enabled" : "badge-disabled"}`;
  enabledBadge.textContent = skill.enabled ? "已启用" : "已禁用";
  nameRow.appendChild(enabledBadge);

  if (skill.version) {
    const versionBadge = document.createElement("span");
    versionBadge.className = "badge";
    versionBadge.textContent = `v${skill.version}`;
    nameRow.appendChild(versionBadge);
  }

  info.appendChild(nameRow);

  if (skill.description) {
    const desc = document.createElement("div");
    desc.className = "skill-description";
    desc.textContent = skill.description;
    info.appendChild(desc);
  }

  row.appendChild(info);

  // 操作按钮区
  const actions = document.createElement("div");
  actions.className = "skill-actions";

  // 详情按钮
  const detailBtn = document.createElement("button");
  detailBtn.className = "icon-btn";
  detailBtn.title = "查看详情";
  detailBtn.innerHTML = `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>`;
  detailBtn.addEventListener("click", () => showDetail(skill.id));
  actions.appendChild(detailBtn);

  // env 编辑按钮
  const envBtn = document.createElement("button");
  envBtn.className = "icon-btn";
  envBtn.title = "编辑环境变量";
  envBtn.innerHTML = `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/></svg>`;
  envBtn.addEventListener("click", () => showEnvEditor(skill.id));
  actions.appendChild(envBtn);

  // 打开目录按钮
  const revealBtn = document.createElement("button");
  revealBtn.className = "icon-btn";
  revealBtn.title = "在文件管理器中打开";
  revealBtn.innerHTML = `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>`;
  revealBtn.addEventListener("click", () => revealDir(skill.id));
  actions.appendChild(revealBtn);

  // 开关
  const switchBtn = document.createElement("button");
  switchBtn.className = "switch";
  switchBtn.dataset.on = String(skill.enabled);
  switchBtn.title = skill.enabled ? "禁用" : "启用";
  switchBtn.setAttribute("role", "switch");
  switchBtn.setAttribute("aria-checked", String(skill.enabled));
  switchBtn.addEventListener("click", () => toggleEnabled(skill.id, !skill.enabled));
  actions.appendChild(switchBtn);

  // 删除按钮
  const removeBtn = document.createElement("button");
  removeBtn.className = "icon-btn danger";
  removeBtn.title = "删除";
  removeBtn.innerHTML = `<svg viewBox="0 0 24 24" aria-hidden="true"><polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><path d="M10 11v6M14 11v6"/></svg>`;
  removeBtn.addEventListener("click", () => removeSkill(skill.id));
  actions.appendChild(removeBtn);

  row.appendChild(actions);
  return row;
}

// ── 操作 ──

async function toggleEnabled(id, enabled) {
  try {
    await callHost("toggle", JSON.stringify({ id, enabled }));
    await loadSkills();
  } catch (error) {
    showStatus(`操作失败：${error.message || error}`, true);
  }
}

// 待删除的 skill id（删除确认模态框打开期间有效）。
let removingSkillId = null;

/// 打开删除确认框。插件页运行在 sandbox="allow-scripts" 的 iframe 中，
/// window.confirm 会被宿主静默拦截并返回 false，因此改用页内模态框确认。
function removeSkill(id) {
  if (!id) {
    showStatus("删除失败：skill id 为空", true);
    return;
  }
  removingSkillId = id;
  const skill = skills.find((item) => item.id === id);
  const label = skill && skill.name && skill.name !== id ? `${skill.name}（${id}）` : id;
  document.getElementById("remove-message").textContent =
    `确定删除 Skill「${label}」？该 Skill 目录将被删除，此操作不可恢复。`;
  document.getElementById("remove-status").hidden = true;
  document.getElementById("remove-confirm-btn").disabled = false;
  openModal("remove-modal");
}

async function confirmRemoveSkill() {
  const id = removingSkillId;
  if (!id) return;
  const confirmBtn = document.getElementById("remove-confirm-btn");
  confirmBtn.disabled = true;
  try {
    const raw = await callHost("remove", JSON.stringify({ id }));
    const data = raw ? JSON.parse(raw) : {};
    // 孤儿 MCP 清理由 host 入口层处理（wasm 仅返回 orphan 列表）。
    removingSkillId = null;
    closeModal("remove-modal");
    await loadSkills();
    showStatus(data.message || `已删除：${id}`, false);
  } catch (error) {
    const status = document.getElementById("remove-status");
    status.textContent = `删除失败：${error.message || error}`;
    status.hidden = false;
  } finally {
    confirmBtn.disabled = false;
  }
}

async function refreshSkills() {
  refreshBtn.disabled = true;
  refreshIcon.classList.add("spinning");
  try {
    const raw = await callHost("refresh", "{}");
    const data = raw ? JSON.parse(raw) : {};
    await loadSkills();
    showStatus(data.message || "已刷新", false);
  } catch (error) {
    showStatus(`刷新失败：${error.message || error}`, true);
  } finally {
    refreshBtn.disabled = false;
    refreshIcon.classList.remove("spinning");
  }
}

// ── 详情模态框 ──

// 详情/编辑模态框状态
let detailSkillId = null;
let detailEditing = false;

async function showDetail(id) {
  try {
    const raw = await callHost("detail", JSON.stringify({ id }));
    const data = raw ? JSON.parse(raw) : {};
    const detail = data.detail || {};

    detailSkillId = id;
    detailEditing = false;

    document.getElementById("detail-title").textContent = detail.name || id;

    const badges = document.getElementById("detail-badges");
    badges.innerHTML = "";
    const addBadge = (text, cls = "") => {
      const b = document.createElement("span");
      b.className = `badge ${cls}`;
      b.textContent = text;
      badges.appendChild(b);
    };
    addBadge(detail.id);
    addBadge(detail.enabled ? "已启用" : "已禁用", detail.enabled ? "badge-enabled" : "badge-disabled");
    if (detail.version) addBadge(`v${detail.version}`);
    if (detail.entry) addBadge(detail.entry);

    document.getElementById("detail-desc").textContent = detail.description || "";
    document.getElementById("detail-readme").textContent = detail.readme || "（无说明）";
    document.getElementById("detail-textarea").value = detail.readme || "";

    // 重置为查看模式
    setDetailEditMode(false);
    // 显示编辑说明 + 打开目录按钮
    document.getElementById("detail-edit-btn").hidden = false;
    document.getElementById("detail-reveal-btn").hidden = false;
    hideDetailEditStatus();

    openModal("detail-modal");
  } catch (error) {
    showStatus(`读取详情失败：${error.message || error}`, true);
  }
}

/// 切换详情模态框的查看/编辑模式。
function setDetailEditMode(editing) {
  detailEditing = editing;
  document.getElementById("detail-readme").hidden = editing;
  document.getElementById("detail-editor").hidden = !editing;
  document.getElementById("detail-footer").hidden = !editing;
  document.getElementById("detail-edit-btn").hidden = editing;
  document.getElementById("detail-reveal-btn").hidden = editing;
  syncHostMask();
}

async function saveSkillMd() {
  if (!detailSkillId) return;
  const content = document.getElementById("detail-textarea").value;
  const saveBtn = document.getElementById("detail-save-btn");
  saveBtn.disabled = true;
  try {
    await callHost("update_md", JSON.stringify({ id: detailSkillId, content }));
    // 保存成功后回到查看模式并刷新 readme 展示
    document.getElementById("detail-readme").textContent = content || "（无说明）";
    setDetailEditMode(false);
    await loadSkills();
  } catch (error) {
    const status = document.getElementById("detail-edit-status");
    status.textContent = `保存失败：${error.message || error}`;
    status.className = "page-status error";
    status.hidden = false;
  } finally {
    saveBtn.disabled = false;
  }
}

function hideDetailEditStatus() {
  document.getElementById("detail-edit-status").hidden = true;
}

/// 在系统文件管理器中打开 skill 目录（经 sidecar reveal 操作）。
async function revealDir(id) {
  if (!id) {
    showStatus("打开目录失败：skill id 为空", true);
    return;
  }
  try {
    await callHost("reveal", JSON.stringify({ id }));
  } catch (error) {
    showStatus(`打开目录失败：${error.message || error}`, true);
  }
}

// ── env 编辑模态框 ──

async function showEnvEditor(id) {
  editingEnvId = id;
  document.getElementById("env-title").textContent = `编辑环境变量：${id}`;
  const rowsEl = document.getElementById("env-rows");
  rowsEl.innerHTML = "";
  hideStatus();

  try {
    const raw = await callHost("get_env", JSON.stringify({ id }));
    const data = raw ? JSON.parse(raw) : {};
    const env = data.env || {};
    for (const [key, value] of Object.entries(env)) {
      addEnvRow(key, value);
    }
  } catch {
    // 读取失败则空表。
  }
  if (rowsEl.children.length === 0) {
    addEnvRow("", "");
  }

  openModal("env-modal");
}

function addEnvRow(key = "", value = "") {
  const rowsEl = document.getElementById("env-rows");
  const row = document.createElement("div");
  row.className = "env-row";

  const keyInput = document.createElement("input");
  keyInput.className = "env-key";
  keyInput.type = "text";
  keyInput.value = key;
  keyInput.placeholder = "KEY";

  const valueInput = document.createElement("input");
  valueInput.type = "text";
  valueInput.value = value;
  valueInput.placeholder = "VALUE";

  const removeBtn = document.createElement("button");
  removeBtn.className = "icon-btn danger";
  removeBtn.type = "button";
  removeBtn.innerHTML = `<svg viewBox="0 0 24 24" aria-hidden="true"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>`;
  removeBtn.addEventListener("click", () => {
    row.remove();
  });

  row.appendChild(keyInput);
  row.appendChild(valueInput);
  row.appendChild(removeBtn);
  rowsEl.appendChild(row);
}

async function saveEnv() {
  if (!editingEnvId) return;
  const env = {};
  const rows = document.querySelectorAll("#env-rows .env-row");
  for (const row of rows) {
    const key = row.querySelector(".env-key").value.trim();
    const value = row.querySelector('input[type="text"]:not(.env-key)').value;
    if (key) {
      env[key] = value;
    }
  }

  const saveBtn = document.getElementById("env-save-btn");
  saveBtn.disabled = true;
  try {
    await callHost("set_env", JSON.stringify({ id: editingEnvId, env }));
    closeModal("env-modal");
    showStatus("环境变量已保存", false);
  } catch (error) {
    const envStatus = document.getElementById("env-status");
    envStatus.textContent = `保存失败：${error.message || error}`;
    envStatus.className = "page-status error";
    envStatus.hidden = false;
  } finally {
    saveBtn.disabled = false;
  }
}

// ── 模态框控制 ──

function openModal(id) {
  document.getElementById(id).hidden = false;
  syncHostMask();
}

function closeModal(id) {
  document.getElementById(id).hidden = true;
  syncHostMask();
}

// ── 状态提示 ──

function showStatus(message, isError) {
  statusEl.textContent = message;
  statusEl.className = `page-status${isError ? " error" : " success"}`;
  statusEl.hidden = !message;
  if (!isError) {
    setTimeout(() => {
      statusEl.hidden = true;
    }, 3000);
  }
}

function hideStatus() {
  statusEl.hidden = true;
}

// ── 事件绑定 ──

refreshBtn.addEventListener("click", refreshSkills);
document.getElementById("env-add-btn").addEventListener("click", () => addEnvRow());
document.getElementById("env-save-btn").addEventListener("click", saveEnv);
document.getElementById("detail-edit-btn").addEventListener("click", () => setDetailEditMode(true));
document.getElementById("detail-reveal-btn").addEventListener("click", () => {
  if (detailSkillId) revealDir(detailSkillId);
});
document.getElementById("detail-save-btn").addEventListener("click", saveSkillMd);
document.getElementById("detail-cancel-btn").addEventListener("click", () => setDetailEditMode(false));
document.getElementById("remove-confirm-btn").addEventListener("click", confirmRemoveSkill);

// 所有 data-close 元素关闭对应模态框。
document.querySelectorAll("[data-close]").forEach((el) => {
  el.addEventListener("click", () => closeModal(el.dataset.close));
});

// 点击遮罩关闭。
document.querySelectorAll(".modal-overlay").forEach((overlay) => {
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) {
      closeModal(overlay.id);
    }
  });
});

// ESC 关闭。
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    document.querySelectorAll(".modal-overlay:not([hidden])").forEach((m) => closeModal(m.id));
  }
});

// 初始加载。
loadSkills();
