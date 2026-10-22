//! 工具调用分派：把宿主直连送来的 `file_read` / `file_write` / `current_time`
//! 调用参数解析为具体的文件操作请求。
//!
//! 纯函数，便于单测；参数缺失或类型不符时返回给模型的中文错误（不执行操作）。

use serde_json::{Map, Value};
use tiangong_plugin_fs_protocol::tools::{
    ApplyPatchRequest, ListDirRequest, ReadFileRequest, ReadImageRequest, ReplaceInFileRequest,
    TreeDirRequest, WriteFileRequest,
};
use tiangong_plugin_fs_protocol::{
    FsAccessContext, READ_ACTION_IMAGE, READ_ACTION_LIST, READ_ACTION_READ, READ_ACTION_TREE,
    READ_ACTIONS, TOOL_CURRENT_TIME, TOOL_FILE_READ, TOOL_FILE_WRITE, WRITE_ACTION_PATCH,
    WRITE_ACTION_REPLACE, WRITE_ACTION_WRITE, WRITE_ACTIONS,
};

/// 一次工具调用解析后的具体操作。
#[derive(Debug)]
pub enum FsCall {
    List(ListDirRequest),
    Tree(TreeDirRequest),
    Read(ReadFileRequest),
    Image(ReadImageRequest),
    Write(WriteFileRequest),
    Replace(ReplaceInFileRequest),
    Patch(ApplyPatchRequest),
    CurrentTime,
}

/// 分派失败的原因。
#[derive(Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// 不是本插件的工具。
    UnknownTool(String),
    /// 参数不合法：作为失败的工具结果返回给模型。
    InvalidArgument(String),
}

/// 解析一次工具调用。
pub fn parse_call(
    tool: &str,
    arguments: &Value,
    access: FsAccessContext,
) -> Result<FsCall, DispatchError> {
    match tool {
        TOOL_FILE_READ => parse_read(&Args::parse(tool, arguments, READ_ACTIONS)?, access),
        TOOL_FILE_WRITE => parse_write(&Args::parse(tool, arguments, WRITE_ACTIONS)?, access),
        TOOL_CURRENT_TIME => Ok(FsCall::CurrentTime),
        other => Err(DispatchError::UnknownTool(other.to_string())),
    }
}

fn parse_read(args: &Args<'_>, access: FsAccessContext) -> Result<FsCall, DispatchError> {
    match args.action {
        READ_ACTION_LIST => Ok(FsCall::List(ListDirRequest {
            path: args.opt_str("path")?,
            access,
        })),
        READ_ACTION_TREE => Ok(FsCall::Tree(TreeDirRequest {
            path: args.opt_str("path")?,
            max_depth: args.opt_u64("max_depth")?.map_or(0, to_usize),
            access,
        })),
        READ_ACTION_READ => Ok(FsCall::Read(ReadFileRequest {
            path: args.required_str("path")?,
            start_line: args.opt_u64("start_line")?.map_or(1, to_usize),
            max_lines: args.opt_u64("max_lines")?.map_or(0, to_usize),
            access,
        })),
        READ_ACTION_IMAGE => {
            let max_dimension = match args.opt_u64("max_dimension")? {
                None => None,
                Some(0) => return Err(args.invalid("max_dimension 需为正整数")),
                Some(n) => Some(u32::try_from(n).unwrap_or(u32::MAX)),
            };
            Ok(FsCall::Image(ReadImageRequest {
                path: args.required_str("path")?,
                max_dimension,
                access,
            }))
        }
        _ => unreachable!("action 已按 READ_ACTIONS 校验"),
    }
}

fn parse_write(args: &Args<'_>, access: FsAccessContext) -> Result<FsCall, DispatchError> {
    match args.action {
        WRITE_ACTION_WRITE => Ok(FsCall::Write(WriteFileRequest {
            path: args.required_str("path")?,
            content: args.required_str("content")?,
            append: args.opt_bool("append")?.unwrap_or(false),
            access,
        })),
        WRITE_ACTION_REPLACE => Ok(FsCall::Replace(ReplaceInFileRequest {
            path: args.required_str("path")?,
            old: args.required_str("old")?,
            new: args.required_str("new")?,
            replace_all: args.opt_bool("replace_all")?.unwrap_or(false),
            expected_count: args.opt_u64("expected_count")?.map(to_usize),
            access,
        })),
        WRITE_ACTION_PATCH => Ok(FsCall::Patch(ApplyPatchRequest {
            patch: args.required_str("patch")?,
            verify: args.opt_bool("verify")?.unwrap_or(false),
            workdir: args.opt_str("workdir")?,
            access,
        })),
        _ => unreachable!("action 已按 WRITE_ACTIONS 校验"),
    }
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// 带错误上下文（工具名与 action）的参数读取器。
struct Args<'a> {
    tool: &'a str,
    action: &'static str,
    map: Map<String, Value>,
}

impl<'a> Args<'a> {
    /// 解析参数对象并校验 action（必须是 allowed 之一）。
    ///
    /// 参数为 null 时视为空对象（随后因缺少 action 报错）；其余必须是 JSON 对象。
    fn parse(
        tool: &'a str,
        arguments: &Value,
        allowed: &[&'static str],
    ) -> Result<Self, DispatchError> {
        let map = match arguments {
            Value::Object(map) => map.clone(),
            Value::Null => Map::new(),
            _ => {
                return Err(DispatchError::InvalidArgument(format!(
                    "{tool} 的参数需为 JSON 对象"
                )));
            }
        };
        let expected = allowed.join("/");
        let value = map.get("action").and_then(Value::as_str).ok_or_else(|| {
            DispatchError::InvalidArgument(format!("{tool} 缺少必填参数 action（{expected}）"))
        })?;
        let action = allowed
            .iter()
            .copied()
            .find(|candidate| *candidate == value)
            .ok_or_else(|| {
                DispatchError::InvalidArgument(format!(
                    "{tool} 的 action 需为 {expected}，收到：{value}"
                ))
            })?;
        Ok(Self { tool, action, map })
    }

    fn invalid(&self, message: &str) -> DispatchError {
        DispatchError::InvalidArgument(format!(
            "{}（action={}）的 {message}",
            self.tool, self.action
        ))
    }

    /// 取存在且非 null 的字段。
    fn present(&self, key: &str) -> Option<&Value> {
        self.map.get(key).filter(|value| !value.is_null())
    }

    fn opt_str(&self, key: &str) -> Result<Option<String>, DispatchError> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(self.invalid(&format!("{key} 需为字符串"))),
        }
    }

    fn required_str(&self, key: &str) -> Result<String, DispatchError> {
        self.opt_str(key)?
            .ok_or_else(|| self.invalid(&format!("缺少必填参数 {key}")))
    }

    fn opt_u64(&self, key: &str) -> Result<Option<u64>, DispatchError> {
        match self.present(key) {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| self.invalid(&format!("{key} 需为非负整数"))),
        }
    }

    fn opt_bool(&self, key: &str) -> Result<Option<bool>, DispatchError> {
        match self.present(key) {
            None => Ok(None),
            Some(value) => value
                .as_bool()
                .map(Some)
                .ok_or_else(|| self.invalid(&format!("{key} 需为布尔值"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access() -> FsAccessContext {
        FsAccessContext {
            workspace: Some("/ws".to_string()),
            full_trust: false,
        }
    }

    /// 测试用：空串表示无参数（宿主对空参数传 null）。
    fn parse(tool: &str, arguments: &str) -> Result<FsCall, DispatchError> {
        let value = if arguments.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(arguments).expect("测试参数应为合法 JSON")
        };
        parse_call(tool, &value, access())
    }

    fn invalid(tool: &str, arguments: &str) -> String {
        match parse(tool, arguments) {
            Err(DispatchError::InvalidArgument(message)) => message,
            other => panic!("期望参数错误，实际：{other:?}"),
        }
    }

    #[test]
    fn file_read_各_action_映射到对应请求() {
        let FsCall::List(list) = parse("file_read", r#"{"action":"list"}"#).unwrap() else {
            panic!("list");
        };
        assert_eq!(list.path, None);
        assert_eq!(list.access.workspace.as_deref(), Some("/ws"));

        let FsCall::Tree(tree) = parse(
            "file_read",
            r#"{"action":"tree","path":"src","max_depth":3}"#,
        )
        .unwrap() else {
            panic!("tree");
        };
        assert_eq!((tree.path.as_deref(), tree.max_depth), (Some("src"), 3));

        let FsCall::Read(read) = parse(
            "file_read",
            r#"{"action":"read","path":"a.rs","max_lines":10}"#,
        )
        .unwrap() else {
            panic!("read");
        };
        assert_eq!(
            (read.path.as_str(), read.start_line, read.max_lines),
            ("a.rs", 1, 10)
        );

        let FsCall::Image(image) =
            parse("file_read", r#"{"action":"image","path":"a.png"}"#).unwrap()
        else {
            panic!("image");
        };
        assert_eq!((image.path.as_str(), image.max_dimension), ("a.png", None));
    }

    #[test]
    fn file_write_各_action_映射到对应请求() {
        let FsCall::Write(write) = parse(
            "file_write",
            r#"{"action":"write","path":"a","content":"","append":true}"#,
        )
        .unwrap() else {
            panic!("write");
        };
        assert_eq!(
            (write.path.as_str(), write.content.as_str(), write.append),
            ("a", "", true)
        );

        let FsCall::Replace(replace) = parse(
            "file_write",
            r#"{"action":"replace","path":"a","old":"x","new":"","expected_count":2}"#,
        )
        .unwrap() else {
            panic!("replace");
        };
        assert_eq!(
            (
                replace.old.as_str(),
                replace.new.as_str(),
                replace.expected_count,
                replace.replace_all
            ),
            ("x", "", Some(2), false)
        );

        let FsCall::Patch(patch) = parse(
            "file_write",
            r#"{"action":"patch","patch":"--- a","verify":true}"#,
        )
        .unwrap() else {
            panic!("patch");
        };
        assert!(patch.verify);
        assert_eq!(patch.workdir, None);
    }

    #[test]
    fn current_time_忽略参数() {
        assert!(matches!(parse("current_time", ""), Ok(FsCall::CurrentTime)));
        assert!(matches!(
            parse("current_time", "{}"),
            Ok(FsCall::CurrentTime)
        ));
    }

    #[test]
    fn 旧工具名与未知工具不分派() {
        for tool in ["read_file", "write_file", "list_dir", "apply_patch", "nope"] {
            assert_eq!(
                parse(tool, "{}").unwrap_err(),
                DispatchError::UnknownTool(tool.to_string())
            );
        }
    }

    #[test]
    fn action_缺失或越界时报错且列出可选值() {
        assert!(invalid("file_read", "{}").contains("缺少必填参数 action（list/tree/read/image）"));
        assert!(invalid("file_read", "").contains("缺少必填参数 action"));
        // 写类 action 不能经 file_read 调用，反之亦然。
        assert!(invalid("file_read", r#"{"action":"write"}"#).contains("收到：write"));
        assert!(
            invalid("file_write", r#"{"action":"read"}"#)
                .contains("action 需为 write/replace/patch")
        );
    }

    #[test]
    fn 必填字段缺失时报错而不是按空串执行() {
        assert!(
            invalid("file_read", r#"{"action":"read"}"#)
                .contains("file_read（action=read）的 缺少必填参数 path")
        );
        assert!(invalid("file_write", r#"{"action":"write","path":"a"}"#).contains("content"));
        assert!(
            invalid("file_write", r#"{"action":"replace","path":"a","old":"x"}"#).contains("new")
        );
        assert!(invalid("file_write", r#"{"action":"patch"}"#).contains("patch"));
        // null 视为未提供。
        assert!(invalid("file_read", r#"{"action":"image","path":null}"#).contains("path"));
    }

    #[test]
    fn 参数类型不符时报错() {
        assert!(invalid("file_read", "[1]").contains("需为 JSON 对象"));
        assert!(invalid("file_read", r#"{"action":"read","path":1}"#).contains("path 需为字符串"));
        assert!(
            invalid("file_read", r#"{"action":"tree","max_depth":-1}"#)
                .contains("max_depth 需为非负整数")
        );
        assert!(
            invalid(
                "file_write",
                r#"{"action":"write","path":"a","content":"x","append":"yes"}"#
            )
            .contains("append 需为布尔值")
        );
        assert!(
            invalid(
                "file_read",
                r#"{"action":"image","path":"a","max_dimension":0}"#
            )
            .contains("max_dimension 需为正整数")
        );
    }
}
