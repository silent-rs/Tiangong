//! 请求分发：握手、协议校验与各文件操作。
//!
//! 业务失败以 `{ok:false, summary}` 成功响应返回，页面据此展示中文错误；
//! 只有协议层问题才返回协议错误。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, PROTOCOL_VERSION, Request, Response,
};

use crate::files::{self, CreateKind, Workspace};

pub struct WorkspaceFilesService;

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for WorkspaceFilesService {
    async fn dispatch(&self, request: Request) -> Response {
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request.request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "workspace-files 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }
        let authoritative = tiangong_plugin_sidecar::invocation_context()
            .map(|context| context.workspace)
            .filter(|workspace| !workspace.trim().is_empty())
            .map(PathBuf::from);
        let payload = match handle(&request.operation, request.payload, authoritative) {
            Ok(value) => value,
            Err(error) => json!({ "ok": false, "summary": format!("{error:#}") }),
        };
        Response::success(&request.request_id, payload)
    }
}

#[derive(Deserialize)]
struct Base {
    #[serde(default)]
    workspace: String,
}

#[derive(Deserialize)]
struct PathArgs {
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    expected_hash: Option<String>,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Deserialize)]
struct CreateArgs {
    path: String,
    kind: CreateKind,
}

#[derive(Deserialize)]
struct RenameArgs {
    from: String,
    to: String,
}

fn parse<T: DeserializeOwned>(payload: &Value) -> Result<T> {
    serde_json::from_value(payload.clone()).context("请求参数格式不正确")
}

/// 工作区根：宿主调用上下文优先；页面经 bridge 调用时上下文缺省，取页面
/// 提供的会话工作区（OS 沙箱可写域同为宿主权威工作区，越界写入会被拦截）。
fn resolve_workspace(payload: &Value, authoritative: Option<PathBuf>) -> Result<Workspace> {
    let root = match authoritative {
        Some(root) => root,
        None => {
            let base: Base = parse(payload)?;
            if base.workspace.trim().is_empty() {
                bail!("当前会话没有可用的工作区");
            }
            PathBuf::from(base.workspace.trim())
        }
    };
    Workspace::open(Path::new(&root))
}

fn handle(operation: &str, payload: Value, authoritative: Option<PathBuf>) -> Result<Value> {
    if operation == HANDSHAKE_OPERATION {
        return Ok(json!({
            "plugin_id": "workspace-files",
            "plugin_version": env!("CARGO_PKG_VERSION"),
            "sidecar_version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "business_protocol": 1,
            "capabilities": ["list_dir", "read_file", "write_file", "create", "rename", "delete"],
            "instance_id": format!("workspace-files-sidecar-{}", std::process::id()),
            "status": "ready",
        }));
    }
    let workspace = resolve_workspace(&payload, authoritative)?;
    let value = match operation {
        "list_dir" => {
            let args: PathArgs = parse(&payload)?;
            serde_json::to_value(files::list_dir(&workspace, &args.path)?)?
        }
        "read_file" => {
            let args: PathArgs = parse(&payload)?;
            serde_json::to_value(files::read_file(&workspace, &args.path)?)?
        }
        "write_file" => {
            let args: WriteArgs = parse(&payload)?;
            serde_json::to_value(files::write_file(
                &workspace,
                &args.path,
                &args.content,
                args.expected_hash.as_deref(),
                args.overwrite,
            )?)?
        }
        "create" => {
            let args: CreateArgs = parse(&payload)?;
            json!({ "path": files::create(&workspace, &args.path, args.kind)? })
        }
        "rename" => {
            let args: RenameArgs = parse(&payload)?;
            json!({ "path": files::rename(&workspace, &args.from, &args.to)? })
        }
        "delete" => {
            let args: PathArgs = parse(&payload)?;
            files::delete(&workspace, &args.path)?;
            json!({})
        }
        other => bail!("未知操作: {other}"),
    };
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object.insert("ok".to_string(), Value::Bool(true));
        object.insert(
            "workspace".to_string(),
            Value::String(workspace.root().to_string_lossy().into_owned()),
        );
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_plugin_sidecar::SidecarService;

    fn request(operation: &str, payload: Value) -> Request {
        Request::new(operation, payload)
    }

    #[tokio::test]
    async fn dispatch_round_trip() {
        let dir = tempfile::tempdir().expect("临时目录");
        let ws = dir.path().to_string_lossy().into_owned();
        let service = WorkspaceFilesService;
        let created = service
            .dispatch(request(
                "create",
                json!({"workspace": ws, "path": "a.txt", "kind": "file"}),
            ))
            .await;
        assert!(created.success);
        assert_eq!(created.payload.as_ref().expect("payload")["ok"], true);

        let listed = service
            .dispatch(request("list_dir", json!({"workspace": ws, "path": ""})))
            .await;
        let payload = listed.payload.expect("payload");
        assert_eq!(payload["entries"][0]["name"], "a.txt");
    }

    #[tokio::test]
    async fn business_errors_are_reported_in_payload() {
        let service = WorkspaceFilesService;
        let missing = service
            .dispatch(request("list_dir", json!({"path": ""})))
            .await;
        assert!(missing.success);
        let payload = missing.payload.expect("payload");
        assert_eq!(payload["ok"], false);
        assert!(
            payload["summary"]
                .as_str()
                .expect("summary")
                .contains("工作区")
        );

        let dir = tempfile::tempdir().expect("临时目录");
        let unknown = service
            .dispatch(request(
                "chmod",
                json!({"workspace": dir.path().to_string_lossy()}),
            ))
            .await;
        assert_eq!(unknown.payload.expect("payload")["ok"], false);
    }

    #[tokio::test]
    async fn rejects_protocol_mismatch() {
        let mut req = request("list_dir", json!({}));
        req.protocol_version = "0.0.0".to_string();
        let response = WorkspaceFilesService.dispatch(req).await;
        assert!(!response.success);
    }

    #[test]
    fn authoritative_workspace_overrides_payload() {
        let real = tempfile::tempdir().expect("临时目录");
        let ws = resolve_workspace(
            &json!({"workspace": "/definitely/not/here"}),
            Some(real.path().to_path_buf()),
        )
        .expect("解析工作区");
        assert_eq!(
            ws.root(),
            real.path().canonicalize().expect("规范化").as_path()
        );
    }
}
