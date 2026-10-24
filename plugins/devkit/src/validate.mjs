// validate：清单与结构校验（plugin.json 解析、必要字段、UI 入口存在性）。
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { assertNoSymlinkPath, fail, requireProject, resolveInside } from './common.mjs';

export async function validate(argv, ctx) {
  const [id] = argv;
  if (!id) return fail('用法：validate <id>');
  let projectDir;
  try {
    projectDir = requireProject(ctx, id);
  } catch (error) {
    return fail(error.message);
  }
  const errors = [];
  const warnings = [];
  const manifestPath = join(projectDir, 'plugin.json');
  if (!existsSync(manifestPath)) {
    return { ok: false, errors: [`缺少 plugin.json`], warnings };
  }
  let manifest;
  try {
    manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  } catch (error) {
    return { ok: false, errors: [`plugin.json 不是有效 JSON：${error.message}`], warnings };
  }
  if (manifest.schema_version !== 2) errors.push(`schema_version 应为 2（收到 ${manifest.schema_version}）`);
  if (!validIdField(manifest.id)) errors.push(`id 非法：${manifest.id}（字母数字与 - _ .）`);
  if (!manifest.version) errors.push('version 为空');
  if (manifest.id && manifest.id !== id) {
    errors.push(`plugin.json id（${manifest.id}）与项目 ID（${id}）不一致`);
  }
  const contributions = manifest.ui?.contributions ?? [];
  for (const contribution of contributions) {
    const entry = contribution.entry ?? '';
    if (!entry) {
      errors.push('ui.contributions 缺少 entry');
      continue;
    }
    let resolved;
    try {
      resolved = resolveInside(projectDir, entry, `UI 入口 ${entry}`);
      assertNoSymlinkPath(projectDir, resolved);
    } catch (error) {
      errors.push(error.message);
      continue;
    }
    if (!existsSync(resolved)) {
      warnings.push(`UI 入口 ${entry} 尚不存在（通常表示还未构建）`);
    }
  }
  for (const directory of manifest.resources ?? []) {
    try {
      const resolved = resolveInside(projectDir, directory, `资源目录 ${directory}`);
      assertNoSymlinkPath(projectDir, resolved);
    } catch (error) {
      errors.push(error.message);
    }
  }
  if (contributions.length === 0 && !manifest.tools?.length && !manifest.wasm) {
    warnings.push('清单未声明任何 UI 贡献、工具或逻辑层，安装后不会有可见效果');
  }
  const mention = manifest.mention;
  if (mention && !mention.candidates_op && mention.hint) {
    warnings.push(
      'mention.hint 为 v0.17.6 兼容写法，仅限存量插件；新插件请使用 candidates_op（node 系模板，需 sidecar）'
    );
  }
  if (mention && !mention.candidates_op && !mention.hint) {
    errors.push('mention 需要 candidates_op 或 hint 至少其一');
  }
  const sidecar = manifest.sidecar;
  if (sidecar && !(manifest.permissions ?? []).includes('sidecar.invoke')) {
    errors.push('声明 sidecar 时必须声明 sidecar.invoke 权限');
  }
  const SIDEAR_RUNTIMES = ['node', 'python'];
  if (sidecar?.runtime && !SIDEAR_RUNTIMES.includes(sidecar.runtime)) {
    errors.push(`sidecar.runtime 非法值 ${sidecar.runtime}（支持 ${SIDEAR_RUNTIMES.join(' / ')}；原生二进制省略该字段）`);
  }
  if (sidecar?.runtime && SIDEAR_RUNTIMES.includes(sidecar.runtime)) {
    if (!sidecar.entry) {
      errors.push(`解释器 sidecar（${sidecar.runtime}）必须声明 entry 入口脚本`);
    } else if (!/^[\w.-]+(\/[\w.-]+)+$/.test(sidecar.entry)) {
      errors.push(`sidecar.entry 必须是子目录内的相对路径: ${sidecar.entry}`);
    }
    if (sidecar.binary) {
      errors.push('解释器 sidecar 不允许声明 binary（解释器由宿主选择）');
    }
  }
  return {
    ok: errors.length === 0,
    errors,
    warnings,
    id: manifest.id,
    version: manifest.version,
    permissions: manifest.permissions ?? [],
    tools: (manifest.tools ?? []).map((tool) => tool.name),
  };
}

function validIdField(id) {
  return typeof id === 'string' && /^[A-Za-z0-9._-]+$/.test(id) && id !== '.' && id !== '..';
}
