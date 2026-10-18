#!/usr/bin/env python3
"""主程序 Release 镜像到阿里云 OSS 时，以 latest.json 为唯一事实来源。

用法：
  oss_release_manifest.py assets  <latest.json> <repo> <tag>
      打印 updater 引用的全部产物文件名（去重），并校验 URL 均指向本次 Release。
  oss_release_manifest.py check-sigs <latest.json> <dir>
      校验目录内 <产物>.sig 与清单 signature 字段一致（防止镜像了错版本的包）。
  oss_release_manifest.py rewrite <latest.json> <out.json> <repo> <tag> <oss_base>
      把 GitHub 下载地址改写为 <oss_base>/<tag>/<文件名>，其余字段原样保留。
  oss_release_manifest.py verify  <oss-latest.json> <dir>
      逐个下载改写后的 OSS 地址，与本地文件比较 SHA-256；任一失败即非零退出。

清单中出现、但无法按以上规则处理的 URL 一律报错，避免再次发布指向
不存在对象的 latest.json（v0.17.2～v0.17.4 的 macOS 应用内更新 404 即源于此）。
"""
from __future__ import annotations

import hashlib
import json
import sys
import urllib.request
from pathlib import Path
from urllib.parse import unquote, urlparse


def load(path: str) -> dict:
    manifest = json.loads(Path(path).read_text(encoding="utf-8"))
    platforms = manifest.get("platforms")
    if not isinstance(platforms, dict) or not platforms:
        sys.exit(f"{path}: platforms 为空")
    return manifest


def asset_name(url: str, prefix: str) -> str:
    if not url.startswith(prefix):
        sys.exit(f"URL 不属于本次 Release（期望前缀 {prefix}）：{url}")
    name = unquote(url[len(prefix) :])
    if not name or "/" in name:
        sys.exit(f"无法从 URL 解析文件名：{url}")
    return name


def github_prefix(repo: str, tag: str) -> str:
    return f"https://github.com/{repo}/releases/download/{tag}/"


def cmd_assets(path: str, repo: str, tag: str) -> None:
    prefix = github_prefix(repo, tag)
    names = {asset_name(p["url"], prefix) for p in load(path)["platforms"].values()}
    for name in sorted(names):
        print(name)


def cmd_check_sigs(path: str, directory: str) -> None:
    for platform, entry in load(path)["platforms"].items():
        name = Path(urlparse(entry["url"]).path).name
        sig_file = Path(directory) / f"{unquote(name)}.sig"
        if not sig_file.is_file():
            sys.exit(f"{platform}: 缺少签名文件 {sig_file.name}")
        if sig_file.read_text(encoding="utf-8").strip() != entry["signature"].strip():
            sys.exit(
                f"{platform}: {sig_file.name} 与 latest.json 中的 signature 不一致"
            )
        print(f"{platform}: 签名一致")


def cmd_rewrite(path: str, out: str, repo: str, tag: str, oss_base: str) -> None:
    prefix = github_prefix(repo, tag)
    manifest = load(path)
    for entry in manifest["platforms"].values():
        entry["url"] = (
            f"{oss_base.rstrip('/')}/{tag}/{asset_name(entry['url'], prefix)}"
        )
    Path(out).write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )


def sha256_of(reader) -> str:
    digest = hashlib.sha256()
    for chunk in iter(lambda: reader.read(1 << 20), b""):
        digest.update(chunk)
    return digest.hexdigest()


def cmd_verify(path: str, directory: str) -> None:
    urls = sorted({p["url"] for p in load(path)["platforms"].values()})
    failed = False
    for url in urls:
        local = Path(directory) / unquote(Path(urlparse(url).path).name)
        if not local.is_file():
            print(f"FAIL {url}: 本地缺少 {local.name}")
            failed = True
            continue
        with local.open("rb") as fh:
            expected = sha256_of(fh)
        try:
            with urllib.request.urlopen(url, timeout=120) as resp:
                actual = sha256_of(resp)
        except Exception as exc:  # noqa: BLE001 - 任何下载异常都视为不可用
            print(f"FAIL {url}: {exc}")
            failed = True
            continue
        if actual != expected:
            print(f"FAIL {url}: SHA-256 不一致（OSS {actual}，本地 {expected}）")
            failed = True
        else:
            print(f"OK   {url} sha256={actual}")
    if failed:
        sys.exit("OSS 产物回读校验失败，未发布 latest.json")


def main(argv: list[str]) -> None:
    commands = {
        "assets": (cmd_assets, 3),
        "check-sigs": (cmd_check_sigs, 2),
        "rewrite": (cmd_rewrite, 5),
        "verify": (cmd_verify, 2),
    }
    if (
        len(argv) < 2
        or argv[1] not in commands
        or len(argv) - 2 != commands[argv[1]][1]
    ):
        sys.exit(__doc__)
    func, _ = commands[argv[1]]
    func(*argv[2:])


if __name__ == "__main__":
    main(sys.argv)
