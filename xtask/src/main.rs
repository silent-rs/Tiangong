//! 天工辅助构建任务。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const WASM_TARGET: &str = "wasm32-wasip2";
const RUNTIME_MANIFEST: &str = "crates/tiangong-plugin-runtime/Cargo.toml";
const PLUGIN_DIST: &str = "target/plugin-dist";
const DEFAULT_OSS_BASE_URL: &str = "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com";
const PLUGIN_CATALOG_VERSION: u32 = 1;
const PRESERVED_DIRS: [&str; 3] = ["runtime", "logs", "data"];

#[derive(Debug, Deserialize)]
struct PublishedPluginCatalog {
    version: u32,
    plugins: Vec<PublishedPluginRelease>,
}

#[derive(Debug, Deserialize)]
struct PublishedPluginRelease {
    id: String,
    name: String,
    version: String,
    #[serde(default, rename = "description")]
    _description: String,
    manifest: PublishedRemoteArtifact,
    /// 纯 UI 插件无 WASM 制品，目录条目省略 wasm。
    #[serde(default)]
    wasm: Option<PublishedRemoteArtifact>,
    #[serde(default)]
    signed_releases: BTreeMap<String, PublishedSignedRelease>,
    #[serde(default)]
    sidecars: BTreeMap<String, PublishedRemoteArtifact>,
    #[serde(default)]
    ui: BTreeMap<String, PublishedRemoteArtifact>,
}

#[derive(Debug, Deserialize)]
struct PublishedSignedRelease {
    url: String,
    signature_url: String,
}

#[derive(Debug, Deserialize)]
struct PublishedRemoteArtifact {
    url: String,
    checksum: String,
}

/// 单个插件的构建配置。纯 UI 插件（无 WASM）省略 protocol/wasm 字段，
/// 声明 sidecar 的插件（如 terminal）仍要求官方签名发布。
struct PluginConfig {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    protocol_crate: Option<&'static str>,
    wasm_crate: Option<&'static str>,
    wasm_artifact: Option<&'static str>,
    sidecar_crate: Option<&'static str>,
    sidecar_artifact: Option<&'static str>,
    plugin_root: &'static str,
    plugin_manifest: &'static str,
    protocol_manifest: Option<&'static str>,
}

const MEMORY: PluginConfig = PluginConfig {
    id: "memory",
    name: "Memory",
    description: "对话记忆、召回与数据管理",
    protocol_crate: Some("tiangong-plugin-memory-protocol"),
    wasm_crate: Some("tiangong-plugin-memory-wasm"),
    wasm_artifact: Some("tiangong_plugin_memory_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-memory-sidecar"),
    sidecar_artifact: Some("tiangong-memory-sidecar"),
    plugin_root: "plugins/tiangong-plugin-memory",
    plugin_manifest: "plugins/tiangong-plugin-memory/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-memory/protocol/Cargo.toml"),
};

const MCP: PluginConfig = PluginConfig {
    id: "mcp",
    name: "MCP",
    description: "MCP server 管理与工具桥接",
    protocol_crate: Some("tiangong-plugin-mcp-protocol"),
    wasm_crate: Some("tiangong-plugin-mcp-wasm"),
    wasm_artifact: Some("tiangong_plugin_mcp_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-mcp-sidecar"),
    sidecar_artifact: Some("tiangong-mcp-sidecar"),
    plugin_root: "plugins/tiangong-plugin-mcp",
    plugin_manifest: "plugins/tiangong-plugin-mcp/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-mcp/protocol/Cargo.toml"),
};

const FETCH: PluginConfig = PluginConfig {
    id: "fetch",
    name: "Fetch",
    description: "URL 获取（text 提取正文 / download 落盘），含 SSRF 防护",
    protocol_crate: Some("tiangong-plugin-fetch-protocol"),
    wasm_crate: Some("tiangong-plugin-fetch-wasm"),
    wasm_artifact: Some("tiangong_plugin_fetch_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-fetch-sidecar"),
    sidecar_artifact: Some("tiangong-fetch-sidecar"),
    plugin_root: "plugins/tiangong-plugin-fetch",
    plugin_manifest: "plugins/tiangong-plugin-fetch/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-fetch/protocol/Cargo.toml"),
};

const INDEX: PluginConfig = PluginConfig {
    id: "index",
    name: "Index",
    description: "工作区文件索引、对话历史索引与代码检索",
    protocol_crate: Some("tiangong-plugin-index-protocol"),
    wasm_crate: Some("tiangong-plugin-index-wasm"),
    wasm_artifact: Some("tiangong_plugin_index_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-index-sidecar"),
    sidecar_artifact: Some("tiangong-index-sidecar"),
    plugin_root: "plugins/tiangong-plugin-index",
    plugin_manifest: "plugins/tiangong-plugin-index/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-index/protocol/Cargo.toml"),
};

const SCHEDULER: PluginConfig = PluginConfig {
    id: "scheduler",
    name: "Scheduler",
    description: "定时任务调度与执行",
    protocol_crate: Some("tiangong-plugin-scheduler-protocol"),
    wasm_crate: Some("tiangong-plugin-scheduler-wasm"),
    wasm_artifact: Some("tiangong_plugin_scheduler_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-scheduler-sidecar"),
    sidecar_artifact: Some("tiangong-scheduler-sidecar"),
    plugin_root: "plugins/tiangong-plugin-scheduler",
    plugin_manifest: "plugins/tiangong-plugin-scheduler/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-scheduler/protocol/Cargo.toml"),
};

const SKILL: PluginConfig = PluginConfig {
    id: "skill",
    name: "Skill",
    description: "Skill 注册表管理、详情查询与 prompt 段落注入",
    protocol_crate: Some("tiangong-plugin-skill-protocol"),
    wasm_crate: Some("tiangong-plugin-skill-wasm"),
    wasm_artifact: Some("tiangong_plugin_skill_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-skill-sidecar"),
    sidecar_artifact: Some("tiangong-skill-sidecar"),
    plugin_root: "plugins/tiangong-plugin-skill",
    plugin_manifest: "plugins/tiangong-plugin-skill/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-skill/protocol/Cargo.toml"),
};

const CODING: PluginConfig = PluginConfig {
    id: "coding",
    name: "Coding",
    description: "通用开发工作流、项目上下文与交付审查",
    protocol_crate: Some("tiangong-plugin-coding-protocol"),
    wasm_crate: Some("tiangong-plugin-coding-wasm"),
    wasm_artifact: Some("tiangong_plugin_coding_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-coding-sidecar"),
    sidecar_artifact: Some("tiangong-coding-sidecar"),
    plugin_root: "plugins/tiangong-plugin-coding",
    plugin_manifest: "plugins/tiangong-plugin-coding/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-coding/protocol/Cargo.toml"),
};

const PROMPT: PluginConfig = PluginConfig {
    id: "prompt",
    name: "Prompt",
    description: "产品文案与自定义指令注入",
    protocol_crate: Some("tiangong-plugin-prompt-protocol"),
    wasm_crate: Some("tiangong-plugin-prompt-wasm"),
    wasm_artifact: Some("tiangong_plugin_prompt_wasm.wasm"),
    sidecar_crate: None,
    sidecar_artifact: None,
    plugin_root: "plugins/tiangong-plugin-prompt",
    plugin_manifest: "plugins/tiangong-plugin-prompt/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-prompt/protocol/Cargo.toml"),
};

const FS: PluginConfig = PluginConfig {
    id: "fs",
    name: "Fs",
    description: "基础文件工具（读写/补丁/目录树）+ 进程级文件锁表",
    protocol_crate: Some("tiangong-plugin-fs-protocol"),
    wasm_crate: Some("tiangong-plugin-fs-wasm"),
    wasm_artifact: Some("tiangong_plugin_fs_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-fs-sidecar"),
    sidecar_artifact: Some("tiangong-fs-sidecar"),
    plugin_root: "plugins/tiangong-plugin-fs",
    plugin_manifest: "plugins/tiangong-plugin-fs/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-fs/protocol/Cargo.toml"),
};

const COMMAND: PluginConfig = PluginConfig {
    id: "command",
    name: "Command",
    description: "基础命令执行（run_command/run_shell）+ 命令校验策略",
    protocol_crate: Some("tiangong-plugin-command-protocol"),
    wasm_crate: Some("tiangong-plugin-command-wasm"),
    wasm_artifact: Some("tiangong_plugin_command_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-command-sidecar"),
    sidecar_artifact: Some("tiangong-command-sidecar"),
    plugin_root: "plugins/tiangong-plugin-command",
    plugin_manifest: "plugins/tiangong-plugin-command/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-command/protocol/Cargo.toml"),
};

const COMPUTER_USE: PluginConfig = PluginConfig {
    id: "computer-use",
    name: "Computer Use",
    description: "跨平台桌面应用控制（Windows UI Automation / macOS AXUIElement / Linux AT-SPI2）",
    protocol_crate: Some("tiangong-plugin-computer-use-protocol"),
    wasm_crate: Some("tiangong-plugin-computer-use-wasm"),
    wasm_artifact: Some("tiangong_plugin_computer_use_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-computer-use-sidecar"),
    sidecar_artifact: Some("tiangong-computer-use-sidecar"),
    plugin_root: "plugins/tiangong-plugin-computer-use",
    plugin_manifest: "plugins/tiangong-plugin-computer-use/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-computer-use/protocol/Cargo.toml"),
};

const VOLCENGINE: PluginConfig = PluginConfig {
    id: "volcengine",
    name: "Volcengine",
    description: "火山引擎：图片/视频生成、语音合成与语音识别",
    protocol_crate: Some("tiangong-plugin-volcengine-protocol"),
    wasm_crate: Some("tiangong-plugin-volcengine-wasm"),
    wasm_artifact: Some("tiangong_plugin_volcengine_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-volcengine-sidecar"),
    sidecar_artifact: Some("tiangong-volcengine-sidecar"),
    plugin_root: "plugins/tiangong-plugin-volcengine",
    plugin_manifest: "plugins/tiangong-plugin-volcengine/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-volcengine/protocol/Cargo.toml"),
};

const GENERATE_IMAGE_OPENAI: PluginConfig = PluginConfig {
    id: "generate-image-openai",
    name: "Generate Image OpenAI",
    description: "Generate Image OpenAI",
    protocol_crate: Some("tiangong-plugin-generate-image-openai-protocol"),
    wasm_crate: Some("tiangong-plugin-generate-image-openai-wasm"),
    wasm_artifact: Some("tiangong_plugin_generate_image_openai_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-generate-image-openai-sidecar"),
    sidecar_artifact: Some("tiangong-generate-image-openai-sidecar"),
    plugin_root: "plugins/tiangong-plugin-generate-image-openai",
    plugin_manifest: "plugins/tiangong-plugin-generate-image-openai/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-generate-image-openai/protocol/Cargo.toml"),
};

const ANALYZE_ATTACHMENT: PluginConfig = PluginConfig {
    id: "analyze-attachment",
    name: "Analyze Attachment",
    description: "Analyze Attachment",
    protocol_crate: Some("tiangong-plugin-analyze-attachment-protocol"),
    wasm_crate: Some("tiangong-plugin-analyze-attachment-wasm"),
    wasm_artifact: Some("tiangong_plugin_analyze_attachment_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-analyze-attachment-sidecar"),
    sidecar_artifact: Some("tiangong-analyze-attachment-sidecar"),
    plugin_root: "plugins/tiangong-plugin-analyze-attachment",
    plugin_manifest: "plugins/tiangong-plugin-analyze-attachment/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-analyze-attachment/protocol/Cargo.toml"),
};

const SCREENSHOT_INPUT: PluginConfig = PluginConfig {
    id: "screenshot-input",
    name: "Screenshot Input",
    description: "跨平台区域截图并加入当前输入草稿",
    protocol_crate: Some("tiangong-plugin-screenshot-input-protocol"),
    wasm_crate: Some("tiangong-plugin-screenshot-input-wasm"),
    wasm_artifact: Some("tiangong_plugin_screenshot_input_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-screenshot-input-sidecar"),
    sidecar_artifact: Some("tiangong-screenshot-input-sidecar"),
    plugin_root: "plugins/screenshot-input",
    plugin_manifest: "plugins/screenshot-input/plugin.json",
    protocol_manifest: Some("plugins/screenshot-input/protocol/Cargo.toml"),
};

const INTERACTION: PluginConfig = PluginConfig {
    id: "interaction",
    name: "Interaction",
    description: "审批征询交互处理器：审批、确认、选择、输入与表单",
    protocol_crate: None,
    wasm_crate: None,
    wasm_artifact: None,
    sidecar_crate: None,
    sidecar_artifact: None,
    plugin_root: "plugins/tiangong-plugin-interaction",
    plugin_manifest: "plugins/tiangong-plugin-interaction/plugin.json",
    protocol_manifest: None,
};

const PLUGIN_CREATOR: PluginConfig = PluginConfig {
    id: "plugin-creator",
    name: "Plugin Creator",
    description: "插件创作：经 Agent 与 devkit 生成、构建与安装自建插件",
    protocol_crate: None,
    wasm_crate: None,
    wasm_artifact: None,
    sidecar_crate: None,
    sidecar_artifact: None,
    plugin_root: "plugins/tiangong-plugin-creator",
    plugin_manifest: "plugins/tiangong-plugin-creator/plugin.json",
    protocol_manifest: None,
};

const BROWSER: PluginConfig = PluginConfig {
    id: "browser",
    name: "Browser",
    description: "嵌入式浏览器：页面、标签与浏览工具（webview 引擎由宿主提供）",
    protocol_crate: None,
    wasm_crate: None,
    wasm_artifact: None,
    sidecar_crate: None,
    sidecar_artifact: None,
    plugin_root: "plugins/tiangong-plugin-browser",
    plugin_manifest: "plugins/tiangong-plugin-browser/plugin.json",
    protocol_manifest: None,
};

const TERMINAL: PluginConfig = PluginConfig {
    id: "terminal",
    name: "Terminal",
    description: "嵌入式终端：多标签、会话隔离与命令执行",
    protocol_crate: None,
    wasm_crate: None,
    wasm_artifact: None,
    sidecar_crate: Some("tiangong-plugin-terminal-sidecar"),
    sidecar_artifact: Some("tiangong-terminal-sidecar"),
    plugin_root: "plugins/tiangong-plugin-terminal",
    plugin_manifest: "plugins/tiangong-plugin-terminal/plugin.json",
    protocol_manifest: None,
};

const SUBAGENT: PluginConfig = PluginConfig {
    id: "subagent",
    name: "Subagent",
    description: "持久 Subagent 统一管理：身份、激活、任务运行与 Hook 反馈",
    protocol_crate: Some("tiangong-plugin-subagent-protocol"),
    wasm_crate: Some("tiangong-plugin-subagent-wasm"),
    wasm_artifact: Some("tiangong_plugin_subagent_wasm.wasm"),
    sidecar_crate: Some("tiangong-plugin-subagent-sidecar"),
    sidecar_artifact: Some("tiangong-subagent-sidecar"),
    plugin_root: "plugins/tiangong-plugin-subagent",
    plugin_manifest: "plugins/tiangong-plugin-subagent/plugin.json",
    protocol_manifest: Some("plugins/tiangong-plugin-subagent/protocol/Cargo.toml"),
};

fn plugin_ui_entries(config: &PluginConfig) -> &'static [&'static str] {
    match config.id {
        "screenshot-input" | "interaction" | "browser" | "terminal" | "plugin-creator"
        | "subagent" => &["dist/index.html"],
        // 手写自包含单文件 UI（无前端构建链）。
        "coding" => &["app/index.html"],
        "computer-use" => &["app/restore-window.html"],
        "volcengine" => &[
            "app/settings.html",
            "app/voice-input.html",
            "app/voice-overlay.html",
            "app/message-read.html",
            "app/voice-bubble.html",
            "app/auto-read.html",
        ],
        _ => &[],
    }
}

fn plugin_config(id: &str) -> io::Result<&'static PluginConfig> {
    match id {
        "memory" => Ok(&MEMORY),
        "mcp" => Ok(&MCP),
        "fetch" => Ok(&FETCH),
        "index" => Ok(&INDEX),
        "scheduler" => Ok(&SCHEDULER),
        "skill" => Ok(&SKILL),
        "coding" => Ok(&CODING),
        "prompt" => Ok(&PROMPT),
        "volcengine" => Ok(&VOLCENGINE),
        "generate-image-openai" => Ok(&GENERATE_IMAGE_OPENAI),
        "analyze-attachment" => Ok(&ANALYZE_ATTACHMENT),
        "fs" => Ok(&FS),
        "command" => Ok(&COMMAND),
        "computer-use" => Ok(&COMPUTER_USE),
        "screenshot-input" => Ok(&SCREENSHOT_INPUT),
        "interaction" => Ok(&INTERACTION),
        "plugin-creator" => Ok(&PLUGIN_CREATOR),
        "browser" => Ok(&BROWSER),
        "terminal" => Ok(&TERMINAL),
        "subagent" => Ok(&SUBAGENT),
        other => Err(invalid_input(format!("暂不支持插件: {other}"))),
    }
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match args.as_slice() {
        [command, plugin] if command == "build-plugin" => match plugin_config(plugin) {
            Ok(config) => build_plugin(config),
            Err(error) => Err(error),
        },
        [command, plugin, output] if command == "build-plugin-wasm" => {
            match plugin_config(plugin) {
                Ok(config) => build_plugin_wasm(config, output),
                Err(error) => Err(error),
            }
        }
        [command, plugin] if command == "validate-plugin" => match plugin_config(plugin) {
            Ok(config) => validate_plugin(config),
            Err(error) => Err(error),
        },
        [command, directory] if command == "sign-plugin" => sign_plugin(Path::new(directory)),
        [command, key_path] if command == "generate-plugin-test-key" => {
            generate_plugin_test_key(Path::new(key_path))
        }
        [command, release_path] if command == "verify-official-release" => {
            verify_official_release(Path::new(release_path))
        }
        [command, input, output] if command == "merge-plugin-dist" => {
            merge_plugin_distributions(Path::new(input), Path::new(output), None)
        }
        [command, plugin, input, output] if command == "merge-plugin-dist" => plugin_config(plugin)
            .and_then(|_| {
                merge_plugin_distributions(Path::new(input), Path::new(output), Some(plugin))
            }),
        [command, current, plugin_release, output] if command == "merge-plugin-catalog" => {
            merge_plugin_catalog(
                Path::new(current),
                Path::new(plugin_release),
                Path::new(output),
            )
        }
        [command, catalog] if command == "validate-plugin-catalog" => {
            validate_plugin_catalog_file(Path::new(catalog))
        }
        [command, plugin_id] if command == "new-plugin" => new_plugin(plugin_id, None),
        [command, plugin_id, output_dir] if command == "new-plugin" => {
            new_plugin(plugin_id, Some(output_dir))
        }
        [command] if command == "sign-sandbox" => sign_sandbox_debug(),
        [command] if command == "prepare-sandbox-release" => prepare_sandbox_release(),
        [command, artifact, signature] if command == "verify-sandbox-artifact" => {
            verify_sandbox_artifact(artifact, signature)
        }
        [command] if command == "build-wasm" || command == "build-sidecar" => {
            eprintln!("[xtask] {command} 已合并到 build-plugin <id>");
            Err(invalid_input("请使用 build-plugin <id>"))
        }
        [command] if command == "help" || command == "--help" || command == "-h" => {
            print_help();
            Ok(())
        }
        [] => {
            print_help();
            Ok(())
        }
        _ => {
            print_help();
            Err(invalid_input("参数无效"))
        }
    };

    if let Err(error) = result {
        eprintln!("xtask 失败: {error}");
        std::process::exit(1);
    }
}

fn print_help() {
    eprintln!("xtask - 天工辅助构建任务\n");
    eprintln!("用法:");
    eprintln!("  cargo run -p xtask -- validate-plugin <id>");
    eprintln!("  cargo run -p xtask -- build-plugin-wasm <id> <输出WASM>");
    eprintln!("  cargo run -p xtask -- build-plugin <id>");
    eprintln!("  cargo run -p xtask -- sign-plugin <插件包目录>");
    eprintln!("  cargo run -p xtask -- generate-plugin-test-key <私钥路径>");
    eprintln!("  cargo run -p xtask -- verify-official-release <release.json路径>");
    eprintln!("  cargo run -p xtask -- merge-plugin-dist [plugin-id] <输入目录> <输出目录>");
    eprintln!(
        "  cargo run -p xtask -- merge-plugin-catalog <当前catalog或-> <插件release> <输出catalog>"
    );
    eprintln!("  cargo run -p xtask -- validate-plugin-catalog <catalog或->");
    eprintln!("  cargo run -p xtask -- prepare-sandbox-release");
    eprintln!("  cargo run -p xtask -- verify-sandbox-artifact <制品> <签名>");
    eprintln!("  cargo run -p xtask -- sign-sandbox");
}

fn validate_plugin(config: &PluginConfig) -> io::Result<()> {
    let workspace_root = workspace_root();
    validate_versions(&workspace_root, config)?;
    eprintln!("[xtask] validation passed");
    Ok(())
}

fn build_plugin_wasm(config: &PluginConfig, output: &str) -> io::Result<()> {
    let Some(wasm_crate) = config.wasm_crate else {
        return Err(invalid_input(format!("插件 {} 无 WASM 制品", config.id)));
    };
    let protocol_crate = config.protocol_crate.expect("无 WASM 的插件必有 protocol");
    let workspace_root = workspace_root();
    validate_versions(&workspace_root, config)?;
    eprintln!("[xtask] CI build-plugin-wasm");
    run_cargo(&workspace_root, &["check", "-p", protocol_crate])?;
    run_cargo(
        &workspace_root,
        &["check", "-p", protocol_crate, "--target", WASM_TARGET],
    )?;
    run_cargo(
        &workspace_root,
        &[
            "build",
            "-p",
            wasm_crate,
            "--target",
            WASM_TARGET,
            "--release",
        ],
    )?;
    eprintln!("[xtask] WASM check ok");
    let wasm_artifact = config
        .wasm_artifact
        .expect("wasm_crate 存在时必有 artifact");
    let wasm = workspace_root
        .join("target")
        .join(WASM_TARGET)
        .join("release")
        .join(wasm_artifact);
    require_file(&wasm)?;
    let dest = Path::new(output);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&wasm, dest)?;
    eprintln!("[xtask] WASM copied to: {}", dest.display());
    Ok(())
}

fn build_plugin(config: &PluginConfig) -> io::Result<()> {
    let workspace_root = workspace_root();
    validate_versions(&workspace_root, config)?;

    let plugin_name = config.name;
    if let Some(protocol_crate) = config.protocol_crate {
        eprintln!("[xtask] 检查 {plugin_name} 私有协议（native）...");
        run_cargo(&workspace_root, &["check", "-p", protocol_crate])?;
        eprintln!("[xtask] 检查 {plugin_name} 私有协议（{WASM_TARGET}）...");
        run_cargo(
            &workspace_root,
            &["check", "-p", protocol_crate, "--target", WASM_TARGET],
        )?;
    }
    let wasm: Option<PathBuf> = match config.wasm_artifact {
        Some(wasm_artifact) => {
            let wasm = match non_empty_env_os("TIANGONG_PLUGIN_PREBUILT_WASM").map(PathBuf::from) {
                Some(path) => {
                    eprintln!("[xtask] using prebuilt wasm");
                    require_file(&path)?;
                    path
                }
                None => {
                    eprintln!("[xtask] build wasm...");
                    let wasm_crate = config.wasm_crate.expect("wasm_artifact 存在时必有 crate");
                    run_cargo(
                        &workspace_root,
                        &[
                            "build",
                            "-p",
                            wasm_crate,
                            "--target",
                            WASM_TARGET,
                            "--release",
                        ],
                    )?;
                    let p = workspace_root
                        .join("target")
                        .join(WASM_TARGET)
                        .join("release")
                        .join(wasm_artifact);
                    require_file(&p)?;
                    p
                }
            };
            Some(wasm)
        }
        None => {
            eprintln!("[xtask] {plugin_name} 无 WASM 制品，跳过 WASM 构建");
            None
        }
    };

    let plugins_dir = storage_root().join("plugins");
    std::fs::create_dir_all(&plugins_dir)?;
    let staging = plugins_dir.join(format!(".{}-staging-{}", config.id, std::process::id()));
    let destination = plugins_dir.join(config.id);
    // 部署中断会留下旧 staging（含清单，会被 App 误读为已装版本），
    // 每次部署前清掉同插件的全部历史 staging（含本进程旧目录）。
    let staging_prefix = format!(".{}-staging-", config.id);
    for entry in std::fs::read_dir(&plugins_dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&staging_prefix)
        {
            remove_dir_if_exists(&entry.path())?;
        }
    }
    std::fs::create_dir_all(&staging)?;

    if let (Some(wasm), Some(wasm_artifact)) = (&wasm, config.wasm_artifact) {
        std::fs::copy(wasm, staging.join(wasm_artifact))?;
    }

    // sidecar 构建和复制（无 sidecar 的插件跳过）。
    if let (Some(sidecar_crate), Some(sidecar_artifact)) =
        (config.sidecar_crate, config.sidecar_artifact)
    {
        let sidecar = match non_empty_env_os("TIANGONG_PLUGIN_PREBUILT_SIDECAR").map(PathBuf::from)
        {
            Some(path) => {
                eprintln!("[xtask] using prebuilt sidecar");
                require_file(&path)?;
                path
            }
            None => {
                eprintln!("[xtask] 构建 {plugin_name} sidecar...");
                run_cargo(
                    &workspace_root,
                    &["build", "-p", sidecar_crate, "--release"],
                )?;
                let path = workspace_root.join("target").join("release").join(format!(
                    "{sidecar_artifact}{}",
                    std::env::consts::EXE_SUFFIX
                ));
                require_file(&path)?;
                path
            }
        };
        let staged_sidecar = staging.join(format!(
            "{sidecar_artifact}{}",
            std::env::consts::EXE_SUFFIX
        ));
        std::fs::copy(&sidecar, &staged_sidecar)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged_sidecar, std::fs::Permissions::from_mode(0o755))?;
        }
    }

    std::fs::copy(
        workspace_root.join(config.plugin_manifest),
        staging.join("plugin.json"),
    )?;
    stage_plugin_ui(&workspace_root, &staging, config)?;
    stage_interpreter_sidecar(&workspace_root, &staging, config)?;
    for directory in PRESERVED_DIRS {
        std::fs::create_dir_all(staging.join(directory))?;
    }

    if let Some(wasm_artifact) = config.wasm_artifact {
        eprintln!(
            "[xtask] WASM sha256: {}",
            sha256(&staging.join(wasm_artifact))?
        );
    }
    if let Some(sidecar_artifact) = config.sidecar_artifact {
        let staged_sidecar = staging.join(format!(
            "{sidecar_artifact}{}",
            std::env::consts::EXE_SUFFIX
        ));
        eprintln!("[xtask] sidecar sha256: {}", sha256(&staged_sidecar)?);
    }
    generate_oss_distribution(&workspace_root, &staging, config)?;
    // 部署守卫：仅官方签名形态部署到本机（官方开发者本地即官方形态，
    // App 端内置公钥可直接验证）。非官方密钥（测试密钥等）签名的 staging
    // 验签不过——产物照常生成，部署跳过并警告，避免本机出现必然无效
    // 的插件目录。发布 CI 以 verify-official-release 强制校验正式私钥
    // 与内置公钥匹配。
    if staging_passes_official_verification(&staging)? {
        deploy_atomically(&staging, &destination, config)?;
        eprintln!(
            "[xtask] {plugin_name} 插件已部署到: {}",
            destination.display()
        );
    } else {
        eprintln!(
            "[xtask] 跳过部署：staging 签名未通过官方公钥验证（使用非官方密钥？）\n      \
             发布产物不受影响；如需本机部署请以官方私钥（TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH）运行"
        );
    }
    // 部署成功时 staging 已被原子改名带走（此处 no-op）；守卫跳过或任意
    // 失败路径统一收尾清理，不在插件目录残留暂存。
    remove_dir_if_exists(&staging)?;
    Ok(())
}

fn stage_plugin_ui(workspace_root: &Path, staging: &Path, config: &PluginConfig) -> io::Result<()> {
    let entries = plugin_ui_entries(config);
    if entries.is_empty() {
        return Ok(());
    }

    // CI 对无 UI 插件会传空串占位，空值视为未设置。
    let prebuilt = non_empty_env_os("TIANGONG_PLUGIN_PREBUILT_UI").map(PathBuf::from);
    // 无 package.json 的插件使用手写自包含入口，无前端构建链，直接拷贝。
    let has_package_json = workspace_root
        .join(config.plugin_root)
        .join("package.json")
        .is_file();
    if prebuilt.is_none() && has_package_json {
        let plugin_root = workspace_root.join(config.plugin_root);
        eprintln!("[xtask] 安装并构建 {} UI...", config.name);
        run_yarn(&plugin_root, &["install", "--frozen-lockfile"])?;
        run_yarn(&plugin_root, &["build"])?;
    } else if entries.len() != 1 && prebuilt.is_some() {
        return Err(invalid_input(
            "TIANGONG_PLUGIN_PREBUILT_UI 仅支持单入口 UI 插件",
        ));
    }

    for entry in entries {
        let source = prebuilt
            .as_ref()
            .cloned()
            .unwrap_or_else(|| workspace_root.join(config.plugin_root).join(entry));
        require_file(&source)?;
        let destination = staging.join(entry);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &destination)?;
    }
    Ok(())
}

/// 生成插件签名发布清单（release.json）并对它做 minisign 数字签名。
///
/// 清单结构与运行时 `signature.rs::SignedPluginRelease` 对齐，覆盖
/// schema、id、version、publisher、permissions 以及 plugin.json、WASM
/// UI 入口和（若有）sidecar 的 sha256。签名失败直接报错，绝不写空签名文件。
fn write_signed_release(plugin: &Path, config: &PluginConfig) -> io::Result<()> {
    let manifest_path = plugin.join("plugin.json");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)
        .map_err(|error| invalid_data(format!("解析 plugin.json 失败: {error}")))?;
    let version = manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_data("plugin.json 缺少 version"))?;
    let permissions = manifest
        .get("permissions")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));

    let mut release = serde_json::json!({
        "schema_version": 1,
        "id": config.id,
        "version": version,
        "publisher": "tiangong-official",
        "permissions": permissions,
        "manifest": {
            "path": "plugin.json",
            "sha256": sha256(&manifest_path)?,
        },
    });

    // WASM 条目（纯 UI/sidecar 插件无 wasm 制品，签名清单省略，
    // 与运行时 SignedPluginRelease 的可选 wasm 字段对齐）。
    if let Some(wasm_artifact) = config.wasm_artifact {
        release["wasm"] = serde_json::json!({
            "path": wasm_artifact,
            "sha256": sha256(&plugin.join(wasm_artifact))?,
        });
    }

    // sidecar 声明（无 sidecar 的插件跳过，保持清单与 plugin.json 一致）。
    // 解释器形态：签名锚定完整内容清单（覆盖全树），不签单个二进制。
    let manifest_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(plugin.join("plugin.json"))?)
            .map_err(|error| invalid_data(format!("解析 plugin.json 失败: {error}")))?;
    let runtime = (manifest_value.get("sidecar").cloned().unwrap_or_default())
        .get("runtime")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("native")
        .to_string();
    if runtime != "native" {
        release["content_manifest"] = serde_json::json!({
            "path": "content-manifest.json",
            "sha256": sha256(&plugin.join("content-manifest.json"))?,
        });
    } else if let Some(sidecar_artifact) = config.sidecar_artifact {
        let sidecar_name = format!("{sidecar_artifact}{}", std::env::consts::EXE_SUFFIX);
        release["sidecar"] = serde_json::json!({
            "path": sidecar_name,
            "sha256": sha256(&plugin.join(&sidecar_name))?,
        });
    }
    let ui = plugin_ui_entries(config)
        .iter()
        .map(|entry| {
            Ok(serde_json::json!({
                "path": entry,
                "sha256": sha256(&plugin.join(entry))?,
            }))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if !ui.is_empty() {
        release["ui"] = serde_json::Value::Array(ui);
    }

    let release_path = plugin.join("release.json");
    write_json(&release_path, &release)?;

    let key_path = std::env::var_os("TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            invalid_input("缺少插件签名私钥，请设置 TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH")
        })?;
    let password =
        std::env::var("TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD").unwrap_or_default();
    sign_file_minisign(&key_path, &password, &release_path)?;
    eprintln!("[xtask] release.json 已签名");
    Ok(())
}

/// 用 minisign 私钥对文件签名，落盘「签名文本整体 base64」格式的 `.sig`
/// （运行时 verify_minisign 读取格式）。私钥兼容 tauri signer 生成的
/// minisign 密钥（含密码加密形态），签出的制品与 `cargo tauri signer sign`
/// 完全同构，本地发布不再依赖 tauri-cli。
fn sign_file_minisign(key_path: &Path, password: &str, content_path: &Path) -> io::Result<()> {
    let raw = std::fs::read_to_string(key_path)
        .map_err(|error| invalid_input(format!("读取插件签名私钥失败: {error}")))?;
    let key_text = normalize_signing_key_text(&raw)?;
    // 双路径加载：未加密密钥（generate-plugin-test-key 产物）走非交互的
    // 未加密通道；加密密钥（tauri signer / 正式密钥）以显式密码解密——
    // 传 Some 会拒绝未加密密钥（"Key is not encrypted"），传 None 会对
    // 加密密钥触发交互提示（CI 卡死），因此按先未加密后加密的顺序尝试。
    let secret_key = minisign::SecretKeyBox::from_string(&key_text)
        .and_then(|secret_key_box| secret_key_box.into_unencrypted_secret_key())
        .or_else(|_| {
            minisign::SecretKeyBox::from_string(&key_text)?
                .into_secret_key(Some(password.to_string()))
        })
        .map_err(|error| {
            invalid_input(format!(
                "加载插件签名私钥失败（未加密密钥不适用或密码错误）: {error}"
            ))
        })?;
    let public_key = minisign::PublicKey::from_secret_key(&secret_key)
        .map_err(|error| invalid_data(format!("推导插件签名公钥失败: {error}")))?;
    let content = std::fs::read(content_path)?;
    let signature = minisign::sign(
        Some(&public_key),
        &secret_key,
        content.as_slice(),
        None,
        None,
    )
    .map_err(|error| invalid_data(format!("插件签名生成失败: {error}")))?;
    use base64::Engine;
    let signature_b64 = base64::engine::general_purpose::STANDARD.encode(signature.into_string());
    let mut signature_path = content_path.as_os_str().to_os_string();
    signature_path.push(".sig");
    std::fs::write(PathBuf::from(signature_path), signature_b64)?;
    Ok(())
}

/// 私钥文件形态归一：标准 minisign 密钥是两行文本；tauri signer 生成的
/// 密钥文件是整体 base64 包装的两行文本（与其 .pub 一致）。两种都接受。
fn normalize_signing_key_text(raw: &str) -> io::Result<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("untrusted comment") {
        return Ok(trimmed.to_string());
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|_| {
            invalid_input("插件签名私钥格式无效（期望 minisign 密钥文本或其 base64 包装）")
        })?;
    let text = String::from_utf8(decoded)
        .map_err(|_| invalid_input("插件签名私钥 base64 内容不是有效 UTF-8"))?;
    if !text.trim().starts_with("untrusted comment") {
        return Err(invalid_input(
            "插件签名私钥内容无效（缺少 minisign 注释头）",
        ));
    }
    Ok(text.trim().to_string())
}

/// 生成插件签名测试密钥对（CI 端到端验证用）：`generate-plugin-test-key
/// <私钥路径>`，公钥落在 `<路径>.pub`。密钥为 minisign 格式、无密码；
/// `.pub` 内容为 base64(公钥文本)——与 tauri signer 公钥文件及运行时
/// 运行时公钥环境格式一致（运行时官方信任根为内置公钥，测试密钥仅用于
/// 局部签名闭环与第三方信任链测试，不可被识别为官方密钥）。
fn generate_plugin_test_key(key_path: &Path) -> io::Result<()> {
    let keypair = minisign::KeyPair::generate_unencrypted_keypair()
        .map_err(|error| invalid_data(format!("生成测试密钥失败: {error}")))?;
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        key_path,
        keypair
            .sk
            .to_box(None)
            .map_err(invalid_data_string)?
            .into_string(),
    )?;
    let mut public_path = key_path.as_os_str().to_os_string();
    public_path.push(".pub");
    use base64::Engine;
    std::fs::write(
        PathBuf::from(public_path),
        base64::engine::general_purpose::STANDARD.encode(
            keypair
                .pk
                .to_box()
                .map_err(invalid_data_string)?
                .into_string(),
        ),
    )?;
    eprintln!("[xtask] 测试密钥已生成: {}", key_path.display());
    Ok(())
}

fn invalid_data_string(error: minisign::PError) -> io::Error {
    invalid_data(format!("序列化密钥失败: {error}"))
}

/// 为本地开发插件包生成签名发布清单：`sign-plugin <插件包目录>`。
///
/// 与 CI 的 `build-plugin` 同构：生成 release.json 后用 tauri signer
/// （官方私钥）签名，签出的制品可被运行时内置官方公钥直接验证。
/// 需要设置 `TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH` 指向本地官方私钥，
/// 私钥有密码时再设 `TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD`。
fn sign_plugin(directory: &Path) -> io::Result<()> {
    let manifest_path = directory.join("plugin.json");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)
        .map_err(|error| invalid_data(format!("解析 plugin.json 失败: {error}")))?;
    let plugin_id = manifest
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_input("plugin.json 缺少 id"))?;
    if manifest.get("sidecar").is_none() {
        return Err(invalid_input(format!(
            "插件 {plugin_id} 未声明 sidecar，无需签名清单"
        )));
    }

    let mut release = serde_json::json!({
        "schema_version": 1,
        "id": plugin_id,
        "version": manifest.get("version").cloned().unwrap_or(serde_json::json!("")),
        "publisher": "tiangong-official",
        "permissions": manifest.get("permissions").cloned().unwrap_or_else(|| serde_json::json!([])),
        "manifest": {
            "path": "plugin.json",
            "sha256": sha256(&manifest_path)?,
        },
    });
    if let Some(wasm_binary) = manifest
        .get("wasm")
        .and_then(|wasm| wasm.get("binary"))
        .and_then(serde_json::Value::as_str)
    {
        let wasm_path = directory.join(wasm_binary);
        if wasm_path.is_file() {
            release["wasm"] = serde_json::json!({
                "path": wasm_binary,
                "sha256": sha256(&wasm_path)?,
            });
        }
    }
    if let Some(binary) = manifest
        .get("sidecar")
        .and_then(|sidecar| sidecar.get("binary"))
        .and_then(serde_json::Value::as_str)
    {
        let sidecar_path = directory.join(binary);
        release["sidecar"] = serde_json::json!({
            "path": binary,
            "sha256": sha256(&sidecar_path)?,
        });
    }
    // UI 入口制品与 build-plugin/CI 同格式：verify 要求签名清单的 ui
    // 条目与 plugin.json 的 ui.contributions[].entry 完全一致。
    let ui_entries = manifest
        .get("ui")
        .and_then(|ui| ui.get("contributions"))
        .and_then(|contribs| contribs.as_array())
        .map(|contribs| {
            contribs
                .iter()
                .filter_map(|contribution| {
                    contribution
                        .get("entry")
                        .and_then(serde_json::Value::as_str)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let ui = ui_entries
        .iter()
        .map(|entry| {
            Ok(serde_json::json!({
                "path": entry,
                "sha256": sha256(&directory.join(entry))?,
            }))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if !ui.is_empty() {
        release["ui"] = serde_json::Value::Array(ui);
    }
    let release_path = directory.join("release.json");
    write_json(&release_path, &release)?;

    // 与 write_signed_release 一致：minisign 库直签（base64 包装的签名文本，
    // 即运行时 verify_minisign 读取的格式）。
    let key_path = std::env::var_os("TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            invalid_input(
                "缺少插件签名私钥：请设置 TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH \
                 指向本地官方私钥（有密码时另设 TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD）",
            )
        })?;
    let password =
        std::env::var("TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD").unwrap_or_default();
    sign_file_minisign(&key_path, &password, &release_path)?;
    eprintln!("[xtask] release.json 已签名（key: {}）", key_path.display());
    Ok(())
}

/// 解释器插件的官方发布：单一 tar.zst 归档（含全部受管文件与签名清单）
/// 置于版本目录根（平台无关）；目录条目 sidecars.any 指向归档、
/// signed_releases.any 指向签名；fragment 为 <id>-any.json。
#[allow(clippy::too_many_arguments)]
fn generate_interpreter_distribution(
    _workspace_root: &Path,
    plugin: &Path,
    config: &PluginConfig,
    _manifest: &serde_json::Value,
    version: &str,
    base_url: &str,
) -> io::Result<()> {
    let release_url = format!("{base_url}/plugins/{}/{}", config.id, version);
    let dist_root = _workspace_root.join(PLUGIN_DIST);
    let release_root = dist_root.join("plugins").join(config.id).join(version);
    let index_root = dist_root.join("plugins-index");
    std::fs::create_dir_all(&release_root)?;
    std::fs::create_dir_all(index_root.join("fragments"))?;

    // 独立清单（目录发现用）与签名清单复制到版本目录根。
    std::fs::copy(plugin.join("plugin.json"), release_root.join("plugin.json"))?;
    for file in ["release.json", "release.json.sig"] {
        std::fs::copy(plugin.join(file), release_root.join(file)).map_err(|error| {
            invalid_data(format!(
                "缺少 {file}（先经 write_signed_release 签名）：{error}"
            ))
        })?;
    }
    // 确定性归档（排除本地信任标记、运行时目录与签名文件）。
    // 幂等保护：同版本归档已存在则跳过（多平台 CI 产物合并语义）。
    let archive_name = format!("{}-{}.tar.zst", config.id, version);
    let archive_path = release_root.join(&archive_name);
    if !archive_path.exists() {
        create_plugin_archive(plugin, &archive_path)?;
    }
    let archive_checksum = format!("sha256:{}", sha256(&archive_path)?);

    let manifest_checksum = format!("sha256:{}", sha256(&plugin.join("plugin.json"))?);
    let release = serde_json::json!({
        "id": config.id,
        "name": config.name,
        "version": version,
        "description": config.description,
        "manifest": {
            "url": format!("{release_url}/plugin.json"),
            "checksum": manifest_checksum,
        },
        "sidecars": {
            "any": {
                "url": format!("{release_url}/{archive_name}"),
                "checksum": archive_checksum,
            }
        },
        "ui": {},
        "signed_releases": {
            "any": {
                "url": format!("{release_url}/release.json"),
                "signature_url": format!("{release_url}/release.json.sig"),
            }
        },
    });
    write_json(
        &index_root.join("catalog.json"),
        &serde_json::json!({"version": 1, "plugins": [release.clone()]}),
    )?;
    write_json(
        &index_root
            .join("fragments")
            .join(format!("{}-any.json", config.id)),
        &release,
    )?;
    let mut checksums = format!(
        "{}  release.json\n{}  release.json.sig\n{}  {}\n",
        sha256(&release_root.join("release.json"))?,
        sha256(&release_root.join("release.json.sig"))?,
        sha256(&archive_path)?,
        archive_name,
    );
    checksums.push_str(&format!(
        "{}  plugin.json\n",
        sha256(&plugin.join("plugin.json"))?
    ));
    std::fs::write(release_root.join("SHA256SUMS"), checksums)?;
    eprintln!(
        "[xtask] 解释器插件 {} 归档已生成: {}",
        config.id,
        archive_path.display()
    );
    Ok(())
}

/// 生成确定性插件归档（tar.zst）：条目 mtime/uid/gid/mode 固定，同内容必得
/// 同哈希——多平台 CI 各自生成可幂等合并。排除本地信任标记与打包元数据
///（官方签名与本地信任不混用；归档内容为发布受管文件全集，含签名清单）。
fn create_plugin_archive(source: &Path, destination: &Path) -> io::Result<()> {
    let file = std::fs::File::create(destination)?;
    let zstd_encoder = zstd::Encoder::new(file, 19)?;
    let mut builder = tar::Builder::new(zstd_encoder);
    let mut stack = vec![source.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if directory == source
                && matches!(
                    name.as_ref(),
                    "local-trust.json"
                        | ".package-info"
                        | "runtime"
                        | "logs"
                        | "data"
                        // 签名非确定（minisign 随机 nonce），独立于归档存在——
                        // 归档保持内容确定性，多平台产物可幂等合并。
                        | "release.json"
                        | "release.json.sig"
                )
            {
                continue;
            }
            if entry.file_type()?.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(source)
                .map_err(|error| invalid_data(format!("归档相对路径推算失败: {error}")))?;
            let mut header = tar::Header::new_gnu();
            header.set_size(entry.metadata()?.len());
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            builder.append_data(&mut header, relative, std::fs::File::open(&path)?)?;
        }
    }
    let zstd_encoder = builder
        .into_inner()
        .map_err(|error| io::Error::other(format!("归档写入失败: {error}")))?;
    zstd_encoder
        .finish()
        .map_err(|error| io::Error::other(format!("归档压缩失败: {error}")))?;
    Ok(())
}

fn generate_oss_distribution(
    workspace_root: &Path,
    plugin: &Path,
    config: &PluginConfig,
) -> io::Result<()> {
    // 只有带 sidecar 的插件才生成签名发布清单：签名用于建立 sidecar 信任边界，
    // 纯 WASM 插件（如 prompt）不需要 sidecar，运行时不强制要求签名。
    // 签名触发：native sidecar 插件与解释器插件（官方信任根，解释器签名
    // 锚定内容清单）。纯 WASM/UI 插件不携带签名。
    let manifest_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(plugin.join("plugin.json"))?)
            .map_err(|error| invalid_data(format!("解析 plugin.json 失败: {error}")))?;
    let interpreter_release = manifest_value
        .get("sidecar")
        .and_then(|sidecar| sidecar.get("runtime"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|runtime| runtime != "native");
    let has_sidecar = config.sidecar_artifact.is_some() || interpreter_release;
    if has_sidecar {
        write_signed_release(plugin, config)?;
    }

    let manifest_path = plugin.join("plugin.json");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)
        .map_err(|error| invalid_data(format!("解析 {} 失败: {error}", manifest_path.display())))?;
    let version = manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_data("plugin.json 缺少 version"))?;
    let base_url = env_var_or("TIANGONG_PLUGIN_OSS_BASE_URL", DEFAULT_OSS_BASE_URL)
        .trim_end_matches('/')
        .to_string();
    let release_url = format!("{base_url}/plugins/{}/{}", config.id, version);
    let platform = current_platform_key();
    // 解释器 sidecar 插件：发布形态为单一确定性归档（平台无关，目录键 any）；
    // 官方资格由 plugin_config 白名单决定（build-plugin 仅接受登记插件）。
    let interpreter_runtime = manifest
        .get("sidecar")
        .and_then(|sidecar| sidecar.get("runtime"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|runtime| runtime != "native");
    if interpreter_runtime {
        return generate_interpreter_distribution(
            workspace_root,
            plugin,
            config,
            &manifest,
            version,
            &base_url,
        );
    }
    let dist_root = workspace_root.join(PLUGIN_DIST);
    let release_root = dist_root.join("plugins").join(config.id).join(version);
    let platform_root = release_root.join(&platform);
    let index_root = dist_root.join("plugins-index");
    std::fs::create_dir_all(&platform_root)?;
    std::fs::create_dir_all(index_root.join("fragments"))?;

    let dist_manifest = release_root.join("plugin.json");
    std::fs::copy(&manifest_path, &dist_manifest)?;
    if let Some(wasm_artifact) = config.wasm_artifact {
        std::fs::copy(plugin.join(wasm_artifact), release_root.join(wasm_artifact))?;
    }
    let mut ui_artifacts = serde_json::Map::new();
    for entry in plugin_ui_entries(config) {
        let destination = release_root.join(entry);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(plugin.join(entry), &destination)?;
        ui_artifacts.insert(
            (*entry).to_string(),
            serde_json::json!({
                "url": format!("{release_url}/{entry}"),
                "checksum": format!("sha256:{}", sha256(&destination)?),
            }),
        );
    }
    // 签名清单与签名文件复制到分平台目录，供运行时验签（仅 sidecar 插件）。
    if has_sidecar {
        std::fs::copy(
            plugin.join("release.json"),
            platform_root.join("release.json"),
        )?;
        std::fs::copy(
            plugin.join("release.json.sig"),
            platform_root.join("release.json.sig"),
        )?;
    }

    // sidecar 分发（无 sidecar 的插件跳过）。
    let sidecar_entry = if let Some(sidecar_artifact) = config.sidecar_artifact {
        let dist_sidecar = platform_root.join(format!(
            "{sidecar_artifact}{}",
            std::env::consts::EXE_SUFFIX
        ));
        std::fs::copy(
            plugin.join(format!(
                "{sidecar_artifact}{}",
                std::env::consts::EXE_SUFFIX
            )),
            &dist_sidecar,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dist_sidecar, std::fs::Permissions::from_mode(0o755))?;
        }
        let sidecar_checksum = format!("sha256:{}", sha256(&dist_sidecar)?);
        Some(serde_json::json!({
            platform.clone(): {
                "url": format!(
                    "{release_url}/{platform}/{}{}",
                    sidecar_artifact,
                    std::env::consts::EXE_SUFFIX
                ),
                "checksum": sidecar_checksum,
            }
        }))
    } else {
        None
    };

    let manifest_checksum = format!("sha256:{}", sha256(&dist_manifest)?);
    let mut release = serde_json::json!({
        "id": config.id,
        "name": config.name,
        "version": version,
        "description": config.description,
        "manifest": {
            "url": format!("{release_url}/plugin.json"),
            "checksum": manifest_checksum,
        },
        "sidecars": sidecar_entry.unwrap_or_else(|| serde_json::json!({})),
        "ui": ui_artifacts,
    });
    // WASM 条目（纯 UI 插件无 wasm 制品，目录条目省略；
    // 客户端按 plugin.json 是否声明 wasm 决定下载）。
    if let Some(wasm_artifact) = config.wasm_artifact {
        let dist_wasm = release_root.join(wasm_artifact);
        release["wasm"] = serde_json::json!({
            "url": format!("{release_url}/{}", wasm_artifact),
            "checksum": format!("sha256:{}", sha256(&dist_wasm)?),
        });
    }
    // 签名清单仅对 sidecar 插件生成，纯 WASM 插件不携带签名。
    if has_sidecar {
        release["signed_releases"] = serde_json::json!({
            platform.clone(): {
                "url": format!("{release_url}/{platform}/release.json"),
                "signature_url": format!("{release_url}/{platform}/release.json.sig"),
            }
        });
    }
    write_json(
        &index_root.join("catalog.json"),
        &serde_json::json!({"version": 1, "plugins": [release.clone()]}),
    )?;
    write_json(
        &index_root
            .join("fragments")
            .join(format!("{}-{platform}.json", config.id)),
        &release,
    )?;

    let mut checksums = format!("{}  plugin.json\n", sha256(&dist_manifest)?);
    if let Some(wasm_artifact) = config.wasm_artifact {
        checksums.push_str(&format!(
            "{}  {}\n",
            sha256(&release_root.join(wasm_artifact))?,
            wasm_artifact,
        ));
    }
    if let Some(sidecar_artifact) = config.sidecar_artifact {
        let dist_sidecar = platform_root.join(format!(
            "{sidecar_artifact}{}",
            std::env::consts::EXE_SUFFIX
        ));
        checksums.push_str(&format!(
            "{}  {}/{}{}\n",
            sha256(&dist_sidecar)?,
            platform,
            sidecar_artifact,
            std::env::consts::EXE_SUFFIX,
        ));
    }
    for entry in plugin_ui_entries(config) {
        checksums.push_str(&format!(
            "{}  {}\n",
            sha256(&release_root.join(entry))?,
            entry,
        ));
    }
    std::fs::write(
        release_root.join(format!("SHA256SUMS-{platform}")),
        checksums,
    )?;

    eprintln!("[xtask] OSS 制品已生成: {}", dist_root.display());
    Ok(())
}

fn write_json(path: &Path, value: &serde_json::Value) -> io::Result<()> {
    let mut content = serde_json::to_vec_pretty(value)
        .map_err(|error| invalid_data(format!("生成 {} 失败: {error}", path.display())))?;
    content.push(b'\n');
    std::fs::write(path, content)
}

fn validate_plugin_catalog_file(path: &Path) -> io::Result<()> {
    let mut content = Vec::new();
    if path == Path::new("-") {
        io::stdin().read_to_end(&mut content)?;
    } else {
        content = std::fs::read(path)?;
    }
    let value: serde_json::Value = serde_json::from_slice(&content)
        .map_err(|error| invalid_data(format!("解析 {} 失败: {error}", path.display())))?;
    let plugin_count = validate_plugin_catalog_value(&value, path)?;
    eprintln!(
        "[xtask] 插件目录验证通过: {}（{plugin_count} 个插件）",
        path.display()
    );
    Ok(())
}

fn validate_plugin_catalog_value(value: &serde_json::Value, path: &Path) -> io::Result<usize> {
    let catalog: PublishedPluginCatalog = serde_json::from_value(value.clone())
        .map_err(|error| invalid_data(format!("插件目录结构无效: {}: {error}", path.display())))?;
    if catalog.version != PLUGIN_CATALOG_VERSION {
        return Err(invalid_data(format!(
            "插件目录版本无效: expected={PLUGIN_CATALOG_VERSION}, actual={}",
            catalog.version
        )));
    }

    let mut ids = BTreeSet::new();
    for plugin in &catalog.plugins {
        if plugin.id.trim().is_empty() || !ids.insert(plugin.id.as_str()) {
            return Err(invalid_data(format!(
                "插件目录包含空 ID 或重复 ID: {}",
                plugin.id
            )));
        }
        if plugin.name.trim().is_empty() {
            return Err(invalid_data(format!("插件 {} 的名称为空", plugin.id)));
        }
        semver::Version::parse(&plugin.version).map_err(|error| {
            invalid_data(format!(
                "插件 {} 版本 {} 无效: {error}",
                plugin.id, plugin.version
            ))
        })?;
        validate_catalog_artifact(&plugin.manifest, "插件清单")?;
        if let Some(wasm) = &plugin.wasm {
            validate_catalog_artifact(wasm, "WASM 制品")?;
        }
        for (entry, artifact) in &plugin.ui {
            validate_ui_entry(entry)?;
            validate_catalog_artifact(artifact, "UI 制品")?;
        }

        for (platform, artifact) in &plugin.sidecars {
            if platform.trim().is_empty() {
                return Err(invalid_data(format!(
                    "插件 {} 包含空 sidecar 平台",
                    plugin.id
                )));
            }
            validate_catalog_artifact(artifact, "sidecar 制品")?;
            let signed = plugin.signed_releases.get(platform).ok_or_else(|| {
                invalid_data(format!("插件 {} 的平台 {platform} 缺少签名清单", plugin.id))
            })?;
            validate_catalog_url(&signed.url, "插件签名清单")?;
            validate_catalog_url(&signed.signature_url, "插件签名文件")?;
        }
        for signed in plugin.signed_releases.values() {
            validate_catalog_url(&signed.url, "插件签名清单")?;
            validate_catalog_url(&signed.signature_url, "插件签名文件")?;
        }
    }
    Ok(catalog.plugins.len())
}

fn validate_catalog_artifact(artifact: &PublishedRemoteArtifact, label: &str) -> io::Result<()> {
    validate_catalog_url(&artifact.url, label)?;
    let checksum = artifact
        .checksum
        .strip_prefix("sha256:")
        .ok_or_else(|| invalid_data(format!("{label} checksum 必须使用 sha256:<hex> 格式")))?;
    let bytes = hex::decode(checksum)
        .map_err(|error| invalid_data(format!("{label} checksum 不是有效十六进制: {error}")))?;
    if bytes.len() != 32 {
        return Err(invalid_data(format!("{label} SHA-256 长度无效")));
    }
    Ok(())
}

fn validate_catalog_url(value: &str, label: &str) -> io::Result<()> {
    let parsed = url::Url::parse(value)
        .map_err(|error| invalid_data(format!("{label} URL 无效: {value}: {error}")))?;
    let local_http = parsed.scheme() == "http"
        && parsed
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    if parsed.scheme() != "https" && !local_http {
        return Err(invalid_data(format!("{label} 必须使用 HTTPS: {value}")));
    }
    Ok(())
}

fn validate_ui_entry(entry: &str) -> io::Result<()> {
    let path = Path::new(entry);
    if entry.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid_data(format!("UI 制品路径无效: {entry}")));
    }
    Ok(())
}

fn current_platform_key() -> String {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    };
    format!("{os}-{arch}")
}

fn validate_versions(workspace_root: &Path, config: &PluginConfig) -> io::Result<()> {
    let manifest_path = workspace_root.join(config.plugin_manifest);
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)
        .map_err(|error| invalid_data(format!("解析 {} 失败: {error}", manifest_path.display())))?;
    let manifest_version = manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_data("plugin.json 缺少 version"))?;

    // 版本一致性来源：wasm 插件对照 protocol crate；纯 sidecar 插件
    // （如 terminal，无独立协议 crate）对照 sidecar crate；纯 UI 插件
    // 对照 package.json。发布版本必须全仓唯一可信。
    let version_source: String = if let Some(protocol_manifest) = config.protocol_manifest {
        read_toml(&workspace_root.join(protocol_manifest))?
            .get("package")
            .and_then(|value| value.get("version"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid_data("无法读取 protocol package.version"))?
            .to_string()
    } else if config.sidecar_crate.is_some() {
        let sidecar_manifest = workspace_root
            .join(config.plugin_root)
            .join("sidecar/Cargo.toml");
        read_toml(&sidecar_manifest)?
            .get("package")
            .and_then(|value| value.get("version"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid_data("无法读取 sidecar package.version"))?
            .to_string()
    } else {
        let package_json = workspace_root.join(config.plugin_root).join("package.json");
        let package: serde_json::Value = serde_json::from_slice(&std::fs::read(&package_json)?)
            .map_err(|error| {
                invalid_data(format!("解析 {} 失败: {error}", package_json.display()))
            })?;
        package
            .get("version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid_data("package.json 缺少 version"))?
            .to_string()
    };
    if manifest_version != version_source {
        return Err(invalid_data(format!(
            "插件版本不一致: 工程={version_source}, plugin.json={manifest_version}"
        )));
    }

    // business-protocol 校验需要 protocol crate 元数据；纯 sidecar 插件
    // 直接依赖插件运行时，仅校验 transport 协议与清单一致。
    if config.sidecar_artifact.is_some() {
        if let Some(protocol_manifest) = config.protocol_manifest {
            let protocol = read_toml(&workspace_root.join(protocol_manifest))?;
            let business_protocol = protocol
                .get("package")
                .and_then(|value| value.get("metadata"))
                .and_then(|value| value.get("tiangong"))
                .and_then(|value| value.get("business-protocol"))
                .and_then(toml::Value::as_integer)
                .ok_or_else(|| invalid_data("Protocol 缺少 business-protocol 元数据"))?;
            let manifest_business_protocol = manifest
                .get("sidecar")
                .and_then(|value| value.get("business_protocol"))
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| invalid_data("plugin.json 缺少 sidecar.business_protocol"))?;
            if u64::try_from(business_protocol).ok() != Some(manifest_business_protocol) {
                return Err(invalid_data(format!(
                    "业务协议版本不一致: protocol={business_protocol}, plugin.json={manifest_business_protocol}"
                )));
            }
        }

        let runtime = read_toml(&workspace_root.join(RUNTIME_MANIFEST))?;
        let transport_protocol = runtime
            .get("package")
            .and_then(|value| value.get("metadata"))
            .and_then(|value| value.get("tiangong"))
            .and_then(|value| value.get("transport-protocol"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid_data("插件运行时缺少 transport-protocol 元数据"))?;
        let manifest_transport_protocol = manifest
            .get("sidecar")
            .and_then(|value| value.get("transport_protocol"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid_data("plugin.json 缺少 sidecar.transport_protocol"))?;
        if transport_protocol != manifest_transport_protocol {
            return Err(invalid_data(format!(
                "sidecar transport 版本不一致: runtime={transport_protocol}, plugin.json={manifest_transport_protocol}"
            )));
        }
    }

    let wasm_binary = manifest
        .get("wasm")
        .and_then(|value| value.get("binary"))
        .and_then(serde_json::Value::as_str);
    let sidecar_binary = manifest
        .get("sidecar")
        .and_then(|value| value.get("binary"))
        .and_then(serde_json::Value::as_str);
    // 纯 UI 插件清单不声明 wasm；声明与配置必须两侧同时存在且一致。
    if wasm_binary != config.wasm_artifact {
        return Err(invalid_data("plugin.json wasm.binary 与构建产物不一致"));
    }
    if let Some(expected) = config.sidecar_artifact
        && sidecar_binary != Some(expected)
    {
        return Err(invalid_data("plugin.json sidecar.binary 与构建产物不一致"));
    }
    let manifest_ui_entries = manifest
        .get("ui")
        .and_then(|value| value.get("contributions"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.get("entry").and_then(serde_json::Value::as_str))
        .collect::<BTreeSet<_>>();
    let configured_ui_entries = plugin_ui_entries(config)
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if manifest_ui_entries != configured_ui_entries {
        return Err(invalid_data(format!(
            "plugin.json UI 入口与构建配置不一致: manifest={manifest_ui_entries:?}, config={configured_ui_entries:?}"
        )));
    }

    let plugin_root = workspace_root.join(config.plugin_root);
    if config.protocol_crate.is_some() {
        require_file(&plugin_root.join("protocol/Cargo.toml"))?;
    }
    if config.wasm_crate.is_some() {
        require_file(&plugin_root.join("wasm/Cargo.toml"))?;
    }
    if config.sidecar_crate.is_some() {
        require_file(&plugin_root.join("sidecar/Cargo.toml"))?;
    }
    Ok(())
}

/// 官方公钥单一来源：与运行时共用同一文件（include_str!），杜绝两份
/// 常量漂移导致发布验签与运行时信任根不一致。
const OFFICIAL_PLUGIN_PUBKEY_B64: &str =
    include_str!("../../crates/tiangong-plugin-runtime/src/official-pubkey.b64");
/// Sandbox 自管理官方根：发布守卫与 Sandbox 内置验证共用此文件，
/// 不随插件签名根的后续轮换漂移。
const OFFICIAL_SANDBOX_PUBKEY_B64: &str =
    include_str!("../../crates/tiangong-sandbox/src/official-pubkey.b64");

/// staging 的 release.json 签名是否通过内置官方公钥验证（官方信任根
/// 固定，不接受环境变量或配置覆盖）。无签名清单的插件（纯 UI 等）不设部署门槛。
fn staging_passes_official_verification(staging: &Path) -> io::Result<bool> {
    let release_path = staging.join("release.json");
    let signature_path = staging.join("release.json.sig");
    if !release_path.is_file() && !signature_path.is_file() {
        return Ok(true);
    }
    if !release_path.is_file() || !signature_path.is_file() {
        return Ok(false);
    }
    // 官方信任根唯一且不可配置：不读环境变量（测试密钥无法绕过守卫）。
    let public_b64 = OFFICIAL_PLUGIN_PUBKEY_B64;
    let Some(public_text) = decode_base64_utf8(public_b64) else {
        return Ok(false);
    };
    let Ok(public_key) = minisign::PublicKeyBox::from_string(public_text.trim()) else {
        return Ok(false);
    };
    let Ok(public_key) = public_key.into_public_key() else {
        return Ok(false);
    };
    let Ok(signature_raw) = std::fs::read_to_string(&signature_path) else {
        return Ok(false);
    };
    let Some(signature_text) = decode_base64_utf8(signature_raw.trim()) else {
        return Ok(false);
    };
    let Ok(signature_box) = minisign::SignatureBox::from_string(signature_text.trim()) else {
        return Ok(false);
    };
    let content = std::fs::read(&release_path)?;
    let mut reader = std::io::Cursor::new(content);
    Ok(minisign::verify(
        &public_key,
        &signature_box,
        &mut reader,
        false,
        false,
        false,
    )
    .is_ok())
}

/// 校验 release.json 与内置官方公钥匹配（发布 CI 强制步骤）：签名验证
/// 失败即非零退出——正式私钥与内置公钥不匹配时发布中止，防止错误密钥
/// 产物流入官方目录。
fn verify_official_release(release_path: &Path) -> io::Result<()> {
    let signature_path = release_path.with_extension("json.sig");
    if !release_path.is_file() || !signature_path.is_file() {
        return Err(invalid_input(format!(
            "缺少签名清单或签名文件: {}",
            release_path.display()
        )));
    }
    let verified = verify_with_official_pubkey(release_path, &signature_path)?;
    if verified {
        eprintln!("[xtask] 官方公钥验签通过: {}", release_path.display());
        Ok(())
    } else {
        Err(invalid_input(
            "当前签名私钥与应用内置官方公钥不匹配，拒绝发布或部署",
        ))
    }
}

/// 以内置官方公钥验证签名文件（true = 匹配）。
fn verify_with_official_pubkey(release_path: &Path, signature_path: &Path) -> io::Result<bool> {
    verify_with_pubkey(release_path, signature_path, OFFICIAL_PLUGIN_PUBKEY_B64)
}

fn verify_with_pubkey(
    release_path: &Path,
    signature_path: &Path,
    public_key_b64: &str,
) -> io::Result<bool> {
    let Some(public_text) = decode_base64_utf8(public_key_b64) else {
        return Ok(false);
    };
    let Ok(public_key) = minisign::PublicKeyBox::from_string(public_text.trim()) else {
        return Ok(false);
    };
    let Ok(public_key) = public_key.into_public_key() else {
        return Ok(false);
    };
    let Ok(signature_raw) = std::fs::read_to_string(signature_path) else {
        return Ok(false);
    };
    let Some(signature_text) = decode_base64_utf8(signature_raw.trim()) else {
        return Ok(false);
    };
    let Ok(signature_box) = minisign::SignatureBox::from_string(signature_text.trim()) else {
        return Ok(false);
    };
    let content = std::fs::read(release_path)?;
    let mut reader = std::io::Cursor::new(content);
    Ok(minisign::verify(
        &public_key,
        &signature_box,
        &mut reader,
        false,
        false,
        false,
    )
    .is_ok())
}

fn decode_base64_utf8(value: &str) -> Option<String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

fn deploy_atomically(staging: &Path, destination: &Path, config: &PluginConfig) -> io::Result<()> {
    if !destination.exists() {
        return std::fs::rename(staging, destination);
    }

    let parent = destination
        .parent()
        .ok_or_else(|| invalid_input("插件安装目录缺少父目录"))?;
    let backup = parent.join(format!(".{}-backup-{}", config.id, std::process::id()));
    remove_dir_if_exists(&backup)?;
    std::fs::rename(destination, &backup)?;

    let result = (|| {
        for directory in PRESERVED_DIRS {
            let staged = staging.join(directory);
            let preserved = backup.join(directory);
            if preserved.exists() {
                remove_dir_if_exists(&staged)?;
                std::fs::rename(preserved, staged)?;
            }
        }
        std::fs::rename(staging, destination)
    })();

    if let Err(error) = result {
        for directory in PRESERVED_DIRS {
            let staged = staging.join(directory);
            let preserved = backup.join(directory);
            if staged.exists() && !preserved.exists() {
                let _ = std::fs::rename(staged, preserved);
            }
        }
        let _ = std::fs::rename(&backup, destination);
        return Err(error);
    }

    remove_dir_if_exists(&backup)
}

fn run_cargo(workspace_root: &Path, args: &[&str]) -> io::Result<()> {
    let status = Command::new(env_var_or("CARGO", "cargo"))
        .current_dir(workspace_root)
        .args(args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "cargo {} 执行失败",
            args.join(" ")
        )))
    }
}

fn prepare_sandbox_release() -> io::Result<()> {
    let workspace_root = workspace_root();
    let target = sandbox_target_triple()?;
    let platform = launcher_platform_key(&target)?;
    let version = launcher_crate_version(&workspace_root)?;
    eprintln!("[xtask] 构建 Launcher 发布制品（{platform} {version}）...");
    // Windows 制品必须静态链接 CRT：探针进程在 AppContainer 内加载
    // VCRUNTIME140.dll，而长期使用的机器上该文件常被第三方安装器覆盖
    // 并丢失“所有应用包”授权，探针会以 0xC0000022 拒绝启动。静态链接
    // 后可写目录之外仅依赖受系统保护的 DLL。
    let mut build_envs = Vec::new();
    if target.contains("windows") {
        let base = std::env::var("RUSTFLAGS").unwrap_or_default();
        let flags = if base.trim().is_empty() {
            "-C target-feature=+crt-static".to_string()
        } else {
            format!("{base} -C target-feature=+crt-static")
        };
        eprintln!("[xtask] Windows 制品启用 crt-static: {flags}");
        build_envs.push(("RUSTFLAGS", std::ffi::OsString::from(flags)));
    }
    let status = Command::new(env_var_or("CARGO", "cargo"))
        .current_dir(&workspace_root)
        .envs(build_envs)
        .args([
            "build",
            "-p",
            "tiangong-sandbox",
            "--release",
            "--target",
            target.as_str(),
        ])
        .status()?;
    if !status.success() {
        return Err(io::Error::other("cargo build tiangong-sandbox 执行失败"));
    }

    let executable_suffix = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let target_root = non_empty_env_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        })
        .unwrap_or_else(|| workspace_root.join("target"));
    let built = target_root
        .join(&target)
        .join("release")
        .join(format!("tiangong-sandbox{executable_suffix}"));
    require_file(&built)?;

    let dist = target_root.join("sandbox-dist");
    // 清理旧发布目录：跨平台/缓存恢复的残留片段会混入本次发布
    //（官方验签步骤也会因多平台名失败），生成前整体重建。
    if dist.exists() {
        std::fs::remove_dir_all(&dist)
            .map_err(|error| invalid_data(format!("清理旧发布目录失败: {error}")))?;
    }
    let artifact_dir = dist.join("sandbox").join(&version);
    std::fs::create_dir_all(&artifact_dir)?;
    let artifact_name = format!("tiangong-sb-{platform}");
    let artifact = artifact_dir.join(&artifact_name);
    std::fs::copy(&built, &artifact)?;

    let key_path = non_empty_env_os("TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH")
        .filter(|path| Path::new(&path).is_file())
        .ok_or_else(|| {
            invalid_input(
                "缺少签名私钥（TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH 未设置或文件不存在），\
                 拒绝产出无签名的 Launcher 发布制品。",
            )
        })?;
    let password =
        std::env::var("TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD").unwrap_or_default();
    sign_file_minisign(Path::new(&key_path), &password, &artifact)?;

    let base_url = env_var_or("TIANGONG_PLUGIN_OSS_BASE_URL", DEFAULT_OSS_BASE_URL)
        .trim_end_matches('/')
        .to_string();
    let fragment = serde_json::json!({
        "url": format!("{base_url}/sandbox/{version}/{artifact_name}"),
        "checksum": format!("sha256:{}", sha256(&artifact)?),
        "signature_url": format!("{base_url}/sandbox/{version}/{artifact_name}.sig"),
    });
    let fragments = dist.join("fragments");
    std::fs::create_dir_all(&fragments)?;
    std::fs::write(
        fragments.join(format!("{platform}.json")),
        serde_json::to_vec_pretty(&fragment)?,
    )?;
    // 结构化发布信息：协议/策略版本直接引用 crate 唯一常量定义，
    // workflow 读取本文件组清单与断言，不做源码正则提取。
    let release_info = serde_json::json!({
        "version": version,
        "protocol_version": tiangong_sandbox::LAUNCHER_PROTOCOL_VERSION,
        "policy_schema_max": tiangong_sandbox::LAUNCHER_POLICY_SCHEMA,
    });
    std::fs::write(
        dist.join("release-info.json"),
        serde_json::to_vec_pretty(&release_info)?,
    )?;
    eprintln!(
        "[xtask] Launcher 发布制品就绪: {}（片段 fragments/{platform}.json，release-info.json）",
        artifact.display()
    );
    Ok(())
}

/// target triple → 发布平台键（与宿主 artifacts::current_platform_key 对齐）。
fn launcher_platform_key(target: &str) -> io::Result<String> {
    if target.contains("windows") {
        Ok("windows-x86_64".to_string())
    } else if target.contains("linux") {
        Ok("linux-x86_64".to_string())
    } else if target.contains("darwin") {
        let arch = if target.contains("aarch64") {
            "aarch64"
        } else {
            "x86_64"
        };
        Ok(format!("darwin-{arch}"))
    } else {
        Err(invalid_input(format!("不支持的目标平台: {target}")))
    }
}

/// Launcher crate 独立版本（Cargo.toml，脱离 workspace 版本）。
fn launcher_crate_version(workspace_root: &Path) -> io::Result<String> {
    let manifest =
        std::fs::read_to_string(workspace_root.join("crates/tiangong-sandbox/Cargo.toml"))
            .map_err(|error| {
                invalid_data(format!("读取 tiangong-sandbox/Cargo.toml 失败: {error}"))
            })?;
    for line in manifest.lines() {
        if let Some(version) = line.trim().strip_prefix("version = ") {
            return Ok(version.trim_matches('"').to_string());
        }
    }
    Err(invalid_data(
        "tiangong-sandbox/Cargo.toml 缺少独立 version 声明",
    ))
}

/// 用内置官方公钥验证 Launcher 制品签名（发布守卫：私钥与内置公钥
/// 不匹配时立即失败，防止把全部客户端都会拒绝的制品发布上线）。
fn verify_sandbox_artifact(artifact: &str, signature: &str) -> io::Result<()> {
    let artifact = Path::new(artifact);
    let signature = Path::new(signature);
    require_file(artifact)?;
    require_file(signature)?;
    match verify_with_pubkey(artifact, signature, OFFICIAL_SANDBOX_PUBKEY_B64) {
        Ok(true) => {
            eprintln!("[xtask] 官方公钥验签通过: {}", artifact.display());
            Ok(())
        }
        Ok(false) => Err(invalid_data(format!(
            "官方公钥验签不通过（签名与内置公钥不匹配）: {}",
            artifact.display()
        ))),
        Err(error) => Err(error),
    }
}

fn sign_sandbox_debug() -> io::Result<()> {
    let workspace_root = workspace_root();
    let target_root = non_empty_env_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        })
        .unwrap_or_else(|| workspace_root.join("target"));
    let executable_suffix = if cfg!(windows) { ".exe" } else { "" };
    let launcher = target_root
        .join("debug")
        .join(format!("tiangong-sandbox{executable_suffix}"));
    require_file(&launcher)?;
    let key_path = non_empty_env_os("TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH")
        .filter(|path| Path::new(&path).is_file())
        .ok_or_else(|| {
            invalid_input(
                "缺少签名私钥（TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH 未设置或文件不存在）。\
                 本地开发请指向官方私钥或本机用户密钥（keys/user-signing.key）。",
            )
        })?;
    let password =
        std::env::var("TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD").unwrap_or_default();
    sign_file_minisign(Path::new(&key_path), &password, &launcher)?;
    eprintln!("[xtask] debug Launcher 已签名: {}.sig", launcher.display());
    Ok(())
}

fn sandbox_target_triple() -> io::Result<String> {
    for key in ["TAURI_ENV_TARGET_TRIPLE", "CARGO_BUILD_TARGET"] {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
        {
            return validate_target_triple(value.trim());
        }
    }

    let output = Command::new(env_var_or("RUSTC", "rustc"))
        .arg("-vV")
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("rustc -vV 执行失败"));
    }
    let version = String::from_utf8(output.stdout)
        .map_err(|error| invalid_data(format!("rustc -vV 输出不是 UTF-8: {error}")))?;
    let host = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| invalid_data("rustc -vV 输出缺少 host triple"))?;
    validate_target_triple(host)
}

fn validate_target_triple(value: &str) -> io::Result<String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid_input(format!("无效的 Rust target triple: {value}")));
    }
    Ok(value.to_string())
}

fn run_yarn(directory: &Path, args: &[&str]) -> io::Result<()> {
    let status = Command::new(env_var_or("YARN", "yarn"))
        .current_dir(directory)
        .args(args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "yarn {} 执行失败",
            args.join(" ")
        )))
    }
}

fn read_toml(path: &Path) -> io::Result<toml::Value> {
    let content = std::fs::read_to_string(path)?;
    toml::from_str(&content)
        .map_err(|error| invalid_data(format!("解析 {} 失败: {error}", path.display())))
}

fn require_file(path: &Path) -> io::Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("文件不存在: {}", path.display()),
        ))
    }
}

fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn sha256(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// 解释器形态 sidecar 插件（如 plugin-creator）：部署自包含 sidecar 产物、
/// 随行资源与内容清单。信任由官方签名清单承载（部署前经官方公钥
/// 验签守卫），不再落本地信任锚。
fn stage_interpreter_sidecar(
    workspace_root: &Path,
    staging: &Path,
    config: &PluginConfig,
) -> io::Result<()> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(workspace_root.join(config.plugin_manifest))?)
            .map_err(|error| invalid_data(format!("解析 plugin.json 失败: {error}")))?;
    let Some(sidecar) = manifest.get("sidecar") else {
        return Ok(());
    };
    let runtime = sidecar.get("runtime").and_then(serde_json::Value::as_str);
    if runtime.is_none_or(|value| value == "native") {
        return Ok(());
    }
    let entry = sidecar
        .get("entry")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_data("解释器 sidecar 缺少 entry"))?;
    let plugin_root = workspace_root.join(config.plugin_root);
    // 产物由该插件的 yarn build（scripts/build-sidecar.mjs）生成。
    let bundled = plugin_root.join("build/sidecar-main.mjs");
    require_file(&bundled)?;
    let destination = staging.join(entry);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&bundled, &destination)?;
    eprintln!("[xtask] sidecar sha256: {}", sha256(&destination)?);

    // creator 专属随行资源：devkit 模板（bundle 内按 ../templates 相对定位）。
    if config.id == "plugin-creator" {
        copy_dir_recursive(
            &workspace_root.join("plugins/devkit/templates"),
            &staging.join("templates"),
        )?;
    }

    // 内容清单：staging 全树（排除清单自身），路径 + sha256。
    fn walk_files(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk_files(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk_files(staging, &mut files)?;
    files.sort();
    let entries: Vec<serde_json::Value> = files
        .iter()
        .filter(|path| !path.ends_with("content-manifest.json"))
        .map(|path| {
            Ok(serde_json::json!({
                "path": path.strip_prefix(staging).unwrap().to_string_lossy().replace('\\', "/"),
                "sha256": sha256(path)?,
            }))
        })
        .collect::<io::Result<_>>()?;
    let content = serde_json::json!({ "algorithm": "sha256", "files": entries });
    let content_raw = serde_json::to_vec_pretty(&content)
        .map_err(|error| invalid_data(format!("序列化内容清单失败: {error}")))?;
    std::fs::write(staging.join("content-manifest.json"), &content_raw)?;
    Ok(())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn storage_root() -> PathBuf {
    user_home_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".tiangong")
}

fn user_home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    let drive = std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty());
    let path = std::env::var_os("HOMEPATH").filter(|value| !value.is_empty());
    match (drive, path) {
        (Some(drive), Some(path)) => {
            let mut buffer = PathBuf::from(drive);
            buffer.push(path);
            Some(buffer)
        }
        _ => None,
    }
}

fn env_var_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn non_empty_env_os(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

/// 从模板生成纯 UI 插件骨架：复制 templates/ui-app 并替换插件 ID。
fn new_plugin(plugin_id: &str, output_dir: Option<&str>) -> io::Result<()> {
    if plugin_id.is_empty()
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_input(
            "插件 ID 只能包含字母数字与 - _ .（建议反向域名如 com.example.board）",
        ));
    }
    let manifest_dir = Path::new("plugins").join("templates").join("ui-app");
    if !manifest_dir.join("plugin.json").is_file() {
        return Err(invalid_input(format!(
            "模板目录不存在: {}（请在仓库根目录执行）",
            manifest_dir.display()
        )));
    }
    let target = match output_dir {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("dist").join("plugins").join(plugin_id),
    };
    if target.exists() {
        return Err(invalid_input(format!(
            "目标目录已存在: {}",
            target.display()
        )));
    }
    copy_dir_recursive(&manifest_dir, &target)?;
    let manifest_path = target.join("plugin.json");
    let manifest = std::fs::read_to_string(&manifest_path)?.replace("com.example.board", plugin_id);
    std::fs::write(&manifest_path, manifest)?;
    println!("[xtask] 插件骨架已生成: {}", target.display());
    println!("[xtask] 本地导入: 在天工「设置 → 插件管理 → 导入本地插件」选择该目录");
    println!("[xtask] 工程化开发: 参阅 plugins/sdk/README.md 与 docs/plugin-development.md");
    Ok(())
}

fn copy_dir_recursive(source: &Path, target: &Path) -> io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn merge_plugin_distributions(
    input_root: &Path,
    output_root: &Path,
    expected_plugin: Option<&str>,
) -> io::Result<()> {
    if !input_root.is_dir() {
        return Err(invalid_input(format!(
            "插件平台制品输入目录不存在: {}",
            input_root.display()
        )));
    }
    remove_dir_if_exists(output_root)?;
    std::fs::create_dir_all(output_root.join("plugins-index"))?;

    let mut fragments = Vec::new();
    collect_fragment_files(input_root, &mut fragments)?;
    if fragments.is_empty() {
        return Err(invalid_data("没有找到插件目录片段"));
    }
    fragments.sort();

    let mut releases = BTreeMap::<String, serde_json::Value>::new();
    let mut fragment_platforms = BTreeMap::<String, BTreeSet<String>>::new();
    for fragment_path in fragments {
        let release: serde_json::Value = serde_json::from_slice(&std::fs::read(&fragment_path)?)
            .map_err(|error| {
                invalid_data(format!("解析 {} 失败: {error}", fragment_path.display()))
            })?;
        let id = required_json_string(&release, "id", &fragment_path)?;
        if expected_plugin.is_some_and(|expected| expected != id) {
            continue;
        }
        let version = required_json_string(&release, "version", &fragment_path)?;
        let file_stem = fragment_path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| invalid_data(format!("{} 文件名无效", fragment_path.display())))?;
        let prefix = format!("{id}-");
        let platform = file_stem
            .strip_prefix(&prefix)
            .ok_or_else(|| {
                invalid_data(format!(
                    "{} 文件名缺少 {prefix} 前缀",
                    fragment_path.display()
                ))
            })?
            .to_string();

        let manifest = release
            .get("manifest")
            .cloned()
            .ok_or_else(|| invalid_data(format!("{} 缺少 manifest", fragment_path.display())))?;
        // 纯 UI 插件无 wasm 条目，跨平台一致性由下方 existing 比较保证。
        let wasm = release.get("wasm").cloned();

        let key = format!("{id}@{version}");
        fragment_platforms
            .entry(key.clone())
            .or_default()
            .insert(platform);

        if let Some(existing) = releases.get_mut(&key) {
            for field in ["id", "name", "version", "description"] {
                if existing.get(field) != release.get(field) {
                    return Err(invalid_data(format!(
                        "插件 {key} 的 {field} 在不同平台不一致"
                    )));
                }
            }
            if existing.get("manifest") != Some(&manifest)
                || existing.get("wasm") != wasm.as_ref()
                || existing.get("ui") != release.get("ui")
            {
                return Err(invalid_data(format!(
                    "插件 {key} 的 plugin.json、WASM 或 UI 在不同平台不一致"
                )));
            }
            merge_platform_map(existing, &release, "sidecars", &key)?;
            merge_platform_map(existing, &release, "signed_releases", &key)?;
        } else {
            releases.insert(key, release);
        }
    }

    if let Some(plugin) = expected_plugin {
        if releases.is_empty() {
            return Err(invalid_data(format!("没有找到插件 {plugin} 的目录片段")));
        }
        if releases.len() != 1 {
            return Err(invalid_data(format!(
                "插件 {plugin} 的输入包含多个版本，独立发布一次只能发布一个版本"
            )));
        }
    }

    let expected_platforms = std::env::var("TIANGONG_PLUGIN_EXPECTED_PLATFORMS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    for (key, platforms) in &fragment_platforms {
        if !expected_platforms.is_empty() && platforms != &expected_platforms {
            return Err(invalid_data(format!(
                "插件 {key} 平台不完整: expected={expected_platforms:?}, actual={platforms:?}"
            )));
        }
    }

    if let Some(plugin) = expected_plugin {
        copy_plugin_distribution_files(input_root, output_root, plugin)?;
    } else {
        copy_distribution_files(input_root, output_root)?;
    }
    let plugins = releases.into_values().collect::<Vec<_>>();
    let plugin_count = plugins.len();
    let catalog_path = output_root.join("plugins-index/catalog.json");
    let catalog = serde_json::json!({"version": PLUGIN_CATALOG_VERSION, "plugins": plugins});
    validate_plugin_catalog_value(&catalog, &catalog_path)?;
    write_json(&catalog_path, &catalog)?;
    eprintln!(
        "[xtask] 已合并 {} 个插件的多平台 OSS 制品: {}",
        plugin_count,
        output_root.display()
    );
    Ok(())
}

fn collect_fragment_files(directory: &Path, output: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_fragment_files(&path, output)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("json")
            && path
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                == Some("fragments")
        {
            output.push(path);
        }
    }
    Ok(())
}

fn required_json_string<'a>(
    value: &'a serde_json::Value,
    field: &str,
    path: &Path,
) -> io::Result<&'a str> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_data(format!("{} 缺少 {field}", path.display())))
}

fn merge_platform_map(
    existing: &mut serde_json::Value,
    incoming: &serde_json::Value,
    field: &str,
    key: &str,
) -> io::Result<()> {
    let incoming = match incoming[field].as_object() {
        Some(map) => map,
        // sidecars 为 null（无 sidecar 插件）时无需合并。
        None => return Ok(()),
    };
    let existing = existing[field]
        .as_object_mut()
        .ok_or_else(|| invalid_data(format!("插件 {key} 的 {field} 无效")))?;
    for (platform, artifact) in incoming {
        if existing
            .insert(platform.clone(), artifact.clone())
            .is_some()
        {
            return Err(invalid_data(format!("插件 {key} 的平台 {platform} 重复")));
        }
    }
    Ok(())
}

fn copy_distribution_files(input_root: &Path, output_root: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(input_root)? {
        let entry = entry?;
        let source = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.file_name() == "plugins" {
            merge_directory(&source, &output_root.join("plugins"))?;
        } else {
            copy_distribution_files(&source, output_root)?;
        }
    }
    Ok(())
}

fn merge_directory(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            merge_directory(&source_path, &destination_path)?;
        } else if destination_path.exists() {
            if std::fs::read(&source_path)? != std::fs::read(&destination_path)? {
                return Err(invalid_data(format!(
                    "多平台制品内容冲突: {}",
                    destination_path.display()
                )));
            }
        } else {
            std::fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn copy_plugin_distribution_files(
    input_root: &Path,
    output_root: &Path,
    plugin: &str,
) -> io::Result<()> {
    for entry in std::fs::read_dir(input_root)? {
        let entry = entry?;
        let source = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.file_name() == "plugins" {
            let plugin_source = source.join(plugin);
            if plugin_source.is_dir() {
                merge_directory(&plugin_source, &output_root.join("plugins").join(plugin))?;
            }
        } else {
            copy_plugin_distribution_files(&source, output_root, plugin)?;
        }
    }
    Ok(())
}

fn merge_plugin_catalog(
    current_catalog: &Path,
    plugin_release: &Path,
    output_catalog: &Path,
) -> io::Result<()> {
    let incoming_root: serde_json::Value = serde_json::from_slice(&std::fs::read(plugin_release)?)
        .map_err(|error| {
            invalid_data(format!("解析 {} 失败: {error}", plugin_release.display()))
        })?;
    validate_plugin_catalog_value(&incoming_root, plugin_release)?;
    let incoming_plugins = incoming_root
        .get("plugins")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| invalid_data("插件发布目录缺少 plugins"))?;
    if incoming_plugins.len() != 1 {
        return Err(invalid_data("独立插件发布目录必须只包含一个插件"));
    }
    let incoming = incoming_plugins[0].clone();
    let incoming_id = required_json_string(&incoming, "id", plugin_release)?.to_string();
    let incoming_version = required_json_string(&incoming, "version", plugin_release)?.to_string();
    semver::Version::parse(&incoming_version).map_err(|error| {
        invalid_data(format!(
            "插件 {incoming_id} 版本 {incoming_version} 无效: {error}"
        ))
    })?;

    let mut plugins = BTreeMap::<String, serde_json::Value>::new();
    if current_catalog != Path::new("-") && current_catalog.is_file() {
        let current: serde_json::Value = serde_json::from_slice(&std::fs::read(current_catalog)?)
            .map_err(|error| {
            invalid_data(format!("解析 {} 失败: {error}", current_catalog.display()))
        })?;
        if current.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
            return Err(invalid_data("当前插件目录版本无效"));
        }
        for release in current
            .get("plugins")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| invalid_data("当前插件目录缺少 plugins"))?
        {
            let id = required_json_string(release, "id", current_catalog)?.to_string();
            if plugins.insert(id.clone(), release.clone()).is_some() {
                return Err(invalid_data(format!("当前插件目录包含重复 ID: {id}")));
            }
        }
    }
    plugins.insert(incoming_id.clone(), incoming);
    if let Some(parent) = output_catalog.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let merged = serde_json::json!({
        "version": PLUGIN_CATALOG_VERSION,
        "plugins": plugins.into_values().collect::<Vec<_>>()
    });
    validate_plugin_catalog_value(&merged, output_catalog)?;
    write_json(output_catalog, &merged)?;
    eprintln!(
        "[xtask] 已将插件 {incoming_id}@{incoming_version} 合并到目录: {}",
        output_catalog.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CI 同款链路：generate-plugin-test-key（未加密）→ 签名 → 公钥验证。
    /// 部署守卫只认内置官方公钥：测试密钥签名（即使注入同名环境变量）
    /// 验不过；无签名清单的纯 UI 插件不设门槛。
    #[test]
    fn 部署守卫_只认内置官方公钥() {
        let root = std::env::temp_dir().join(format!("xtask-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("plugin.json"), b"{}").unwrap();

        // 测试密钥签名：内置官方公钥验不过 → 跳过部署（守卫只认内置公钥，
        // 不读环境变量——环境注入测试公钥也无法绕过）。
        let key_path = root.join("test.key");
        generate_plugin_test_key(&key_path).unwrap();
        let release_path = staging.join("release.json");
        std::fs::write(&release_path, br#"{"schema_version":1}"#).unwrap();
        sign_file_minisign(&key_path, "", &release_path).unwrap();
        let previous = std::env::var("TIANGONG_PLUGIN_PUBKEY_B64").ok();
        let public_b64 = std::fs::read_to_string(key_path.with_extension("key.pub"))
            .unwrap()
            .trim()
            .to_string();
        unsafe {
            std::env::set_var("TIANGONG_PLUGIN_PUBKEY_B64", &public_b64);
        }
        let guarded = staging_passes_official_verification(&staging).unwrap();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("TIANGONG_PLUGIN_PUBKEY_B64", value),
                None => std::env::remove_var("TIANGONG_PLUGIN_PUBKEY_B64"),
            }
        }
        assert!(!guarded, "测试密钥（即使注入同名环境变量）不得通过部署守卫");

        // 无签名清单的插件（纯 UI）不设部署门槛。
        std::fs::remove_file(&release_path).unwrap();
        let _ = std::fs::remove_file(staging.join("release.json.sig"));
        assert!(staging_passes_official_verification(&staging).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 测试密钥_未加密签名与验证闭环() {
        let root = std::env::temp_dir().join(format!("xtask-sign-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key_path = root.join("test.key");
        generate_plugin_test_key(&key_path).expect("生成测试密钥");

        let content_path = root.join("release.json");
        std::fs::write(&content_path, br#"{"schema_version":1}"#).unwrap();
        sign_file_minisign(&key_path, "", &content_path).expect("未加密密钥签名");

        // 用 .pub 公钥验证签名（与运行时 verify_minisign 相同的格式链）。
        verify_signature_with_pub(&key_path.with_extension("key.pub"), &content_path)
            .expect("签名应可通过公钥验证");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 正式密钥形态（密码加密）同样可加载与签名，错误密码明确失败。
    #[test]
    fn 加密密钥_密码加载与错误密码拒绝() {
        let root = std::env::temp_dir().join(format!("xtask-sign-enc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let keypair =
            minisign::KeyPair::generate_encrypted_keypair(Some("secret-pass".to_string()))
                .expect("生成加密密钥");
        let key_path = root.join("enc.key");
        std::fs::write(
            &key_path,
            keypair
                .sk
                .to_box(Some("secret-pass"))
                .expect("导出私钥")
                .into_string(),
        )
        .unwrap();
        use base64::Engine;
        std::fs::write(
            key_path.with_extension("key.pub"),
            base64::engine::general_purpose::STANDARD
                .encode(keypair.pk.to_box().expect("导出公钥").into_string()),
        )
        .unwrap();

        let content_path = root.join("release.json");
        std::fs::write(&content_path, b"encrypted key signing").unwrap();
        sign_file_minisign(&key_path, "secret-pass", &content_path).expect("加密密钥签名");
        verify_signature_with_pub(&key_path.with_extension("key.pub"), &content_path)
            .expect("签名应可通过公钥验证");

        // 错误密码必须明确失败（不得触发交互提示）。
        std::fs::write(&content_path, b"another content").unwrap();
        let error =
            sign_file_minisign(&key_path, "wrong-pass", &content_path).expect_err("错误密码应拒绝");
        assert!(
            error.to_string().contains("加载插件签名私钥失败"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 用 minisign 公钥文本验证「整体 base64 包装」签名文件（与运行时
    /// verify_minisign 的读取格式一致）。
    fn verify_signature_with_pub(public_key_path: &Path, content_path: &Path) -> io::Result<()> {
        use base64::Engine;
        // .pub 文件为 base64(公钥文本)——与 generate_plugin_test_key 输出一致。
        let public_key_text = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(std::fs::read_to_string(public_key_path)?.trim())
                .map_err(|error| invalid_data(format!("公钥文件 base64 解码失败: {error}")))?,
        )
        .map_err(|error| invalid_data(format!("公钥文件非 UTF-8: {error}")))?;
        let public_key_box = minisign::PublicKeyBox::from_string(&public_key_text)
            .map_err(|error| invalid_data(format!("解析公钥失败: {error}")))?;
        let public_key = public_key_box
            .into_public_key()
            .map_err(|error| invalid_data(format!("加载公钥失败: {error}")))?;
        let signature_raw = std::fs::read_to_string(content_path.with_extension("json.sig"))?;
        let signature_text = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(signature_raw.trim())
                .map_err(|error| invalid_data(format!("签名文件 base64 解码失败: {error}")))?,
        )
        .map_err(|error| invalid_data(format!("签名文件非 UTF-8: {error}")))?;
        let signature_box = minisign::SignatureBox::from_string(&signature_text)
            .map_err(|error| invalid_data(format!("解析签名失败: {error}")))?;
        let content = std::fs::read(content_path)?;
        let mut reader = std::io::Cursor::new(content);
        minisign::verify(
            &public_key,
            &signature_box,
            &mut reader,
            false,
            false,
            false,
        )
        .map_err(|error| invalid_data(format!("签名验证失败: {error}")))?;
        Ok(())
    }
}
