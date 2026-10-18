//! 应用内更新的 GitHub 兜底源。
//!
//! tauri updater 只在获取 `latest.json` 失败时按 endpoints 顺序回退；一旦 OSS 清单
//! 可读，下载地址就固定指向 OSS，OSS 上缺包时下载阶段直接 404 而不会回退。
//! 本模块只用 GitHub Release 的清单重新检查一次，得到的 `Update` 放进 webview
//! 资源表，前端用官方 `Update` 句柄继续走插件自带的 download / install（签名校验不变）。

use serde::Serialize;
use tauri::{Manager, Runtime, Webview};
use tauri_plugin_updater::UpdaterExt;
use url::Url;

/// GitHub Release 的 latest.json（与 `tauri.conf.json` 中第二个 endpoint 指向同一仓库）。
const GITHUB_LATEST_JSON: &str =
    "https://github.com/silent-rs/silent-Tiangong/releases/latest/download/latest.json";

/// 与 `@tauri-apps/plugin-updater` 中 `UpdateMetadata` 字段一致，供前端 `new Update(...)`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FallbackUpdateMetadata {
    rid: tauri::ResourceId,
    current_version: String,
    version: String,
    date: Option<String>,
    body: Option<String>,
    raw_json: serde_json::Value,
}

/// 只用 GitHub 源重新检查更新；没有更新时返回 `None`。
#[tauri::command]
pub async fn check_update_github_fallback<R: Runtime>(
    webview: Webview<R>,
    timeout_ms: Option<u64>,
) -> Result<Option<FallbackUpdateMetadata>, String> {
    let endpoint = Url::parse(GITHUB_LATEST_JSON).map_err(|e| e.to_string())?;
    let mut builder = webview
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?;
    if let Some(ms) = timeout_ms {
        builder = builder.timeout(std::time::Duration::from_millis(ms));
    }
    let updater = builder.build().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let date = update
        .raw_json
        .get("pub_date")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Ok(Some(FallbackUpdateMetadata {
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        date,
        body: update.body.clone(),
        raw_json: update.raw_json.clone(),
        rid: webview.resources_table().add(update),
    }))
}

#[cfg(test)]
mod tests {
    use super::GITHUB_LATEST_JSON;

    #[test]
    fn github_endpoint_matches_tauri_conf() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let endpoints = conf["plugins"]["updater"]["endpoints"].as_array().unwrap();
        let github = endpoints
            .iter()
            .filter_map(|v| v.as_str())
            .find(|v| v.starts_with("https://github.com/"))
            .expect("tauri.conf.json 应包含 GitHub endpoint");
        assert_eq!(github, GITHUB_LATEST_JSON);
    }
}
