#!/usr/bin/env bash
# 本地 https://localhost 联调：以 HTTPS 启动中继。
#
#   bash scripts/relay/dev-local.sh                       # 默认 https://localhost:8443
#   PORT=9443 bash scripts/relay/dev-local.sh
#   CERT=~/certs/localhost.pem KEY=~/certs/localhost-key.pem bash scripts/relay/dev-local.sh
#
# 证书查找顺序：
#   1. 环境变量 CERT / KEY 指定的文件；
#   2. ~/Documents/certs/localhost+2.pem 与 localhost+2-key.pem（mkcert 默认命名）；
#   3. 用 mkcert 在 target/relay-dev-certs/ 下签发。
# 证书需由系统信任的 CA 签发（mkcert -install），浏览器与天工桌面端才会接受。
set -euo pipefail

PORT="${PORT:-8443}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DOC_CERT="${HOME}/Documents/certs/localhost+2.pem"
DOC_KEY="${HOME}/Documents/certs/localhost+2-key.pem"

if [[ -n "${CERT:-}" || -n "${KEY:-}" ]]; then
  [[ -s "${CERT:-}" && -s "${KEY:-}" ]] || { echo "CERT 与 KEY 需同时指定且文件存在" >&2; exit 1; }
elif [[ -s "$DOC_CERT" && -s "$DOC_KEY" ]]; then
  CERT="$DOC_CERT"
  KEY="$DOC_KEY"
else
  command -v mkcert >/dev/null 2>&1 || { echo "缺少 mkcert：brew install mkcert && mkcert -install" >&2; exit 1; }
  CERT_DIR="${ROOT}/target/relay-dev-certs"
  CERT="${CERT_DIR}/localhost.pem"
  KEY="${CERT_DIR}/localhost-key.pem"
  mkdir -p "$CERT_DIR"
  if [[ ! -s "$CERT" || ! -s "$KEY" ]]; then
    mkcert -cert-file "$CERT" -key-file "$KEY" localhost 127.0.0.1 ::1
  fi
fi

echo "证书：$CERT"
echo "中继地址：https://localhost:${PORT}（在天工「设置 → 远程访问 → 中继服务」中填写）"
cd "$ROOT"
RUST_LOG="${RUST_LOG:-info,tiangong_relay=debug}" exec cargo run -p tiangong-relay -- \
  --listen "127.0.0.1:${PORT}" \
  --tls-cert "$CERT" \
  --tls-key "$KEY"
