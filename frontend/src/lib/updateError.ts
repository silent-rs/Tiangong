/**
 * 应用内更新错误分类：决定是否改用 GitHub 源重试，以及给用户什么提示。
 *
 * tauri updater 下载失败时只返回字符串，例如
 * `Download request failed with status: 404 Not Found`。
 * 只有网络类错误才可能与 VPN/代理有关（#662）；服务端缺包（404/403 等）
 * 与 VPN 无关，此前一律提示 VPN 会误导排查。
 */

export type UpdateErrorKind = 'missing' | 'server' | 'network' | 'signature' | 'other';

export const UPDATE_RELEASES_URL = 'https://github.com/silent-rs/silent-Tiangong/releases/latest';

/** 应用更新请求读取启动时的代理环境变量，关闭 VPN/代理后需重启才能恢复（#662）。 */
export const UPDATE_NETWORK_HINT = '如刚开启或关闭过 VPN/代理，请重启天工后重试。';

export function classifyUpdateError(error: unknown): UpdateErrorKind {
  const text = String(error);
  const status = /status:\s*(\d{3})/i.exec(text)?.[1];
  if (status) {
    if (status === '404' || status === '403' || status === '410') return 'missing';
    if (status.startsWith('5') || status === '429') return 'server';
    return 'other';
  }
  if (/signature|minisign|verify/i.test(text)) return 'signature';
  if (/error sending request|timed? ?out|connect|dns|resolve|network|tls|certificate|proxy|reset|unreachable/i.test(text)) {
    return 'network';
  }
  return 'other';
}

/** 下载失败后是否值得换 GitHub 源再试一次（签名错误不重试，避免掩盖问题）。 */
export function shouldRetryWithGithub(kind: UpdateErrorKind): boolean {
  return kind === 'missing' || kind === 'server' || kind === 'network';
}

export function updateErrorHint(kind: UpdateErrorKind): string {
  switch (kind) {
    case 'missing':
      return '更新源缺少本平台安装包，可前往下载页手动下载安装。';
    case 'server':
      return '更新服务器暂时不可用，请稍后重试，或前往下载页手动下载安装。';
    case 'network':
      return UPDATE_NETWORK_HINT;
    case 'signature':
      return '安装包签名校验失败，已拒绝安装，请前往下载页手动下载安装。';
    default:
      return '可稍后重试，或前往下载页手动下载安装。';
  }
}
