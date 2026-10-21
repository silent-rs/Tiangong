#!/usr/bin/env bash
# 供 pre-commit pre-push hook 使用：只对受影响的 crate 执行 cargo 子命令。
#
# pre-commit 会把改动的文件路径作为参数传入（pass_filenames: true）。
# 本脚本从中提取 crate 名，去重后直接执行：
#   cargo <sub> -p <crate1> -p <crate2> ...
#
# 用法（由 .pre-commit-config.yaml 的 entry 调用）：
#   pre-commit-affected-crates.sh <check|clippy|nextest> [文件...]
set -euo pipefail

sub="${1:-check}"
shift || true

# 从文件路径提取 crate 名并去重：
#   crates/<name>/...           → <name>
#   plugins/tiangong-plugin-memory/<part>/... → tiangong-plugin-memory-<part>
#   crates/plugins/<name>/...   → <name>（插件 crate 嵌套在 plugins/ 子目录下）
#   crates/plugin/<name>/...    → <name>（插件基础设施 crate 嵌套在 plugin/ 子目录下）
#   crates/sandbox/<name>/...   → <name>（沙箱 crate 嵌套在 sandbox/ 子目录下）
#   tests/plugin-sidecar-e2e/...→ tiangong-plugin-sidecar-e2e
#   src-tauri/...               → tiangong-app
#   src/...                     → tiangong
# 用 if [[ ]] 模式匹配 + sort -u 去重，兼容老版本 bash，避免 case 在命令替换内的解析问题
crates="$(
  for f in "$@"; do
    # WASM 的真实目标编译由发布流程承担；提交阶段仍执行 rustfmt。
    if [[ "$f" == plugins/*/wasm/* ]]; then
      continue
    elif [[ "$f" == plugins/tiangong-plugin-memory/protocol/* ]]; then
      printf '%s\n' "tiangong-plugin-memory-protocol"
    elif [[ "$f" == plugins/tiangong-plugin-memory/sidecar/* ]]; then
      printf '%s\n' "tiangong-plugin-memory-sidecar"
    elif [[ "$f" == plugins/tiangong-plugin-mcp/protocol/* ]]; then
      printf '%s\n' "tiangong-plugin-mcp-protocol"
    elif [[ "$f" == plugins/tiangong-plugin-mcp/sidecar/* ]]; then
      printf '%s\n' "tiangong-plugin-mcp-sidecar"
    elif [[ "$f" == plugins/tiangong-plugin-index/protocol/* ]]; then
      printf '%s\n' "tiangong-plugin-index-protocol"
    elif [[ "$f" == plugins/tiangong-plugin-index/sidecar/* ]]; then
      printf '%s\n' "tiangong-plugin-index-sidecar"
    elif [[ "$f" == plugins/tiangong-plugin-scheduler/protocol/* ]]; then
      printf '%s\n' "tiangong-plugin-scheduler-protocol"
    elif [[ "$f" == plugins/tiangong-plugin-scheduler/sidecar/* ]]; then
      printf '%s\n' "tiangong-plugin-scheduler-sidecar"
    elif [[ "$f" == plugins/tiangong-plugin-scheduler/wasm/* ]]; then
      printf '%s\n' "tiangong-plugin-scheduler-wasm"
    elif [[ "$f" == plugins/tiangong-plugin-terminal/sidecar/* ]]; then
      printf '%s\n' "tiangong-plugin-terminal-sidecar"
    elif [[ "$f" == plugins/*/* ]]; then
      rest="${f#plugins/}"
      printf '%s\n' "${rest%%/*}"
    elif [[ "$f" == crates/plugins/*/* ]]; then
      rest="${f#crates/plugins/}"
      printf '%s\n' "${rest%%/*}"
    elif [[ "$f" == crates/plugin/*/* ]]; then
      rest="${f#crates/plugin/}"
      printf '%s\n' "${rest%%/*}"
    elif [[ "$f" == crates/sandbox/*/* ]]; then
      rest="${f#crates/sandbox/}"
      printf '%s\n' "${rest%%/*}"
    elif [[ "$f" == crates/*/* ]]; then
      rest="${f#crates/}"
      printf '%s\n' "${rest%%/*}"
    elif [[ "$f" == tests/plugin-sidecar-e2e/* ]]; then
      printf '%s\n' "tiangong-plugin-sidecar-e2e"
    elif [[ "$f" == src-tauri/* ]]; then
      printf '%s\n' "tiangong-app"
    elif [[ "$f" == src/* ]]; then
      printf '%s\n' "tiangong"
    fi
  done | sort -u
)"

if [[ -z "$crates" ]]; then
  echo "pre-push[${sub}]: 无受影响 crate，跳过"
  exit 0
fi

echo "pre-push[${sub}]: 受影响 crate = $(echo "$crates" | tr '\n' ' ')"
# 拼成 -p a -p b ... 参数；跳过 workspace 中已不存在的包（如已删除的旧 crate，
# 其路径仍可能作为本次改动被 pre-commit 传入）。
args=()
while IFS= read -r c; do
  if cargo metadata --no-deps --format-version 1 >/dev/null 2>&1 &&
     grep -q "\"name\":\"$c\"" <(cargo metadata --no-deps --format-version 1 2>/dev/null); then
    args+=("-p" "$c")
  else
    echo "pre-push[${sub}]: 跳过不存在的 crate: $c"
  fi
done <<< "$crates"

if [[ ${#args[@]} -eq 0 ]]; then
  echo "pre-push[${sub}]: 受影响 crate 均不存在，跳过"
  exit 0
fi

case "$sub" in
  check)   cargo check "${args[@]}" ;;
  clippy)  cargo clippy "${args[@]}" --all-targets --tests --benches -- -D warnings ;;
  nextest)
    # Runtime 的真实 Launcher 测试会从 target/debug 直接启动这两个独立程序。
    # 它们不是 Runtime 的依赖，nextest 不会自动重建；先构建可避免复用旧版
    # command sidecar 后出现握手版本不一致。
    prepare_runtime_test_binaries=false
    for ((i = 0; i < ${#args[@]}; i += 2)); do
      if [[ "${args[i]}" == "-p" && "${args[i + 1]}" == "tiangong-plugin-runtime" ]]; then
        prepare_runtime_test_binaries=true
        break
      fi
    done
    if [[ "$prepare_runtime_test_binaries" == "true" ]]; then
      cargo build -p tiangong-sandbox --bin tiangong-sandbox \
        -p tiangong-plugin-command-sidecar --bin tiangong-command-sidecar \
        -p tiangong-plugin-sidecar-e2e --bin test-stdio-host
    fi
    cargo nextest run "${args[@]}" --no-tests pass
    ;;
  *) echo "pre-push: 未知子命令 '$sub'（应为 check/clippy/nextest）" >&2; exit 1 ;;
esac
