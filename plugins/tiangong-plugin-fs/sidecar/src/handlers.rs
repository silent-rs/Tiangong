//! Fs 工具业务实现：从原进程内 `handler.rs` 迁移。
//!
//! 改造点：
//! - 入参由 `ToolCall`（JSON）改为 protocol 的类型化 Request
//! - 输出由 core `ToolResult` 改为 protocol `FsToolResponse`
//! - 路径只解析为绝对路径（toolkit），读写范围由 OS 沙箱裁决
//! - 锁的 acquire/release 在写工具内部完成（一次调用完成加锁+写+解锁，避免锁泄漏），
//!   响应里带回 locked/unlocked 路径供 wasm 发 StreamEvent

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tiangong_plugin_fs_protocol::FsAccessContext;
use tiangong_plugin_fs_protocol::tools::{
    ApplyPatchRequest, FsToolResponse, ListDirRequest, ReadFileRequest, ReplaceInFileRequest,
    TreeDirRequest, WriteFileRequest,
};
use tiangong_toolkit as shared;

use crate::file_lock::{FileLockTable, canonicalize_for_lock};

const DEFAULT_TREE_MAX_DEPTH: usize = 2;
const MAX_TREE_MAX_DEPTH: usize = 8;
const MAX_TREE_NODES: usize = 1200;
const DEFAULT_READ_MAX_LINES: usize = 200;
const MAX_READ_MAX_LINES: usize = 2000;

/// 取访问上下文里的 workspace 作为 base，未注入时报错（fs 工具必须知道 workspace）。
fn base_of(access: &FsAccessContext) -> Result<PathBuf> {
    access
        .workspace
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("会话工作目录未注入，无法执行文件工具"))
}

/// 在工具结果 summary 头部回显工作目录与可写范围（规范化绝对路径）。
///
/// 工作目录与允许目录不进入 system prompt（会话动态信息，cwd 迟落定/变更时
/// 改写已发送前缀会作废对话缓存），模型经 fs 工具调用获知。可写范围与宿主
/// 沙箱策略的统一写域一致：存储根只开放 data/<插件 id>、media、tmp，临时
/// 文件引导至 /tmp，避免模型把临时脚本堆进存储根。写进 summary 而非
/// stdout，避免污染 read_file 等工具的文件正文。无状态设计：每次调用都回显。
pub fn annotate_workdir(mut resp: FsToolResponse, access: &FsAccessContext) -> FsToolResponse {
    if let Some(base) = access.workspace.as_deref() {
        let canonical = std::fs::canonicalize(base).unwrap_or_else(|_| PathBuf::from(base));
        let storage = shared::app_storage_root();
        resp.summary = format!(
            "工作目录：{}（另可写：/tmp，{} 下的 media/、tmp/；临时文件写 /tmp）；{}",
            canonical.display(),
            storage.display(),
            resp.summary
        );
    }
    resp
}

// ── list_dir ─────────────────────────────────────────────────

pub fn handle_list_dir(req: ListDirRequest) -> FsToolResponse {
    let result = (|| {
        let base = base_of(&req.access)?;
        let path = req.path.as_deref().unwrap_or(".");
        let full_path = shared::resolve_read_path_with(path, &base)?;
        ensure_dir_target(&full_path)?;
        let mut items = Vec::new();
        for entry in fs::read_dir(&full_path)
            .with_context(|| format!("读取目录失败：{}", full_path.display()))?
        {
            let entry = entry?;
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let name = entry.file_name().to_string_lossy().to_string();
            items.push(if is_dir { format!("{name}/") } else { name });
        }
        items.sort();
        Ok::<_, anyhow::Error>(FsToolResponse {
            ok: true,
            summary: format!(
                "目录列表：{}",
                shared::display_rel_path_with(&full_path, &base)
            ),
            stdout: items.join("\n"),
            exit_code: 0,
            ..Default::default()
        })
    })();
    result.unwrap_or_else(|e| error_response("list_dir", e))
}

// ── tree_dir ─────────────────────────────────────────────────

pub fn handle_tree_dir(req: TreeDirRequest) -> FsToolResponse {
    let result = (|| {
        let base = base_of(&req.access)?;
        let path = req.path.as_deref().unwrap_or(".");
        let max_depth = if req.max_depth == 0 {
            DEFAULT_TREE_MAX_DEPTH
        } else {
            req.max_depth.min(MAX_TREE_MAX_DEPTH)
        };
        let full_path = shared::resolve_read_path_with(path, &base)?;
        ensure_dir_target(&full_path)?;
        let rel = shared::display_rel_path_with(&full_path, &base);
        let mut lines = vec![if rel == "." {
            "./".to_string()
        } else {
            format!("{rel}/")
        }];
        let mut visited = 0usize;
        let mut truncated = false;
        append_tree_lines(
            &full_path,
            0,
            max_depth,
            "",
            &mut lines,
            &mut visited,
            &mut truncated,
        )?;
        if truncated {
            lines.push(format!(
                "...(节点数量超过限制，已截断，max_nodes={MAX_TREE_NODES})"
            ));
        }
        Ok(FsToolResponse {
            ok: true,
            summary: format!("目录树：{} (max_depth={max_depth})", rel),
            stdout: lines.join("\n"),
            exit_code: 0,
            ..Default::default()
        })
    })();
    result.unwrap_or_else(|e| error_response("tree_dir", e))
}

// ── read_file ────────────────────────────────────────────────

pub fn handle_read_file(req: ReadFileRequest) -> FsToolResponse {
    let result = (|| {
        let base = base_of(&req.access)?;
        let start_line = req.start_line.max(1);
        let max_lines = if req.max_lines == 0 {
            DEFAULT_READ_MAX_LINES
        } else {
            req.max_lines.clamp(1, MAX_READ_MAX_LINES)
        };
        let full_path = shared::resolve_read_path_with(&req.path, &base)?;
        // 读路径解析对不存在路径静默放行，这里必须区分"不存在"与"类型不对"，
        // 否则文件缺失会被误报成"目标不是文件"，误导调用方反复重试。
        match fs::metadata(&full_path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err(anyhow!("目标不是文件：{}", full_path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(anyhow!("文件不存在：{}", full_path.display()));
            }
            Err(e) => {
                return Err(anyhow!("无法访问目标文件：{}（{e}）", full_path.display()));
            }
        }
        let content = fs::read_to_string(&full_path)
            .with_context(|| format!("读取文件失败：{}", full_path.display()))?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start_idx = (start_line - 1).min(total);
        let end_idx = (start_idx + max_lines).min(total);
        let stdout = lines[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(idx, line)| format!("{:>6}\t{}", start_idx + idx + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(FsToolResponse {
            ok: true,
            summary: format!(
                "已读取文件：{} (range={}..{}, total_lines={})",
                shared::display_rel_path_with(&full_path, &base),
                start_idx + 1,
                end_idx,
                total
            ),
            stdout,
            exit_code: 0,
            ..Default::default()
        })
    })();
    result.unwrap_or_else(|e| error_response("read_file", e))
}

// ── write_file ───────────────────────────────────────────────

pub fn handle_write_file(req: WriteFileRequest) -> FsToolResponse {
    let (locked, unlocked, result) =
        with_write_lock(&req.access, std::slice::from_ref(&req.path), |base| {
            let full_path = shared::resolve_write_path_from_base(&req.path, base)?;
            if let Some(parent) = full_path.parent()
                && let Err(e) = fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录失败：{}", parent.display()))
            {
                return Err(e);
            }
            if req.append {
                use std::fs::OpenOptions;
                use std::io::Write;
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&full_path)
                    .with_context(|| format!("追加打开文件失败：{}", full_path.display()))?;
                file.write_all(req.content.as_bytes())
                    .with_context(|| format!("追加写入文件失败：{}", full_path.display()))?;
            } else {
                atomic_write_file(&full_path, req.content.as_bytes())?;
            }
            Ok(FsToolResponse {
                ok: true,
                summary: format!(
                    "文件写入成功：{} (mode={})",
                    shared::display_rel_path_with(&full_path, base),
                    if req.append {
                        "append"
                    } else {
                        "overwrite-atomic"
                    }
                ),
                stdout: format!("written_bytes={},append={}", req.content.len(), req.append),
                exit_code: 0,
                ..Default::default()
            })
        });
    finalize_write(result, locked, unlocked)
}

// ── replace_in_file ──────────────────────────────────────────

pub fn handle_replace_in_file(req: ReplaceInFileRequest) -> FsToolResponse {
    let (locked, unlocked, result) =
        with_write_lock(&req.access, std::slice::from_ref(&req.path), |base| {
            if req.old.is_empty() {
                return Err(anyhow!("replace_in_file old 参数不能为空"));
            }
            let full_path = shared::resolve_write_path_from_base(&req.path, base)?;
            if !full_path.is_file() {
                return Err(anyhow!("目标不是文件：{}", full_path.display()));
            }
            let content = fs::read_to_string(&full_path)
                .with_context(|| format!("读取文件失败：{}", full_path.display()))?;
            let mut old = req.old.clone();
            let mut new = req.new.clone();
            let line_ending = preferred_line_ending(&content);
            if new.contains('\n') || new.contains('\r') {
                new = normalize_line_endings(&new, line_ending);
            }
            let mut count = content.matches(&old).count();
            if count == 0 && (old.contains('\n') || old.contains('\r')) {
                old = normalize_line_endings(&old, line_ending);
                count = content.matches(&old).count();
            }
            if count == 0 {
                return Err(anyhow!("未找到待替换内容"));
            }
            if let Some(expected) = req.expected_count
                && count != expected
            {
                return Err(anyhow!(
                    "命中数量不符合预期：expected={expected}, actual={count}"
                ));
            }
            let (replaced, replaced_count) = if req.replace_all {
                (content.replace(&old, &new), count)
            } else {
                if count != 1 {
                    return Err(anyhow!(
                        "默认仅允许单点替换，当前命中 {} 处；如需全量替换请设置 replace_all=true",
                        count
                    ));
                }
                (content.replacen(&old, &new, 1), 1)
            };
            fs::write(&full_path, replaced.as_bytes())
                .with_context(|| format!("写入替换结果失败：{}", full_path.display()))?;
            Ok(FsToolResponse {
                ok: true,
                summary: format!(
                    "文件替换成功：{} (replacements={}, replace_all={})",
                    shared::display_rel_path_with(&full_path, base),
                    replaced_count,
                    req.replace_all
                ),
                exit_code: 0,
                ..Default::default()
            })
        });
    finalize_write(result, locked, unlocked)
}

// ── apply_patch ──────────────────────────────────────────────

pub fn handle_apply_patch(req: ApplyPatchRequest) -> FsToolResponse {
    // 先解析出本次补丁涉及的所有目标路径（用于全有或全无加锁）。
    let targets_result = (|| {
        let base = base_of(&req.access)?;
        let workdir_raw = req.workdir.as_deref();
        let effective_cwd = shared::resolve_effective_cwd_with(workdir_raw, &base)?;
        if req.patch.trim().is_empty() {
            return Err(anyhow!("apply_patch patch 内容不能为空"));
        }
        enumerate_patch_targets(&req.patch, &effective_cwd)
    })();
    let targets = match targets_result {
        Ok(t) => t,
        Err(e) => return error_response("apply_patch", e),
    };
    let target_strs: Vec<String> = targets
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let (locked, unlocked, result) = with_write_lock(&req.access, &target_strs, |base| {
        let workdir_raw = req.workdir.as_deref();
        let effective_cwd = shared::resolve_effective_cwd_with(workdir_raw, base)?;
        let stats = apply_unified_diff_patch(&req.patch, &effective_cwd, req.verify)?;
        let summary = format!(
            "补丁{}成功：add={}, delete={}, update={}, move={}",
            if req.verify { "校验" } else { "应用" },
            stats.added,
            stats.deleted,
            stats.updated,
            stats.moved
        );
        let stdout = json!({
            "verify": req.verify,
            "effective_cwd": effective_cwd.display().to_string(),
            "counts": {
                "add": stats.added,
                "delete": stats.deleted,
                "update": stats.updated,
                "move": stats.moved,
            },
            "files": stats.files,
        })
        .to_string();
        Ok(FsToolResponse {
            ok: true,
            summary,
            stdout: shared::truncate_output(&stdout),
            exit_code: 0,
            ..Default::default()
        })
    });
    finalize_write(result, locked, unlocked)
}

// ── 写工具锁包装 ─────────────────────────────────────────────

/// 写工具统一锁包装：对目标路径加锁 → 执行业务 → 解锁。
///
/// 锁在本次调用内 acquire/release，与写操作绑定（一次 IPC 完成加锁+写+解锁，
/// 避免锁泄漏）。返回 `(locked_paths, unlocked_paths, 业务结果)`。
///
/// 注意：`raw_paths` 是**解析前**的原始路径字符串，需先解析并
/// canonicalize_for_lock 规范化后才能加锁（与原实现一致，防软链接/.. 绕过）。
fn with_write_lock<F>(
    access: &FsAccessContext,
    raw_paths: &[String],
    body: F,
) -> (Vec<PathBuf>, Vec<PathBuf>, Result<FsToolResponse>)
where
    F: FnOnce(&Path) -> Result<FsToolResponse>,
{
    let base = match base_of(access) {
        Ok(b) => b,
        Err(e) => return (Vec::new(), Vec::new(), Err(e)),
    };

    // 解析并规范化每个写路径为锁表 key（写路径在 body 内再解析一次）。
    // 注意：这里只为加锁取得稳定 key，用 resolve_write 解析真实写路径再 canonicalize_for_lock。
    let mut keys = Vec::with_capacity(raw_paths.len());
    for raw in raw_paths {
        match shared::resolve_write_path_from_base(raw, &base) {
            Ok(resolved) => keys.push(canonicalize_for_lock(&resolved)),
            Err(e) => return (Vec::new(), Vec::new(), Err(e)),
        }
    }

    if keys.is_empty() {
        // 无可加锁目标，直接执行。
        return (Vec::new(), Vec::new(), body(&base));
    }

    let now = chrono::Local::now().naive_local();
    let operation_id = match FileLockTable::acquire(&keys, &now) {
        Ok(id) => id,
        Err(e) => return (Vec::new(), Vec::new(), Err(anyhow!(e))),
    };
    let locked: Vec<PathBuf> = keys.clone();

    let result = body(&base);

    // 无论成功/失败都释放本次操作取得的锁。release 内部靠 operation_id 校验，
    // 若某路径已被新操作接管（旧操作超时后），不会误删，也不会出现在 released 里。
    let unlocked = FileLockTable::release(&keys, &operation_id);
    (locked, unlocked, result)
}

/// 把锁包装的结果组装成最终响应：业务结果失败时仍带上 locked/unlocked 路径，
/// 这样 wasm 侧无论业务成败都能正确发 locked/unlocked 事件。
fn finalize_write(
    result: Result<FsToolResponse>,
    locked: Vec<PathBuf>,
    unlocked: Vec<PathBuf>,
) -> FsToolResponse {
    let locked_str: Vec<String> = locked.iter().map(|p| p.display().to_string()).collect();
    let unlocked_str: Vec<String> = unlocked.iter().map(|p| p.display().to_string()).collect();
    match result {
        Ok(mut resp) => {
            resp.locked_paths = locked_str;
            resp.unlocked_paths = unlocked_str;
            resp
        }
        Err(e) => {
            let resp = error_response("write", e);
            FsToolResponse {
                locked_paths: locked_str,
                unlocked_paths: unlocked_str,
                ..resp
            }
        }
    }
}

// ── 辅助函数 ─────────────────────────────────────────────────

/// 目录类读工具的目标判定：区分目录不存在 / 目标不是目录 / 无法访问
/// （读路径解析对不存在路径静默放行，不能依赖单一 is_dir 判定）。
fn ensure_dir_target(full_path: &Path) -> Result<()> {
    match fs::metadata(full_path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(anyhow!("目标不是目录：{}", full_path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(anyhow!("目录不存在：{}", full_path.display()))
        }
        Err(e) => Err(anyhow!("无法访问目标目录：{}（{e}）", full_path.display())),
    }
}

fn error_response(tool: &str, e: anyhow::Error) -> FsToolResponse {
    // `{:#}` 展开完整错误链，保留底层 IO 原因（如沙箱拒绝时的
    // Operation not permitted），否则只剩外层上下文。
    let summary = format!("{tool} 失败：{e:#}");
    FsToolResponse {
        ok: false,
        summary: summary.clone(),
        stderr: summary,
        exit_code: 1,
        ..Default::default()
    }
}

fn atomic_write_file(path: &Path, content: &[u8]) -> Result<()> {
    // TODO: 当前是 remove→rename 非原子写，与原 fs 实现一致。
    // 未来可考虑改为 rename 覆盖（Unix 原子），属行为变更需单独评估。
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("无法确定目标文件父目录：{}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("目标文件名非法：{}", path.display()))?;
    let temp_path = parent.join(format!(".{}.tmp-{}", file_name, scru128::new()));
    fs::write(&temp_path, content)
        .with_context(|| format!("写入临时文件失败：{}", temp_path.display()))?;
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("删除旧文件失败：{}", path.display()))?;
    }
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "原子替换失败：temp={}, target={}",
            temp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn append_tree_lines(
    path: &Path,
    current_depth: usize,
    max_depth: usize,
    prefix: &str,
    lines: &mut Vec<String>,
    visited: &mut usize,
    truncated: &mut bool,
) -> Result<()> {
    if current_depth >= max_depth {
        return Ok(());
    }
    if *visited >= MAX_TREE_NODES {
        *truncated = true;
        return Ok(());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("读取目录失败：{}", path.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录项失败：{}", path.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry
            .file_type()
            .with_context(|| format!("读取目录项类型失败：{}", path.display()))?
            .is_dir();
        entries.push((name, is_dir, entry.path()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let total = entries.len();
    for (idx, (name, is_dir, child_path)) in entries.into_iter().enumerate() {
        if *visited >= MAX_TREE_NODES {
            *truncated = true;
            return Ok(());
        }

        *visited += 1;
        let last = idx + 1 == total;
        let branch = if last { "`-- " } else { "|-- " };
        let display = if is_dir { format!("{name}/") } else { name };
        lines.push(format!("{prefix}{branch}{display}"));

        if is_dir {
            let next_prefix = if last {
                format!("{prefix}    ")
            } else {
                format!("{prefix}|   ")
            };
            append_tree_lines(
                &child_path,
                current_depth + 1,
                max_depth,
                &next_prefix,
                lines,
                visited,
                truncated,
            )?;
            if *truncated {
                return Ok(());
            }
        }
    }
    Ok(())
}

// ── apply_patch 实现（unified diff）──────────────────────────

#[derive(Default)]
struct PatchStats {
    added: usize,
    deleted: usize,
    updated: usize,
    moved: usize,
    files: Vec<Value>,
}

fn apply_unified_diff_patch(patch: &str, effective_cwd: &Path, verify: bool) -> Result<PatchStats> {
    use diffy::{Patch, apply as diffy_apply};

    let sections = split_unified_diff_sections(patch)?;
    let mut stats = PatchStats::default();

    for section in &sections {
        let parsed =
            Patch::from_str(section).map_err(|err| anyhow!("解析 unified diff 失败：{err}"))?;
        let original = normalize_diff_filename(parsed.original().unwrap_or_default())?;
        let modified = normalize_diff_filename(parsed.modified().unwrap_or_default())?;

        let is_add = original == "/dev/null" && modified != "/dev/null";
        let is_delete = modified == "/dev/null" && original != "/dev/null";

        if is_add {
            let target = shared::resolve_write_path_from_base(&modified, effective_cwd)?;
            if target.exists() {
                return Err(anyhow!("新增文件已存在：{}", target.display()));
            }
            let content =
                diffy_apply("", &parsed).map_err(|err| anyhow!("新增文件补丁应用失败：{err}"))?;
            if !verify {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|err| anyhow!("创建目录失败：{}，{err}", parent.display()))?;
                }
                fs::write(&target, content.as_bytes())
                    .map_err(|err| anyhow!("写入新增文件失败：{}，{err}", target.display()))?;
            }
            stats.added += 1;
            stats.files.push(json!({
                "action": "add",
                "target": shared::display_rel_path_with(&target, effective_cwd),
            }));
            continue;
        }

        if is_delete {
            let source = shared::resolve_write_path_from_base(&original, effective_cwd)?;
            if !source.is_file() {
                return Err(anyhow!("删除目标不是文件：{}", source.display()));
            }
            let base_content = fs::read_to_string(&source)
                .map_err(|err| anyhow!("读取删除目标失败：{}，{err}", source.display()))?;
            let content = apply_text_patch(&base_content, &parsed, "删除补丁应用失败")?;
            if !content.is_empty() {
                return Err(anyhow!(
                    "删除补丁校验失败：应用后内容非空：{}",
                    source.display()
                ));
            }
            if !verify {
                fs::remove_file(&source)
                    .map_err(|err| anyhow!("删除文件失败：{}，{err}", source.display()))?;
            }
            stats.deleted += 1;
            stats.files.push(json!({
                "action": "delete",
                "source": shared::display_rel_path_with(&source, effective_cwd),
            }));
            continue;
        }

        let source = shared::resolve_write_path_from_base(&original, effective_cwd)?;
        let target = shared::resolve_write_path_from_base(&modified, effective_cwd)?;
        if !source.is_file() {
            return Err(anyhow!("修改目标不是文件：{}", source.display()));
        }
        let base_content = fs::read_to_string(&source)
            .map_err(|err| anyhow!("读取文件失败：{}，{err}", source.display()))?;
        let content = apply_text_patch(&base_content, &parsed, "修改补丁应用失败")?;

        if !verify {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| anyhow!("创建目录失败：{}，{err}", parent.display()))?;
            }
            fs::write(&target, content.as_bytes())
                .map_err(|err| anyhow!("写入文件失败：{}，{err}", target.display()))?;
            if source != target {
                fs::remove_file(&source)
                    .map_err(|err| anyhow!("删除原文件失败：{}，{err}", source.display()))?;
            }
        }

        stats.updated += 1;
        if source != target {
            stats.moved += 1;
        }
        stats.files.push(json!({
            "action": if source == target { "update" } else { "move_update" },
            "source": shared::display_rel_path_with(&source, effective_cwd),
            "target": shared::display_rel_path_with(&target, effective_cwd),
        }));
    }

    Ok(stats)
}

fn apply_text_patch(
    base_content: &str,
    patch: &diffy::Patch<'_, str>,
    error_context: &str,
) -> Result<String> {
    let crlf = uses_crlf_line_endings(base_content);
    let normalized;
    let base = if crlf {
        normalized = base_content.replace("\r\n", "\n");
        normalized.as_str()
    } else {
        base_content
    };
    let content = diffy::apply(base, patch).map_err(|err| anyhow!("{error_context}：{err}"))?;
    Ok(if crlf {
        content.replace('\n', "\r\n")
    } else {
        content
    })
}

fn preferred_line_ending(content: &str) -> &'static str {
    if uses_crlf_line_endings(content) {
        "\r\n"
    } else {
        "\n"
    }
}

fn normalize_line_endings(value: &str, line_ending: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', line_ending)
}

fn uses_crlf_line_endings(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut saw_line_ending = false;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'\n' => {
                saw_line_ending = true;
                if index == 0 || bytes[index - 1] != b'\r' {
                    return false;
                }
            }
            b'\r' if bytes.get(index + 1) != Some(&b'\n') => return false,
            _ => {}
        }
    }
    saw_line_ending
}

fn split_unified_diff_sections(patch: &str) -> Result<Vec<String>> {
    let lines = patch.lines().collect::<Vec<_>>();
    if lines.len() < 3 {
        return Err(anyhow!("unified diff 内容过短，无法解析"));
    }

    let mut section_starts = Vec::new();
    for idx in 0..(lines.len() - 1) {
        if lines[idx].starts_with("--- ") && lines[idx + 1].starts_with("+++ ") {
            section_starts.push(idx);
        }
    }
    if section_starts.is_empty() {
        return Err(anyhow!("unified diff 缺少文件头（--- / +++）"));
    }

    let mut sections = Vec::new();
    for (index, start) in section_starts.iter().enumerate() {
        let end = section_starts
            .get(index + 1)
            .copied()
            .unwrap_or(lines.len());
        let mut section = lines[*start..end].join("\n");
        if !section.ends_with('\n') {
            section.push('\n');
        }
        sections.push(section);
    }
    Ok(sections)
}

/// 枚举 patch 涉及的所有目标路径（写前加锁用）。
///
/// - add：目标 = modified
/// - delete：目标 = original
/// - update / move：目标 = original + modified（move 涉及删旧建新两个路径）
fn enumerate_patch_targets(patch: &str, effective_cwd: &Path) -> Result<Vec<PathBuf>> {
    use diffy::Patch;

    let sections = split_unified_diff_sections(patch)?;
    let mut targets = Vec::new();
    for section in &sections {
        let parsed =
            Patch::from_str(section).map_err(|err| anyhow!("解析 unified diff 失败：{err}"))?;
        let original = normalize_diff_filename(parsed.original().unwrap_or_default())?;
        let modified = normalize_diff_filename(parsed.modified().unwrap_or_default())?;

        let is_add = original == "/dev/null" && modified != "/dev/null";
        let is_delete = modified == "/dev/null" && original != "/dev/null";

        if is_add {
            targets.push(shared::resolve_write_path_from_base(
                &modified,
                effective_cwd,
            )?);
        } else if is_delete {
            targets.push(shared::resolve_write_path_from_base(
                &original,
                effective_cwd,
            )?);
        } else {
            targets.push(shared::resolve_write_path_from_base(
                &original,
                effective_cwd,
            )?);
            if original != modified {
                targets.push(shared::resolve_write_path_from_base(
                    &modified,
                    effective_cwd,
                )?);
            }
        }
    }
    Ok(targets)
}

fn normalize_diff_filename(raw: &str) -> Result<String> {
    let path = raw.trim();
    if path.is_empty() {
        return Err(anyhow!("unified diff 文件路径为空"));
    }
    if path == "/dev/null" {
        return Ok(path.to_string());
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .trim();
    if path.is_empty() {
        return Err(anyhow!("unified diff 文件路径非法"));
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(root: &Path) -> FsAccessContext {
        FsAccessContext {
            workspace: Some(root.to_string_lossy().into_owned()),
            full_trust: false,
        }
    }

    #[test]
    fn list_dir_missing_path_reports_not_found() {
        let root = tempfile::tempdir().unwrap();

        let response = handle_list_dir(ListDirRequest {
            path: Some("docs".to_string()),
            access: access(root.path()),
        });

        assert!(!response.ok);
        assert!(
            response.stderr.contains("目录不存在"),
            "stderr={}",
            response.stderr
        );
    }

    #[test]
    fn list_dir_file_target_reports_not_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "hi\n").unwrap();

        let response = handle_list_dir(ListDirRequest {
            path: Some("README.md".to_string()),
            access: access(root.path()),
        });

        assert!(!response.ok);
        assert!(
            response.stderr.contains("目标不是目录"),
            "stderr={}",
            response.stderr
        );
    }

    #[test]
    fn read_file_missing_path_reports_not_found() {
        let root = tempfile::tempdir().unwrap();

        let response = handle_read_file(ReadFileRequest {
            path: "docs/requirements.md".to_string(),
            access: access(root.path()),
            ..Default::default()
        });

        assert!(!response.ok);
        assert!(
            response.stderr.contains("文件不存在"),
            "stderr={}",
            response.stderr
        );
    }

    #[test]
    fn read_file_directory_target_reports_not_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("docs")).unwrap();

        let response = handle_read_file(ReadFileRequest {
            path: "docs".to_string(),
            access: access(root.path()),
            ..Default::default()
        });

        assert!(!response.ok);
        assert!(
            response.stderr.contains("目标不是文件"),
            "stderr={}",
            response.stderr
        );
    }

    #[test]
    fn read_file_reads_markdown_content() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "第一行\n第二行\n").unwrap();

        let response = handle_read_file(ReadFileRequest {
            path: "README.md".to_string(),
            access: access(root.path()),
            ..Default::default()
        });

        assert!(response.ok, "{}", response.stderr);
        assert!(response.stdout.contains("第一行"));
    }

    #[test]
    fn replace_in_file_matches_lf_text_and_preserves_crlf() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("windows.txt");
        fs::write(&path, b"alpha\r\nbeta\r\ngamma\r\n").unwrap();

        let response = handle_replace_in_file(ReplaceInFileRequest {
            path: "windows.txt".to_string(),
            old: "alpha\nbeta".to_string(),
            new: "alpha\nBETA".to_string(),
            expected_count: Some(1),
            access: access(root.path()),
            ..Default::default()
        });

        assert!(response.ok, "{}", response.stderr);
        assert_eq!(fs::read(&path).unwrap(), b"alpha\r\nBETA\r\ngamma\r\n");
    }

    #[test]
    fn replace_in_file_matches_crlf_text_and_preserves_lf() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("unix.txt");
        fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();

        let response = handle_replace_in_file(ReplaceInFileRequest {
            path: "unix.txt".to_string(),
            old: "alpha\r\nbeta".to_string(),
            new: "alpha\r\nBETA".to_string(),
            expected_count: Some(1),
            access: access(root.path()),
            ..Default::default()
        });

        assert!(response.ok, "{}", response.stderr);
        assert_eq!(fs::read(&path).unwrap(), b"alpha\nBETA\ngamma\n");
    }

    #[test]
    fn apply_patch_updates_and_preserves_crlf() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("windows.txt");
        fs::write(&path, b"alpha\r\nbeta\r\ngamma\r\n").unwrap();
        let patch =
            "--- a/windows.txt\n+++ b/windows.txt\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";

        let response = handle_apply_patch(ApplyPatchRequest {
            patch: patch.to_string(),
            access: access(root.path()),
            ..Default::default()
        });

        assert!(response.ok, "{}", response.stderr);
        assert_eq!(fs::read(&path).unwrap(), b"alpha\r\nBETA\r\ngamma\r\n");
    }

    #[test]
    fn apply_patch_updates_and_preserves_lf() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("unix.txt");
        fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
        let patch =
            "--- a/unix.txt\n+++ b/unix.txt\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";

        let response = handle_apply_patch(ApplyPatchRequest {
            patch: patch.to_string(),
            access: access(root.path()),
            ..Default::default()
        });

        assert!(response.ok, "{}", response.stderr);
        assert_eq!(fs::read(&path).unwrap(), b"alpha\nBETA\ngamma\n");
    }
}
