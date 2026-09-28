(() => {
  'use strict';

  const PAGE_SIZE = 50;
  const GRAPH_INITIAL_LIMIT = 240;
  const GRAPH_LIMIT_STEP = 120;
  const GRAPH_MAX_LIMIT = 500;
  const HOST_TIMEOUT_MS = 45000;
  const LLM_REMOTE = '__remote__';
  const SVG_NS = 'http://www.w3.org/2000/svg';

  const memoryTypes = [
    ['factual', '事实性'],
    ['user_preference', '用户偏好'],
    ['user_habit', '用户习惯'],
    ['skill', '技能型'],
    ['project_structure', '项目结构'],
    ['architecture_decision', '架构决策'],
    ['problem_incident', '问题故障'],
    ['domain_knowledge', '领域知识'],
  ];

  const relationKinds = [
    ['related_to', '相关'],
    ['depends_on', '依赖'],
    ['supports', '支撑'],
    ['contradicts', '冲突'],
    ['supersedes', '替代'],
    ['caused_by', '源于'],
    ['belongs_to', '归属'],
    ['learned_from', '学习自'],
    ['validated_by', '验证自'],
  ];

  const memoryColors = {
    factual: '#687585',
    user_preference: '#2878c7',
    user_habit: '#168f82',
    skill: '#318a48',
    project_structure: '#c37a11',
    architecture_decision: '#7756bd',
    problem_incident: '#c7443e',
    domain_knowledge: '#1686a0',
  };

  const state = {
    activeTab: 'data',
    viewMode: 'graph',
    query: '',
    status: 'active',
    page: 1,
    graphLimit: GRAPH_INITIAL_LIMIT,
    graphScale: 1,
    graphPanX: 0,
    graphPanY: 0,
    graphDragging: false,
    graphDidDrag: false,
    graphPointer: null,
    hoveredNodeId: null,
    relationTargetIndex: -1,
    relationTargetMatches: [],
    configLoaded: false,
    nodes: [],
    relations: [],
    selectedIds: new Set(),
    selectedNodeId: null,
    draft: emptyDraft(),
    draftRelations: [],
    totalCount: 0,
    activeCount: 0,
    weekCount: 0,
    config: {
      local_tier: 'mid',
      llm: { source: 'models_ref', key: null },
      embedding: { source: 'disabled', remote: null },
      rerank: { source: 'disabled', remote: null },
      vector_mode: 'auto',
    },
    models: [],
    defaultLlm: null,
    loadVersion: 0,
  };

  let requestSequence = 0;
  let searchTimer = null;
  let configSaveTimer = null;
  let configRevision = 0;
  let configPending = false;
  let configSaveQueue = Promise.resolve();
  let hostChannel = null;
  let hostReadyResolve;
  let lastModalFocus = null;
  const hostReady = new Promise((resolve) => {
    hostReadyResolve = resolve;
  });

  const byId = (id) => document.getElementById(id);

  function emptyDraft() {
    return {
      id: null,
      memory_type: 'factual',
      title: '',
      summary: '',
      keywords: [],
      importance: 0.6,
    };
  }

  function errorText(error) {
    return error instanceof Error ? error.message : String(error);
  }

  function escapeHtml(value) {
    return String(value ?? '')
      .replaceAll('&', '&amp;')
      .replaceAll('<', '&lt;')
      .replaceAll('>', '&gt;')
      .replaceAll('"', '&quot;')
      .replaceAll("'", '&#039;');
  }

  function truncate(value, maxLength) {
    const text = String(value ?? '');
    return text.length > maxLength ? `${text.slice(0, maxLength - 1)}…` : text;
  }

  function applyHostContext(context) {
    if (hostChannel && context.channel !== hostChannel) return;
    hostChannel = context.channel;
    const root = document.documentElement;
    root.dataset.theme = context.theme === 'dark' ? 'dark' : 'light';
    Object.entries(context.tokens || {}).forEach(([name, value]) => {
      if (typeof value === 'string' && value) {
        root.style.setProperty(`--host-${name}`, value);
      }
    });
    if (typeof context.fontFamily === 'string' && context.fontFamily) {
      root.style.setProperty('--host-font-family', context.fontFamily);
    }
    hostReadyResolve?.();
    hostReadyResolve = null;
  }

  window.addEventListener('message', (event) => {
    if (event.source !== window.parent || !event.data) return;
    if (event.data.type === 'tiangong_host_context' && typeof event.data.channel === 'string') {
      applyHostContext(event.data);
    }
  });
  // 独立配置页（tiangong-memory-sidecar --config）：没有宿主 iframe，改走本机 HTTP。
  // 仅当页面是顶层窗口（未嵌入宿主 iframe）且服务端注入了标记时才生效，
  // 避免宿主内误入独立模式。
  const standalone = window.top === window.self ? (window.__MEMORY_STANDALONE__ || null) : null;
  if (standalone) {
    document.documentElement.classList.add('standalone');
    const prefersDark = window.matchMedia?.('(prefers-color-scheme: dark)').matches;
    applyHostContext({ channel: 'standalone', theme: prefersDark ? 'dark' : 'light', tokens: {} });
    mountStandaloneClose();
  } else {
    window.parent.postMessage({ type: 'plugin_host_ready' }, '*');
  }

  // "完成并关闭"只在独立配置页创建；天工内嵌页面中不存在该元素。
  function mountStandaloneClose() {
    const button = document.createElement('button');
    button.id = 'standalone-close';
    button.className = 'primary-button';
    button.type = 'button';
    button.textContent = '完成并关闭';
    button.addEventListener('click', closeStandalone);
    document.querySelector('.topbar')?.appendChild(button);
  }

  async function callStandalone(path, body) {
    const response = await fetch(path, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-Memory-Token': standalone.token },
      body: JSON.stringify(body),
    });
    const text = await response.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch (_) { data = null; }
    if (!response.ok) {
      throw new Error(data?.error || `请求失败（HTTP ${response.status}）`);
    }
    return data;
  }

  async function callHost(method, payload = '') {
    if (standalone) {
      const result = await callStandalone(`/view/${encodeURIComponent(method)}`, { payload });
      return result == null ? '' : JSON.stringify(result);
    }
    if (!hostChannel) await hostReady;
    return new Promise((resolve, reject) => {
      const id = `memory-${Date.now()}-${++requestSequence}`;
      const channel = hostChannel;
      const timeout = window.setTimeout(() => {
        window.removeEventListener('message', handler);
        reject(new Error('插件请求超时'));
      }, HOST_TIMEOUT_MS);
      const handler = (event) => {
        if (
          event.source !== window.parent
          || !event.data
          || event.data.id !== id
          || event.data.channel !== channel
        ) return;
        window.clearTimeout(timeout);
        window.removeEventListener('message', handler);
        if (event.data.error) {
          reject(new Error(String(event.data.error)));
        } else {
          resolve(event.data.result ?? '');
        }
      };
      window.addEventListener('message', handler);
      window.parent.postMessage({ type: 'plugin_call', channel, id, method, payload }, '*');
    });
  }

  async function memoryRequest(payload) {
    const raw = await callHost('memory_request', JSON.stringify(payload));
    return raw ? JSON.parse(raw) : {};
  }

  function setRuntimeStatus(text, isError = false) {
    const target = byId('runtime-status');
    target.textContent = text;
    target.classList.toggle('error', isError);
  }

  function showToast(message, type = 'success') {
    const toast = document.createElement('div');
    toast.className = `toast ${type === 'error' ? 'error' : ''}`;
    toast.textContent = message;
    byId('toast-region').appendChild(toast);
    window.setTimeout(() => toast.remove(), 3600);
  }

  function setPreviewLoading(loading) {
    byId('preview-loading').classList.toggle('hidden', !loading);
    byId('refresh-button').disabled = loading;
  }

  function localDateTime(date) {
    const pad = (value) => String(value).padStart(2, '0');
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
  }

  function listQuery(overrides = {}) {
    return {
      workspace_id: null,
      query: state.query.trim() || null,
      status: state.status,
      created_after: null,
      offset: 0,
      limit: 0,
      ...overrides,
    };
  }

  async function loadBootstrap() {
    try {
      const raw = await callHost('bootstrap', '');
      const bootstrap = raw ? JSON.parse(raw) : {};
      state.config = normalizeConfig(bootstrap.config);
      state.models = Array.isArray(bootstrap.models) ? bootstrap.models : [];
      state.defaultLlm = bootstrap.default_llm || null;
      state.localModels = Array.isArray(bootstrap.local_models) ? bootstrap.local_models : [];
      state.configLoaded = true;
      renderConfig();
      scheduleLocalModelPoll();
      byId('config-status').textContent = '';
      setConfigControlsDisabled(false);
    } catch (error) {
      setRuntimeStatus('配置读取失败', true);
      showToast(`配置读取失败：${errorText(error)}`, 'error');
    }
  }

  async function loadRelationsForNodes(nodes) {
    if (nodes.length === 0) return [];
    try {
      const response = await memoryRequest({
        method: 'list_relations_batch',
        node_ids: nodes.map((node) => node.id),
      });
      return Array.isArray(response.items) ? response.items : [];
    } catch {
      const responses = await Promise.all(nodes.map(async (node) => {
        try {
          return await memoryRequest({ method: 'list_relations', node_id: node.id });
        } catch {
          return { items: [] };
        }
      }));
      const relations = new Map();
      responses.flatMap((response) => response.items || []).forEach((relation) => {
        relations.set(relation.id, relation);
      });
      return [...relations.values()];
    }
  }

  async function loadData() {
    const loadVersion = ++state.loadVersion;
    setPreviewLoading(true);
    const limit = state.viewMode === 'list' ? PAGE_SIZE : state.graphLimit;
    const offset = state.viewMode === 'list' ? (state.page - 1) * PAGE_SIZE : 0;
    const weekAgo = new Date();
    weekAgo.setDate(weekAgo.getDate() - 7);

    try {
      const [nodesResponse, totalResponse, activeResponse, weekResponse] = await Promise.all([
        memoryRequest({ method: 'list_nodes', query: listQuery({ limit, offset }) }),
        memoryRequest({ method: 'count_nodes', query: listQuery() }),
        memoryRequest({ method: 'count_nodes', query: listQuery({ status: 'active' }) }),
        memoryRequest({
          method: 'count_nodes',
          query: listQuery({ created_after: localDateTime(weekAgo) }),
        }),
      ]);
      if (loadVersion !== state.loadVersion) {
        return null;
      }

      state.nodes = Array.isArray(nodesResponse.items) ? nodesResponse.items : [];
      state.totalCount = Number(totalResponse.count || 0);
      state.activeCount = Number(activeResponse.count || 0);
      state.weekCount = Number(weekResponse.count || 0);
      state.selectedIds = new Set(
        [...state.selectedIds].filter((id) => state.nodes.some((node) => node.id === id)),
      );

      if (state.nodes.length > 0) {
        state.relations = await loadRelationsForNodes(state.nodes);
        if (loadVersion !== state.loadVersion) return null;
      } else {
        state.relations = [];
      }

      setRuntimeStatus('已连接');
      renderData();
      return true;
    } catch (error) {
      if (loadVersion === state.loadVersion) {
        setRuntimeStatus('连接失败', true);
        showToast(`无法加载记忆：${errorText(error)}`, 'error');
      }
      if (loadVersion !== state.loadVersion) return null;
      return false;
    } finally {
      if (loadVersion === state.loadVersion) {
        setPreviewLoading(false);
      }
    }
  }

  function renderData() {
    renderStats();
    renderGraph();
    renderList();
    renderRelationTargets();
    renderRelations();
  }

  function renderStats() {
    byId('stat-total').textContent = String(state.totalCount);
    byId('stat-active').textContent = String(state.activeCount);
    byId('stat-week').textContent = String(state.weekCount);
    byId('stat-relations').textContent = String(visibleRelations().length);
    byId('stat-loaded').textContent = state.totalCount > state.nodes.length
      ? `已加载 ${state.nodes.length}`
      : `已加载全部 ${state.nodes.length}`;
  }

  function visibleRelations() {
    const visible = new Set(state.nodes.map((node) => node.id));
    return state.relations.filter(
      (relation) => visible.has(relation.from_node_id) && visible.has(relation.to_node_id),
    );
  }

  function relationStats(relations) {
    const stats = new Map(state.nodes.map((node) => [node.id, 0]));
    relations.forEach((relation) => {
      stats.set(relation.from_node_id, (stats.get(relation.from_node_id) || 0) + 1);
      stats.set(relation.to_node_id, (stats.get(relation.to_node_id) || 0) + 1);
    });
    return stats;
  }

  function graphLayout(nodes, relations) {
    const degrees = relationStats(relations);
    const selectedId = state.selectedNodeId && nodes.some((node) => node.id === state.selectedNodeId)
      ? state.selectedNodeId
      : null;
    const adjacency = new Map(nodes.map((node) => [node.id, []]));
    relations.forEach((relation) => {
      adjacency.get(relation.from_node_id)?.push(relation.to_node_id);
      adjacency.get(relation.to_node_id)?.push(relation.from_node_id);
    });

    const anchor = selectedId || [...nodes]
      .sort((left, right) => (degrees.get(right.id) || 0) - (degrees.get(left.id) || 0))[0]?.id;
    const orderedIds = [];
    const seen = new Set();
    if (anchor) {
      const queue = [anchor];
      seen.add(anchor);
      while (queue.length > 0) {
        const id = queue.shift();
        orderedIds.push(id);
        const neighbors = [...(adjacency.get(id) || [])]
          .sort((left, right) => (degrees.get(right) || 0) - (degrees.get(left) || 0));
        neighbors.forEach((neighbor) => {
          if (!seen.has(neighbor)) {
            seen.add(neighbor);
            queue.push(neighbor);
          }
        });
      }
    }
    [...nodes]
      .sort((left, right) => (degrees.get(right.id) || 0) - (degrees.get(left.id) || 0))
      .forEach((node) => {
        if (!seen.has(node.id)) {
          seen.add(node.id);
          orderedIds.push(node.id);
        }
      });

    const positions = new Map();
    if (orderedIds.length === 0) {
      return { positions, degrees };
    }
    positions.set(orderedIds[0], { x: 500, y: 280 });
    let cursor = 1;
    let ring = 1;
    while (cursor < orderedIds.length) {
      const capacity = Math.min(42, 8 + ring * 6);
      const count = Math.min(capacity, orderedIds.length - cursor);
      const radiusX = Math.min(440, 92 + ring * 76);
      const radiusY = Math.min(238, 60 + ring * 43);
      for (let index = 0; index < count; index += 1) {
        const angle = (Math.PI * 2 * index) / count - Math.PI / 2 + ring * 0.19;
        positions.set(orderedIds[cursor + index], {
          x: 500 + Math.cos(angle) * radiusX,
          y: 280 + Math.sin(angle) * radiusY,
        });
      }
      cursor += count;
      ring += 1;
    }
    return { positions, degrees };
  }

  function svgElement(name, attributes = {}) {
    const element = document.createElementNS(SVG_NS, name);
    Object.entries(attributes).forEach(([key, value]) => element.setAttribute(key, String(value)));
    return element;
  }

  function renderGraph() {
    const relations = visibleRelations();
    const selected = state.nodes.find((node) => node.id === state.selectedNodeId);
    const graphContent = byId('graph-content');
    graphContent.replaceChildren();
    graphContent.setAttribute(
      'transform',
      `translate(${state.graphPanX} ${state.graphPanY}) translate(500 280) scale(${state.graphScale}) translate(-500 -280)`,
    );

    byId('graph-title').textContent = selected?.title || 'Memory 图谱';
    byId('graph-edit-selected').classList.toggle('hidden', !selected);
    byId('graph-status-selected').classList.toggle('hidden', !selected);
    if (selected) {
      byId('graph-status-selected').textContent = selected.status === 'active' ? '归档' : '恢复';
    }
    byId('graph-meta').textContent = state.totalCount > state.nodes.length
      ? `已加载 ${state.nodes.length} / 共 ${state.totalCount} 个节点 · ${relations.length} 条连接`
      : `${state.nodes.length} 个节点 · ${relations.length} 条连接`;
    byId('graph-empty').classList.toggle('hidden', state.nodes.length > 0);

    const canLoadMore = state.nodes.length < state.totalCount && state.graphLimit < GRAPH_MAX_LIMIT;
    byId('graph-load-more').classList.toggle('hidden', !canLoadMore);

    if (state.nodes.length === 0) {
      return;
    }

    const { positions, degrees } = graphLayout(state.nodes, relations);
    const neighbors = new Set();
    if (state.selectedNodeId) {
      neighbors.add(state.selectedNodeId);
      relations.forEach((relation) => {
        if (relation.from_node_id === state.selectedNodeId) neighbors.add(relation.to_node_id);
        if (relation.to_node_id === state.selectedNodeId) neighbors.add(relation.from_node_id);
      });
    }
    const relationColors = {
      contradicts: 'var(--danger)',
      supersedes: 'var(--status-warning-color)',
      depends_on: 'var(--status-info-color)',
      caused_by: 'var(--status-error-color)',
      supports: 'var(--status-success-color)',
    };
    const labelLimit = state.nodes.length <= 18 ? state.nodes.length : state.nodes.length <= 50 ? 5 : 1;
    const labelIds = new Set(
      [...state.nodes]
        .sort((left, right) => (degrees.get(right.id) || 0) - (degrees.get(left.id) || 0))
        .slice(0, labelLimit)
        .map((node) => node.id),
    );
    if (state.selectedNodeId) {
      labelIds.add(state.selectedNodeId);
    }
    const edges = svgElement('g', { class: 'graph-edges' });
    relations.forEach((relation) => {
      const from = positions.get(relation.from_node_id);
      const to = positions.get(relation.to_node_id);
      if (!from || !to) {
        return;
      }
      const connected = !state.selectedNodeId
        || relation.from_node_id === state.selectedNodeId
        || relation.to_node_id === state.selectedNodeId;
      const line = svgElement('line', {
        x1: from.x,
        y1: from.y,
        x2: to.x,
        y2: to.y,
        class: 'graph-edge',
        stroke: relationColors[relation.relation_kind] || 'var(--border-strong)',
        'stroke-width': connected && state.selectedNodeId
          ? Math.max(2, Math.min(4, Number(relation.weight || 1) + 1))
          : Math.max(1, Math.min(3, Number(relation.weight || 1))),
        opacity: state.selectedNodeId ? (connected ? 0.9 : 0.12) : (state.nodes.length > 100 ? 0.38 : 0.58),
      });
      if (state.nodes.length <= 100) {
        line.setAttribute('marker-end', 'url(#arrow)');
      }
      edges.appendChild(line);
    });
    graphContent.appendChild(edges);

    const nodesGroup = svgElement('g', { class: 'graph-nodes' });
    state.nodes.forEach((node) => {
      const position = positions.get(node.id);
      if (!position) {
        return;
      }
      const selectedNode = node.id === state.selectedNodeId;
      const degree = degrees.get(node.id) || 0;
      const radius = Math.min(28, 11 + degree * 1.4 + (selectedNode ? 4 : 0));
      const group = svgElement('g', {
        class: `graph-node ${selectedNode ? 'selected' : ''} ${state.selectedNodeId && !neighbors.has(node.id) ? 'dimmed' : ''}`,
        transform: `translate(${position.x} ${position.y})`,
        'data-node-id': node.id,
        tabindex: '0',
        role: 'button',
        'aria-label': node.title,
      });
      const title = svgElement('title');
      title.textContent = `${node.title}\n${node.summary}`;
      group.appendChild(title);
      group.appendChild(svgElement('circle', {
        r: radius,
        fill: memoryColors[node.memory_type] || memoryColors.factual,
        opacity: Math.max(0.45, Math.min(1, Number(node.importance || 0.5))),
      }));
      if (labelIds.has(node.id)) {
        const label = svgElement('text', { x: 0, y: radius + 17 });
        label.textContent = truncate(node.title, 18);
        group.appendChild(label);
      }
      nodesGroup.appendChild(group);
    });
    graphContent.appendChild(nodesGroup);
  }

  function renderList() {
    const totalPages = Math.max(1, Math.ceil(state.totalCount / PAGE_SIZE));
    if (state.page > totalPages) {
      state.page = totalPages;
    }
    const start = state.totalCount === 0 ? 0 : (state.page - 1) * PAGE_SIZE + 1;
    const end = Math.min(state.totalCount, start + state.nodes.length - 1);
    const allSelected = state.nodes.length > 0 && state.nodes.every((node) => state.selectedIds.has(node.id));
    byId('select-all').checked = allSelected;
    byId('select-all').disabled = state.nodes.length === 0;
    byId('list-meta').textContent = `${start}-${Math.max(start, end)} / ${state.totalCount} · 已选 ${state.selectedIds.size}`;
    byId('page-meta').textContent = `第 ${state.page} / ${totalPages} 页`;
    byId('page-prev').disabled = state.page <= 1;
    byId('page-next').disabled = state.page >= totalPages;
    byId('bulk-status').disabled = state.selectedIds.size === 0;
    byId('bulk-status').textContent = state.status === 'active' ? '批量归档' : '批量恢复';

    if (state.nodes.length === 0) {
      byId('memory-list').innerHTML = '<div class="inline-empty">暂无匹配记忆</div>';
      return;
    }

    byId('memory-list').innerHTML = state.nodes.map((node) => {
      const checked = state.selectedIds.has(node.id) ? 'checked' : '';
      const selected = node.id === state.selectedNodeId ? 'selected' : '';
      const keywords = (node.keywords || []).slice(0, 4)
        .map((keyword) => `<span class="keyword">${escapeHtml(keyword)}</span>`)
        .join('');
      return `
        <article class="memory-row ${selected}" data-node-id="${escapeHtml(node.id)}">
          <input class="row-select" type="checkbox" aria-label="选择 ${escapeHtml(node.title)}" ${checked}>
          <button class="memory-row-main" type="button" data-action="edit">
            <div class="memory-row-title">
              <strong>${escapeHtml(node.title)}</strong>
              <span class="badge">${escapeHtml(memoryTypeLabel(node.memory_type))}</span>
            </div>
            <span class="memory-row-summary">${escapeHtml(node.summary)}</span>
            ${keywords ? `<span class="keyword-list">${keywords}</span>` : ''}
          </button>
          <div class="row-actions">
            <button type="button" data-action="edit">编辑</button>
            <button type="button" data-action="status">${node.status === 'active' ? '归档' : '恢复'}</button>
          </div>
        </article>`;
    }).join('');
  }

  function memoryTypeLabel(value) {
    return memoryTypes.find(([key]) => key === value)?.[1] || value;
  }

  function relationKindLabel(value) {
    return relationKinds.find(([key]) => key === value)?.[1] || value;
  }

  function updateEditor() {
    byId('memory-id').value = state.draft.id || '';
    byId('memory-type').value = state.draft.memory_type || 'factual';
    byId('memory-title').value = state.draft.title || '';
    byId('memory-summary').value = state.draft.summary || '';
    byId('memory-keywords').value = (state.draft.keywords || []).join(', ');
    byId('memory-importance').value = String(state.draft.importance ?? 0.6);
    byId('importance-output').textContent = Number(state.draft.importance ?? 0.6).toFixed(1);
    const enabled = Boolean(state.draft.id);
    byId('relation-target-search').disabled = !enabled;
    byId('relation-kind').disabled = !enabled;
    byId('relation-note').disabled = !enabled;
    byId('save-relation').disabled = !enabled;
    renderRelationTargets();
    renderRelations();
  }

  async function editNode(node) {
    state.selectedNodeId = node.id;
    state.draft = {
      id: node.id,
      memory_type: node.memory_type,
      title: node.title,
      summary: node.summary,
      keywords: Array.isArray(node.keywords) ? node.keywords : [],
      importance: Number(node.importance || 0.6),
    };
    state.draftRelations = [];
    updateEditor();
    renderGraph();
    renderList();
    try {
      const response = await memoryRequest({ method: 'list_relations', node_id: node.id });
      if (state.draft.id === node.id) {
        state.draftRelations = Array.isArray(response.items) ? response.items : [];
        renderRelations();
      }
    } catch (error) {
      showToast(`无法读取记忆关联：${errorText(error)}`, 'error');
    }
  }

  function startNew() {
    state.selectedNodeId = null;
    state.draft = emptyDraft();
    state.draftRelations = [];
    byId('relation-note').value = '';
    updateEditor();
    renderGraph();
    renderList();
  }

  async function saveMemory(event) {
    event.preventDefault();
    const title = byId('memory-title').value.trim();
    const summary = byId('memory-summary').value.trim();
    if (!title || !summary) {
      showToast('标题和内容都不能为空', 'error');
      return;
    }
    const button = byId('save-memory');
    button.disabled = true;
    button.textContent = '保存中';
    const draft = {
      id: byId('memory-id').value || null,
      memory_type: byId('memory-type').value,
      title,
      summary,
      keywords: byId('memory-keywords').value
        .split(/[,，]/)
        .map((item) => item.trim())
        .filter(Boolean),
      importance: Number(byId('memory-importance').value || 0.6),
      workspace_id: null,
      session_id: null,
    };
    try {
      const response = await memoryRequest({ method: 'upsert_manual_memory', draft });
      const saved = response.item;
      if (!saved) {
        throw new Error('保存结果缺少记忆数据');
      }
      showToast('记忆已保存');
      await loadData();
      await editNode(saved);
    } catch (error) {
      showToast(`保存失败：${errorText(error)}`, 'error');
    } finally {
      button.disabled = false;
      button.textContent = '保存记忆';
    }
  }

  async function setNodeStatus(node, nextStatus) {
    try {
      await memoryRequest({ method: 'set_node_status', node_id: node.id, status: nextStatus });
      if (state.selectedNodeId === node.id) {
        startNew();
      }
      showToast(nextStatus === 'archived' ? '记忆已归档' : '记忆已恢复');
      await loadData();
    } catch (error) {
      showToast(`操作失败：${errorText(error)}`, 'error');
    }
  }

  async function setSelectedStatus() {
    if (state.selectedIds.size === 0) {
      return;
    }
    const button = byId('bulk-status');
    const nextStatus = state.status === 'active' ? 'archived' : 'active';
    const ids = [...state.selectedIds];
    button.disabled = true;
    button.textContent = '处理中';
    try {
      await Promise.all(ids.map((nodeId) => memoryRequest({
        method: 'set_node_status',
        node_id: nodeId,
        status: nextStatus,
      })));
      if (state.selectedNodeId && state.selectedIds.has(state.selectedNodeId)) {
        startNew();
      }
      state.selectedIds.clear();
      showToast(nextStatus === 'archived' ? `已归档 ${ids.length} 条记忆` : `已恢复 ${ids.length} 条记忆`);
      await loadData();
    } catch (error) {
      showToast(`批量操作失败：${errorText(error)}`, 'error');
    } finally {
      button.disabled = state.selectedIds.size === 0;
      button.textContent = state.status === 'active' ? '批量归档' : '批量恢复';
    }
  }

  function relationSearchText(node) {
    return [node.title, node.summary, node.memory_type, ...(node.keywords || [])]
      .join(' ')
      .toLocaleLowerCase('zh-CN');
  }

  function renderRelationTargets(open = false) {
    const search = byId('relation-target-search');
    const options = byId('relation-target-options');
    const query = search.value.trim().toLocaleLowerCase('zh-CN');
    state.relationTargetMatches = state.nodes
      .filter((node) => node.id !== state.draft.id)
      .filter((node) => !query || relationSearchText(node).includes(query))
      .sort((left, right) => left.title.localeCompare(right.title, 'zh-CN'))
      .slice(0, 30);
    state.relationTargetIndex = Math.min(
      Math.max(state.relationTargetIndex, 0),
      Math.max(0, state.relationTargetMatches.length - 1),
    );
    options.innerHTML = state.relationTargetMatches.length
      ? state.relationTargetMatches.map((node, index) => `
          <button type="button" role="option" aria-selected="${String(node.id === byId('relation-target').value)}" data-node-id="${escapeHtml(node.id)}" class="combobox-option ${index === state.relationTargetIndex ? 'active' : ''}">
            <span>${escapeHtml(truncate(node.title, 56))}</span>
            <small>${escapeHtml(memoryTypeLabel(node.memory_type))}</small>
          </button>`).join('')
      : '<div class="combobox-empty">没有匹配记忆</div>';
    const visible = open && !search.disabled;
    options.classList.toggle('hidden', !visible);
    search.setAttribute('aria-expanded', String(visible));
  }

  function selectRelationTarget(nodeId) {
    const node = state.nodes.find((item) => item.id === nodeId);
    if (!node) return;
    byId('relation-target').value = node.id;
    byId('relation-target-search').value = node.title;
    renderRelationTargets(false);
  }

  function renderRelations() {
    const target = byId('relation-list');
    if (!state.draft.id) {
      target.innerHTML = '<div class="inline-empty">保存记忆后可建立关联</div>';
      return;
    }
    if (state.draftRelations.length === 0) {
      target.innerHTML = '<div class="inline-empty">暂无关联</div>';
      return;
    }
    target.innerHTML = state.draftRelations.map((relation) => {
      const otherId = relation.from_node_id === state.draft.id
        ? relation.to_node_id
        : relation.from_node_id;
      const other = state.nodes.find((node) => node.id === otherId);
      return `
        <div class="relation-item" data-relation-id="${escapeHtml(relation.id)}">
          <div class="relation-copy">
            <strong>${escapeHtml(relationKindLabel(relation.relation_kind))}：${escapeHtml(other?.title || otherId)}</strong>
            ${relation.note ? `<span>${escapeHtml(relation.note)}</span>` : ''}
          </div>
          <button class="relation-delete" type="button" title="删除关联" aria-label="删除关联">×</button>
        </div>`;
    }).join('');
  }

  async function saveRelation(event) {
    event.preventDefault();
    const targetId = byId('relation-target').value;
    if (!state.draft.id || !targetId || targetId === state.draft.id) {
      showToast('请选择另一条记忆作为关联目标', 'error');
      return;
    }
    const button = byId('save-relation');
    button.disabled = true;
    button.textContent = '保存中';
    try {
      await memoryRequest({
        method: 'upsert_relation',
        draft: {
          id: null,
          from_node_id: state.draft.id,
          to_node_id: targetId,
          relation_kind: byId('relation-kind').value,
          weight: 1,
          note: byId('relation-note').value.trim() || null,
        },
      });
      byId('relation-target').value = '';
      byId('relation-target-search').value = '';
      byId('relation-note').value = '';
      await refreshRelations();
      showToast('关联已保存');
    } catch (error) {
      showToast(`关联失败：${errorText(error)}`, 'error');
    } finally {
      button.disabled = !state.draft.id;
      button.textContent = '保存关联';
    }
  }

  async function refreshRelations() {
    if (!state.draft.id) {
      return;
    }
    const [single, relations] = await Promise.all([
      memoryRequest({ method: 'list_relations', node_id: state.draft.id }),
      loadRelationsForNodes(state.nodes),
    ]);
    state.draftRelations = Array.isArray(single.items) ? single.items : [];
    state.relations = relations;
    renderRelations();
    renderGraph();
    renderStats();
  }

  async function deleteRelation(relationId) {
    try {
      await memoryRequest({ method: 'delete_relation', relation_id: relationId });
      await refreshRelations();
      showToast('关联已删除');
    } catch (error) {
      showToast(`删除失败：${errorText(error)}`, 'error');
    }
  }

  function normalizeRemote(remote) {
    if (!remote) return null;
    return {
      base_url: remote.base_url || '',
      model: remote.model || '',
      protocol: remote.protocol || '',
      timeout_ms: Number(remote.timeout_ms || 0),
      dimension: remote.dimension ?? null,
      has_api_key: Boolean(remote.has_api_key),
    };
  }

  function normalizeConfig(config) {
    return {
      local_tier: config?.local_tier || 'mid',
      llm: {
        source: config?.llm?.source || 'models_ref',
        key: config?.llm?.key ?? null,
        remote: normalizeRemote(config?.llm?.remote),
      },
      embedding: {
        source: config?.embedding?.source || 'disabled',
        remote: normalizeRemote(config?.embedding?.remote),
      },
      rerank: {
        source: config?.rerank?.source || 'disabled',
        remote: normalizeRemote(config?.rerank?.remote),
      },
      vector_mode: config?.vector_mode || 'auto',
    };
  }

  // LLM 下拉：首项"跟随默认（lite → chat）"，其后为路由与 chat 模型，
  // 末项"自定义在线端点"——独立运行（无天工模型配置）时直接填写端点。
  function renderLlmSelect() {
    const select = byId('config-model');
    const llm = state.config.llm;
    const follow = state.defaultLlm
      ? `跟随默认（lite → chat，当前 ${escapeHtml(state.defaultLlm)}）`
      : '跟随默认（lite → chat，当前未配置）';
    const known = state.models.some((model) => model.key === llm.key);
    const missing = llm.source === 'models_ref' && llm.key && !known
      ? `<option value="${escapeHtml(llm.key)}">${escapeHtml(llm.key)}（当前不可用）</option>`
      : '';
    const options = state.models.map((model) => {
      const kind = model.kind === 'route' ? '路由' : '模型';
      return `<option value="${escapeHtml(model.key)}">${escapeHtml(model.key)} · ${kind} · ${escapeHtml(model.provider)} / ${escapeHtml(model.model)}</option>`;
    }).join('');
    select.innerHTML = `<option value="">${follow}</option>${missing}${options}<option value="${LLM_REMOTE}">自定义在线端点</option>`;
    select.value = llm.source === 'remote' ? LLM_REMOTE : (llm.key || '');
  }

  function renderRemoteFields(prefix, component) {
    const remote = component.remote || {};
    byId(`${prefix}-remote`).classList.toggle('hidden', component.source !== 'remote');
    byId(`${prefix}-url`).value = remote.base_url || '';
    byId(`${prefix}-model`).value = remote.model || '';
    byId(`${prefix}-key`).value = '';
    byId(`${prefix}-key`).placeholder = remote.has_api_key
      ? 'API Key 已保存（留空保留，支持 ${ENV}）'
      : 'API Key（可留空，支持 ${ENV}）';
    if (prefix === 'embedding') {
      byId('embedding-dimension').value = remote.dimension || '';
    }
    if (prefix === 'llm') {
      byId('llm-protocol').value = remote.protocol || 'openai_chatcompletions';
    }
  }

  function renderConfig() {
    renderLlmSelect();
    renderRemoteFields('llm', state.config.llm);
    byId('config-tier').value = state.config.local_tier || 'mid';
    byId('config-embedding-source').value = state.config.embedding.source;
    byId('config-rerank-source').value = state.config.rerank.source;
    renderRemoteFields('embedding', state.config.embedding);
    renderRemoteFields('rerank', state.config.rerank);
    byId('config-vector-mode').value = state.config.vector_mode || 'auto';
    renderComponentMeta();
  }

  function renderComponentMeta() {
    const llmRemote = byId('config-model').value === LLM_REMOTE;
    byId('llm-remote').classList.toggle('hidden', !llmRemote);
    const llmMeta = byId('llm-meta');
    llmMeta.classList.remove('warning');
    if (llmRemote) {
      const complete = byId('llm-url').value.trim() && byId('llm-model').value.trim();
      llmMeta.textContent = complete ? '自定义在线端点' : '自定义在线端点 · 需填写地址与模型名';
      if (!complete) llmMeta.classList.add('warning');
    } else if (!byId('config-model').value && !state.defaultLlm) {
      llmMeta.textContent = '未找到可用模型，请选择"自定义在线端点"';
      llmMeta.classList.add('warning');
    } else {
      llmMeta.textContent = '片段提取、回忆规划、结果整理';
    }
    const tierLabel = { low: '低', mid: '中', high: '高' }[byId('config-tier').value] || '中';
    const embeddingSource = byId('config-embedding-source').value;
    const embeddingMeta = byId('embedding-meta');
    embeddingMeta.classList.remove('warning');
    if (embeddingSource === 'builtin') {
      const info = localModelMeta('embedding');
      embeddingMeta.textContent = `内置 · ${tierLabel}档 · ${info.text}`;
      if (info.warning) embeddingMeta.classList.add('warning');
    } else if (embeddingSource === 'remote') {
      const dimension = Number(byId('embedding-dimension').value || 0);
      embeddingMeta.textContent = dimension > 0 ? `在线端点 · 向量维度 ${dimension}` : '在线端点 · 缺少向量维度';
      if (dimension <= 0) embeddingMeta.classList.add('warning');
    } else {
      embeddingMeta.textContent = '语义检索和向量索引';
    }
    const rerankSource = byId('config-rerank-source').value;
    const rerankMeta = byId('rerank-meta');
    rerankMeta.classList.remove('warning');
    if (rerankSource === 'builtin') {
      const info = localModelMeta('rerank');
      rerankMeta.textContent = `内置 · ${tierLabel}档 · ${info.text}`;
      if (info.warning) rerankMeta.classList.add('warning');
    } else if (rerankSource === 'remote') {
      rerankMeta.textContent = '在线端点';
    } else {
      rerankMeta.textContent = '召回结果精排';
    }
    byId('embedding-remote').classList.toggle('hidden', embeddingSource !== 'remote');
    byId('rerank-remote').classList.toggle('hidden', rerankSource !== 'remote');
  }

  function formatSize(bytes) {
    const value = Number(bytes || 0);
    if (value >= 1e9) return `${(value / 1e9).toFixed(1)} GB`;
    return `${Math.max(1, Math.round(value / 1e6))} MB`;
  }

  // 当前档位内置模型的说明：模型名、维度、大小与下载/加载状态。
  function localModelMeta(kind) {
    const tier = byId('config-tier').value || 'mid';
    const model = (state.localModels || []).find((item) => item.tier === tier && item.kind === kind);
    if (!model) return { text: '本地推理', warning: false };
    const name = model.dimension ? `${model.model}（${model.dimension} 维）` : model.model;
    const size = formatSize(model.size);
    switch (model.state) {
      case 'ready':
        return { text: `${name} · 已就绪`, warning: false };
      case 'installed':
        return { text: `${name} · 已下载`, warning: false };
      case 'loading':
        return { text: `${name} · 加载中`, warning: false };
      case 'downloading': {
        const percent = model.size ? Math.floor((model.downloaded / model.size) * 100) : 0;
        return { text: `${name} · 下载中 ${percent}%（共 ${size}）`, warning: false };
      }
      case 'interrupted': {
        // 有残留临时文件但当前无人下载：上次中断，保存后会断点续传。
        const percent = model.size ? Math.floor((model.downloaded / model.size) * 100) : 0;
        return { text: `${name} · 下载已中断 ${percent}%，保存后继续（共 ${size}）`, warning: false };
      }
      case 'failed':
        return { text: `${name} · ${model.error || '不可用'}`, warning: true };
      case 'unsupported':
        return { text: `${name} · 当前平台不支持内置模型，请改用在线端点`, warning: true };
      default:
        return { text: `${name} · 保存后自动下载约 ${size}`, warning: false };
    }
  }

  const CONFIG_CONTROL_IDS = [
    'config-model', 'llm-url', 'llm-model', 'llm-protocol', 'llm-key', 'llm-probe',
    'config-tier', 'config-embedding-source', 'embedding-url', 'embedding-model',
    'embedding-dimension', 'embedding-key', 'embedding-probe', 'config-rerank-source', 'rerank-url',
    'rerank-model', 'rerank-key', 'rerank-probe', 'config-vector-mode', 'save-config',
  ];

  function setConfigControlsDisabled(disabled) {
    CONFIG_CONTROL_IDS.forEach((id) => { byId(id).disabled = disabled; });
  }

  function readRemote(prefix, previous, withDimension) {
    const key = byId(`${prefix}-key`).value.trim();
    const protocolInput = prefix === 'llm' ? byId('llm-protocol').value : '';
    const remote = {
      base_url: byId(`${prefix}-url`).value.trim(),
      model: byId(`${prefix}-model`).value.trim(),
      protocol: protocolInput || previous?.protocol || 'openai_chatcompletions',
      timeout_ms: previous?.timeout_ms || 0,
      has_api_key: Boolean(previous?.has_api_key),
    };
    if (withDimension) {
      const dimension = Number(byId('embedding-dimension').value || 0);
      remote.dimension = dimension > 0 ? dimension : null;
    }
    if (key) remote.api_key = key;
    return remote;
  }

  function currentConfig() {
    const llmValue = byId('config-model').value;
    const llm = llmValue === LLM_REMOTE
      ? { source: 'remote', remote: readRemote('llm', state.config.llm.remote, false) }
      : { source: 'models_ref', key: llmValue || null };
    const embeddingSource = byId('config-embedding-source').value;
    const rerankSource = byId('config-rerank-source').value;
    return {
      local_tier: byId('config-tier').value || 'mid',
      llm,
      embedding: {
        source: embeddingSource,
        remote: embeddingSource === 'remote'
          ? readRemote('embedding', state.config.embedding.remote, true)
          : null,
      },
      rerank: {
        source: rerankSource,
        remote: rerankSource === 'remote'
          ? readRemote('rerank', state.config.rerank.remote, false)
          : null,
      },
      vector_mode: byId('config-vector-mode').value || 'auto',
    };
  }

  function validateConfig(config) {
    if (config.llm.source === 'remote') {
      const remote = config.llm.remote;
      if (!remote.base_url || !remote.model) return '记忆文本模型在线端点需要地址与模型名';
    }
    if (config.embedding.source === 'remote') {
      const remote = config.embedding.remote;
      if (!remote.base_url || !remote.model) return '嵌入模型在线端点需要地址与模型名';
      if (!(remote.dimension > 0)) return '嵌入模型缺少向量维度，可点击"探测"自动获取';
    }
    if (config.rerank.source === 'remote') {
      const remote = config.rerank.remote;
      if (!remote.base_url || !remote.model) return '重排模型在线端点需要地址与模型名';
    }
    return null;
  }

  // 保存成功后：已填写的密钥视为已保存，清空输入框避免重复提交。
  // 保存成功后同步已知状态；`stale` 表示期间用户又改了表单，
  // 此时只记录后端已保存的内容，不能用旧配置重绘表单覆盖新输入。
  function afterConfigSaved(config, stale = false) {
    ['llm', 'embedding', 'rerank'].forEach((prefix) => {
      const remote = config[prefix].remote;
      if (remote?.api_key) remote.has_api_key = true;
      if (remote) delete remote.api_key;
    });
    state.config = normalizeConfig(config);
    if (stale) {
      // 仅刷新与表单输入无关的状态（本地模型下载进度）。
      scheduleLocalModelPoll();
      return;
    }
    renderRemoteFields('llm', state.config.llm);
    renderRemoteFields('embedding', state.config.embedding);
    renderRemoteFields('rerank', state.config.rerank);
    renderComponentMeta();
    scheduleLocalModelPoll();
  }

  let localModelPollTimer = null;

  // 选择了内置来源且模型尚未就绪时，定期刷新下载/加载进度（只更新状态文字，不动表单）。
  function scheduleLocalModelPoll() {
    window.clearTimeout(localModelPollTimer);
    const tier = state.config.local_tier || 'mid';
    const pending = ['embedding', 'rerank'].some((kind) => {
      if (state.config[kind]?.source !== 'builtin') return false;
      const model = (state.localModels || []).find((item) => item.tier === tier && item.kind === kind);
      // ready/failed 是终态；interrupted 表示当前无人下载，等用户保存后再动。
      return !model || !['ready', 'failed', 'interrupted', 'unsupported'].includes(model.state);
    });
    if (!pending) return;
    localModelPollTimer = window.setTimeout(async () => {
      try {
        const raw = await callHost('bootstrap', '');
        const bootstrap = raw ? JSON.parse(raw) : {};
        if (Array.isArray(bootstrap.local_models)) state.localModels = bootstrap.local_models;
        renderComponentMeta();
      } catch (_) {
        // 轮询失败不打扰用户，下次再试。
      }
      scheduleLocalModelPoll();
    }, 3000);
  }

  async function probeRemote(component) {
    const prefix = component;
    const previous = state.config[component].remote;
    const remote = readRemote(prefix, previous, component === 'embedding');
    if (!remote.base_url || !remote.model) {
      showToast('请先填写地址与模型名', 'error');
      return;
    }
    const button = byId(`${prefix}-probe`);
    button.disabled = true;
    button.textContent = '探测中';
    try {
      const raw = await callHost('probe_config', JSON.stringify({ component, remote }));
      const result = raw ? JSON.parse(raw) : {};
      if (!result.ok) {
        showToast(`探测失败：${result.message || '未知错误'}`, 'error');
        return;
      }
      if (component === 'embedding' && result.dimension > 0) {
        byId('embedding-dimension').value = result.dimension;
        renderComponentMeta();
        scheduleConfigSave();
      }
      showToast(result.message || '连接成功');
    } catch (error) {
      showToast(`探测失败：${errorText(error)}`, 'error');
    } finally {
      button.disabled = false;
      button.textContent = '探测';
    }
  }

  function scheduleConfigSave() {
    if (!state.configLoaded) return;
    configRevision += 1;
    configPending = true;
    window.clearTimeout(configSaveTimer);
    byId('config-status').textContent = '等待保存';
    configSaveTimer = window.setTimeout(() => saveConfig(null, true), 500);
  }

  function flushConfigSave(durable = false) {
    if (!configPending) return;
    window.clearTimeout(configSaveTimer);
    configSaveTimer = null;
    const config = currentConfig();
    if (durable && hostChannel && !standalone && !validateConfig(config)) {
      const id = `memory-flush-${Date.now()}-${++requestSequence}`;
      window.parent.postMessage({
        type: 'plugin_call',
        channel: hostChannel,
        id,
        method: 'save_config',
        payload: JSON.stringify(config),
      }, '*');
      afterConfigSaved(config);
      configRevision += 1;
      configPending = false;
      return;
    }
    void saveConfig(null, true);
  }

  async function saveConfig(event, automatic = false) {
    event?.preventDefault();
    window.clearTimeout(configSaveTimer);
    configSaveTimer = null;
    const config = currentConfig();
    const invalid = validateConfig(config);
    if (invalid) {
      const button = byId('save-config');
      button.disabled = false;
      button.textContent = '保存配置';
      byId('config-status').textContent = '配置未完成';
      byId('config-status').classList.add('error');
      if (!automatic) showToast(invalid, 'error');
      return;
    }

    const revision = configRevision;
    const button = byId('save-config');
    const status = byId('config-status');
    button.disabled = true;
    button.textContent = '保存中';
    status.textContent = '正在应用';
    status.classList.remove('error');

    configSaveQueue = configSaveQueue.catch(() => {}).then(async () => {
      if (revision !== configRevision) return;
      await callHost('save_config', JSON.stringify(config));
      // 等待响应期间用户可能继续编辑：此时不得重绘表单，
      // 未保存的新修改由 scheduleConfigSave 排队的下一次保存处理。
      const stale = revision !== configRevision;
      afterConfigSaved(config, stale);
      if (!stale) {
        configPending = false;
        setRuntimeStatus('已连接');
        if (!automatic) showToast('配置已保存');
      }
    });

    try {
      await configSaveQueue;
      if (revision === configRevision) {
        status.textContent = '已保存并生效';
        status.classList.remove('error');
      } else {
        status.textContent = '等待保存';
      }
    } catch (error) {
      status.textContent = '保存失败，正在重试';
      status.classList.add('error');
      if (revision === configRevision) {
        configSaveTimer = window.setTimeout(() => saveConfig(null, true), 1500);
      }
      showToast(`保存失败：${errorText(error)}`, 'error');
    } finally {
      if (revision === configRevision) {
        button.disabled = false;
        button.textContent = '保存配置';
      }
    }
  }

  function openRecall() {
    lastModalFocus = document.activeElement;
    byId('recall-modal').classList.remove('hidden');
    byId('recall-query').focus();
  }

  function closeRecall() {
    byId('recall-modal').classList.add('hidden');
    lastModalFocus?.focus?.();
    lastModalFocus = null;
  }

  function trapModalFocus(event) {
    if (event.key !== 'Tab' || byId('recall-modal').classList.contains('hidden')) return;
    const focusable = [...byId('recall-modal').querySelectorAll(
      'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])',
    )];
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  async function runRecall(event) {
    event.preventDefault();
    const query = byId('recall-query').value.trim();
    const results = byId('recall-results');
    if (!query) {
      results.innerHTML = '<div class="inline-empty">请输入召回问题</div>';
      return;
    }
    const button = byId('recall-run');
    button.disabled = true;
    button.textContent = '召回中';
    results.innerHTML = '<div class="inline-empty"><span class="spinner"></span></div>';
    try {
      const response = await memoryRequest({
        method: 'recall',
        anchors: { keywords: [], query, strategy: null },
        limit: 8,
      });
      renderRecallHits(Array.isArray(response.hits) ? response.hits : []);
    } catch (error) {
      results.innerHTML = `<div class="inline-empty">${escapeHtml(errorText(error))}</div>`;
      showToast(`召回失败：${errorText(error)}`, 'error');
    } finally {
      button.disabled = false;
      button.textContent = '测试';
    }
  }

  function renderRecallHits(hits) {
    if (hits.length === 0) {
      byId('recall-results').innerHTML = '<div class="inline-empty">暂无召回结果</div>';
      return;
    }
    byId('recall-results').innerHTML = hits.map((hit) => `
      <article class="recall-hit">
        <div class="recall-hit-heading">
          <strong>${escapeHtml(hit.title)}</strong>
          <span class="score">${Math.round(Number(hit.score || 0) * 100)}%</span>
        </div>
        <p>${escapeHtml(hit.summary)}</p>
        <footer>重要度 ${Number(hit.importance || 0).toFixed(1)} · ${hit.depth1_loaded ? '已展开' : '基础命中'} · ${escapeHtml(hit.kind)}</footer>
      </article>`).join('');
  }

  function switchTab(tab) {
    if (state.activeTab === 'config' && tab !== 'config') flushConfigSave();
    state.activeTab = tab;
    document.querySelectorAll('.tab').forEach((button) => {
      button.classList.toggle('active', button.dataset.tab === tab);
    });
    byId('view-data').classList.toggle('active', tab === 'data');
    byId('view-config').classList.toggle('active', tab === 'config');
  }

  async function switchMode(mode) {
    if (state.viewMode === mode) {
      return;
    }
    state.viewMode = mode;
    state.page = 1;
    state.selectedIds.clear();
    document.querySelectorAll('.segment').forEach((button) => {
      button.classList.toggle('active', button.dataset.mode === mode);
    });
    byId('graph-view').classList.toggle('active', mode === 'graph');
    byId('list-view').classList.toggle('active', mode === 'list');
    await loadData();
  }

  function bindEvents() {
    document.querySelectorAll('.tab').forEach((button) => {
      button.addEventListener('click', () => switchTab(button.dataset.tab));
    });
    document.querySelectorAll('.segment').forEach((button) => {
      button.addEventListener('click', () => switchMode(button.dataset.mode));
    });

    byId('search-input').addEventListener('input', (event) => {
      window.clearTimeout(searchTimer);
      searchTimer = window.setTimeout(async () => {
        state.query = event.target.value;
        state.page = 1;
        state.graphLimit = GRAPH_INITIAL_LIMIT;
        state.selectedIds.clear();
        await loadData();
      }, 320);
    });
    byId('search-input').addEventListener('keydown', async (event) => {
      if (event.key === 'Enter') {
        event.preventDefault();
        window.clearTimeout(searchTimer);
        state.query = event.currentTarget.value;
        state.page = 1;
        state.graphLimit = GRAPH_INITIAL_LIMIT;
        await loadData();
      }
    });
    byId('status-filter').addEventListener('change', async (event) => {
      const previousStatus = state.status;
      state.status = event.target.value;
      state.page = 1;
      state.graphLimit = GRAPH_INITIAL_LIMIT;
      state.selectedIds.clear();
      const result = await loadData();
      if (result === false) {
        state.status = previousStatus;
        event.target.value = previousStatus;
      }
    });
    byId('refresh-button').addEventListener('click', loadData);

    byId('graph-load-more').addEventListener('click', async () => {
      state.graphLimit = Math.min(GRAPH_MAX_LIMIT, state.graphLimit + GRAPH_LIMIT_STEP);
      await loadData();
    });
    byId('graph-zoom-in').addEventListener('click', () => {
      state.graphScale = Math.min(2.4, state.graphScale + 0.2);
      renderGraph();
    });
    byId('graph-zoom-out').addEventListener('click', () => {
      state.graphScale = Math.max(0.6, state.graphScale - 0.2);
      renderGraph();
    });
    byId('graph-fit').addEventListener('click', () => {
      state.graphScale = 1;
      state.graphPanX = 0;
      state.graphPanY = 0;
      renderGraph();
    });
    byId('graph-edit-selected').addEventListener('click', () => {
      const selected = state.nodes.find((node) => node.id === state.selectedNodeId);
      if (selected) editNode(selected);
    });
    byId('graph-status-selected').addEventListener('click', () => {
      const selected = state.nodes.find((node) => node.id === state.selectedNodeId);
      if (selected) setNodeStatus(selected, selected.status === 'active' ? 'archived' : 'active');
    });
    const graph = byId('memory-graph');
    graph.addEventListener('wheel', (event) => {
      event.preventDefault();
      state.graphScale = Math.max(0.5, Math.min(2.8, state.graphScale * (event.deltaY > 0 ? 0.9 : 1.1)));
      renderGraph();
    }, { passive: false });
    graph.addEventListener('pointerdown', (event) => {
      if (event.target.closest?.('.graph-node')) return;
      state.graphDragging = true;
      state.graphDidDrag = false;
      state.graphPointer = { x: event.clientX, y: event.clientY };
      graph.setPointerCapture(event.pointerId);
      graph.classList.add('dragging');
    });
    graph.addEventListener('pointermove', (event) => {
      if (!state.graphDragging || !state.graphPointer) return;
      state.graphDidDrag = true;
      state.graphPanX += event.clientX - state.graphPointer.x;
      state.graphPanY += event.clientY - state.graphPointer.y;
      state.graphPointer = { x: event.clientX, y: event.clientY };
      renderGraph();
    });
    const stopGraphDrag = () => {
      state.graphDragging = false;
      state.graphPointer = null;
      graph.classList.remove('dragging');
    };
    graph.addEventListener('pointerup', stopGraphDrag);
    graph.addEventListener('pointercancel', stopGraphDrag);
    graph.addEventListener('click', (event) => {
      if (state.graphDidDrag) {
        state.graphDidDrag = false;
        return;
      }
      const group = event.target.closest?.('.graph-node');
      const node = group ? state.nodes.find((item) => item.id === group.dataset.nodeId) : null;
      if (node) {
        editNode(node);
      } else if (event.target.id === 'memory-graph') {
        startNew();
      }
    });
    byId('memory-graph').addEventListener('keydown', (event) => {
      if (!['Enter', ' '].includes(event.key)) {
        return;
      }
      const group = event.target.closest?.('.graph-node');
      const node = group ? state.nodes.find((item) => item.id === group.dataset.nodeId) : null;
      if (node) {
        event.preventDefault();
        editNode(node);
      }
    });

    byId('memory-list').addEventListener('click', (event) => {
      const row = event.target.closest('.memory-row');
      if (!row) {
        return;
      }
      const node = state.nodes.find((item) => item.id === row.dataset.nodeId);
      if (!node) {
        return;
      }
      const action = event.target.closest('button')?.dataset.action;
      if (action === 'edit') {
        editNode(node);
      } else if (action === 'status') {
        setNodeStatus(node, node.status === 'active' ? 'archived' : 'active');
      }
    });
    byId('memory-list').addEventListener('change', (event) => {
      if (!event.target.classList.contains('row-select')) {
        return;
      }
      const row = event.target.closest('.memory-row');
      if (!row) {
        return;
      }
      if (event.target.checked) {
        state.selectedIds.add(row.dataset.nodeId);
      } else {
        state.selectedIds.delete(row.dataset.nodeId);
      }
      renderList();
    });
    byId('select-all').addEventListener('change', (event) => {
      state.selectedIds = event.target.checked
        ? new Set(state.nodes.map((node) => node.id))
        : new Set();
      renderList();
    });
    byId('bulk-status').addEventListener('click', setSelectedStatus);
    byId('page-prev').addEventListener('click', async () => {
      state.page = Math.max(1, state.page - 1);
      await loadData();
    });
    byId('page-next').addEventListener('click', async () => {
      state.page += 1;
      await loadData();
    });

    byId('new-memory').addEventListener('click', startNew);
    byId('memory-form').addEventListener('submit', saveMemory);
    byId('memory-importance').addEventListener('input', (event) => {
      byId('importance-output').textContent = Number(event.target.value).toFixed(1);
    });
    byId('relation-form').addEventListener('submit', saveRelation);
    const relationSearch = byId('relation-target-search');
    relationSearch.addEventListener('focus', () => {
      state.relationTargetIndex = 0;
      renderRelationTargets(true);
    });
    relationSearch.addEventListener('input', () => {
      byId('relation-target').value = '';
      state.relationTargetIndex = 0;
      renderRelationTargets(true);
    });
    relationSearch.addEventListener('keydown', (event) => {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        const delta = event.key === 'ArrowDown' ? 1 : -1;
        state.relationTargetIndex = Math.max(0, Math.min(
          state.relationTargetMatches.length - 1,
          state.relationTargetIndex + delta,
        ));
        renderRelationTargets(true);
      } else if (event.key === 'Enter') {
        event.preventDefault();
        const node = state.relationTargetMatches[state.relationTargetIndex];
        if (node) selectRelationTarget(node.id);
      } else if (event.key === 'Escape') {
        renderRelationTargets(false);
      }
    });
    byId('relation-target-options').addEventListener('click', (event) => {
      const option = event.target.closest('.combobox-option');
      if (option) selectRelationTarget(option.dataset.nodeId);
    });
    document.addEventListener('pointerdown', (event) => {
      if (!event.target.closest('#relation-target-combobox')) renderRelationTargets(false);
    });
    byId('relation-list').addEventListener('click', (event) => {
      const button = event.target.closest('.relation-delete');
      const item = button?.closest('.relation-item');
      if (item) {
        deleteRelation(item.dataset.relationId);
      }
    });

    byId('config-form').addEventListener('submit', (event) => saveConfig(event, false));
    ['config-model', 'config-tier', 'config-embedding-source', 'config-rerank-source', 'config-vector-mode']
      .forEach((id) => {
        byId(id).addEventListener('change', () => {
          renderComponentMeta();
          scheduleConfigSave();
        });
      });
    // 在线端点文本字段：失焦时保存，避免输入过程中频繁热更新。
    ['llm-url', 'llm-model', 'llm-key', 'embedding-url', 'embedding-model', 'embedding-dimension', 'embedding-key', 'rerank-url', 'rerank-model', 'rerank-key']
      .forEach((id) => {
        byId(id).addEventListener('input', renderComponentMeta);
        byId(id).addEventListener('change', scheduleConfigSave);
      });
    byId('llm-protocol').addEventListener('change', scheduleConfigSave);
    byId('llm-probe').addEventListener('click', () => probeRemote('llm'));
    byId('embedding-probe').addEventListener('click', () => probeRemote('embedding'));
    byId('rerank-probe').addEventListener('click', () => probeRemote('rerank'));

    byId('recall-open').addEventListener('click', openRecall);
    byId('recall-close').addEventListener('click', closeRecall);
    byId('recall-form').addEventListener('submit', runRecall);
    byId('recall-modal').addEventListener('click', (event) => {
      if (event.target.id === 'recall-modal') {
        closeRecall();
      }
    });
    window.addEventListener('pagehide', () => flushConfigSave(true));
    document.addEventListener('visibilitychange', () => {
      if (document.visibilityState === 'hidden') flushConfigSave(true);
    });
    window.addEventListener('keydown', (event) => {
      trapModalFocus(event);
      if (event.key === 'Escape' && !byId('recall-modal').classList.contains('hidden')) {
        closeRecall();
      }
    });
  }

  // 独立配置页"完成并关闭"：先落盘未保存的配置，再通知服务退出。
  async function closeStandalone() {
    if (!standalone) return;
    const button = byId('standalone-close');
    button.disabled = true;
    button.textContent = '正在关闭';
    try {
      if (configPending) {
        const invalid = validateConfig(currentConfig());
        if (invalid) {
          showToast(`配置未完成：${invalid}`, 'error');
          button.disabled = false;
          button.textContent = '完成并关闭';
          return;
        }
        await saveConfig(null, true);
      }
      await configSaveQueue;
      await callStandalone('/close', {});
      document.body.innerHTML = '<div class="standalone-closed"><h1>配置已保存</h1><p>服务已关闭，可以关闭此页面。</p></div>';
      window.setTimeout(() => window.close(), 400);
    } catch (error) {
      showToast(`关闭失败：${errorText(error)}`, 'error');
      button.disabled = false;
      button.textContent = '完成并关闭';
    }
  }

  function populateFixedOptions() {
    byId('memory-type').innerHTML = memoryTypes
      .map(([value, label]) => `<option value="${value}">${label}</option>`)
      .join('');
    byId('relation-kind').innerHTML = relationKinds
      .map(([value, label]) => `<option value="${value}">${label}</option>`)
      .join('');
  }

  async function init() {
    populateFixedOptions();
    byId('config-status').textContent = '正在加载配置';
    setConfigControlsDisabled(true);
    bindEvents();
    updateEditor();
    byId('recall-results').innerHTML = '<div class="inline-empty">输入问题后开始测试</div>';
    await Promise.all([loadBootstrap(), loadData()]);
  }

  init();
})();
