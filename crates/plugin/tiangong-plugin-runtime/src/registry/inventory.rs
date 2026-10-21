//! 自制插件判据、对话内清单与能力指纹。
//!
//! local 发布者的能力经固定工具 + 清单提供（见 core_bridge 动态通道），
//! 不进 tools 声明与 system prompt；指纹只计入进入模型请求前缀的插件。

use super::*;

pub fn is_local_plugin(plugin_id: &str) -> bool {
    loaded_plugins()
        .lock()
        .ok()
        .and_then(|plugins| {
            plugins.get(plugin_id).map(|loaded| {
                loaded
                    .signed_release
                    .as_ref()
                    .is_some_and(|release| release.publisher == crate::trust::LOCAL_PUBLISHER)
            })
        })
        .unwrap_or(false)
}

/// 本机自制插件的对话内能力清单（JSON）。
///
/// 纯 manifest 级信息（不实例化 WASM）：每个自制插件的版本与工具签名。
/// 该清单经注入通道追加到会话历史（append-only，cache 前缀不受影响），
/// 模型据此填写 `call_local_plugin` 的 plugin_name / function_name。
pub fn local_plugin_inventory() -> serde_json::Value {
    let Ok(plugins) = loaded_plugins().lock() else {
        return serde_json::json!({ "plugins": [], "note": LIST_NOTE });
    };
    let mut entries: Vec<serde_json::Value> = plugins
        .iter()
        .filter(|(_, loaded)| {
            loaded.enabled
                && loaded
                    .signed_release
                    .as_ref()
                    .is_some_and(|release| release.publisher == crate::trust::LOCAL_PUBLISHER)
        })
        .map(|(id, loaded)| {
            let functions: Vec<serde_json::Value> = loaded
                .manifest
                .tools
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "args_schema": tool.input_schema,
                    })
                })
                .collect();
            let mut entry = serde_json::json!({
                "name": id,
                "version": loaded.manifest.version,
                "functions": functions,
            });
            // prompt 段落同样经清单注入对话（local 插件的 prompt_sections
            // 不进 system prompt，见 core_bridge 的分流），保持两头一致：
            // 纯 prompt 的自制插件也出现在清单里。
            if let Some(prompts) = loaded
                .manifest
                .prompt
                .as_ref()
                .filter(|prompts| !prompts.is_empty())
            {
                entry["prompt"] = serde_json::json!(prompts);
            }
            entry
        })
        .collect();
    entries.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["name"].as_str().unwrap_or_default())
    });
    serde_json::json!({ "plugins": entries, "note": LIST_NOTE })
}

/// 清单提示语：强调时效性（防翻旧清单）并指明权威查询通道——注入清单
/// 是尽力而为的加速缓存，压缩后可能不再出现在模型可见历史里。
const LIST_NOTE: &str =
    "本条为截至当前的清单，早前清单作废；如需确认可用 list_local_plugins 重新查询";

/// 当前会**进入模型请求前缀**的插件集合能力指纹（id@version 的稳定摘要）。
///
/// 插件的装卸、升级、启停全部经本注册表，因此「插件是否发生变化」由
/// runtime 自己回答最准确——调用方无需了解启用判定、版本字段回落顺序
/// 或摘要算法，只比较两次取值是否相同即可。
///
/// 指纹输入是插件的**声明态**，且只计入影响模型请求前缀的插件：
///
/// - **本机自制插件（local 发布者）不计入**：其工具与 prompt 经固定工具
///   `call_local_plugin` + 对话内清单提供（见 `local_plugin_inventory`），
///   装卸迭代不改变 tools 声明与 system prompt 段落，不该触发上下文交接；
/// - **纯 UI 插件不计入**：判据是「确定无逻辑层」的充分条件——manifest
///   无 wasm 制品、无 sidecar、无 TS tools/prompt 声明（四者皆空才排除）。
///   注意 WASM 插件的工具声明在组件内（`tool-specs` 接缝），manifest 的
///   tools/prompt 恒为空，**不能**以 manifest 声明判其无能力——否则官方
///   WASM 插件（coding/fs/memory 等）升级将漏出交接。宁可多计（纯 UI
///   之外的形态一律计入）也不漏报，与下方锁中毒策略一致。
///
/// 键排序去重后参与摘要——加载顺序与运行期抖动（如 sidecar 临时掉线）
/// 不构成能力变化；反之插件升级即使工具名不变也会改变指纹，因为历史里
/// 按旧版本产生的调用与新版本行为可能已不一致。
///
/// 凭据与运行参数不参与：前者绝不落盘，后者不属于插件能力。
pub fn enabled_plugin_fingerprint() -> String {
    let Ok(plugins) = loaded_plugins().lock() else {
        // 锁中毒时返回空集合指纹：调用方据此判定为「与任何已定档值不同」，
        // 宁可多做一次处理也不要漏掉真实变化。
        return fingerprint_of(&[]);
    };
    let keys: Vec<String> = plugins
        .iter()
        .filter(|(_, loaded)| {
            loaded.enabled
                && !loaded
                    .signed_release
                    .as_ref()
                    .is_some_and(|release| release.publisher == crate::trust::LOCAL_PUBLISHER)
                && (loaded.manifest.wasm.is_some()
                    || loaded.manifest.sidecar.is_some()
                    || loaded
                        .manifest
                        .tools
                        .as_ref()
                        .is_some_and(|tools| !tools.is_empty())
                    || loaded
                        .manifest
                        .prompt
                        .as_ref()
                        .is_some_and(|prompts| !prompts.is_empty()))
        })
        .map(|(id, loaded)| {
            let version = loaded
                .descriptor
                .as_ref()
                .map(|value| value.version.clone())
                .unwrap_or_else(|| loaded.manifest.version.clone());
            if version.is_empty() {
                id.clone()
            } else {
                format!("{id}@{version}")
            }
        })
        .collect();
    fingerprint_of(&keys)
}

/// 插件键集合的稳定摘要：排序去重后以分隔符拼接求 SHA-256。
pub(super) fn fingerprint_of(plugin_keys: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut keys = plugin_keys.to_vec();
    keys.sort();
    keys.dedup();
    let mut hasher = Sha256::new();
    for key in keys {
        hasher.update(key.as_bytes());
        // 分隔符不可省略：否则 ["ab","c"] 与 ["a","bc"] 会拼成同一串。
        hasher.update(b"\x1f");
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
