//! 方舟管控面 OpenAPI：Access Key 签名（HMAC-SHA256）与 Agent Plan 模型列表。
//!
//! `ListArkAgentPlanModel` 仅支持 Access Key 鉴权，签名算法与火山引擎官方
//! SDK（volc-sdk-python `SignerV4`）一致：
//! 规范请求 = `METHOD\\n/\\n规范查询\\n签名头\\n签名头列表\\nsha256(body)`，
//! 派生密钥 = HMAC 链（日期 → 地域 → 服务 → `request`）。
//!
//! 参考：<https://www.volcengine.com/docs/6369/67269>（签名方法）、
//! <https://www.volcengine.com/docs/82379/2366394>（Agent Plan 支持模型）。

use anyhow::{Context, Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tiangong_plugin_volcengine_protocol::AgentPlanModels;

use crate::ark::{http_client, preview};

const SERVICE: &str = "ark";
const REGION: &str = "cn-beijing";
const API_VERSION: &str = "2024-01-01";
const LIST_MODELS_ACTION: &str = "ListArkAgentPlanModel";

/// 管控面凭据。
pub struct AccessKey {
    pub id: String,
    pub secret: String,
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], message: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// RFC 3986 编码：仅保留 `A-Z a-z 0-9 - _ . ~`。
fn uri_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// 签名结果：需附加到请求上的头。
#[derive(Debug, PartialEq, Eq)]
pub struct SignedHeaders {
    pub authorization: String,
    pub x_date: String,
    pub x_content_sha256: String,
}

/// 按官方 SignerV4 规则为 POST JSON 请求签名。
///
/// 参与签名的头固定为 `content-type`、`host`、`x-content-sha256`、`x-date`。
pub fn sign_post(
    key: &AccessKey,
    host: &str,
    query: &[(&str, &str)],
    body: &[u8],
    x_date: &str,
) -> SignedHeaders {
    let body_hash = sha256_hex(body);
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(name, value)| (uri_encode(name), uri_encode(value)))
        .collect();
    pairs.sort();
    let canonical_query = pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let signed_header_names = "content-type;host;x-content-sha256;x-date";
    let canonical_headers = format!(
        "content-type:application/json\nhost:{host}\nx-content-sha256:{body_hash}\nx-date:{x_date}\n"
    );
    let canonical_request = format!(
        "POST\n/\n{canonical_query}\n{canonical_headers}\n{signed_header_names}\n{body_hash}"
    );
    let date = &x_date[..8.min(x_date.len())];
    let scope = format!("{date}/{REGION}/{SERVICE}/request");
    let string_to_sign = format!(
        "HMAC-SHA256\n{x_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(key.secret.as_bytes(), date);
    let k_region = hmac_sha256(&k_date, REGION);
    let k_service = hmac_sha256(&k_region, SERVICE);
    let signing_key = hmac_sha256(&k_service, "request");
    let signature = hex::encode(hmac_sha256(&signing_key, &string_to_sign));
    SignedHeaders {
        authorization: format!(
            "HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_header_names}, Signature={signature}",
            key.id
        ),
        x_date: x_date.to_string(),
        x_content_sha256: body_hash,
    }
}

/// 模型所属能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Image,
    Video,
    Tts,
    Asr,
    Embedding,
    Text,
}

/// 按模型 ID 命名规则分类（接口只返回 ModelID，没有类型字段）。
pub fn classify(model_id: &str) -> ModelKind {
    let id = model_id.to_ascii_lowercase();
    if id.contains("seedream") {
        ModelKind::Image
    } else if id.contains("seedance") {
        ModelKind::Video
    } else if id.contains("tts") {
        ModelKind::Tts
    } else if id.contains("asr") {
        ModelKind::Asr
    } else if id.contains("embedding") {
        ModelKind::Embedding
    } else {
        ModelKind::Text
    }
}

/// 解析 `ListArkAgentPlanModel` 响应并分类（去重、保持接口顺序）。
pub fn parse_models(value: &Value, edition: &str, fetched_at: String) -> Result<AgentPlanModels> {
    if let Some(error) = value.pointer("/ResponseMetadata/Error") {
        let code = error.get("Code").and_then(Value::as_str).unwrap_or("");
        let message = error.get("Message").and_then(Value::as_str).unwrap_or("");
        bail!("查询 Agent Plan 模型列表失败（{code}）：{message}");
    }
    let datas = value
        .pointer("/Result/Datas")
        .and_then(Value::as_array)
        .with_context(|| {
            format!(
                "模型列表响应缺少 Result.Datas：{}",
                preview(&value.to_string())
            )
        })?;
    let mut models = AgentPlanModels {
        edition: edition.to_string(),
        fetched_at,
        ..AgentPlanModels::default()
    };
    for item in datas {
        let Some(id) = item
            .get("ModelID")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let bucket = match classify(id) {
            ModelKind::Image => &mut models.image,
            ModelKind::Video => &mut models.video,
            ModelKind::Tts => &mut models.tts,
            ModelKind::Asr => &mut models.asr,
            ModelKind::Embedding => &mut models.embedding,
            ModelKind::Text => &mut models.text,
        };
        if !bucket.iter().any(|existing| existing == id) {
            bucket.push(id.to_string());
        }
    }
    Ok(models)
}

/// 调用 `ListArkAgentPlanModel`。
pub async fn list_agent_plan_models(
    base_url: &str,
    key: &AccessKey,
    edition: &str,
) -> Result<AgentPlanModels> {
    let url = reqwest::Url::parse(base_url).context("管控面地址无效")?;
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => bail!("管控面地址缺少主机名"),
    };
    let body = serde_json::to_vec(&json!({ "Edition": edition })).context("序列化请求失败")?;
    let x_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let query = [("Action", LIST_MODELS_ACTION), ("Version", API_VERSION)];
    let signed = sign_post(key, &host, &query, &body, &x_date);

    let response = http_client()?
        .post(format!(
            "{}/?Action={LIST_MODELS_ACTION}&Version={API_VERSION}",
            base_url.trim_end_matches('/')
        ))
        .header("Content-Type", "application/json")
        .header("X-Date", &signed.x_date)
        .header("X-Content-Sha256", &signed.x_content_sha256)
        .header("Authorization", &signed.authorization)
        .body(body)
        .send()
        .await
        .context("请求 Agent Plan 模型列表失败")?;
    let status = response.status();
    let text = response.text().await.context("读取模型列表响应失败")?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("解析模型列表响应失败 ({status})：{}", preview(&text)))?;
    let fetched_at = chrono::Local::now()
        .naive_local()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let models = parse_models(&value, edition, fetched_at)?;
    if !status.is_success() {
        bail!(
            "查询 Agent Plan 模型列表失败 ({status})：{}",
            preview(&text)
        );
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_official_sdk_reference() {
        // 期望值由 volc-sdk-python SignerV4 同参数计算（固定时间、固定凭据）。
        let signed = sign_post(
            &AccessKey {
                id: "AKLTexample".to_string(),
                secret: "c2VjcmV0LWV4YW1wbGU=".to_string(),
            },
            "ark.cn-beijing.volcengineapi.com",
            &[
                ("Version", "2024-01-01"),
                ("Action", "ListArkAgentPlanModel"),
            ],
            br#"{"Edition":"personal"}"#,
            "20260929T070000Z",
        );
        assert_eq!(
            signed.x_content_sha256,
            "be6aff1f0d67674666086588f30ec531d8263e1e138de36a505ed26a4b90dd80"
        );
        assert_eq!(
            signed.authorization,
            "HMAC-SHA256 Credential=AKLTexample/20260929/cn-beijing/ark/request, \
             SignedHeaders=content-type;host;x-content-sha256;x-date, \
             Signature=33a34fd84401eeaed289bf429b7a136bc6a10bb0547cb9058f5cc12747524f51"
        );
    }

    #[test]
    fn uri_encode_keeps_unreserved_only() {
        assert_eq!(uri_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(uri_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn classify_and_parse_models_by_capability() {
        let value = json!({
            "ResponseMetadata": { "RequestId": "r" },
            "Result": { "Datas": [
                { "ModelID": "doubao-seedream-5-0-pro" },
                { "ModelID": "doubao-seedance-2.5" },
                { "ModelID": "doubao-seedance-2.0-fast" },
                { "ModelID": "doubao-seed-tts-2.0" },
                { "ModelID": "doubao-seed-asr-2.0" },
                { "ModelID": "doubao-embedding-vision" },
                { "ModelID": "glm-5.3" },
                { "ModelID": "glm-5.3" },
                { "ModelID": " " }
            ]}
        });
        let models = parse_models(&value, "personal", "t".to_string()).unwrap();
        assert_eq!(models.image, vec!["doubao-seedream-5-0-pro"]);
        assert_eq!(
            models.video,
            vec!["doubao-seedance-2.5", "doubao-seedance-2.0-fast"]
        );
        assert_eq!(models.tts, vec!["doubao-seed-tts-2.0"]);
        assert_eq!(models.asr, vec!["doubao-seed-asr-2.0"]);
        assert_eq!(models.embedding, vec!["doubao-embedding-vision"]);
        assert_eq!(models.text, vec!["glm-5.3"]);
        assert_eq!(models.edition, "personal");
    }

    #[test]
    fn parse_models_reports_openapi_error() {
        let value = json!({
            "ResponseMetadata": { "Error": { "Code": "SignatureDoesNotMatch", "Message": "bad sign" } }
        });
        let error = parse_models(&value, "personal", String::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("SignatureDoesNotMatch") && error.contains("bad sign"));
    }

    #[tokio::test]
    async fn list_models_round_trip_against_stub() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request).to_string();
                if read == 0 || text.ends_with(r#"{"Edition":"personal"}"#) {
                    break;
                }
            }
            let body = r#"{"ResponseMetadata":{},"Result":{"Datas":[{"ModelID":"doubao-seedream-5-0-pro"}]}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).to_ascii_lowercase()
        });
        let models = list_agent_plan_models(
            &format!("http://{address}"),
            &AccessKey {
                id: "AK".to_string(),
                secret: "SK".to_string(),
            },
            "personal",
        )
        .await
        .unwrap();
        assert_eq!(models.image, vec!["doubao-seedream-5-0-pro"]);
        let request = server.await.unwrap();
        assert!(request.starts_with("post /?action=listarkagentplanmodel&version=2024-01-01"));
        assert!(request.contains("authorization: hmac-sha256 credential=ak/"));
        assert!(request.contains("x-content-sha256: be6aff1f"));
        assert!(request.contains("x-date: "));
    }
}
