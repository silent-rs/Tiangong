// init：按模板生成项目骨架（复制 + 占位符替换 + 元数据）。
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { devRoot, fail, storageRoot, validId } from './common.mjs';

const TEXT_EXTENSIONS = new Set([
  'json', 'html', 'ts', 'tsx', 'vue', 'js', 'mjs', 'cjs', 'css', 'md', 'txt', 'yml', 'yaml',
]);

function templatesDir() {
  return join(dirname(fileURLToPath(import.meta.url)), '..', 'templates');
}

export function availableTemplates() {
  return readdirSync(templatesDir()).filter((name) => statSync(join(templatesDir(), name)).isDirectory()).sort();
}

function copyWithPlaceholders(source, destination, pluginId, name) {
  let files = 0;
  for (const entry of readdirSync(source, { withFileTypes: true })) {
    if (entry.name === 'node_modules' || entry.name === '.git') continue;
    const from = join(source, entry.name);
    const to = join(destination, entry.name);
    if (entry.isDirectory()) {
      mkdirSync(to, { recursive: true });
      files += copyWithPlaceholders(from, to, pluginId, name);
    } else if (entry.isFile() && !entry.isSymbolicLink()) {
      const extension = entry.name.includes('.') ? entry.name.split('.').pop() : '';
      if (TEXT_EXTENSIONS.has(extension)) {
        const content = readFileSync(from, 'utf8')
          .replaceAll('{{PLUGIN_ID}}', pluginId)
          .replaceAll('{{PLUGIN_NAME}}', name);
        writeFileSync(to, content);
      } else {
        cpSync(from, to);
      }
      files += 1;
    }
  }
  return files;
}

export async function init(argv, ctx) {
  const [template, id] = argv.filter((arg) => !arg.startsWith('--'));
  const nameIndex = argv.indexOf('--name');
  const name = nameIndex !== -1 && argv[nameIndex + 1] ? argv[nameIndex + 1] : (id ?? '');
  if (!template || !id) {
    return fail('用法：init <template> <id> [--name 显示名]');
  }
  if (!validId(id)) {
    return fail(`插件 ID 只能包含字母数字与 - _ .：${id}`);
  }
  // 防劫持：不得与已安装插件同名（自建插件 id 也不应与 plugin-creator 相关保留字冲突）。
  if (existsSync(join(storageRoot(), 'plugins', id, 'plugin.json'))) {
    return fail(`插件 ID ${id} 已被已安装插件占用，请更换 ID`);
  }
  const templateDir = join(templatesDir(), template);
  if (!existsSync(templateDir)) {
    return fail(`模板 ${template} 不存在。可用模板：${availableTemplates().join('、')}`);
  }
  const projectDir = join(devRoot(ctx), id);
  if (existsSync(projectDir)) {
    return fail(`开发项目 ${id} 已存在：${projectDir}（迭代请直接编辑该项目，勿重复 init）`);
  }
  mkdirSync(projectDir, { recursive: true });
  const files = copyWithPlaceholders(templateDir, projectDir, id, name);
  writeFileSync(
    join(projectDir, '.plugin-dev.json'),
    `${JSON.stringify(
      {
        plugin_id: id,
        name,
        template,
        created_at: new Date().toISOString().slice(0, 19).replace('T', ' '),
      },
      null,
      2,
    )}\n`,
  );
  return {
    ok: true,
    plugin_id: id,
    name,
    template,
    directory: projectDir,
    files,
    next: '在项目内按需求实现（模板内有示例与说明），然后 validate → build → 安装（plugin_creator 工具 action=install）',
  };
}

export { relative };
