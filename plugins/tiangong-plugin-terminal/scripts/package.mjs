// 开发期打包：构建 UI 并组装可直接「导入本地插件」的插件包目录（release/）。
// 产物 = plugin.json + dist/（不含源码与 node_modules），走天工正式导入流程
// （清单校验 → 事务安装 → 注册表加载），用于开发验证的完整闭环。
// sidecar 插件末尾经 xtask 生成官方开发签名（release.json + release.json.sig），
// 否则导入后启动 sidecar 会被「未签名插件不允许启动原生 sidecar」拒绝。
import { cpSync, copyFileSync, existsSync, mkdirSync, rmSync, readFileSync, writeFileSync } from 'node:fs';
import { execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = dirname(fileURLToPath(import.meta.url));
const pluginRoot = join(root, '..');
const release = join(pluginRoot, 'release');

console.log('[package] vite build...');
execSync('yarn build', { cwd: pluginRoot, stdio: 'inherit' });

console.log('[package] 组装插件包 release/...');
rmSync(release, { recursive: true, force: true });
mkdirSync(release, { recursive: true });
cpSync(join(pluginRoot, 'plugin.json'), join(release, 'plugin.json'));
cpSync(join(pluginRoot, 'dist'), join(release, 'dist'), { recursive: true });
// 自带 App 图标（plugin.json resources 声明的 assets/）随包分发。
cpSync(join(pluginRoot, 'assets'), join(release, 'assets'), { recursive: true });
// sidecar 制品随插件包分发：优先取 release/ 已有制品（build-sidecar.sh 已放）；
// 不存在时自动构建（确保 yarn package 一步产出完整可导入的包）。
const sidecarName = 'tiangong-terminal-sidecar';
const releaseSidecar = join(release, sidecarName);
if (!existsSync(releaseSidecar)) {
  execSync('bash scripts/build-sidecar.sh', { cwd: pluginRoot, stdio: 'inherit' });
}

// 打包即校验：清单与 entry 就位，避免导入时才发现问题
const manifest = JSON.parse(readFileSync(join(release, 'plugin.json'), 'utf8'));
const entry = manifest.ui?.contributions?.[0]?.entry;
if (!entry) throw new Error('plugin.json 缺少 ui.contributions[].entry');
// 单文件 entry（Vite 产物在 dist/ 下）时复制到 release 根，兼容
// dist/index.html + entry: index.html 与 dist/ 子目录两种清单布局
if (!existsSync(join(release, entry)) && existsSync(join(release, 'dist', entry))) {
  copyFileSync(join(release, 'dist', entry), join(release, entry));
}
readFileSync(join(release, entry));
const fingerprint = `id=${manifest.id} version=${manifest.version} entry=${entry}`;
writeFileSync(join(release, '.package-info'), `${fingerprint}\n`);
if (manifest.sidecar) {
  execSync(`cargo run -p xtask -- sign-plugin "${release}"`, {
    cwd: join(pluginRoot, '..', '..'),
    stdio: 'inherit',
  });
}
console.log(`[package] 完成: release/（${fingerprint}）`);
console.log('[package] 天工「设置 → 插件管理 → 导入本地插件」选择 release 目录');
