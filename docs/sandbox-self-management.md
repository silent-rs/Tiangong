# Sandbox 自管理与首版发布

## 目标

`tiangong-sandbox` 形成独立、自然闭环：自身包含平台隔离、版本信息、官方信任根、更新清单客户端、下载校验、候选自检、原子版本落位和防降级逻辑。宿主只负责调用 Sandbox，不再实现下载、签名验证或版本切换业务。

## 命令

```bash
# 只检查当前进程是否有官方更新，不写磁盘
tiangong-sandbox check-update

# 下载并更新当前正在执行的 Sandbox 自身
tiangong-sandbox update

# 将官方最新版安装到指定目录
tiangong-sandbox update --root <绝对目录>
```

默认清单：

```text
https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/latest.json
```

无 `--root` 时，目标是当前 `current_exe()`：Unix 通过同目录原子重命名替换；Windows 由已验签候选进程等待旧进程退出后完成替换。指定目录时只安装：

```text
<目录>/tiangong-sandbox[.exe]
<目录>/tiangong-sandbox[.exe].sig
```

不会创建 `sandbox/versions/active` 版本仓库。App 集成使用 `<storage>/sandbox/` 作为安装目录，生产启动验签只认 Sandbox 独立官方根；详见 `docs/sandbox-direct-storage.md`。`--manifest-url` 仅用于受控测试；生产使用 HTTPS，测试构建只允许本机回环 HTTP。

## 安全门禁

- 清单、制品和签名必须使用 HTTPS；
- 不按协议版本或策略 Schema 版本化；清单中的 `protocol_version`（冻结为 1）与 `policy_schema_max`（冻结为 2）仅为兼容已发布 App 保留，新版本不校验；
- 制品逐块下载且有 128 MiB 上限；
- SHA-256 必须匹配清单；
- minisign 必须通过内置官方公钥；
- 候选必须是普通文件，不接受符号链接；
- 候选 `--self-check` 必须退出 0，且自报版本与清单一致；
- 进程级文件锁串行化检查和升级，进程异常退出后锁自动释放；
- 指定目录中的已有程序必须先验签、自检并检查版本，禁止降级；
- 程序与签名使用同目录事务文件成对替换，失败时恢复旧文件。

## 发布

首版 `0.1.0` 是 bootstrap 版本，必须通过官方发布工作流投放。线上尚无清单时允许首次发布；之后候选版本必须严格高于线上版本。

发布入口：

```text
sandbox/v0.1.0
```

发布工作流要求：

- 标签版本等于 crate 版本；
- 标签提交已进入 main；手动发布只能从 main；
- 生产发布全局串行，不取消正在发布的任务；
- Linux x86_64、macOS aarch64、macOS x86_64、Windows x86_64 四平台构建；
- 每个平台最终制品先用内置官方公钥验签，再执行自检；
- 版本对象不可覆盖；
- 上传后公开回读并验证 SHA-256；
- `latest.json.next` 回读一致后，最后写 `latest.json`；
- 正式清单写入后只读确认，不做不安全回滚。

## 首版发布步骤

1. 合并本分支到 main，并确认 Sandbox CI 全部通过。
2. 确认 GitHub Secrets：
   - `TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY`
   - `TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD`（私钥无密码时可空）
   - `ALIYUN_OSS_ACCESS_KEY_ID`
   - `ALIYUN_OSS_ACCESS_KEY_SECRET`
3. 确认官方私钥能被仓库内置公钥验证。
4. 在 main 的合并提交创建并推送 `sandbox/v0.1.0`。
5. 等待 `Publish Sandbox` 四平台构建与 OSS 发布成功。
6. 公开回读 `sandbox/latest.json` 和四个平台制品。
7. 在隔离临时存储根执行：

```bash
tiangong-sandbox check-update
tiangong-sandbox update --root <临时绝对目录>
```

0.1.0 发布后，0.1.1 及后续版本可以由已安装 Sandbox 自身执行 `update` 完成升级。
