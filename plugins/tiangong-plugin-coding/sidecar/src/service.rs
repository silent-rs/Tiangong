use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use tiangong_plugin_coding_protocol::*;
use tiangong_plugin_sidecar::env::PLUGIN_DATA_DIR_ENV;
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};

const PROJECT_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "Gemfile",
    "mix.exs",
    "deno.json",
    "deno.jsonc",
    "Makefile",
    "Justfile",
];
const RULE_FILE_NAMES: &[&str] = &[
    "agents.md",
    "claude.md",
    "contributing.md",
    "development.md",
    "instructions.md",
    "copilot-instructions.md",
];
const WORKFLOW_FILE_TOKENS: &[&str] = &[
    "plan",
    "planning",
    "roadmap",
    "task",
    "tasks",
    "todo",
    "backlog",
    "milestone",
    "milestones",
    "requirement",
    "requirements",
    "spec",
    "progress",
    "计划",
    "规划",
    "路线",
    "任务",
    "需求",
    "进度",
];
const IGNORED_DISCOVERY_DIRS: &[&str] = &[
    ".git",
    ".idea",
    ".venv",
    ".vscode",
    "build",
    "dist",
    "node_modules",
    "target",
    "vendor",
];
/// 单条 git 命令的执行上限。
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// 一次业务请求内全部 git 调用的总预算，避免大仓库上串行等待过久。
const GIT_BUDGET: Duration = Duration::from_secs(20);
/// 每个工作区保留的进度记录条数上限（按任务分槽位，超出淘汰最旧）。
const MAX_CHECKPOINT_ENTRIES: usize = 20;
/// preflight 中未提交文件清单的截断上限。
const UNCOMMITTED_FILES_LIMIT: usize = 50;
/// 规则文件摘要：每文件保留行数与全局字节上限。
const RULE_EXCERPT_LINES: usize = 40;
const RULE_EXCERPT_TOTAL_BYTES: usize = 8 * 1024;
/// 规则文件读取大小上限。
const RULE_FILE_READ_LIMIT: u64 = 512 * 1024;
/// review 空白错误输出截断上限。
const WHITESPACE_ERROR_LIMIT: usize = 20;
/// 直接工作会被提醒的主分支名。
const TRUNK_BRANCHES: &[&str] = &["main", "master", "develop"];
/// 常见分支名前缀约定。
const BRANCH_TYPE_PREFIXES: &[&str] = &[
    "feature", "feat", "fix", "bugfix", "hotfix", "perf", "refactor", "docs", "doc", "chore",
    "test", "release", "revert",
];
/// 规则文件中可识别为检查命令的程序名。
const CHECK_PROGRAMS: &[&str] = &[
    "cargo", "yarn", "npm", "pnpm", "bun", "make", "just", "go", "uv", "python", "python3", "deno",
    "gradle", "mvn", "ruff", "eslint", "tsc",
];
/// 检查命令需包含的关键词（程序名之外）。
const CHECK_KEYWORDS: &[&str] = &[
    "check",
    "clippy",
    "fmt",
    "format",
    "lint",
    "test",
    "typecheck",
    "type-check",
    "audit",
    "verify",
    "validate",
    "build",
];
/// Makefile/Justfile 中视为检查目标的词根。
const CHECK_TARGET_WORDS: &[&str] = &[
    "check",
    "lint",
    "test",
    "build",
    "typecheck",
    "fmt",
    "format",
    "audit",
];
/// 命令行允许出现的字符（超出则不提取，避免误抓含管道/替换的复杂命令）。
const SAFE_COMMAND_CHARS: &str = " _@%./:=+-";
/// 疑似密钥/凭据文件名片段。
const SECRET_FILE_HINTS: &[&str] = &[
    ".env",
    "id_rsa",
    "id_ed25519",
    "credential",
    "secret",
    "token",
    ".pem",
    ".p12",
    ".pfx",
    ".key",
];
/// 锁文件名单（按文件名精确匹配）。
const LOCKFILE_NAMES: &[&str] = &[
    "Cargo.lock",
    "yarn.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "poetry.lock",
    "uv.lock",
    "go.sum",
    "Gemfile.lock",
    "composer.lock",
    "flake.lock",
    "deno.lock",
    "Pipfile.lock",
];
/// CI 配置路径前缀/文件名。
const CI_CONFIG_HINTS: &[&str] = &[
    ".github/",
    ".gitlab-ci",
    ".circleci/",
    "Jenkinsfile",
    ".travis.yml",
    "azure-pipelines",
    "cloudbuild",
    ".drone.yml",
];
/// 部署配置路径线索。
const DEPLOY_CONFIG_HINTS: &[&str] = &[
    "Dockerfile",
    "docker-compose",
    "compose.yaml",
    "compose.yml",
    "k8s/",
    "helm/",
    "kustomize",
];

/// 分发错误分类：参数/输入问题映射 BadRequest，其余按服务错误处理。
#[derive(Debug)]
enum DispatchError {
    Bad(String),
    Internal(anyhow::Error),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::Bad(message) => write!(formatter, "{message}"),
            DispatchError::Internal(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for DispatchError {}

impl From<anyhow::Error> for DispatchError {
    fn from(error: anyhow::Error) -> Self {
        DispatchError::Internal(error)
    }
}

pub struct CodingService {
    data_dir: PathBuf,
}

impl CodingService {
    pub fn new() -> Result<Self> {
        let data_dir = coding_data_dir()?;
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("创建 Coding 数据目录失败: {}", data_dir.display()))?;
        Ok(Self { data_dir })
    }

    async fn dispatch_operation(
        &self,
        operation: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        macro_rules! decode {
            ($ty:ty) => {
                serde_json::from_value::<$ty>(payload)
                    .map_err(|error| DispatchError::Bad(format!("参数无效: {error}")))?
            };
        }
        match operation {
            HANDSHAKE_OPERATION => self.handshake().map_err(DispatchError::Internal),
            name if name == ProjectContext::NAME => {
                let request = decode!(WorkspaceRequest);
                serde_json::to_value(self.project_context(&request)?)
                    .context("序列化项目上下文失败")
                    .map_err(DispatchError::Internal)
            }
            name if name == Preflight::NAME => {
                let request = decode!(PreflightRequest);
                serde_json::to_value(self.preflight(&request)?)
                    .context("序列化开发前检查失败")
                    .map_err(DispatchError::Internal)
            }
            name if name == Checkpoint::NAME => {
                let request = decode!(CheckpointRequest);
                serde_json::to_value(self.save_checkpoint(&request)?)
                    .context("序列化进度记录失败")
                    .map_err(DispatchError::Internal)
            }
            name if name == Review::NAME => {
                let request = decode!(ReviewRequest);
                serde_json::to_value(review(&request)?)
                    .context("序列化交付审查失败")
                    .map_err(DispatchError::Internal)
            }
            name if name == Branches::NAME => {
                let request = decode!(BranchesRequest);
                serde_json::to_value(branches(&request)?)
                    .context("序列化分支列表失败")
                    .map_err(DispatchError::Internal)
            }
            name if name == Switch::NAME => {
                let request = decode!(SwitchRequest);
                serde_json::to_value(switch_branch(&request)?)
                    .context("序列化分支切换结果失败")
                    .map_err(DispatchError::Internal)
            }
            other => Err(DispatchError::Bad(format!("未知的 Coding 操作: {other}"))),
        }
    }

    fn handshake(&self) -> Result<serde_json::Value> {
        serde_json::to_value(HandshakeResponse {
            plugin_id: PLUGIN_ID.to_string(),
            plugin_version: PLUGIN_VERSION.to_string(),
            sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION.to_string(),
            business_protocol: CODING_PROTOCOL_VERSION,
            capabilities: vec![
                TOOL_PROJECT_CONTEXT.to_string(),
                TOOL_PREFLIGHT.to_string(),
                TOOL_CHECKPOINT.to_string(),
                TOOL_REVIEW.to_string(),
            ],
            instance_id: format!("coding-sidecar-{}", std::process::id()),
            status: ServiceStatus::Ready,
        })
        .context("序列化 Coding 握手失败")
    }

    async fn dispatch_request(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                "Coding sidecar 协议版本不匹配",
                false,
            );
        }
        match self
            .dispatch_operation(&request.operation, request.payload)
            .await
        {
            Ok(payload) => Response::success(&request_id, payload),
            Err(DispatchError::Bad(message)) => {
                Response::error(&request_id, ErrorCode::BadRequest, &message, false)
            }
            Err(DispatchError::Internal(error)) => Response::error(
                &request_id,
                ErrorCode::ServiceError,
                error.to_string(),
                false,
            ),
        }
    }

    fn save_checkpoint(
        &self,
        request: &CheckpointRequest,
    ) -> std::result::Result<CheckpointResponse, DispatchError> {
        let workspace = workspace_path(&request.workspace)?;
        let checkpoint_dir = self.data_dir.join("checkpoints");
        std::fs::create_dir_all(&checkpoint_dir)
            .context("创建 Coding 进度目录失败")
            .map_err(DispatchError::Internal)?;

        let checkpoint_path = self.checkpoint_path(&workspace);
        let (mut entries, _notice) = self.load_checkpoint_store(&checkpoint_path);
        let saved_at = timestamp_now();
        // 同任务旧记录先移除，保证每个任务只保留最新一条。
        entries.retain(|entry| entry.state.task.trim() != request.task.trim());
        entries.push(SavedCheckpoint {
            saved_at: saved_at.clone(),
            state: request.clone(),
        });
        let overflow = entries.len().saturating_sub(MAX_CHECKPOINT_ENTRIES);
        if overflow > 0 {
            entries.drain(..overflow);
        }
        let store = CheckpointStore { entries };
        let content = serde_json::to_vec_pretty(&store)
            .context("序列化进度记录失败")
            .map_err(DispatchError::Internal)?;
        // 同目录临时文件 + rename，避免崩溃留下截断的进度文件。
        let temporary = checkpoint_path.with_extension("json.tmp");
        std::fs::write(&temporary, content)
            .with_context(|| format!("写入进度记录失败: {}", temporary.display()))
            .map_err(DispatchError::Internal)?;
        std::fs::rename(&temporary, &checkpoint_path)
            .with_context(|| format!("落盘进度记录失败: {}", checkpoint_path.display()))
            .map_err(DispatchError::Internal)?;

        Ok(CheckpointResponse {
            saved_at,
            checkpoint_path: checkpoint_path.display().to_string(),
        })
    }

    fn checkpoint_path(&self, workspace: &Path) -> PathBuf {
        let digest = Sha256::digest(workspace.to_string_lossy().as_bytes());
        let workspace_key = &hex::encode(digest)[..24];
        self.data_dir
            .join("checkpoints")
            .join(format!("{workspace_key}.json"))
    }

    /// 读取进度存储；损坏时返回空存储并带提示（不静默丢失）。
    fn load_checkpoint_store(
        &self,
        checkpoint_path: &Path,
    ) -> (Vec<SavedCheckpoint>, Option<String>) {
        let Ok(content) = std::fs::read(checkpoint_path) else {
            return (Vec::new(), None);
        };
        if let Ok(store) = serde_json::from_slice::<CheckpointStore>(&content) {
            return (store.entries, None);
        }
        // 兼容旧版单条格式。
        if let Ok(legacy) = serde_json::from_slice::<SavedCheckpoint>(&content) {
            return (vec![legacy], None);
        }
        let notice = format!(
            "进度文件损坏或不可解析（{}），此前的任务进度已丢失",
            checkpoint_path.display()
        );
        tracing::warn!("{notice}");
        (Vec::new(), Some(notice))
    }

    /// 按任务恢复进度：精确匹配优先；无匹配时回退最近一条并标注不匹配。
    fn restore_checkpoint(
        &self,
        workspace: &Path,
        task: Option<&str>,
    ) -> (Option<SavedCheckpoint>, bool, Option<String>) {
        let (mut entries, notice) = self.load_checkpoint_store(&self.checkpoint_path(workspace));
        let Some(task) = task.map(str::trim).filter(|task| !task.is_empty()) else {
            return (None, false, notice);
        };
        if let Some(index) = entries
            .iter()
            .rposition(|entry| entry.state.task.trim() == task)
        {
            return (Some(entries.swap_remove(index)), false, notice);
        }
        match entries.last() {
            Some(latest) => (Some(latest.clone()), true, notice),
            None => (None, false, notice),
        }
    }

    fn project_context(
        &self,
        request: &WorkspaceRequest,
    ) -> std::result::Result<ProjectContextResponse, DispatchError> {
        let gathered = gather_context(request)?;
        let (checkpoint, task_mismatch, notice) =
            self.restore_checkpoint(&gathered.workspace, request.task.as_deref());
        Ok(ProjectContextResponse {
            workspace: gathered.workspace.display().to_string(),
            full_trust: request.full_trust,
            project_types: detect_project_types(&gathered.inventory.project_files),
            project_files: gathered.inventory.project_files,
            rule_files: gathered.inventory.rule_files,
            workflow_files: gathered.inventory.workflow_files,
            version_controlled: gathered.git.version_controlled,
            version_control_inspected: gathered.git.inspection_complete,
            git_branch: gathered.git.branch,
            has_uncommitted_changes: !gathered.git.changed_files.is_empty(),
            recommended_checks: gathered.recommended_checks,
            latest_checkpoint: checkpoint,
            latest_checkpoint_task_mismatch: task_mismatch,
            checkpoint_recovery_notice: notice,
        })
    }

    fn preflight(
        &self,
        request: &PreflightRequest,
    ) -> std::result::Result<PreflightResponse, DispatchError> {
        let gathered = gather_context(&WorkspaceRequest {
            workspace: request.workspace.clone(),
            full_trust: request.full_trust,
            task: Some(request.task.clone()),
        })?;
        let (checkpoint, task_mismatch, notice) =
            self.restore_checkpoint(&gathered.workspace, Some(request.task.as_str()));

        let mut blockers = Vec::new();
        let mut warnings = Vec::new();
        if request.task.trim().is_empty() {
            blockers.push("缺少开发任务说明".to_string());
        }
        if !gathered.git.inspection_complete {
            warnings
                .push("版本控制状态检查未完成，不能据此判断工作区是否存在未提交改动".to_string());
        } else if gathered.git.version_controlled {
            if gathered.git.branch.as_deref().is_some_and(is_trunk_branch) {
                warnings.push(format!(
                    "当前直接在主分支 {} 上工作；项目流程通常要求为每个任务创建独立分支（如 feature/、fix/）",
                    gathered.git.branch.clone().unwrap_or_default()
                ));
            } else if gathered
                .git
                .branch
                .as_deref()
                .is_some_and(|branch| !branch.starts_with("detached@"))
                && !matches_branch_convention(gathered.git.branch.as_deref().unwrap_or_default())
            {
                warnings.push(
                    "分支名不符合常见的 type/description 约定（feature/、fix/ 等），请确认是否符合项目分支规范"
                        .to_string(),
                );
            }
            if !gathered.git.changed_files.is_empty() {
                warnings.push(format!(
                    "工作区存在 {} 处未提交改动（见 uncommitted_files），修改前需确认归属",
                    gathered.git.changed_files.len()
                ));
            }
        }
        if gathered.recommended_checks.is_empty() {
            warnings.push(
                "未从项目配置发现可直接执行的检查命令，需要按仓库约定确定验证方式".to_string(),
            );
        }
        if let Some(notice) = notice {
            warnings.push(notice);
        }

        let uncommitted_files_total = gathered.git.changed_files.len();
        let uncommitted_files = gathered
            .git
            .changed_files
            .iter()
            .take(UNCOMMITTED_FILES_LIMIT)
            .cloned()
            .collect();
        let rule_file_excerpts =
            rule_file_excerpts(&gathered.workspace, &gathered.inventory.rule_files);

        // 任务有精确匹配的进度记录且带完成标准时，沿用记录的标准。
        let (completion_criteria, restored_checkpoint) = match checkpoint.as_ref() {
            Some(entry) if !task_mismatch && !entry.state.completion_criteria.is_empty() => {
                (entry.state.completion_criteria.clone(), true)
            }
            _ => (
                vec![
                    "任务目标和边界已经明确".to_string(),
                    "遵循当前项目自身的规则和工作流".to_string(),
                    "最终改动只包含任务所需内容".to_string(),
                    "实际验证结果通过".to_string(),
                ],
                false,
            ),
        };

        Ok(PreflightResponse {
            workspace: gathered.workspace.display().to_string(),
            task: request.task.trim().to_string(),
            version_controlled: gathered.git.version_controlled,
            version_control_inspected: gathered.git.inspection_complete,
            git_branch: gathered.git.branch,
            has_uncommitted_changes: !gathered.git.changed_files.is_empty(),
            uncommitted_files,
            uncommitted_files_total,
            blockers,
            warnings,
            completion_criteria,
            rule_file_excerpts,
            restored_checkpoint,
        })
    }
}
#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for CodingService {
    async fn dispatch(&self, request: Request) -> Response {
        self.dispatch_request(request).await
    }
}

fn coding_data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(PLUGIN_DATA_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let runtime_dir = tiangong_plugin_sidecar::endpoint::runtime_dir(PLUGIN_ID)?;
    let plugin_dir = runtime_dir
        .parent()
        .ok_or_else(|| anyhow!("Coding 运行目录缺少父目录"))?;
    Ok(plugin_dir.join("data"))
}

fn workspace_path(path: &str) -> std::result::Result<PathBuf, DispatchError> {
    let path = PathBuf::from(path.trim());
    if path.as_os_str().is_empty() || !path.is_dir() {
        return Err(DispatchError::Bad("工作区不存在或不是目录".to_string()));
    }
    path.canonicalize()
        .context("解析工作区路径失败")
        .map_err(DispatchError::Internal)
}

fn timestamp_now() -> String {
    chrono::Local::now()
        .naive_local()
        .format("%Y-%m-%dT%H:%M:%S%.3f")
        .to_string()
}

fn is_trunk_branch(branch: &str) -> bool {
    TRUNK_BRANCHES.contains(&branch)
}

fn matches_branch_convention(branch: &str) -> bool {
    branch
        .split('/')
        .next()
        .is_some_and(|first| BRANCH_TYPE_PREFIXES.contains(&first))
}

/// 单次请求聚合的项目上下文原料。
struct GatheredContext {
    workspace: PathBuf,
    inventory: FileInventory,
    git: GitContext,
    recommended_checks: Vec<RecommendedCheck>,
}

fn gather_context(
    request: &WorkspaceRequest,
) -> std::result::Result<GatheredContext, DispatchError> {
    let workspace = workspace_path(&request.workspace)?;
    let deadline = git_deadline();
    let inventory = discover_files(&workspace, deadline);
    let recommended_checks = recommend_checks(&workspace, &inventory);
    let git = git_context(&workspace, deadline);
    Ok(GatheredContext {
        workspace,
        inventory,
        git,
        recommended_checks,
    })
}

/// 发现的三类项目文件。
#[derive(Default)]
struct FileInventory {
    project_files: Vec<String>,
    rule_files: Vec<String>,
    workflow_files: Vec<String>,
}

/// 文件发现：git 仓库内用 ls-files（尊重 .gitignore、天然覆盖子目录），
/// 非 git 或调用失败时回退到目录扫描（根 + 两层）。
fn discover_files(workspace: &Path, deadline: Instant) -> FileInventory {
    if let Some(files) = git_ls_files(workspace, deadline) {
        return inventory_from_paths(&files);
    }
    scan_inventory(workspace)
}

fn git_ls_files(workspace: &Path, deadline: Instant) -> Option<Vec<String>> {
    let CommandOutput::Success(output) = git_read(
        workspace,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        deadline,
    ) else {
        return None;
    };
    Some(
        output
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| normalize_relative_path(&String::from_utf8_lossy(path)))
            .filter(|path| !path.is_empty())
            .collect(),
    )
}

fn inventory_from_paths(files: &[String]) -> FileInventory {
    let mut inventory = FileInventory::default();
    for file in files {
        let path = Path::new(file);
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if PROJECT_MANIFESTS.contains(&name) {
            inventory.project_files.push(file.clone());
        } else if RULE_FILE_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
            inventory.rule_files.push(file.clone());
        } else if is_workflow_markdown(path) {
            inventory.workflow_files.push(file.clone());
        }
    }
    inventory
}

/// 目录扫描回退：工作区根 + 两层子目录（排除常见无关目录）。
fn discovery_roots(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = vec![workspace.to_path_buf()];
    let mut frontier = vec![workspace.to_path_buf()];
    for _ in 0..2 {
        let mut next = Vec::new();
        for directory in frontier {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if IGNORED_DISCOVERY_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                roots.push(entry.path());
                next.push(entry.path());
            }
        }
        frontier = next;
    }
    roots.sort();
    roots
}

fn scan_inventory(workspace: &Path) -> FileInventory {
    let mut inventory = FileInventory::default();
    let roots = discovery_roots(workspace);
    let mut project = BTreeSet::new();
    let mut rule = BTreeSet::new();
    let mut workflow = BTreeSet::new();
    for root in &roots {
        for name in PROJECT_MANIFESTS {
            let path = root.join(name);
            if path.is_file() {
                project.insert(relative_path(workspace, &path));
            }
        }
    }
    for root in &roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if RULE_FILE_NAMES.contains(&name.as_str()) {
                rule.insert(relative_path(workspace, &path));
            } else if is_workflow_markdown(&path) {
                workflow.insert(relative_path(workspace, &path));
            }
        }
    }
    inventory.project_files = project.into_iter().collect();
    inventory.rule_files = rule.into_iter().collect();
    inventory.workflow_files = workflow.into_iter().collect();
    inventory
}

fn is_workflow_markdown(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_none_or(|value| !value.eq_ignore_ascii_case("md"))
    {
        return false;
    }
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tokens = stem
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    tokens
        .iter()
        .any(|token| WORKFLOW_FILE_TOKENS.contains(token))
        || WORKFLOW_FILE_TOKENS
            .iter()
            .any(|token| !token.is_ascii() && stem.contains(token))
}

fn detect_project_types(project_files: &[String]) -> Vec<String> {
    let mut types = BTreeSet::new();
    for file in project_files {
        let name = Path::new(file)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        match name {
            "Cargo.toml" => {
                types.insert("rust".to_string());
            }
            "package.json" | "deno.json" | "deno.jsonc" => {
                types.insert("javascript-typescript".to_string());
            }
            "pyproject.toml" => {
                types.insert("python".to_string());
            }
            "go.mod" => {
                types.insert("go".to_string());
            }
            "pom.xml" | "build.gradle" | "build.gradle.kts" => {
                types.insert("jvm".to_string());
            }
            "composer.json" => {
                types.insert("php".to_string());
            }
            "Gemfile" => {
                types.insert("ruby".to_string());
            }
            "mix.exs" => {
                types.insert("elixir".to_string());
            }
            _ => {}
        }
    }
    types.into_iter().collect()
}

fn recommend_checks(workspace: &Path, inventory: &FileInventory) -> Vec<RecommendedCheck> {
    let mut checks: Vec<RecommendedCheck> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut push = |cwd: String, command: String, source: &str| {
        if seen.insert((cwd.clone(), command.clone())) {
            checks.push(RecommendedCheck {
                cwd,
                command,
                source: source.to_string(),
            });
        }
    };

    // 规则文件里写明的检查命令最贴合仓库约定，放在最前。
    for rule in &inventory.rule_files {
        for command in checks_from_rule_file(workspace, rule) {
            push(
                Path::new(rule)
                    .parent()
                    .map(|parent| normalize_relative_path(&parent.to_string_lossy()))
                    .filter(|parent| !parent.is_empty())
                    .unwrap_or_else(|| ".".to_string()),
                command,
                "rule_file",
            );
        }
    }

    let has_root_cargo = inventory
        .project_files
        .iter()
        .any(|file| file == "Cargo.toml");
    for file in &inventory.project_files {
        let relative = Path::new(file);
        let name = relative
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let cwd = if parent.as_os_str().is_empty() {
            ".".to_string()
        } else {
            normalize_relative_path(&parent.to_string_lossy())
        };
        match name {
            "Cargo.toml" if !has_root_cargo || file == "Cargo.toml" => {
                push(cwd, "cargo check".to_string(), "manifest");
            }
            "package.json" => {
                let manifest = workspace.join(relative);
                for command in package_checks(&manifest) {
                    push(cwd.clone(), command, "manifest");
                }
            }
            "go.mod" => {
                push(cwd, "go test ./...".to_string(), "manifest");
            }
            "pyproject.toml" => {
                let manifest = workspace.join(relative);
                let content = std::fs::read_to_string(&manifest).unwrap_or_default();
                if content.contains("pytest") {
                    let command = if manifest
                        .parent()
                        .is_some_and(|directory| directory.join("uv.lock").is_file())
                    {
                        "uv run pytest"
                    } else {
                        "python -m pytest"
                    };
                    push(cwd, command.to_string(), "manifest");
                }
            }
            "Makefile" => {
                for target in task_targets(&workspace.join(relative), makefile_target_names) {
                    push(cwd.clone(), format!("make {target}"), "makefile");
                }
            }
            "Justfile" => {
                for target in task_targets(&workspace.join(relative), justfile_target_names) {
                    push(cwd.clone(), format!("just {target}"), "justfile");
                }
            }
            _ => {}
        }
    }
    checks
}

/// 从规则文件内容中提取检查命令行（支持 `&&` 链拆分与 markdown 列表装饰）。
fn checks_from_rule_file(workspace: &Path, rule: &str) -> Vec<String> {
    let path = workspace.join(rule);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Vec::new();
    };
    if !metadata.is_file() || metadata.len() > RULE_FILE_READ_LIMIT {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    command_lines_from_rule_text(&content)
}

fn command_lines_from_rule_text(content: &str) -> Vec<String> {
    let mut commands = Vec::new();
    for raw_line in content.lines() {
        let stripped = strip_markdown_decoration(raw_line);
        for segment in stripped.split("&&") {
            let command = segment.trim().trim_matches('`').trim();
            if is_check_command(command) {
                commands.push(command.to_string());
            }
        }
    }
    commands
}

fn strip_markdown_decoration(line: &str) -> &str {
    let mut text = line.trim();
    loop {
        let trimmed = text
            .trim_start_matches(['-', '*', '+', '>', ' ', '\t'])
            .trim_start_matches(|character: char| character.is_ascii_digit())
            .trim_start_matches(['.', ')', ' ', '\t']);
        if trimmed.len() == text.len() {
            return text;
        }
        text = trimmed;
    }
}

fn is_check_command(command: &str) -> bool {
    let Some(program) = command.split_whitespace().next() else {
        return false;
    };
    if !CHECK_PROGRAMS.contains(&program) {
        return false;
    }
    if !CHECK_KEYWORDS
        .iter()
        .any(|keyword| command.contains(keyword))
    {
        return false;
    }
    // 只提取由安全字符组成的简单命令，避免抓进管道/替换/重定向复合命令。
    command.chars().all(|character| {
        character.is_ascii_alphanumeric() || SAFE_COMMAND_CHARS.contains(character)
    })
}

/// Makefile 目标行解析：`name:` 形式且排除赋值与 recipe 行。
fn makefile_target_names(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in content.lines() {
        if line.starts_with(['\t', '#']) {
            continue;
        }
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.is_empty() || name.contains(['=', ' ', '\t']) || rest.starts_with('=') {
            continue;
        }
        if name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        }) {
            names.push(name.to_string());
        }
    }
    names
}

/// Justfile recipe 解析：顶层 `name:` 形式（带参数的 recipe 保守跳过）。
fn justfile_target_names(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in content.lines() {
        if line.starts_with([' ', '\t', '#', '@']) {
            continue;
        }
        let Some(head) = line.split_once(':').map(|(head, _)| head) else {
            continue;
        };
        let name = head.trim();
        if name.is_empty() || name.contains(' ') {
            continue;
        }
        if name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        }) {
            names.push(name.to_string());
        }
    }
    names
}

fn task_targets(path: &Path, parse: fn(&str) -> Vec<String>) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse(&content)
        .into_iter()
        .filter(|name| {
            CHECK_TARGET_WORDS.iter().any(|word| {
                name == word
                    || name.starts_with(&format!("{word}-"))
                    || name.starts_with(&format!("{word}_"))
            })
        })
        .collect()
}

fn package_checks(manifest: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let Ok(package) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    let Some(scripts) = package
        .get("scripts")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let manager = package_manager(manifest, &package);
    ["build", "check", "typecheck", "lint", "test"]
        .into_iter()
        .filter(|script| scripts.contains_key(*script))
        .map(|script| match manager.as_str() {
            "yarn" => format!("yarn {script}"),
            "pnpm" => format!("pnpm run {script}"),
            "bun" => format!("bun run {script}"),
            _ => format!("npm run {script}"),
        })
        .collect()
}

fn package_manager(manifest: &Path, package: &serde_json::Value) -> String {
    if let Some(manager) = package
        .get("packageManager")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.split('@').next())
        .filter(|value| matches!(*value, "yarn" | "pnpm" | "bun" | "npm"))
    {
        return manager.to_string();
    }
    let directory = manifest.parent().unwrap_or_else(|| Path::new("."));
    if directory.join("yarn.lock").is_file() {
        "yarn".to_string()
    } else if directory.join("pnpm-lock.yaml").is_file() {
        "pnpm".to_string()
    } else if directory.join("bun.lock").is_file() || directory.join("bun.lockb").is_file() {
        "bun".to_string()
    } else {
        "npm".to_string()
    }
}

fn rule_file_excerpts(workspace: &Path, rule_files: &[String]) -> Vec<RuleFileExcerpt> {
    let mut excerpts = Vec::new();
    let mut budget = RULE_EXCERPT_TOTAL_BYTES;
    for rule in rule_files {
        if budget == 0 {
            break;
        }
        let path = workspace.join(rule);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<&str> = content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(RULE_EXCERPT_LINES)
            .collect();
        let mut excerpt = lines.join("\n");
        if excerpt.len() > budget {
            excerpt.truncate(budget);
        }
        budget = budget.saturating_sub(excerpt.len());
        excerpts.push(RuleFileExcerpt {
            path: rule.clone(),
            excerpt,
        });
    }
    excerpts
}

#[derive(Default)]
struct GitContext {
    version_controlled: bool,
    inspection_complete: bool,
    branch: Option<String>,
    changed_files: Vec<String>,
}

#[derive(Default)]
struct GitReviewContext {
    version_controlled: bool,
    inspection_complete: bool,
    base_ref: Option<String>,
    merge_base: Option<String>,
    committed_files: Vec<String>,
    worktree_files: Vec<String>,
    changed_files: Vec<String>,
}

fn review(request: &ReviewRequest) -> std::result::Result<ReviewResponse, DispatchError> {
    let workspace = workspace_path(&request.workspace)?;
    let deadline = git_deadline();
    let git = git_review_context(&workspace, request.base_ref.as_deref(), deadline)?;
    let allowed_paths = request
        .allowed_paths
        .iter()
        .map(|file| normalize_relative_path(file))
        .filter(|path| !path.is_empty())
        .collect::<BTreeSet<_>>();
    let unexpected_files = git
        .changed_files
        .iter()
        .filter(|file| {
            !allowed_paths.is_empty()
                && !allowed_paths
                    .iter()
                    .any(|allowed| path_is_within_scope(file, allowed))
        })
        .cloned()
        .collect::<Vec<_>>();

    let failed_verifications = request
        .verification
        .iter()
        .filter(|result| !result.passed)
        .map(|result| {
            if result.name.trim().is_empty() {
                "未命名检查".to_string()
            } else {
                result.name.clone()
            }
        })
        .collect::<Vec<_>>();
    // 声明通过但拿不出有效执行痕迹（无 evidence 或退出码与结论矛盾）的不计入完成。
    let unverified_claims = request
        .verification
        .iter()
        .filter(|result| result.passed && !has_valid_evidence(result))
        .map(|result| result.name.clone())
        .filter(|name| !name.trim().is_empty())
        .collect::<Vec<_>>();
    let verification_complete = !request.verification.is_empty()
        && failed_verifications.is_empty()
        && unverified_claims.is_empty();

    let whitespace_errors = git_whitespace_errors(&workspace, git.base_ref.as_deref(), deadline);
    let diff_stats = git_diff_stats(&workspace, git.base_ref.as_deref(), deadline);
    let risky_files = classify_risky_files(&git.changed_files);
    let ready = verification_complete && unexpected_files.is_empty() && git.inspection_complete;
    let mut notes = Vec::new();
    if request.verification.is_empty() {
        notes.push("尚未记录验证结果".to_string());
    } else if !failed_verifications.is_empty() {
        notes.push("仍有未通过的验证".to_string());
    }
    if !unverified_claims.is_empty() {
        notes.push(format!(
            "{} 项验证声明通过但缺少有效执行痕迹（command/exit_code），需附 evidence 后才计入完成",
            unverified_claims.len()
        ));
    }
    if !unexpected_files.is_empty() {
        notes.push("存在预期范围外的改动".to_string());
    }
    if !whitespace_errors.is_empty() {
        notes.push(format!(
            "存在 {} 处空白错误（git diff --check）",
            whitespace_errors.len()
        ));
    }
    if !risky_files.is_empty() {
        notes.push(format!(
            "改动涉及 {} 个需人工留意的风险文件（锁文件/CI/部署/疑似密钥）",
            risky_files.len()
        ));
    }
    if !git.inspection_complete {
        notes.push("未能确定 Git 基线或读取完整改动范围".to_string());
    } else if !git.version_controlled {
        notes.push("未检测到版本控制，无法自动核对改动文件范围".to_string());
    }

    Ok(ReviewResponse {
        version_controlled: git.version_controlled,
        version_control_inspected: git.inspection_complete,
        base_ref: git.base_ref,
        merge_base: git.merge_base,
        has_committed_changes: !git.committed_files.is_empty(),
        has_uncommitted_changes: !git.worktree_files.is_empty(),
        changed_files: git.changed_files,
        unexpected_files,
        verification_complete,
        failed_verifications,
        unverified_claims,
        whitespace_errors,
        diff_stats,
        risky_files,
        ready,
        notes,
    })
}

fn has_valid_evidence(result: &VerificationResult) -> bool {
    result
        .evidence
        .as_ref()
        .is_some_and(|evidence| evidence.exit_code == 0 && !evidence.command.trim().is_empty())
}

fn classify_risky_files(changed_files: &[String]) -> Vec<RiskyFile> {
    changed_files
        .iter()
        .filter_map(|path| {
            let name = Path::new(path)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            let lower = path.to_ascii_lowercase();
            if LOCKFILE_NAMES.contains(&name) {
                Some(RiskyFile {
                    path: path.clone(),
                    reason: "lockfile".to_string(),
                })
            } else if CI_CONFIG_HINTS.iter().any(|hint| lower.starts_with(hint))
                || CI_CONFIG_HINTS.contains(&name)
            {
                Some(RiskyFile {
                    path: path.clone(),
                    reason: "ci-config".to_string(),
                })
            } else if DEPLOY_CONFIG_HINTS
                .iter()
                .any(|hint| name.starts_with(hint) || lower.contains(hint))
            {
                Some(RiskyFile {
                    path: path.clone(),
                    reason: "deploy-config".to_string(),
                })
            } else if SECRET_FILE_HINTS.iter().any(|hint| lower.contains(hint)) {
                Some(RiskyFile {
                    path: path.clone(),
                    reason: "secret-like".to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn branches(request: &BranchesRequest) -> std::result::Result<BranchesResponse, DispatchError> {
    let workspace = workspace_path(&request.workspace)?;
    let deadline = git_deadline();
    let version_controlled = match git_read(
        &workspace,
        &["rev-parse", "--is-inside-work-tree"],
        deadline,
    ) {
        CommandOutput::Success(output) => String::from_utf8_lossy(&output).trim() == "true",
        CommandOutput::ExitFailure { .. } => false,
        CommandOutput::Unavailable => {
            // git 不可执行/超预算：如实上报并留日志，不再伪装成"非 git 仓库"
            //（伪装会让界面无声消失且无诊断线索）。
            tracing::warn!(
                workspace = %workspace.display(),
                "分支列表探测失败：git 不可用或调用超时"
            );
            return Ok(BranchesResponse {
                is_repo: false,
                check_error: Some("git 不可用或调用超时，无法检测仓库状态".to_string()),
                ..BranchesResponse::default()
            });
        }
    };
    if !version_controlled {
        return Ok(BranchesResponse {
            is_repo: false,
            ..BranchesResponse::default()
        });
    }

    let current = git_text(&workspace, &["branch", "--show-current"], deadline)
        .filter(|value| !value.is_empty());
    let detached = current.is_none();
    let head_short = if detached {
        git_text(&workspace, &["rev-parse", "--short", "HEAD"], deadline)
    } else {
        None
    };
    let has_uncommitted_changes =
        git_changed_files(&workspace, deadline).is_some_and(|files| !files.is_empty());

    let mut response = BranchesResponse {
        is_repo: true,
        check_error: None,
        current: current.clone(),
        detached,
        head_short,
        has_uncommitted_changes,
        branches: Vec::new(),
    };
    for (is_remote, args) in [
        (
            false,
            &[
                "for-each-ref",
                "refs/heads",
                "--format=%(refname:short)%09%(HEAD)",
            ][..],
        ),
        (
            true,
            &[
                "for-each-ref",
                "refs/remotes",
                "--format=%(refname:short)%09%(HEAD)",
            ][..],
        ),
    ] {
        // 不走 git_text：全局 trim 会剥掉非当前分支行的尾随制表符，破坏逐行解析。
        let CommandOutput::Success(raw) = git_read(&workspace, args, deadline) else {
            continue;
        };
        for line in String::from_utf8_lossy(&raw).lines() {
            let (name, head_mark) = match line.split_once('\t') {
                Some((name, mark)) => (name.trim(), mark.trim()),
                // 兼容行尾空白被剥离的形态（非当前分支行）。
                None => (line.trim(), ""),
            };
            // 过滤 HEAD 符号引用与不带分支段的裸远端引用（如残留的 refs/remotes/origin）。
            if name.is_empty() || name.ends_with("/HEAD") || (is_remote && !name.contains('/')) {
                continue;
            }
            response.branches.push(BranchInfo {
                name: name.to_string(),
                is_current: !is_remote && head_mark.trim() == "*",
                is_remote,
            });
        }
    }
    Ok(response)
}

fn switch_branch(request: &SwitchRequest) -> std::result::Result<SwitchResponse, DispatchError> {
    let workspace = workspace_path(&request.workspace)?;
    let deadline = git_deadline();
    let branch = request.branch.trim();
    if branch.is_empty() || branch.starts_with('-') || branch.contains("..") || branch.contains(' ')
    {
        return Err(DispatchError::Bad(format!(
            "分支名不合法: {}",
            request.branch
        )));
    }
    // 先校验 ref 形态，拒绝奇怪的输入。
    match command_output(
        &workspace,
        &["git", "check-ref-format", "--branch", branch],
        deadline,
    ) {
        CommandOutput::Success(_) => {}
        _ => {
            return Err(DispatchError::Bad(format!("分支名不合法: {branch}")));
        }
    }

    let local_ref = format!("refs/heads/{branch}");
    if git_ref_exists(&workspace, &local_ref, deadline) {
        git_switch(&workspace, &["switch", branch], deadline)?;
        return Ok(SwitchResponse {
            switched_to: branch.to_string(),
            created_tracking: false,
        });
    }

    // 远端分支：本地无同名分支时建跟踪分支；已有同名分支则直接切过去。
    let remote_ref = format!("refs/remotes/{branch}");
    if git_ref_exists(&workspace, &remote_ref, deadline)
        && let Some((_, local_name)) = branch
            .split_once('/')
            .filter(|(_, local_name)| !local_name.is_empty())
    {
        if git_ref_exists(&workspace, &format!("refs/heads/{local_name}"), deadline) {
            git_switch(&workspace, &["switch", local_name], deadline)?;
            return Ok(SwitchResponse {
                switched_to: local_name.to_string(),
                created_tracking: false,
            });
        }
        git_switch(&workspace, &["switch", "--track", branch], deadline)?;
        return Ok(SwitchResponse {
            switched_to: local_name.to_string(),
            created_tracking: true,
        });
    }
    Err(DispatchError::Bad(format!("分支不存在: {branch}")))
}

/// 切分支是写操作：不加 --no-optional-locks，失败时透传 git 的 stderr 说明。
fn git_switch(workspace: &Path, args: &[&str], deadline: Instant) -> Result<()> {
    let mut full_args: Vec<&str> = vec!["git"];
    full_args.extend_from_slice(args);
    match command_output(workspace, &full_args, deadline) {
        CommandOutput::Success(_) => {
            tracing::info!(workspace = %workspace.display(), args = ?args, "切换分支成功");
            Ok(())
        }
        CommandOutput::ExitFailure { stderr } => {
            let message = if stderr.trim().is_empty() {
                format!("git {:?} 执行失败", args)
            } else {
                stderr.trim().to_string()
            };
            Err(anyhow!("{message}"))
        }
        CommandOutput::Unavailable => Err(anyhow!(
            "git {:?} 执行超时或不可用，请确认没有其他 git 操作持有锁",
            args
        )),
    }
}

fn git_deadline() -> Instant {
    Instant::now() + GIT_BUDGET
}

fn git_review_context(
    workspace: &Path,
    requested_base: Option<&str>,
    deadline: Instant,
) -> Result<GitReviewContext> {
    let worktree = git_context(workspace, deadline);
    if !worktree.version_controlled {
        return Ok(GitReviewContext {
            version_controlled: false,
            inspection_complete: worktree.inspection_complete,
            worktree_files: worktree.changed_files.clone(),
            changed_files: worktree.changed_files,
            ..GitReviewContext::default()
        });
    }

    let base_ref = resolve_review_base(workspace, requested_base, deadline)?;
    let merge_base = base_ref
        .as_deref()
        .and_then(|base| git_text(workspace, &["merge-base", "HEAD", base], deadline))
        .filter(|value| !value.is_empty());
    let committed_files = merge_base
        .as_deref()
        .and_then(|base| git_diff_files(workspace, base, deadline));
    let inspection_complete = worktree.inspection_complete
        && base_ref.is_some()
        && merge_base.is_some()
        && committed_files.is_some();
    let committed_files = committed_files.unwrap_or_default();
    let worktree_files = worktree.changed_files;
    let mut changed_files = committed_files.iter().cloned().collect::<BTreeSet<_>>();
    changed_files.extend(worktree_files.iter().cloned());

    Ok(GitReviewContext {
        version_controlled: true,
        inspection_complete,
        base_ref,
        merge_base,
        committed_files,
        worktree_files,
        changed_files: changed_files.into_iter().collect(),
    })
}

/// 自动基线顺序：显式指定 > 上游分支 > 远端默认分支 > 本地主分支。
/// feature 分支场景下上游正是任务分支的对照基线，优先于字面量主分支，
/// 避免把已合入上游的改动误算成本次任务改动。
fn resolve_review_base(
    workspace: &Path,
    requested_base: Option<&str>,
    deadline: Instant,
) -> Result<Option<String>> {
    if let Some(base) = requested_base
        .map(str::trim)
        .filter(|base| !base.is_empty())
    {
        if git_ref_exists(workspace, base, deadline) {
            return Ok(Some(base.to_string()));
        }
        return Err(anyhow!("Git 基线引用不存在: {base}"));
    }

    let mut candidates = Vec::new();
    if let Some(upstream) = git_text(
        workspace,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        deadline,
    ) {
        candidates.push(upstream);
    }
    if let Some(remote_head) = git_text(
        workspace,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        deadline,
    ) {
        candidates.push(remote_head);
    }
    candidates.extend(
        ["main", "develop", "master"]
            .into_iter()
            .map(str::to_string),
    );

    Ok(candidates
        .into_iter()
        .find(|candidate| git_ref_exists(workspace, candidate, deadline)))
}

fn git_ref_exists(workspace: &Path, reference: &str, deadline: Instant) -> bool {
    let commit_ref = format!("{reference}^{{commit}}");
    matches!(
        git_read(
            workspace,
            &["rev-parse", "--verify", "--quiet", &commit_ref],
            deadline,
        ),
        CommandOutput::Success(_)
    )
}

fn git_diff_files(workspace: &Path, base: &str, deadline: Instant) -> Option<Vec<String>> {
    let CommandOutput::Success(output) = git_read(
        workspace,
        &[
            "diff",
            "--name-only",
            "-z",
            "--diff-filter=ACDMRTUXB",
            base,
            "HEAD",
        ],
        deadline,
    ) else {
        return None;
    };
    Some(
        output
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| normalize_relative_path(&String::from_utf8_lossy(path)))
            .collect(),
    )
}

fn git_whitespace_errors(workspace: &Path, base: Option<&str>, deadline: Instant) -> Vec<String> {
    let mut errors = Vec::new();
    let mut scopes: Vec<Vec<&str>> = Vec::new();
    if let Some(base) = base {
        scopes.push(vec!["diff", "--check", base, "HEAD"]);
    }
    scopes.push(vec!["diff", "--check"]);
    for args in scopes {
        if let CommandOutput::Success(output) = git_read(workspace, &args, deadline) {
            for line in String::from_utf8_lossy(&output).lines() {
                let line = line.trim();
                if !line.is_empty() && !errors.iter().any(|existing| existing == line) {
                    errors.push(line.to_string());
                }
            }
        }
        if errors.len() >= WHITESPACE_ERROR_LIMIT {
            errors.truncate(WHITESPACE_ERROR_LIMIT);
            break;
        }
    }
    errors
}

/// 汇总已提交（base..HEAD）与工作区改动的规模。
fn git_diff_stats(workspace: &Path, base: Option<&str>, deadline: Instant) -> DiffStats {
    let mut stats = DiffStats::default();
    let mut scopes: Vec<Vec<&str>> = Vec::new();
    if let Some(base) = base {
        scopes.push(vec!["diff", "--numstat", "-z", base, "HEAD"]);
    }
    scopes.push(vec!["diff", "--numstat", "-z"]);
    for args in scopes {
        let CommandOutput::Success(output) = git_read(workspace, &args, deadline) else {
            continue;
        };
        for field in output.split(|byte| *byte == 0) {
            let field = String::from_utf8_lossy(field);
            let Some((insertions, rest)) = field.split_once('\t') else {
                // numstat -z 下 rename 的旧路径是独立字段，跳过。
                continue;
            };
            let Some((deletions, path)) = rest.split_once('\t') else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            if insertions == "-" || deletions == "-" {
                stats.binary_files += 1;
            } else {
                stats.insertions += insertions.parse::<u64>().unwrap_or(0);
                stats.deletions += deletions.parse::<u64>().unwrap_or(0);
            }
            stats.files_changed += 1;
        }
    }
    stats
}

fn path_is_within_scope(path: &str, allowed: &str) -> bool {
    allowed == "."
        || path == allowed
        || path
            .strip_prefix(allowed)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn git_context(workspace: &Path, deadline: Instant) -> GitContext {
    let version_controlled =
        match git_read(workspace, &["rev-parse", "--is-inside-work-tree"], deadline) {
            CommandOutput::Success(output) => String::from_utf8_lossy(&output).trim() == "true",
            CommandOutput::ExitFailure { .. } => {
                return GitContext {
                    inspection_complete: true,
                    ..GitContext::default()
                };
            }
            CommandOutput::Unavailable => return GitContext::default(),
        };
    if !version_controlled {
        return GitContext {
            inspection_complete: true,
            ..GitContext::default()
        };
    }
    let branch = git_text(workspace, &["branch", "--show-current"], deadline)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            git_text(workspace, &["rev-parse", "--short", "HEAD"], deadline)
                .map(|commit| format!("detached@{commit}"))
        });
    let changed_files = git_changed_files(workspace, deadline);
    GitContext {
        version_controlled,
        inspection_complete: changed_files.is_some(),
        branch,
        changed_files: changed_files.unwrap_or_default(),
    }
}

enum CommandOutput {
    Success(Vec<u8>),
    ExitFailure { stderr: String },
    Unavailable,
}

/// 只读 git 调用统一加 `--no-optional-locks`，避免与用户手头的 git 操作争锁。
fn git_read(workspace: &Path, args: &[&str], deadline: Instant) -> CommandOutput {
    let mut full_args: Vec<&str> = vec!["git", "--no-optional-locks"];
    full_args.extend_from_slice(args);
    command_output(workspace, &full_args, deadline)
}

fn git_text(workspace: &Path, args: &[&str], deadline: Instant) -> Option<String> {
    match git_read(workspace, args, deadline) {
        CommandOutput::Success(output) => Some(String::from_utf8_lossy(&output).trim().to_string()),
        CommandOutput::ExitFailure { .. } | CommandOutput::Unavailable => None,
    }
}

fn command_output(workspace: &Path, args: &[&str], deadline: Instant) -> CommandOutput {
    let Some(program) = args.first().copied() else {
        return CommandOutput::Unavailable;
    };
    // 单命令上限取全局预算剩余量与固定上限的较小值。
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        tracing::warn!(args = ?args, "git 调用预算耗尽，跳过执行");
        return CommandOutput::Unavailable;
    }
    let command_deadline = Instant::now() + GIT_COMMAND_TIMEOUT.min(remaining);
    let Ok(mut child) = Command::new(program)
        .args(&args[1..])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return CommandOutput::Unavailable;
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return CommandOutput::Unavailable;
    };
    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return CommandOutput::Unavailable;
    };
    let stdout_reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let stderr_reader = thread::spawn(move || {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).map(|_| output)
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < command_deadline => {
                thread::sleep(Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                // kill 后管道写端关闭，读线程随 EOF 退出，join 回收避免线程泄漏。
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                tracing::warn!(args = ?args, "命令执行超时，已终止");
                return CommandOutput::Unavailable;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return CommandOutput::Unavailable;
            }
        }
    };
    let (stdout, stderr) = match (stdout_reader.join(), stderr_reader.join()) {
        (Ok(Ok(stdout)), Ok(Ok(stderr))) => (stdout, stderr),
        _ => return CommandOutput::Unavailable,
    };
    if status.success() {
        CommandOutput::Success(stdout)
    } else {
        CommandOutput::ExitFailure {
            stderr: String::from_utf8_lossy(&stderr).to_string(),
        }
    }
}

fn git_changed_files(workspace: &Path, deadline: Instant) -> Option<Vec<String>> {
    let CommandOutput::Success(output) = git_read(
        workspace,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        deadline,
    ) else {
        return None;
    };
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut files = BTreeSet::new();
    while let Some(record) = fields.next() {
        if record.len() < 4 {
            continue;
        }
        let status = &record[..2];
        let path = String::from_utf8_lossy(&record[3..]);
        files.insert(normalize_relative_path(&path));
        if status.contains(&b'R') || status.contains(&b'C') {
            let _ = fields.next();
        }
    }
    Some(files.into_iter().collect())
}

fn relative_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .map(|relative| normalize_relative_path(&relative.to_string_lossy()))
        .unwrap_or_else(|_| path.display().to_string())
}

fn normalize_relative_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

/// 进度存储：同工作区按任务分槽位、最多保留 [`MAX_CHECKPOINT_ENTRIES`] 条。
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct CheckpointStore {
    entries: Vec<SavedCheckpoint>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(workspace: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git 应可执行");
        assert!(status.success(), "git {args:?} 执行失败");
    }

    fn init_repo(workspace: &Path) {
        run_git(workspace, &["init", "-b", "main"]);
        run_git(workspace, &["config", "user.email", "test@example.com"]);
        run_git(workspace, &["config", "user.name", "Test"]);
        std::fs::write(workspace.join("base.txt"), "base").expect("写入基线文件");
        run_git(workspace, &["add", "base.txt"]);
        run_git(workspace, &["commit", "-m", "base"]);
    }

    fn evidence_verification(command: &str) -> Vec<VerificationResult> {
        vec![VerificationResult {
            name: command.to_string(),
            passed: true,
            details: String::new(),
            evidence: Some(VerificationEvidence {
                command: command.to_string(),
                exit_code: 0,
                output_tail: String::new(),
            }),
        }]
    }

    fn bare_verification(command: &str) -> Vec<VerificationResult> {
        vec![VerificationResult {
            name: command.to_string(),
            passed: true,
            details: String::new(),
            evidence: None,
        }]
    }

    #[test]
    fn review_combines_committed_and_worktree_changes() {
        let workspace = tempfile::tempdir().expect("创建临时仓库");
        init_repo(workspace.path());
        run_git(workspace.path(), &["switch", "-c", "feature/review"]);
        std::fs::write(workspace.path().join("committed.rs"), "fn committed() {}")
            .expect("写入已提交文件");
        run_git(workspace.path(), &["add", "committed.rs"]);
        run_git(workspace.path(), &["commit", "-m", "feature"]);

        let clean_review = review(&ReviewRequest {
            workspace: workspace.path().display().to_string(),
            base_ref: Some("main".to_string()),
            allowed_paths: Vec::new(),
            verification: evidence_verification("cargo check"),
        })
        .expect("检查干净功能分支");
        assert_eq!(clean_review.changed_files, vec!["committed.rs"]);
        assert!(clean_review.has_committed_changes);
        assert!(!clean_review.has_uncommitted_changes);
        assert!(clean_review.ready);

        std::fs::write(workspace.path().join("worktree.rs"), "fn worktree() {}")
            .expect("写入工作区文件");
        let combined_review = review(&ReviewRequest {
            workspace: workspace.path().display().to_string(),
            base_ref: Some("main".to_string()),
            allowed_paths: vec!["committed.rs".to_string()],
            verification: evidence_verification("cargo check"),
        })
        .expect("检查组合改动");
        assert_eq!(
            combined_review.changed_files,
            vec!["committed.rs", "worktree.rs"]
        );
        assert_eq!(combined_review.unexpected_files, vec!["worktree.rs"]);
        assert!(!combined_review.ready);
    }

    #[test]
    fn review_rejects_passed_claims_without_evidence() {
        let workspace = tempfile::tempdir().expect("创建临时仓库");
        init_repo(workspace.path());
        run_git(workspace.path(), &["switch", "-c", "feature/evidence"]);
        std::fs::write(workspace.path().join("change.rs"), "fn change() {}").expect("写入改动文件");
        run_git(workspace.path(), &["add", "change.rs"]);
        run_git(workspace.path(), &["commit", "-m", "change"]);

        let no_evidence = review(&ReviewRequest {
            workspace: workspace.path().display().to_string(),
            base_ref: Some("main".to_string()),
            allowed_paths: Vec::new(),
            verification: bare_verification("cargo check"),
        })
        .expect("审查无凭据声明");
        assert!(!no_evidence.verification_complete);
        assert_eq!(
            no_evidence.unverified_claims,
            vec!["cargo check".to_string()]
        );
        assert!(!no_evidence.ready);

        let wrong_exit_code = vec![VerificationResult {
            name: "cargo check".to_string(),
            passed: true,
            details: String::new(),
            evidence: Some(VerificationEvidence {
                command: "cargo check".to_string(),
                exit_code: 1,
                output_tail: String::new(),
            }),
        }];
        let contradictory = review(&ReviewRequest {
            workspace: workspace.path().display().to_string(),
            base_ref: Some("main".to_string()),
            allowed_paths: Vec::new(),
            verification: wrong_exit_code,
        })
        .expect("审查矛盾凭据");
        assert!(!contradictory.verification_complete);
    }

    #[test]
    fn review_base_prefers_upstream_over_literal_main() {
        let workspace = tempfile::tempdir().expect("创建临时仓库");
        init_repo(workspace.path());
        // develop 上有 feature 已合入的提交；feature 的上游指向 develop。
        run_git(workspace.path(), &["switch", "-c", "develop"]);
        std::fs::write(workspace.path().join("merged.rs"), "fn merged() {}")
            .expect("写入已合入文件");
        run_git(workspace.path(), &["add", "merged.rs"]);
        run_git(workspace.path(), &["commit", "-m", "merged"]);
        run_git(workspace.path(), &["switch", "-c", "feature/base"]);
        run_git(workspace.path(), &["branch", "--set-upstream-to=develop"]);
        std::fs::write(workspace.path().join("own.rs"), "fn own() {}").expect("写入本任务文件");
        run_git(workspace.path(), &["add", "own.rs"]);
        run_git(workspace.path(), &["commit", "-m", "own"]);

        let base = resolve_review_base(workspace.path(), None, git_deadline())
            .expect("解析自动基线")
            .expect("应找到基线");
        assert_eq!(base, "develop");

        let own_review = review(&ReviewRequest {
            workspace: workspace.path().display().to_string(),
            base_ref: None,
            allowed_paths: Vec::new(),
            verification: evidence_verification("cargo check"),
        })
        .expect("按上游基线审查");
        // 只剩 feature 自身提交，已合入 develop 的 merged.rs 不算本次改动。
        assert_eq!(own_review.changed_files, vec!["own.rs"]);
    }

    #[test]
    fn allowed_paths_with_trailing_slash_match_children() {
        let workspace = tempfile::tempdir().expect("创建临时仓库");
        init_repo(workspace.path());
        run_git(workspace.path(), &["switch", "-c", "feature/slash"]);
        std::fs::create_dir_all(workspace.path().join("src")).expect("创建目录");
        std::fs::write(workspace.path().join("src/main.rs"), "fn main() {}").expect("写入文件");
        run_git(workspace.path(), &["add", "src/main.rs"]);
        run_git(workspace.path(), &["commit", "-m", "src"]);

        let slash_review = review(&ReviewRequest {
            workspace: workspace.path().display().to_string(),
            base_ref: Some("main".to_string()),
            allowed_paths: vec!["src/".to_string()],
            verification: evidence_verification("cargo check"),
        })
        .expect("带尾斜杠审查");
        assert!(slash_review.unexpected_files.is_empty());
    }

    #[test]
    fn project_context_restores_task_checkpoints_with_fallback() {
        let workspace = tempfile::tempdir().expect("创建临时工作区");
        let data_dir = tempfile::tempdir().expect("创建临时数据目录");
        let service = CodingService {
            data_dir: data_dir.path().to_path_buf(),
        };
        let make_request = |task: &str| WorkspaceRequest {
            workspace: workspace.path().display().to_string(),
            full_trust: true,
            task: Some(task.to_string()),
        };
        let save = |task: &str| {
            service
                .save_checkpoint(&CheckpointRequest {
                    workspace: workspace.path().display().to_string(),
                    task: task.to_string(),
                    completion_criteria: Vec::new(),
                    completed: Vec::new(),
                    changed_files: Vec::new(),
                    verification: Vec::new(),
                    blockers: Vec::new(),
                })
                .expect("保存进度");
        };
        save("修复流式工具参数");
        save("开发其他功能");

        let first = service
            .project_context(&make_request("修复流式工具参数"))
            .expect("恢复第一条");
        assert_eq!(
            first
                .latest_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.state.task.as_str()),
            Some("修复流式工具参数")
        );
        assert!(!first.latest_checkpoint_task_mismatch);

        // 未知任务回退最近一条并标注不匹配。
        let fallback = service
            .project_context(&make_request("全新任务"))
            .expect("回退恢复");
        assert_eq!(
            fallback
                .latest_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.state.task.as_str()),
            Some("开发其他功能")
        );
        assert!(fallback.latest_checkpoint_task_mismatch);
    }

    #[test]
    fn corrupted_checkpoint_reports_notice() {
        let workspace = tempfile::tempdir().expect("创建临时工作区");
        let data_dir = tempfile::tempdir().expect("创建临时数据目录");
        let service = CodingService {
            data_dir: data_dir.path().to_path_buf(),
        };
        // 服务内部保存前会 canonicalize，这里用同一路径形态定位进度文件。
        let canonical = workspace.path().canonicalize().expect("规范化工作区路径");
        let checkpoint_file = service.checkpoint_path(&canonical);
        std::fs::create_dir_all(checkpoint_file.parent().unwrap()).expect("创建进度目录");
        std::fs::write(&checkpoint_file, "{ truncated").expect("写入损坏进度");

        let context = service
            .project_context(&WorkspaceRequest {
                workspace: workspace.path().display().to_string(),
                full_trust: true,
                task: Some("任意任务".to_string()),
            })
            .expect("读取损坏上下文");
        assert!(context.latest_checkpoint.is_none());
        assert!(context.checkpoint_recovery_notice.is_some());
    }

    #[test]
    fn recommend_checks_reads_rule_file_and_makefile() {
        let workspace = tempfile::tempdir().expect("创建临时项目");
        std::fs::write(
            workspace.path().join("CLAUDE.md"),
            "# 项目规则\n\n- `cargo fmt --check && cargo check --workspace && cargo clippy --workspace -- -D warnings`\n- 部署命令 docker push example/app 不应被抓取\n",
        )
        .expect("写入规则文件");
        std::fs::write(
            workspace.path().join("Makefile"),
            "check:\n\tcargo check\n\nbuild:\n\tcargo build\n\ndeploy-prod:\n\techo x\n",
        )
        .expect("写入 Makefile");

        let inventory = scan_inventory(workspace.path());
        let checks = recommend_checks(workspace.path(), &inventory);
        let commands: Vec<&str> = checks.iter().map(|check| check.command.as_str()).collect();
        assert!(commands.contains(&"cargo fmt --check"));
        assert!(commands.contains(&"cargo check --workspace"));
        assert!(commands.contains(&"cargo clippy --workspace -- -D warnings"));
        assert!(commands.contains(&"make check"));
        assert!(commands.contains(&"make build"));
        assert!(!commands.iter().any(|command| command.contains("deploy")));
        assert!(!commands.iter().any(|command| command.contains("docker")));
    }

    #[test]
    fn preflight_warns_on_trunk_and_lists_uncommitted_files() {
        let workspace = tempfile::tempdir().expect("创建临时仓库");
        init_repo(workspace.path());
        std::fs::write(workspace.path().join("dirty.rs"), "fn dirty() {}").expect("写入未提交文件");

        let data_dir = tempfile::tempdir().expect("创建临时数据目录");
        let service = CodingService {
            data_dir: data_dir.path().to_path_buf(),
        };
        let response = service
            .preflight(&PreflightRequest {
                workspace: workspace.path().display().to_string(),
                full_trust: true,
                task: "在主分支上直接开发".to_string(),
            })
            .expect("开发前检查");
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("主分支 main"))
        );
        assert_eq!(response.uncommitted_files, vec!["dirty.rs".to_string()]);
        assert_eq!(response.uncommitted_files_total, 1);
    }

    #[test]
    fn branches_reports_plain_non_git_without_check_error() {
        let workspace = tempfile::tempdir().expect("创建临时目录");
        let response = branches(&BranchesRequest {
            workspace: workspace.path().display().to_string(),
        })
        .expect("读取非 git 目录");
        // 确认非 git：is_repo=false 且不带 check_error（与 git 不可用区分）。
        assert!(!response.is_repo);
        assert!(response.check_error.is_none());
        assert!(response.branches.is_empty());
    }

    #[test]
    fn branches_lists_local_and_current() {
        let workspace = tempfile::tempdir().expect("创建临时仓库");
        init_repo(workspace.path());
        run_git(workspace.path(), &["switch", "-c", "feature/one"]);
        std::fs::write(workspace.path().join("one.rs"), "fn one() {}").expect("写入文件");
        run_git(workspace.path(), &["add", "one.rs"]);
        run_git(workspace.path(), &["commit", "-m", "one"]);

        let response = branches(&BranchesRequest {
            workspace: workspace.path().display().to_string(),
        })
        .expect("读取分支列表");
        assert!(response.is_repo);
        assert_eq!(response.current.as_deref(), Some("feature/one"));
        assert!(!response.detached);
        let names: Vec<&str> = response
            .branches
            .iter()
            .map(|branch| branch.name.as_str())
            .collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"feature/one"));
        assert!(
            response
                .branches
                .iter()
                .any(|branch| branch.name == "feature/one" && branch.is_current)
        );
    }

    #[test]
    fn switch_moves_between_local_branches() {
        let workspace = tempfile::tempdir().expect("创建临时仓库");
        init_repo(workspace.path());
        run_git(workspace.path(), &["switch", "-c", "feature/one"]);
        run_git(workspace.path(), &["switch", "main"]);

        let response = switch_branch(&SwitchRequest {
            workspace: workspace.path().display().to_string(),
            branch: "feature/one".to_string(),
        })
        .expect("切换本地分支");
        assert_eq!(response.switched_to, "feature/one");
        assert!(!response.created_tracking);

        let after = branches(&BranchesRequest {
            workspace: workspace.path().display().to_string(),
        })
        .expect("读取切换后状态");
        assert_eq!(after.current.as_deref(), Some("feature/one"));

        let missing = switch_branch(&SwitchRequest {
            workspace: workspace.path().display().to_string(),
            branch: "no-such-branch".to_string(),
        });
        assert!(missing.is_err());
    }

    #[test]
    fn risky_files_are_classified() {
        let risky = classify_risky_files(&[
            "Cargo.lock".to_string(),
            ".github/workflows/ci.yml".to_string(),
            "Dockerfile".to_string(),
            "secrets/id_rsa".to_string(),
            "src/main.rs".to_string(),
        ]);
        let reasons: Vec<(&str, &str)> = risky
            .iter()
            .map(|file| (file.path.as_str(), file.reason.as_str()))
            .collect();
        assert_eq!(
            reasons,
            vec![
                ("Cargo.lock", "lockfile"),
                (".github/workflows/ci.yml", "ci-config"),
                ("Dockerfile", "deploy-config"),
                ("secrets/id_rsa", "secret-like"),
            ]
        );
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(normalize_relative_path("src/"), "src");
        assert_eq!(normalize_relative_path("./a/b/"), "a/b");
        assert!(path_is_within_scope(
            "src/main.rs",
            &normalize_relative_path("src/")
        ));
    }
}
