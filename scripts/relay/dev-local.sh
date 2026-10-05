#!/usr/bin/env bash
# 本地 https://localhost 联调：用 mkcert 签发本机受信任证书，以 HTTPS 启动中继。
#
#   bash scripts/relay/dev-local.sh            # 默认 https://localhost:8443
#   PORT=9443 bash scripts/relay/dev-local.sh
#
# 前置：brew install mkcert && mkcert -install（把本地 CA 加入系统钥匙串，
# 浏览器与天工桌面端都会信任它签发的 localhost 证书）。
set -euo pipefail

PORT="${PORT:-8443}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CERT_DIR="${ROOT}/target/relay-dev-certs"
CERT="${CERT_DIR}/localhost.pem"
KEY="${CERT_DIR}/localhost-key.pem"

command -v mkcert >/dev/null 2>&1 || { echo "缺少 mkcert：brew install mkcert && mkcert -install" >&2; exit 1; }
mkdir -p "$CERT_DIR"
if [[ ! -s "$CERT" || ! -s "$KEY" ]]; then
  mkcert -cert-file "$CERT" -key-file "$KEY" localhost 127.0.0.1 ::1
fi

echo "中继地址：https://localhost:${PORT}（在天工「设置 → 远程访问 → 中继服务」中填写）"
cd "$ROOT"
RUST_LOG="${RUST_LOG:-info,tiangong_relay=debug}" exec cargo run -p tiangong-relay -- \
  --listen "127.0.0.1:${PORT}" \
  --tls-cert "$CERT" \
  --tls-key "$KEY"
