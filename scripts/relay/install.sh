#!/usr/bin/env bash
# 天工远程中继（tiangong-relay）一键安装 / 升级 / 守护进程管理（仅 Linux）。
#
# 用法：
#   curl -fsSL https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/relay/install.sh | sudo bash
#   sudo bash install.sh install   [--version vX.Y.Z] [--listen 0.0.0.0:8790] [--max-agents 256]
#   sudo bash install.sh upgrade   [--version vX.Y.Z]
#   sudo bash install.sh uninstall [--purge]
#   bash install.sh status
#
# 中继是纯转发服务，不需要配置任何令牌：天工桌面端自动生成通道密钥接入。
# install：下载对应架构的二进制（校验 SHA-256）→ /usr/local/bin/tiangong-relay，
#          创建系统用户 tiangong-relay，写入 /etc/tiangong-relay/relay.env 与 systemd 服务并启动。
# upgrade：只替换二进制并重启服务，保留配置；失败时自动回滚到旧版本。
#
# 环境变量：
#   TIANGONG_RELAY_SOURCE    下载源：oss（默认，阿里云 OSS）/ github（GitHub Release）
#   TIANGONG_RELAY_REPO      GitHub 仓库，默认 silent-rs/Tiangong
#   TIANGONG_RELAY_BASE_URL  自定义下载地址前缀（内网镜像），目录结构同 OSS：<前缀>/<tag>/<制品>
set -euo pipefail

REPO="${TIANGONG_RELAY_REPO:-silent-rs/Tiangong}"
SOURCE="${TIANGONG_RELAY_SOURCE:-oss}"
OSS_BASE_URL="https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/relay"
BASE_URL="${TIANGONG_RELAY_BASE_URL:-}"
TAG_PREFIX="relay-v"
BIN_NAME="tiangong-relay"
BIN_PATH="/usr/local/bin/${BIN_NAME}"
CONF_DIR="/etc/tiangong-relay"
ENV_FILE="${CONF_DIR}/relay.env"
UNIT_FILE="/etc/systemd/system/tiangong-relay.service"
SERVICE_USER="tiangong-relay"

ACTION="install"
VERSION=""
LISTEN="0.0.0.0:8790"
MAX_AGENTS="256"
PURGE=false

log() { printf '\033[1;32m[tiangong-relay]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[tiangong-relay]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[tiangong-relay]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  sed -n '2,19p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

parse_args() {
  if [[ $# -gt 0 && "$1" != -* ]]; then
    ACTION="$1"
    shift
  fi
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --version) VERSION="${2:?缺少版本号}"; shift 2 ;;
      --listen) LISTEN="${2:?缺少监听地址}"; shift 2 ;;
      --max-agents) MAX_AGENTS="${2:?缺少数量}"; shift 2 ;;
      --purge) PURGE=true; shift ;;
      -h|--help) usage 0 ;;
      *) die "未知参数：$1（--help 查看用法）" ;;
    esac
  done
}

require_root() {
  [[ "$(id -u)" -eq 0 ]] || die "需要 root 权限，请使用 sudo 运行"
}

require_linux() {
  [[ "$(uname -s)" == "Linux" ]] || die "中继安装脚本目前仅支持 Linux"
}

detect_target() {
  local arch
  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64) echo "x86_64-unknown-linux-musl" ;;
    aarch64|arm64) echo "aarch64-unknown-linux-musl" ;;
    *) die "不支持的 CPU 架构：$arch（支持 x86_64 / aarch64）" ;;
  esac
}

fetch() {
  # fetch <url> <output>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --connect-timeout 15 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -T 15 -t 3 -O "$2" "$1"
  else
    die "需要 curl 或 wget"
  fi
}

resolve_version() {
  if [[ -n "$VERSION" ]]; then
    [[ "$VERSION" == ${TAG_PREFIX}* ]] || VERSION="${TAG_PREFIX}${VERSION#v}"
    return
  fi
  [[ -z "$BASE_URL" ]] || die "使用 TIANGONG_RELAY_BASE_URL 时请用 --version 指定版本"
  local tmp
  tmp="$(mktemp)"
  if [[ "$SOURCE" == "github" ]]; then
    # 中继与桌面端共用仓库：从 Release 列表中取最新的 relay-v* 标签。
    fetch "https://api.github.com/repos/${REPO}/releases?per_page=50" "$tmp" \
      || { rm -f "$tmp"; die "获取版本列表失败，请用 --version 指定版本"; }
    VERSION="$(grep -o "\"tag_name\": *\"${TAG_PREFIX}[^\"]*\"" "$tmp" | head -n1 | sed 's/.*"\([^"]*\)"$/\1/')"
  else
    # OSS 最新版本指针：{"tag":"relay-vX.Y.Z","version":"X.Y.Z"}
    fetch "${OSS_BASE_URL}/latest.json" "$tmp" \
      || { rm -f "$tmp"; die "获取最新版本失败，请用 --version 指定版本或设置 TIANGONG_RELAY_SOURCE=github"; }
    VERSION="$(grep -o "\"tag\": *\"${TAG_PREFIX}[^\"]*\"" "$tmp" | head -n1 | sed 's/.*"\([^"]*\)"$/\1/')"
  fi
  rm -f "$tmp"
  [[ -n "$VERSION" ]] || die "未找到已发布的中继版本（${TAG_PREFIX}*），请用 --version 指定"
}

installed_version() {
  if [[ -x "$BIN_PATH" ]]; then
    "$BIN_PATH" --version 2>/dev/null | awk '{print $2}'
  fi
}

download_binary() {
  # download_binary <dest>
  local target asset url tmpdir expected actual
  target="$(detect_target)"
  asset="${BIN_NAME}-${target}"
  if [[ -n "$BASE_URL" ]]; then
    url="${BASE_URL%/}/${VERSION}/${asset}"
  elif [[ "$SOURCE" == "github" ]]; then
    url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
  else
    url="${OSS_BASE_URL}/${VERSION}/${asset}"
  fi
  tmpdir="$(mktemp -d)"
  log "下载 ${VERSION} (${target})"
  fetch "$url" "${tmpdir}/${asset}" || { rm -rf "$tmpdir"; die "下载失败：$url"; }
  fetch "${url}.sha256" "${tmpdir}/${asset}.sha256" || { rm -rf "$tmpdir"; die "下载校验文件失败：${url}.sha256"; }
  expected="$(tr -d ' \r\n' < "${tmpdir}/${asset}.sha256" | cut -c1-64)"
  actual="$(sha256sum "${tmpdir}/${asset}" | awk '{print $1}')"
  if [[ "$expected" != "$actual" ]]; then
    rm -rf "$tmpdir"
    die "SHA-256 校验失败（期望 $expected，实际 $actual）"
  fi
  install -m 0755 "${tmpdir}/${asset}" "$1"
  rm -rf "$tmpdir"
  "$1" --version >/dev/null || die "下载的二进制无法运行"
}

has_systemd() {
  command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]]
}

ensure_user() {
  if ! id "$SERVICE_USER" >/dev/null 2>&1; then
    local nologin
    nologin="$(command -v nologin || echo /usr/sbin/nologin)"
    useradd --system --no-create-home --home-dir /nonexistent --shell "$nologin" "$SERVICE_USER"
  fi
}

write_env() {
  mkdir -p "$CONF_DIR"
  if [[ -f "$ENV_FILE" ]]; then
    log "保留已有配置 $ENV_FILE"
    return
  fi
  cat > "$ENV_FILE" <<EOF
# 天工远程中继配置（修改后执行 systemctl restart tiangong-relay）
TIANGONG_RELAY_LISTEN=${LISTEN}
TIANGONG_RELAY_MAX_AGENTS=${MAX_AGENTS}
# 不使用反向代理时可让中继直接提供 HTTPS（证书需对 ${SERVICE_USER} 可读）：
# TIANGONG_RELAY_TLS_CERT=/etc/tiangong-relay/fullchain.pem
# TIANGONG_RELAY_TLS_KEY=/etc/tiangong-relay/privkey.pem
RUST_LOG=info
EOF
  chmod 0644 "$ENV_FILE"
}

write_unit() {
  cat > "$UNIT_FILE" <<EOF
[Unit]
Description=Tiangong remote relay
Documentation=https://github.com/${REPO}/blob/main/docs/remote-access.md
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_USER}
EnvironmentFile=${ENV_FILE}
ExecStart=${BIN_PATH}
Restart=always
RestartSec=3
LimitNOFILE=65536
# 安全加固：中继不读写任何本地数据
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
EOF
}

wait_healthy() {
  local port="${LISTEN##*:}" i
  for i in $(seq 1 20); do
    if command -v curl >/dev/null 2>&1; then
      curl -fsS "http://127.0.0.1:${port}/healthz" >/dev/null 2>&1 && return 0
    elif systemctl is-active --quiet tiangong-relay; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

load_listen_from_env() {
  if [[ -f "$ENV_FILE" ]]; then
    local value
    value="$(grep -E '^TIANGONG_RELAY_LISTEN=' "$ENV_FILE" | tail -n1 | cut -d= -f2- || true)"
    [[ -z "$value" ]] || LISTEN="$value"
  fi
}

do_install() {
  require_linux
  require_root
  resolve_version
  local current
  current="$(installed_version || true)"
  if [[ -n "$current" ]]; then
    log "检测到已安装版本 ${current}，改为执行升级"
    do_upgrade
    return
  fi
  download_binary "$BIN_PATH"
  if ! has_systemd; then
    warn "未检测到 systemd，已仅安装二进制：$BIN_PATH"
    warn "可手动以守护方式运行：nohup $BIN_PATH --listen $LISTEN >/var/log/tiangong-relay.log 2>&1 &"
    return
  fi
  ensure_user
  write_env
  load_listen_from_env
  write_unit
  systemctl daemon-reload
  systemctl enable --now tiangong-relay
  if wait_healthy; then
    log "安装完成：$("$BIN_PATH" --version)，监听 ${LISTEN}"
  else
    warn "服务已启动但健康检查未通过，请查看：journalctl -u tiangong-relay -n 50"
  fi
  cat <<EOF

下一步：
  1. 公网使用请在前面配置 HTTPS 反向代理（需支持 WebSocket 升级），示例见 docs/remote-access.md；
  2. 在天工「设置 → 远程访问 → 中继服务」填写中继地址（如 https://relay.example.com），无需令牌；
  3. 管理：systemctl {status|restart|stop} tiangong-relay；日志：journalctl -u tiangong-relay -f
  4. 升级：sudo bash install.sh upgrade
EOF
}

do_upgrade() {
  require_linux
  require_root
  [[ -x "$BIN_PATH" ]] || die "未安装中继，请先执行 install"
  resolve_version
  local current
  current="$(installed_version || true)"
  if [[ "v${current}" == "v${VERSION#${TAG_PREFIX}}" ]]; then
    log "已是最新版本 ${current}"
    return
  fi
  local staged="${BIN_PATH}.new" backup="${BIN_PATH}.bak"
  download_binary "$staged"
  cp -p "$BIN_PATH" "$backup"
  mv -f "$staged" "$BIN_PATH"
  if has_systemd && systemctl list-unit-files tiangong-relay.service >/dev/null 2>&1; then
    load_listen_from_env
    # 刷新服务单元（新版本可能更新了加固项），保留 relay.env。
    write_unit
    systemctl daemon-reload
    systemctl restart tiangong-relay
    if ! wait_healthy; then
      warn "新版本启动失败，回滚到 ${current}"
      mv -f "$backup" "$BIN_PATH"
      systemctl restart tiangong-relay
      die "升级失败，已回滚；日志：journalctl -u tiangong-relay -n 50"
    fi
  fi
  rm -f "$backup"
  log "升级完成：${current:-未知} → $("$BIN_PATH" --version | awk '{print $2}')"
}

do_uninstall() {
  require_linux
  require_root
  if has_systemd; then
    systemctl disable --now tiangong-relay 2>/dev/null || true
    rm -f "$UNIT_FILE"
    systemctl daemon-reload
  fi
  rm -f "$BIN_PATH" "${BIN_PATH}.bak" "${BIN_PATH}.new"
  if [[ "$PURGE" == "true" ]]; then
    rm -rf "$CONF_DIR"
    id "$SERVICE_USER" >/dev/null 2>&1 && userdel "$SERVICE_USER" 2>/dev/null || true
    log "已卸载并清除配置"
  else
    log "已卸载（配置保留在 ${CONF_DIR}，加 --purge 一并删除）"
  fi
}

do_status() {
  local current
  current="$(installed_version || true)"
  echo "版本：${current:-未安装}"
  if has_systemd; then
    systemctl --no-pager status tiangong-relay 2>/dev/null | head -n 5 || echo "服务：未注册"
  fi
  load_listen_from_env
  if command -v curl >/dev/null 2>&1; then
    echo "健康检查：$(curl -fsS "http://127.0.0.1:${LISTEN##*:}/healthz" 2>/dev/null || echo 不可达)"
  fi
}

main() {
  parse_args "$@"
  case "$SOURCE" in
    oss|github) ;;
    *) die "TIANGONG_RELAY_SOURCE 只能是 oss 或 github，实际：$SOURCE" ;;
  esac
  case "$ACTION" in
    install) do_install ;;
    upgrade|update) do_upgrade ;;
    uninstall|remove) do_uninstall ;;
    status) do_status ;;
    help) usage 0 ;;
    *) die "未知操作：$ACTION（install / upgrade / uninstall / status）" ;;
  esac
}

main "$@"
