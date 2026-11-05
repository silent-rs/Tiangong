<script setup lang="ts">
// 插件创作页（外置化形态）：宿主通道只做安装与只读查询；开发操作
//（init/validate/build/run/logs）由 Agent 经命令通道执行 devkit，页面
// 提供命令速查与项目看板。
import { onMounted, onBeforeUnmount, ref } from 'vue';
import {
  createTiangongBridge,
  createToolProvider,
  getShadowHostRuntime,
  pluginStorage,
  type HostBridge,
  type ToolInvocation,
} from '@tiangong/plugin-sdk';
import { devkitCommand, handleAgentTool, pluginDev, type ProjectEntry } from './tools';

type OutputKind = 'info' | 'success' | 'error';
interface OutputPanel {
  kind: OutputKind;
  title: string;
  body: string;
}

const TEMPLATES = [
  { id: 'ui-app', title: '纯 UI 插件', description: '零构建单页面插件（看板、仪表盘类）。' },
  { id: 'ts-tool', title: 'TS 工具插件', description: '页面 + 向 Agent 提供工具（需要 Node 与 yarn）。' },
  { id: 'ts-npx', title: 'npx 脚本插件', description: '命令行脚本 + 能力说明书，Agent 经命令通道执行。' },
] as const;

const bridge = ref<HostBridge | null>(null);
const connected = ref('正在连接宿主桥接…');
const projects = ref<ProjectEntry[]>([]);
const busy = ref(false);
const output = ref<OutputPanel | null>(null);
const newName = ref('');
const selectedTemplate = ref<string>('ui-app');
const sending = ref(false);

let providerStop: (() => void) | null = null;
let closedStop: (() => void) | null = null;
let pluginsChangedStop: (() => void) | null = null;
let contextChangeStop: (() => void) | null = null;

function show(kind: OutputKind, title: string, body: string) {
  output.value = { kind, title, body };
}

async function refreshProjects() {
  if (!bridge.value) return;
  try {
    projects.value = await pluginDev.list(bridge.value);
  } catch (error) {
    show('error', '项目列表读取失败', error instanceof Error ? error.message : String(error));
  }
}

/** 开始创建：把需求（名称 + 模板）直接交给当前会话的 Agent 处理——
 *  Agent 依名称起合适的插件 id 并执行 devkit init，后续流程在对话中完成。 */
async function startCreation() {
  if (!bridge.value || sending.value) return;
  const name = newName.value.trim();
  if (!name) {
    show('error', '缺少插件名称', '请填写插件名称（如「番茄钟」），插件 ID 由 Agent 依名称拟定。');
    return;
  }
  sending.value = true;
  const templates: Record<string, string> = {
    'ui-app': '纯 UI 页面插件（无工具）',
    'ts-tool': 'TypeScript 工具插件（页面 + 向你提供工具）',
    'ts-npx': 'npx 脚本插件（命令行脚本 + 能力说明书）',
  };
  const instruction = [
    `请为我创建一个天工插件「${name}」。`,
    `形态：${templates[selectedTemplate.value] ?? selectedTemplate.value}（plugin creator 的 ${selectedTemplate.value} 模板）。`,
    '请用 plugin creator 的 devkit 开始：为它起一个合适的英文插件 id（小写字母数字与 - _ .），执行 init 生成骨架后浏览模板结构，然后向我确认需求要点，再继续实现、validate、build，完成后用 plugin_creator 工具 action=install 安装。',
  ].join('\n');
  try {
    await pluginDev.sendToAgent(bridge.value, instruction);
    show('success', '已发送给 Agent', `创建「${name}」的请求已交给当前会话的 Agent 处理，请查看对话进展。`);
    newName.value = '';
  } catch (error) {
    show('error', '发送失败', error instanceof Error ? error.message : String(error));
  } finally {
    sending.value = false;
  }
}

/** 页面直连按需 sidecar 构建（与 Agent 工具同一后端）。 */
async function buildProject(entry: ProjectEntry) {
  if (!bridge.value || busy.value) return;
  busy.value = true;
  try {
    const result = await devkitCommand(bridge.value, 'build', [entry.id]);
    if (result.ok) {
      show('success', `${entry.id} 构建完成`, '产物在项目 release/ 目录，可直接安装。');
      await recordHistory(`构建 ${entry.id}`);
    } else {
      show('error', `${entry.id} 构建失败`, `${result.error ?? '未知错误'}（可让 Agent 用 plugin_creator 的 action=logs 读完整日志）`);
    }
    await refreshProjects();
  } catch (error) {
    show('error', `${entry.id} 构建失败`, error instanceof Error ? error.message : String(error));
  } finally {
    busy.value = false;
  }
}

async function installProject(entry: ProjectEntry) {
  if (!bridge.value || busy.value) return;
  busy.value = true;
  try {
    const result = await pluginDev.install(bridge.value, entry.id);
    show(
      'success',
      `${result.plugin_id} v${result.version} 已安装`,
      `状态：${result.state}${result.enabled ? '（已启用）' : ''}。含 extension.tab 贡献时可在拓展区打开。`,
    );
    await recordHistory(`安装 ${result.plugin_id} v${result.version}`);
    await refreshProjects();
  } catch (error) {
    show('error', `${entry.id} 安装失败`, error instanceof Error ? error.message : String(error));
  } finally {
    busy.value = false;
  }
}

function versionLabel(entry: ProjectEntry): string {
  const parts = [
    entry.source_version ? `源码 ${entry.source_version}` : null,
    entry.release_version ? `构建 ${entry.release_version}` : '未构建',
    entry.installed_version ? `已安装 ${entry.installed_version}` : '未安装',
  ].filter(Boolean);
  return parts.join(' · ');
}

onMounted(async () => {
  try {
    bridge.value = await createTiangongBridge();
    connected.value = '已连接';
    const provider = createToolProvider(bridge.value);
    providerStop = provider.onRequested(async (invocation: ToolInvocation) => {
      const result = await handleAgentTool(
        bridge.value!,
        invocation.name,
        invocation.arguments as Record<string, unknown>,
      );
      // 本页执行的开发操作（init/build/add 等）改变了项目集，立即刷新——
      // 这是 Agent 创建插件时项目列表的主刷新路径。
      const action = String(
        (invocation.arguments as Record<string, unknown> | null)?.action ?? '',
      );
      if (['init', 'validate', 'build', 'add', 'run'].includes(action)) {
        void refreshProjects();
      }
      try {
        await provider.resolve({
          invocation_id: invocation.invocation_id,
          status: 'answered',
          result,
        });
      } catch (error) {
        console.warn('tool.resolve 失败', error);
      }
    });
    closedStop = provider.onClosed(() => undefined);
    // 插件集变更（Agent 在对话里完成 init/build/install 等）经桥接订阅实时
    // 刷新项目列表——页面无需手动刷新 tab。
    pluginsChangedStop = bridge.value.on('plugins.changed', () => {
      void refreshProjects();
    });
    // 兜底：页面切回可见（上下文变化）时刷新，覆盖尚未触发插件变更事件的
    // 中间状态（如 Agent 刚 init 完还没 install）。
    contextChangeStop = getShadowHostRuntime()?.onContextChange(() => {
      void refreshProjects();
    }) ?? null;
    await refreshProjects();
  } catch (error) {
    connected.value = `桥接连接失败：${error instanceof Error ? error.message : String(error)}`;
  }
});

onBeforeUnmount(() => {
  providerStop?.();
  closedStop?.();
  pluginsChangedStop?.();
  contextChangeStop?.();
});

// 安装历史落插件私有存储（storage.private 权限的实际消费），供追溯。
async function recordHistory(text: string) {
  if (!bridge.value) return;
  try {
    const raw = await pluginStorage.get(bridge.value, 'install_history');
    const history = raw ? (JSON.parse(raw) as string[]) : [];
    history.unshift(`${new Date().toISOString().slice(0, 19).replace('T', ' ')} ${text}`);
    await pluginStorage.set(bridge.value, 'install_history', JSON.stringify(history.slice(0, 50)));
  } catch {
    // 历史记录是尽力而为的辅助信息，失败不影响主流程。
  }
}
</script>

<template>
  <div class="app">
    <header class="head">
      <h2>插件创作</h2>
      <span class="status" :data-ok="connected === '已连接'">{{ connected }}</span>
    </header>

    <section class="card">
      <h3>新建插件项目</h3>
      <div class="templates">
        <button
          v-for="item in TEMPLATES"
          :key="item.id"
          class="template"
          :class="{ active: selectedTemplate === item.id }"
          type="button"
          @click="selectedTemplate = item.id"
        >
          <strong>{{ item.title }}</strong>
          <span>{{ item.description }}</span>
          <code>{{ item.id }}</code>
        </button>
      </div>
      <div class="row">
        <input v-model="newName" placeholder="插件名称（如：番茄钟）" @keyup.enter="startCreation" />
        <button class="primary" type="button" :disabled="sending" @click="startCreation">
          {{ sending ? '发送中…' : '开始创建' }}
        </button>
      </div>
      <p class="hint">
        点击「开始创建」后，需求会直接交给当前会话的 Agent：它依名称拟定插件
        ID、经 devkit 生成骨架并继续开发；构建、试运行同样在对话中完成，
        本页面负责看板与安装。
      </p>
    </section>

    <section class="card">
      <h3>项目列表</h3>
      <p v-if="projects.length === 0" class="hint">暂无项目。用上方命令让 Agent 创建，或让 Agent 直接执行 devkit init。</p>
      <ul v-else class="projects">
        <li v-for="entry in projects" :key="entry.id">
          <div class="meta">
            <strong>{{ entry.name }}</strong>
            <code>{{ entry.id }}</code>
            <span class="badge">{{ entry.template }}</span>
            <span class="versions">{{ versionLabel(entry) }}</span>
          </div>
          <div class="actions">
            <button type="button" class="ghost" :disabled="busy" @click="buildProject(entry)">构建</button>
            <button type="button" :disabled="busy" @click="installProject(entry)">安装</button>
          </div>
        </li>
      </ul>
    </section>

    <section v-if="output" class="card" :class="`out-${output.kind}`">
      <h3>{{ output.title }}</h3>
      <pre>{{ output.body }}</pre>
    </section>
  </div>
</template>

<style scoped>
* { box-sizing: border-box; }
.app {
  min-height: 100%;
  padding: 14px;
  font-family: inherit;
  font-size: 13px;
  background: hsl(var(--background, 0 0% 100%));
  color: hsl(var(--foreground, 222.2 47.4% 11.2%));
  overflow: auto;
}
.head { display: flex; align-items: baseline; gap: 10px; margin-bottom: 12px; }
.head h2 { margin: 0; font-size: 15px; }
.status { font-size: 11px; color: hsl(var(--muted-foreground, 215.4 16.3% 46.9%)); }
.status[data-ok='true'] { color: hsl(var(--green, 142.1 76.2% 36.3%)); }
.card {
  border: 1px solid hsl(var(--border, 214.3 31.8% 91.4%));
  border-radius: var(--radius, 0.5rem);
  padding: 12px;
  margin-bottom: 12px;
}
.card h3 { margin: 0 0 10px; font-size: 13px; }
.templates { display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap: 8px; margin-bottom: 10px; }
.template {
  display: flex; flex-direction: column; gap: 4px; align-items: flex-start;
  padding: 10px; text-align: left; cursor: pointer;
  border: 1px solid hsl(var(--border, 214.3 31.8% 91.4%));
  border-radius: var(--radius, 0.5rem);
  background: transparent; color: inherit;
}
.template.active {
  border-color: hsl(var(--primary, 222.2 47.4% 11.2%));
  box-shadow: 0 0 0 1px hsl(var(--primary, 222.2 47.4% 11.2%));
}
.template span { color: hsl(var(--muted-foreground, 215.4 16.3% 46.9%)); font-size: 11px; }
.template code { font-size: 10px; opacity: 0.7; }
.row { display: flex; gap: 8px; }
.row input {
  flex: 1; padding: 6px 8px;
  border: 1px solid hsl(var(--border, 214.3 31.8% 91.4%));
  border-radius: var(--radius, 0.5rem);
  background: transparent; color: inherit;
}
button {
  padding: 6px 12px; border: none; border-radius: var(--radius, 0.5rem);
  background: hsl(var(--secondary, 210 40% 96.1%));
  color: hsl(var(--secondary-foreground, 222.2 47.4% 11.2%));
  cursor: pointer; font-size: 12px;
}
button:disabled { opacity: 0.5; cursor: not-allowed; }
button.primary {
  background: hsl(var(--primary, 222.2 47.4% 11.2%));
  color: hsl(var(--primary-foreground, 210 40% 98%));
}
button.ghost { background: transparent; color: hsl(var(--muted-foreground, 215.4 16.3% 46.9%)); }
.hint { margin: 8px 0 0; font-size: 11px; color: hsl(var(--muted-foreground, 215.4 16.3% 46.9%)); }
.projects { list-style: none; padding: 0; margin: 0; }
.projects li {
  display: flex; justify-content: space-between; gap: 10px; align-items: center;
  padding: 8px; margin-bottom: 6px;
  border: 1px solid hsl(var(--border, 214.3 31.8% 91.4%));
  border-radius: var(--radius, 0.5rem);
}
.meta { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.meta .badge {
  padding: 1px 6px; font-size: 10px; border-radius: 999px;
  border: 1px solid hsl(var(--border, 214.3 31.8% 91.4%));
  color: hsl(var(--muted-foreground, 215.4 16.3% 46.9%));
}
.meta .versions { font-size: 11px; color: hsl(var(--muted-foreground, 215.4 16.3% 46.9%)); }
.actions { display: flex; gap: 6px; flex-shrink: 0; }
pre {
  margin: 0; padding: 10px; max-height: 300px; overflow: auto;
  border-radius: var(--radius, 0.5rem);
  background: hsl(var(--muted, 210 40% 96.1%));
  font-size: 11px; white-space: pre-wrap; word-break: break-all;
}
.out-error h3 { color: hsl(var(--red, 0 84.2% 60.2%)); }
.out-success h3 { color: hsl(var(--green, 142.1 76.2% 36.3%)); }
</style>
