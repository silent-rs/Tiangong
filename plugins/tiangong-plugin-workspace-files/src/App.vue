<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue';
import type { HostContext } from '@tiangong/plugin-sdk';
import { FilesApi, type DirEntry, type FileContent } from './api';

interface Props {
  initialHostContext?: HostContext;
  subscribeHostContext?: (handler: (context: HostContext) => void) => () => void;
}
const props = defineProps<Props>();

interface TreeRow {
  entry: DirEntry;
  depth: number;
}

type DialogState =
  | { kind: 'create'; type: 'file' | 'dir'; parent: string; value: string }
  | { kind: 'rename'; target: DirEntry; value: string }
  | { kind: 'delete'; target: DirEntry }
  | { kind: 'discard'; next: () => void };

/** 右键菜单：target 为空表示在树的空白处右键（作用于工作区根目录）。 */
interface ContextMenuState {
  x: number;
  y: number;
  target: DirEntry | null;
}

const workspace = ref(props.initialHostContext?.session?.workspace ?? '');
const children = ref<Record<string, DirEntry[]>>({});
const truncatedDirs = ref<Set<string>>(new Set());
const expanded = ref<Set<string>>(new Set());
const loadingDirs = ref<Set<string>>(new Set());
const selected = ref<DirEntry | null>(null);
const file = ref<FileContent | null>(null);
const draft = ref('');
const fileLoading = ref(false);
const saving = ref(false);
const error = ref('');
const notice = ref('');
const dialog = ref<DialogState | null>(null);
const menu = ref<ContextMenuState | null>(null);
let stopContext: (() => void) | null = null;
let noticeTimer: ReturnType<typeof setTimeout> | null = null;
const gutter = ref<HTMLElement | null>(null);
const root = ref<HTMLElement | null>(null);
const menuEl = ref<HTMLElement | null>(null);

const api = computed(() => new FilesApi(workspace.value));
const dirty = computed(
  () => file.value?.kind === 'text' && draft.value !== (file.value.content ?? ''),
);
const imageSrc = computed(() =>
  file.value?.kind === 'image' && file.value.base64
    ? `data:${file.value.mime};base64,${file.value.base64}`
    : '',
);
const rows = computed<TreeRow[]>(() => {
  const result: TreeRow[] = [];
  const walk = (dir: string, depth: number) => {
    for (const entry of children.value[dir] ?? []) {
      result.push({ entry, depth });
      if (entry.kind === 'dir' && expanded.value.has(entry.path)) walk(entry.path, depth + 1);
    }
  };
  walk('', 0);
  return result;
});
const lineCount = computed(() => (draft.value.match(/\n/g)?.length ?? 0) + 1);

function parentOf(path: string): string {
  const index = path.lastIndexOf('/');
  return index < 0 ? '' : path.slice(0, index);
}

function joinPath(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

function formatSize(size: number): string {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${(size / 1024 / 1024).toFixed(1)} MB`;
}

function showError(err: unknown) {
  error.value = err instanceof Error ? err.message : String(err);
}

function flash(message: string) {
  notice.value = message;
  if (noticeTimer) clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => (notice.value = ''), 2500);
}

async function loadDir(path: string) {
  if (!workspace.value) return;
  loadingDirs.value = new Set(loadingDirs.value).add(path);
  try {
    const listing = await api.value.listDir(path);
    children.value = { ...children.value, [path]: listing.entries };
    const truncated = new Set(truncatedDirs.value);
    if (listing.truncated) truncated.add(path);
    else truncated.delete(path);
    truncatedDirs.value = truncated;
    error.value = '';
  } catch (err) {
    showError(err);
  } finally {
    const next = new Set(loadingDirs.value);
    next.delete(path);
    loadingDirs.value = next;
  }
}

/** 重新加载根目录与所有已展开目录；已消失的目录从展开集合移除。 */
async function refreshTree() {
  const dirs = ['', ...expanded.value];
  children.value = {};
  await Promise.all(dirs.map((dir) => loadDir(dir)));
  const existing = new Set(Object.keys(children.value));
  expanded.value = new Set([...expanded.value].filter((dir) => existing.has(dir)));
}

function guardDirty(next: () => void) {
  if (dirty.value) dialog.value = { kind: 'discard', next };
  else next();
}

async function toggleDir(entry: DirEntry) {
  selected.value = entry;
  const next = new Set(expanded.value);
  if (next.has(entry.path)) {
    next.delete(entry.path);
    expanded.value = next;
    return;
  }
  next.add(entry.path);
  expanded.value = next;
  if (!children.value[entry.path]) await loadDir(entry.path);
}

async function openFile(entry: DirEntry) {
  selected.value = entry;
  fileLoading.value = true;
  try {
    const content = await api.value.readFile(entry.path);
    file.value = content;
    draft.value = content.content ?? '';
    error.value = '';
  } catch (err) {
    showError(err);
  } finally {
    fileLoading.value = false;
  }
}

function onRowClick(entry: DirEntry) {
  if (entry.kind === 'dir') {
    void toggleDir(entry);
    return;
  }
  if (file.value?.path === entry.path) {
    selected.value = entry;
    return;
  }
  guardDirty(() => void openFile(entry));
}

async function save() {
  const current = file.value;
  if (!current || current.kind !== 'text' || !dirty.value || saving.value) return;
  saving.value = true;
  try {
    const result = await api.value.writeFile(current.path, draft.value, current.hash);
    file.value = {
      ...current,
      content: draft.value,
      hash: result.hash,
      size: result.size,
      modified_ms: result.modified_ms,
    };
    error.value = '';
    flash('已保存');
    void loadDir(parentOf(current.path));
  } catch (err) {
    showError(err);
  } finally {
    saving.value = false;
  }
}

function reloadFile() {
  const current = file.value;
  if (!current) return;
  const entry = selected.value?.path === current.path ? selected.value : null;
  guardDirty(() =>
    void openFile(
      entry ?? { name: current.path, path: current.path, kind: 'file', size: 0, modified_ms: 0 },
    ),
  );
}

function onEditorScroll(event: Event) {
  const target = event.target as HTMLTextAreaElement;
  if (gutter.value) gutter.value.scrollTop = target.scrollTop;
}

function onEditorKeydown(event: KeyboardEvent) {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') {
    event.preventDefault();
    void save();
    return;
  }
  if (event.key === 'Tab' && !event.metaKey && !event.ctrlKey && !event.altKey) {
    event.preventDefault();
    const target = event.target as HTMLTextAreaElement;
    const { selectionStart, selectionEnd } = target;
    draft.value = `${draft.value.slice(0, selectionStart)}  ${draft.value.slice(selectionEnd)}`;
    requestAnimationFrame(() => {
      target.selectionStart = target.selectionEnd = selectionStart + 2;
    });
  }
}

/** 新建目标目录：右键目录则建在其中，右键文件则建在其所在目录，空白处建在根目录。 */
function targetDir(entry: DirEntry | null): string {
  if (!entry) return '';
  return entry.kind === 'dir' ? entry.path : parentOf(entry.path);
}

function openCreate(type: 'file' | 'dir', entry: DirEntry | null) {
  dialog.value = { kind: 'create', type, parent: targetDir(entry), value: '' };
}

function openRename(entry: DirEntry) {
  dialog.value = { kind: 'rename', target: entry, value: entry.name };
}

function openDelete(entry: DirEntry) {
  dialog.value = { kind: 'delete', target: entry };
}

function openMenu(event: MouseEvent, target: DirEntry | null) {
  if (!workspace.value) return;
  const bounds = root.value?.getBoundingClientRect();
  if (!bounds) return;
  if (target) selected.value = target;
  menu.value = { x: event.clientX - bounds.left, y: event.clientY - bounds.top, target };
  // 渲染后按实际尺寸收回到容器内，避免菜单超出右侧或底部
  void nextTick(() => {
    const state = menu.value;
    const el = menuEl.value;
    if (!state || !el) return;
    const maxX = bounds.width - el.offsetWidth - 4;
    const maxY = bounds.height - el.offsetHeight - 4;
    menu.value = {
      ...state,
      x: Math.max(4, Math.min(state.x, maxX)),
      y: Math.max(4, Math.min(state.y, maxY)),
    };
    el.focus();
  });
}

function closeMenu() {
  menu.value = null;
}

type MenuAction = 'newFile' | 'newDir' | 'rename' | 'delete' | 'copyPath' | 'refresh';

function runMenu(action: MenuAction) {
  const target = menu.value?.target ?? null;
  closeMenu();
  if (action === 'newFile') openCreate('file', target);
  else if (action === 'newDir') openCreate('dir', target);
  else if (action === 'refresh') void refreshTree();
  else if (action === 'copyPath' && target) void copyPath(target);
  else if (action === 'rename' && target) openRename(target);
  else if (action === 'delete' && target) openDelete(target);
}

async function copyPath(entry: DirEntry) {
  try {
    await navigator.clipboard.writeText(entry.path);
    flash('已复制相对路径');
  } catch (err) {
    showError(err);
  }
}

/** 文件树获得焦点时的快捷键：F2 重命名，Delete / Cmd+Backspace 删除。 */
function onTreeKeydown(event: KeyboardEvent) {
  const current = selected.value;
  if (!current || dialog.value || menu.value) return;
  if (event.key === 'F2') {
    event.preventDefault();
    openRename(current);
  } else if (
    event.key === 'Delete' ||
    (event.key === 'Backspace' && (event.metaKey || event.ctrlKey))
  ) {
    event.preventDefault();
    openDelete(current);
  }
}

function onMenuKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    event.preventDefault();
    closeMenu();
  }
}

/** 菜单外任意按下即关闭；捕获阶段监听，shadow DOM 下用 composedPath 判断。 */
function onGlobalPointerDown(event: PointerEvent) {
  if (!menu.value || !menuEl.value) return;
  if (!event.composedPath().includes(menuEl.value)) closeMenu();
}

function validName(name: string): boolean {
  return name.length > 0 && !name.includes('/') && !name.includes('\\') && name !== '.' && name !== '..';
}

async function confirmDialog() {
  const state = dialog.value;
  if (!state) return;
  try {
    if (state.kind === 'discard') {
      dialog.value = null;
      state.next();
      return;
    }
    if (state.kind === 'create') {
      const name = state.value.trim();
      if (!validName(name)) throw new Error('名称不能为空，且不能包含路径分隔符');
      const created = await api.value.create(joinPath(state.parent, name), state.type);
      dialog.value = null;
      if (state.parent && !expanded.value.has(state.parent)) {
        expanded.value = new Set(expanded.value).add(state.parent);
      }
      await loadDir(state.parent);
      const entry = children.value[state.parent]?.find((item) => item.path === created.path);
      if (entry && entry.kind === 'file') guardDirty(() => void openFile(entry));
      else if (entry) selected.value = entry;
      return;
    }
    if (state.kind === 'rename') {
      const name = state.value.trim();
      if (!validName(name)) throw new Error('名称不能为空，且不能包含路径分隔符');
      const parent = parentOf(state.target.path);
      const renamed = await api.value.rename(state.target.path, joinPath(parent, name));
      dialog.value = null;
      const oldPath = state.target.path;
      if (file.value && (file.value.path === oldPath || file.value.path.startsWith(`${oldPath}/`))) {
        file.value = { ...file.value, path: renamed.path + file.value.path.slice(oldPath.length) };
      }
      expanded.value = new Set(
        [...expanded.value].map((dir) =>
          dir === oldPath || dir.startsWith(`${oldPath}/`) ? renamed.path + dir.slice(oldPath.length) : dir,
        ),
      );
      await refreshTree();
      selected.value = children.value[parent]?.find((item) => item.path === renamed.path) ?? null;
      return;
    }
    if (state.kind === 'delete') {
      const path = state.target.path;
      await api.value.remove(path);
      dialog.value = null;
      if (file.value && (file.value.path === path || file.value.path.startsWith(`${path}/`))) {
        file.value = null;
        draft.value = '';
      }
      selected.value = null;
      await refreshTree();
      flash('已删除');
    }
  } catch (err) {
    showError(err);
  }
}

function resetForWorkspace(next: string) {
  workspace.value = next;
  children.value = {};
  expanded.value = new Set();
  truncatedDirs.value = new Set();
  selected.value = null;
  file.value = null;
  draft.value = '';
  dialog.value = null;
  menu.value = null;
  error.value = '';
  if (next) void loadDir('');
}

function applyContext(context: HostContext) {
  const next = context.session?.workspace ?? '';
  if (next === workspace.value) return;
  // 工作区切换时未保存内容无法写回旧工作区之外，提示后丢弃
  if (dirty.value) flash('工作区已切换，未保存的修改已丢弃');
  resetForWorkspace(next);
}

watch(dialog, (state) => {
  if (state && state.kind !== 'discard') error.value = '';
});

onMounted(() => {
  stopContext = props.subscribeHostContext?.(applyContext) ?? null;
  if (workspace.value) void loadDir('');
  window.addEventListener('pointerdown', onGlobalPointerDown, true);
  window.addEventListener('blur', closeMenu);
});

onUnmounted(() => {
  window.removeEventListener('pointerdown', onGlobalPointerDown, true);
  window.removeEventListener('blur', closeMenu);
  stopContext?.();
  if (noticeTimer) clearTimeout(noticeTimer);
});
</script>

<template>
  <div ref="root" class="files-app">
    <aside class="sidebar">
      <header class="sidebar-header">
        <div class="workspace" :title="workspace">
          {{ workspace ? workspace.split('/').filter(Boolean).pop() : '无工作区' }}
        </div>
        <div class="actions">
          <button title="刷新" :disabled="!workspace" @click="refreshTree">刷新</button>
        </div>
      </header>
      <div
        class="tree"
        role="tree"
        tabindex="0"
        @contextmenu.prevent="openMenu($event, null)"
        @keydown="onTreeKeydown"
      >
        <p v-if="!workspace" class="empty">当前会话没有可用的工作区</p>
        <p v-else-if="loadingDirs.has('') && !children['']" class="empty">加载中…</p>
        <p v-else-if="children[''] && children[''].length === 0" class="empty">
          工作区为空，右键可新建文件
        </p>
        <div
          v-for="row in rows"
          :key="row.entry.path"
          class="row"
          :class="{
            active: selected?.path === row.entry.path,
            'menu-target': menu?.target?.path === row.entry.path,
          }"
          :style="{ paddingLeft: `${8 + row.depth * 14}px` }"
          role="treeitem"
          :title="row.entry.path"
          @click="onRowClick(row.entry)"
          @contextmenu.prevent.stop="openMenu($event, row.entry)"
        >
          <span class="twisty">
            {{ row.entry.kind === 'dir' ? (expanded.has(row.entry.path) ? '▾' : '▸') : '' }}
          </span>
          <span class="name" :class="row.entry.kind">{{ row.entry.name }}</span>
          <span v-if="loadingDirs.has(row.entry.path)" class="hint">…</span>
          <span v-else-if="truncatedDirs.has(row.entry.path)" class="hint">已截断</span>
        </div>
        <p v-if="truncatedDirs.has('')" class="empty">条目过多，仅显示前 5000 项</p>
      </div>
    </aside>

    <main class="viewer">
      <header class="viewer-header">
        <div class="path">
          <span v-if="file">{{ file.path }}</span>
          <span v-else class="muted">未打开文件</span>
          <span v-if="dirty" class="dirty" title="未保存">●</span>
        </div>
        <div class="actions">
          <span v-if="notice" class="notice">{{ notice }}</span>
          <span v-if="file" class="muted">{{ formatSize(file.size) }}</span>
          <button v-if="file" @click="reloadFile">重新加载</button>
          <button v-if="file?.kind === 'text'" class="primary" :disabled="!dirty || saving" @click="save">
            {{ saving ? '保存中…' : '保存' }}
          </button>
        </div>
      </header>
      <p v-if="error" class="error" role="alert">
        {{ error }}
        <button class="link" @click="error = ''">关闭</button>
      </p>
      <section class="content">
        <p v-if="fileLoading" class="placeholder">读取中…</p>
        <p v-else-if="!file" class="placeholder">在左侧选择文件以预览或编辑</p>
        <p v-else-if="file.too_large" class="placeholder">
          文件过大（{{ formatSize(file.size) }}），不支持在此预览
        </p>
        <div v-else-if="file.kind === 'image'" class="image-wrap">
          <img :src="imageSrc" :alt="file.path" />
        </div>
        <p v-else-if="file.kind === 'binary'" class="placeholder">二进制文件，不支持预览</p>
        <div v-else class="editor">
          <div ref="gutter" class="gutter" aria-hidden="true">
            <div v-for="line in lineCount" :key="line">{{ line }}</div>
          </div>
          <textarea
            v-model="draft"
            spellcheck="false"
            wrap="off"
            @keydown="onEditorKeydown"
          ></textarea>
        </div>
      </section>
    </main>

    <div
      v-if="menu"
      ref="menuEl"
      class="context-menu"
      role="menu"
      tabindex="-1"
      :style="{ left: `${menu.x}px`, top: `${menu.y}px` }"
      @keydown="onMenuKeydown"
      @contextmenu.prevent
    >
      <button type="button" role="menuitem" @click="runMenu('newFile')">新建文件</button>
      <button type="button" role="menuitem" @click="runMenu('newDir')">新建文件夹</button>
      <template v-if="menu.target">
        <div class="menu-separator"></div>
        <button type="button" role="menuitem" @click="runMenu('rename')">
          重命名<span class="shortcut">F2</span>
        </button>
        <button type="button" role="menuitem" @click="runMenu('copyPath')">复制相对路径</button>
        <div class="menu-separator"></div>
        <button type="button" role="menuitem" class="danger" @click="runMenu('delete')">
          删除<span class="shortcut">⌘⌫</span>
        </button>
      </template>
      <template v-else>
        <div class="menu-separator"></div>
        <button type="button" role="menuitem" @click="runMenu('refresh')">刷新</button>
      </template>
    </div>

    <div v-if="dialog" class="overlay" @click.self="dialog = null">
      <form class="dialog" @submit.prevent="confirmDialog">
        <template v-if="dialog.kind === 'create'">
          <h2>新建{{ dialog.type === 'file' ? '文件' : '文件夹' }}</h2>
          <p class="muted">位置：/{{ dialog.parent }}</p>
          <input v-model="dialog.value" autofocus placeholder="名称" />
        </template>
        <template v-else-if="dialog.kind === 'rename'">
          <h2>重命名</h2>
          <p class="muted">{{ dialog.target.path }}</p>
          <input v-model="dialog.value" autofocus placeholder="新名称" />
        </template>
        <template v-else-if="dialog.kind === 'delete'">
          <h2>确认删除</h2>
          <p>
            将永久删除{{ dialog.target.kind === 'dir' ? '文件夹及其全部内容' : '文件' }}
            <strong>{{ dialog.target.path }}</strong>，此操作不可撤销。
          </p>
        </template>
        <template v-else>
          <h2>放弃未保存的修改？</h2>
          <p>当前文件有未保存的修改，继续将丢弃这些修改。</p>
        </template>
        <p v-if="error" class="error">{{ error }}</p>
        <div class="dialog-actions">
          <button type="button" @click="dialog = null">取消</button>
          <button
            type="submit"
            :class="dialog.kind === 'delete' || dialog.kind === 'discard' ? 'danger-solid' : 'primary'"
          >
            {{ dialog.kind === 'delete' ? '删除' : dialog.kind === 'discard' ? '放弃修改' : '确定' }}
          </button>
        </div>
      </form>
    </div>
  </div>
</template>

<style scoped>
.files-app {
  --ui-background: hsl(var(--background, 0 0% 100%));
  --ui-foreground: hsl(var(--foreground, 222.2 47.4% 11.2%));
  --ui-card: hsl(var(--card, 0 0% 100%));
  --ui-muted: hsl(var(--muted, 210 40% 96.1%));
  --ui-muted-foreground: hsl(var(--muted-foreground, 215.4 16.3% 46.9%));
  --ui-border: hsl(var(--border, 214.3 31.8% 91.4%));
  --ui-primary: hsl(var(--primary, 222.2 47.4% 11.2%));
  --ui-primary-foreground: hsl(var(--primary-foreground, 210 40% 98%));
  --ui-destructive: hsl(var(--destructive, 0 84.2% 60.2%));
  --ui-accent: hsl(var(--accent, 210 40% 96.1%));

  position: relative;
  box-sizing: border-box;
  display: flex;
  width: 100%;
  height: 100%;
  min-height: 0;
  overflow: hidden;
  color: var(--ui-foreground);
  background: var(--ui-background);
  font-size: 13px;
}

button {
  padding: 3px 8px;
  border: 1px solid var(--ui-border);
  border-radius: 6px;
  color: var(--ui-foreground);
  background: var(--ui-card);
  font-size: 12px;
  cursor: pointer;
}
button:disabled {
  opacity: 0.45;
  cursor: default;
}
button.primary {
  border-color: var(--ui-primary);
  color: var(--ui-primary-foreground);
  background: var(--ui-primary);
}
button.danger {
  color: var(--ui-destructive);
}
button.danger-solid {
  border-color: var(--ui-destructive);
  color: #fff;
  background: var(--ui-destructive);
}
button.link {
  padding: 0 4px;
  border: none;
  background: transparent;
  text-decoration: underline;
}

.muted {
  color: var(--ui-muted-foreground);
}

.sidebar {
  display: flex;
  flex-direction: column;
  flex: 0 0 260px;
  min-width: 0;
  min-height: 0;
  border-right: 1px solid var(--ui-border);
}
.sidebar-header,
.viewer-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  min-height: 40px;
  padding: 6px 10px;
  border-bottom: 1px solid var(--ui-border);
  box-sizing: border-box;
}
.workspace {
  overflow: hidden;
  font-weight: 600;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.actions {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 4px;
}
.tree {
  flex: 1;
  min-height: 0;
  overflow: auto;
  padding: 4px 0;
}
.row {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 2px 8px;
  cursor: pointer;
  white-space: nowrap;
  user-select: none;
}
.row:hover {
  background: var(--ui-accent);
}
.row.active {
  background: var(--ui-muted);
  font-weight: 500;
}
.twisty {
  flex: 0 0 12px;
  color: var(--ui-muted-foreground);
  font-size: 11px;
}
.name {
  overflow: hidden;
  text-overflow: ellipsis;
}
.name.symlink {
  font-style: italic;
}
.hint {
  color: var(--ui-muted-foreground);
  font-size: 11px;
}
.empty {
  margin: 12px;
  color: var(--ui-muted-foreground);
}
.tree:focus {
  outline: none;
}
.row.menu-target {
  outline: 1px solid var(--ui-primary);
  outline-offset: -1px;
}

.context-menu {
  position: absolute;
  z-index: 20;
  display: flex;
  flex-direction: column;
  min-width: 168px;
  padding: 4px;
  border: 1px solid var(--ui-border);
  border-radius: 8px;
  background: var(--ui-card);
  box-shadow: 0 8px 24px rgb(0 0 0 / 0.18);
  outline: none;
}
.context-menu button {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 5px 8px;
  border: none;
  border-radius: 4px;
  background: transparent;
  font-size: 12.5px;
  text-align: left;
}
.context-menu button:hover,
.context-menu button:focus-visible {
  outline: none;
  background: var(--ui-accent);
}
.shortcut {
  color: var(--ui-muted-foreground);
  font-size: 11px;
}
.menu-separator {
  height: 1px;
  margin: 4px 2px;
  background: var(--ui-border);
}

.viewer {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}
.path {
  display: flex;
  align-items: center;
  gap: 6px;
  overflow: hidden;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.dirty {
  color: var(--ui-primary);
}
.notice {
  color: var(--ui-muted-foreground);
}
.error {
  margin: 0;
  padding: 6px 10px;
  color: var(--ui-destructive);
  word-break: break-all;
}
.content {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow: hidden;
}
.placeholder {
  margin: 24px;
  color: var(--ui-muted-foreground);
}
.image-wrap {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  padding: 12px;
  box-sizing: border-box;
  overflow: auto;
}
.image-wrap img {
  max-width: 100%;
  max-height: 100%;
  object-fit: contain;
}
.editor {
  display: flex;
  height: 100%;
  overflow: hidden;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12.5px;
  line-height: 18px;
}
.gutter {
  flex: 0 0 auto;
  min-width: 36px;
  height: 100%;
  overflow: hidden;
  padding: 8px 8px 8px 4px;
  box-sizing: border-box;
  color: var(--ui-muted-foreground);
  background: var(--ui-muted);
  text-align: right;
  user-select: none;
}
.editor textarea {
  flex: 1;
  min-width: 0;
  height: 100%;
  padding: 8px;
  box-sizing: border-box;
  border: none;
  outline: none;
  overflow: auto;
  resize: none;
  color: var(--ui-foreground);
  background: transparent;
  font: inherit;
  line-height: inherit;
  white-space: pre;
  tab-size: 2;
}

.overlay {
  position: absolute;
  inset: 0;
  z-index: 10;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgb(0 0 0 / 0.3);
}
.dialog {
  width: min(380px, calc(100% - 32px));
  padding: 16px;
  border: 1px solid var(--ui-border);
  border-radius: 10px;
  background: var(--ui-card);
  box-shadow: 0 10px 30px rgb(0 0 0 / 0.2);
}
.dialog h2 {
  margin: 0 0 8px;
  font-size: 15px;
}
.dialog p {
  margin: 0 0 10px;
  word-break: break-all;
}
.dialog input {
  box-sizing: border-box;
  width: 100%;
  padding: 6px 8px;
  border: 1px solid var(--ui-border);
  border-radius: 6px;
  color: var(--ui-foreground);
  background: var(--ui-background);
  font-size: 13px;
}
.dialog-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 12px;
}
</style>
