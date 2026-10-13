#!/usr/bin/env python3
"""找出相对基线只改了版本号的文件，供 Plugin CI 跳过纯版本提升的插件。

用法：version_only_changes.py <base-ref> <file>...
标准输出逐行打印“仅版本号变化”的文件；被识别的插件会顺带校验版本一致性
（plugin.json 与 protocol / sidecar Cargo.toml 或 package.json，规则同
xtask validate-plugin），不一致时以非零退出，避免跳过后漏掉版本错配。

判定规则（任一不满足即视为普通改动，交给原有逻辑）：
- plugin.json / package.json：去掉顶层 version 后内容完全相同；
- Cargo.toml：去掉 [package].version 后内容完全相同；
- Cargo.lock：去掉本仓库路径包（无 source）的 version 后内容完全相同。
基线中不存在的文件（新增）一律不算版本提升。
"""
from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path


def show(ref: str, path: str) -> str | None:
    result = subprocess.run(
        ["git", "show", f"{ref}:{path}"], capture_output=True, text=True
    )
    return result.stdout if result.returncode == 0 else None


def strip_json(text: str) -> object:
    data = json.loads(text)
    if isinstance(data, dict):
        data.pop("version", None)
    return data


def strip_cargo_toml(text: str) -> object:
    data = tomllib.loads(text)
    package = data.get("package")
    if isinstance(package, dict):
        package.pop("version", None)
    return data


def strip_cargo_lock(text: str) -> object:
    data = tomllib.loads(text)
    for package in data.get("package", []):
        if "source" not in package:
            package.pop("version", None)
    return data


def normalizer(path: str):
    name = Path(path).name
    if name in ("plugin.json", "package.json"):
        return strip_json
    if name == "Cargo.toml":
        return strip_cargo_toml
    if path == "Cargo.lock":
        return strip_cargo_lock
    return None


def version_only(base: str, path: str) -> bool:
    normalize = normalizer(path)
    if normalize is None:
        return False
    before = show(base, path)
    if before is None or not Path(path).is_file():
        return False
    try:
        return normalize(before) == normalize(Path(path).read_text())
    except (ValueError, tomllib.TOMLDecodeError):
        return False


def read_version(path: Path) -> str | None:
    if not path.is_file():
        return None
    if path.suffix == ".json":
        return json.loads(path.read_text()).get("version")
    return tomllib.loads(path.read_text()).get("package", {}).get("version")


def check_plugin_versions(root: Path) -> str | None:
    manifest = read_version(root / "plugin.json")
    for source in ("protocol/Cargo.toml", "sidecar/Cargo.toml", "package.json"):
        expected = read_version(root / source)
        if expected is not None:
            if expected != manifest:
                return f"{root}: plugin.json={manifest} 与 {source}={expected} 不一致"
            return None
    return None


def main() -> int:
    base, *files = sys.argv[1:]
    matched = [path for path in files if version_only(base, path)]
    roots = {
        Path(*Path(path).parts[:2])
        for path in matched
        if path.startswith("plugins/") and len(Path(path).parts) > 2
    }
    errors = [
        error
        for root in sorted(roots)
        if (root / "plugin.json").is_file()
        for error in [check_plugin_versions(root)]
        if error
    ]
    if errors:
        for error in errors:
            print(f"版本提升校验失败: {error}", file=sys.stderr)
        return 1
    for path in matched:
        print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
