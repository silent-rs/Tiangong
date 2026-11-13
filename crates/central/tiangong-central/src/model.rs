//! 中枢模式数据模型（设计稿 §13 草案的类型化落地）。
//!
//! 概念字段 → POC 类型映射说明：
//! - 时间统一为本地时间文本（`crate::ids::now_text`）；
//! - ID 统一为 scru128 字符串（`crate::ids::new_id`）；
//! - 结构化自由字段（plan / proposed_options / verification 等）暂用
//!   `serde_json::Value` 承载，随阶段 2 的报告与事件架构决议再收紧；
//! - `central_conversation` 仅定义元数据结构；中枢对话的消息链路复用现有
//!   Session 持久化，不新建消息体系（§12.2）。

use serde::{Deserialize, Serialize};

use crate::ids::{new_id, now_text};
use crate::risk::RiskLevel;
use crate::status::WorkItemStatus;

// ─────────────────────────────────────────────────────────────────────────────
// 用户目标（§13.2 / §7.1）
// ─────────────────────────────────────────────────────────────────────────────

/// 用户目标状态。
///
/// 设计稿 §13.2 仅列出 `status` 字段未定义值域；此处取最小三态（POC 决策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    #[default]
    Active,
    Completed,
    Cancelled,
}

/// 目标优先级（§7.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GoalPriority {
    Low,
    #[default]
    Normal,
    High,
}

/// 用户目标：用户用自然语言表达的意图及约束，是用户侧语义核心（§13.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserGoal {
    pub id: String,
    pub conversation_id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub completion_criteria: Vec<String>,
    #[serde(default)]
    pub priority: GoalPriority,
    #[serde(default)]
    pub risk_level: RiskLevel,
    #[serde(default)]
    pub status: GoalStatus,
    /// 是否允许低风险时自动安排后台工作（§7.1 `auto_dispatch`）
    #[serde(default)]
    pub auto_dispatch: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl UserGoal {
    /// 创建目标（标题必填，其余字段随后续对话补充）。
    pub fn new(conversation_id: impl Into<String>, title: impl Into<String>) -> Self {
        let now = now_text();
        Self {
            id: new_id(),
            conversation_id: conversation_id.into(),
            title: title.into(),
            description: String::new(),
            constraints: Vec::new(),
            completion_criteria: Vec::new(),
            priority: GoalPriority::Normal,
            risk_level: RiskLevel::Low,
            status: GoalStatus::Active,
            auto_dispatch: false,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 后台工作（§13.3 / §6.2）
// ─────────────────────────────────────────────────────────────────────────────

/// 后台工作项：执行层的数据结构，用于调度、状态、依赖和验证（§13.3）。
///
/// 字段合并设计稿 §13.3 与 §6.2 的绑定信息，保证：
/// - 后台工作能准确归属到目标、对话、Workspace 与 Worker Session；
/// - 报告可以追溯验证证据；
/// - Workspace 并发写入可控（`write_lock`）；
/// - 中枢助手重启后能恢复状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub conversation_id: String,
    pub goal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_work_item_id: Option<String>,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_agent_id: Option<String>,
    pub title: String,
    /// 执行计划（结构随阶段 2「目标拆分」决议收紧，暂为自由 JSON）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<serde_json::Value>,
    pub status: WorkItemStatus,
    pub risk_level: RiskLevel,
    /// 是否持有该 Workspace 的写入锁（§6.3：同一 Workspace 同时最多一个写入型工作）
    #[serde(default)]
    pub write_lock: bool,
    /// 最大自动重试次数（POC 简化；完整重试边界见开放问题 18）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<u32>,
    #[serde(default)]
    pub completion_criteria: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

impl WorkItem {
    /// 创建工作项（初始 `draft`）。
    pub fn new(
        conversation_id: impl Into<String>,
        goal_id: impl Into<String>,
        workspace_id: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        let now = now_text();
        Self {
            id: new_id(),
            conversation_id: conversation_id.into(),
            goal_id: goal_id.into(),
            parent_work_item_id: None,
            workspace_id: workspace_id.into(),
            workspace_path: None,
            worker_session_id: None,
            worker_agent_id: None,
            title: title.into(),
            plan: None,
            status: WorkItemStatus::Draft,
            risk_level: RiskLevel::Low,
            write_lock: false,
            retry_policy: None,
            completion_criteria: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            completed_at: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 报告（§13.5 / §9.1 / §9.2）
// ─────────────────────────────────────────────────────────────────────────────

/// 报告类型：单条即时反馈或汇总报告（§9.1 / §9.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportType {
    /// 单条即时反馈（完成、失败、阻塞、高风险审批等）
    Instant,
    /// 汇总报告（多工作、多 Workspace、用户离开期间等）
    Summary,
}

/// 报告：基于事件、证据和产物生成的用户可读汇总（§13.5）。
///
/// `decisions` 引用 `central_decision` 的 id 列表，满足 §9.2 的
/// 「报告应列出替你决定了什么、依据是什么、影响范围」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub id: String,
    pub conversation_id: String,
    pub goal_id: String,
    pub work_item_ids: Vec<String>,
    pub report_type: ReportType,
    pub summary: String,
    /// 验证证据（结构随阶段 2 收紧；禁止无事件声称完成，§9.4）
    #[serde(default)]
    pub verification: serde_json::Value,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
    /// 本报告涉及的中枢代决记录 id（§14.3 审计要求）
    #[serde(default)]
    pub decisions: Vec<String>,
    pub created_at: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// 建议（§13.6 / §10）
// ─────────────────────────────────────────────────────────────────────────────

/// 建议类型（§10.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionType {
    /// 后续工作建议
    FollowUp,
    /// 风险建议
    Risk,
    /// 依赖建议
    Dependency,
    /// 任务收尾建议
    WrapUp,
    /// 异常停止重试建议
    Retry,
    /// 恢复上下文建议
    ContextRecovery,
    /// 每日 / 每次激活摘要建议
    ActivationDigest,
}

/// 建议状态（§10.4：忽略后进入冷却，不重复打扰）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionStatus {
    Proposed,
    Accepted,
    Dismissed,
    Expired,
}

/// 建议：主动观察生成的、必须说明依据的建议（§13.6 / §10.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: String,
    pub conversation_id: String,
    pub suggestion_type: SuggestionType,
    /// 建议依据（每条建议都能回答「为什么现在告诉我」，§19 原则 5）
    pub reason: String,
    pub proposed_action: String,
    pub status: SuggestionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub created_at: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// 模型协商记录（§13.7 / §15.9 / §15.10）
// ─────────────────────────────────────────────────────────────────────────────

/// 模型不可用原因分类（§15.9）。
///
/// 判定由现有端点配置校验与请求错误分类提供，本类型只承载分类结果；
/// 不新建可用性探测子系统。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    AuthExpired,
    QuotaExhausted,
    RateLimited,
    ModelNotFound,
    NetworkTimeout,
    ContextOverflow,
    CapabilityMismatch,
    Unknown,
}

impl FailureClass {
    /// 对用户的关键提示语（§15.9 表述规范；完整话术由中枢对话层组织）。
    pub fn user_hint(self) -> &'static str {
        match self {
            FailureClass::AuthExpired => "这个模型的凭据失效了，需要你更新密钥",
            FailureClass::QuotaExhausted => "额度已用完，换模型或等待额度恢复",
            FailureClass::RateLimited => "当前被限流，可以排队或换一个还有余量的模型",
            FailureClass::ModelNotFound => "配置的模型已不可用，需要重新选一个",
            FailureClass::NetworkTimeout => "连接不稳定，我会先挂起工作",
            FailureClass::ContextOverflow => "当前上下文超过该模型窗口，接力模型需要更大窗口",
            FailureClass::CapabilityMismatch => "这个模型不支持当前工作所需的能力",
            FailureClass::Unknown => "暂时无法确定原因，先把工作挂起",
        }
    }
}

/// 接力协商中用户的决定（§15.10：用户选项至少包括换模型、挂起、缩小范围、取消）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum HandoverDecision {
    /// 换用指定模型继续
    SwitchModel { model: String },
    /// 挂起，等待原模型恢复
    Suspend,
    /// 缩小范围（如改为只读）
    NarrowScope,
    /// 取消这次工作
    Cancel,
}

/// 接力作用范围（§15.10：仅本次工作 / 本次目标 / 之后默认）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoverScope {
    ThisWorkItem,
    ThisGoal,
    FutureDefault,
}

/// 模型接力协商记录（§13.7）。
///
/// 只记录「商量了什么、用户选了什么」；切换动作与留痕完全交给现有
/// `switch_model`（会话 Notice 与 `ModelSwitchStarted` / `ModelSwitched`），
/// 不另建切换记录；工作会话的模型绑定由现有会话结构承载。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelHandoverConsultation {
    pub id: String,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    pub failure_class: FailureClass,
    /// 候选模型及依据（能力匹配、可用性、成本、偏好；结构随阶段 2 收紧）
    #[serde(default)]
    pub proposed_options: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_decision: Option<HandoverDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<HandoverScope>,
    pub created_at: String,
}

impl ModelHandoverConsultation {
    pub fn new(conversation_id: impl Into<String>, failure_class: FailureClass) -> Self {
        Self {
            id: new_id(),
            conversation_id: conversation_id.into(),
            work_item_id: None,
            failure_class,
            proposed_options: serde_json::Value::Null,
            user_decision: None,
            decided_at: None,
            scope: None,
            created_at: now_text(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 中枢代决记录（§13.8 / §5.4）
// ─────────────────────────────────────────────────────────────────────────────

/// 中枢代决记录：替工作会话处理待决事项的依据、决定与影响范围（§13.8）。
///
/// 可复核性硬约束（§5.4 / §19 原则 18）：每次代决都要记录依据、决定、
/// 影响范围、时间和回传对象；汇总报告列出代决清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CentralDecision {
    pub id: String,
    pub conversation_id: String,
    pub work_item_id: String,
    /// 触发本次代决的来源事件 id（`approval.required` / `work_item.waiting_user` 等）
    pub source_event_id: String,
    /// 代决依据（用户目标、约束、偏好、风险分级、事件证据）
    pub reason: String,
    /// 决定内容
    pub decision: String,
    /// 影响范围
    pub impact_scope: String,
    /// 回传对象（哪个 Worker / 事件）
    pub returned_to: String,
    pub created_at: String,
}

impl CentralDecision {
    pub fn new(
        conversation_id: impl Into<String>,
        work_item_id: impl Into<String>,
        source_event_id: impl Into<String>,
    ) -> Self {
        Self {
            id: new_id(),
            conversation_id: conversation_id.into(),
            work_item_id: work_item_id.into(),
            source_event_id: source_event_id.into(),
            reason: String::new(),
            decision: String::new(),
            impact_scope: String::new(),
            returned_to: String::new(),
            created_at: now_text(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 中枢对话元数据（§13.1）
// ─────────────────────────────────────────────────────────────────────────────

/// 中枢自身降级状态标记（§13.1 `degraded_mode` / §15.12）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradedModeInfo {
    /// 临时顶上的模型标识（随机选取的可用模型）
    pub fallback_model: String,
    /// 进入降级的本地时间
    pub since: String,
}

/// 中枢对话元数据（§13.1）。
///
/// 中枢直接使用设置中的主对话模型，不在此结构里重复绑定；
/// `degraded_mode` 只标记当前是否处于临时兜底状态及所用模型。
/// 消息与消息链路复用现有 Session 体系（§12.2），本结构仅承载
/// 中枢模式的元信息，POC 阶段先落类型，持久化接入随后续片进行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CentralConversation {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_profile_id: Option<String>,
    #[serde(default)]
    pub preferences: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_mode: Option<DegradedModeInfo>,
    pub created_at: String,
    pub last_active_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_goal_defaults() {
        let goal = UserGoal::new("conv_1", "排查支付服务响应变慢");
        assert!(!goal.id.is_empty());
        assert_eq!(goal.conversation_id, "conv_1");
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.priority, GoalPriority::Normal);
        assert_eq!(goal.risk_level, RiskLevel::Low);
        assert!(!goal.auto_dispatch);
        assert_eq!(goal.created_at, goal.updated_at);
    }

    #[test]
    fn new_work_item_starts_at_draft() {
        let item = WorkItem::new("conv_1", "goal_1", "payment-service", "检查最近超时日志");
        assert_eq!(item.status, WorkItemStatus::Draft);
        assert!(!item.write_lock);
        assert!(item.worker_session_id.is_none());
        assert!(item.completed_at.is_none());
        assert_eq!(item.goal_id, "goal_1");
    }

    #[test]
    fn work_item_json_skips_optional_none() {
        let item = WorkItem::new("conv_1", "goal_1", "ws_1", "t");
        let json = serde_json::to_value(&item).unwrap();
        assert!(json.get("parent_work_item_id").is_none());
        assert!(json.get("worker_session_id").is_none());
        assert!(json.get("plan").is_none());
        assert!(json.get("completed_at").is_none());
        // 必填字段始终在
        assert_eq!(json["status"], "draft");
        assert_eq!(json["workspace_id"], "ws_1");
    }

    #[test]
    fn failure_class_hints_cover_all() {
        let classes = [
            FailureClass::AuthExpired,
            FailureClass::QuotaExhausted,
            FailureClass::RateLimited,
            FailureClass::ModelNotFound,
            FailureClass::NetworkTimeout,
            FailureClass::ContextOverflow,
            FailureClass::CapabilityMismatch,
            FailureClass::Unknown,
        ];
        for class in classes {
            assert!(!class.user_hint().is_empty(), "{class:?} 缺少用户提示语");
        }
    }

    #[test]
    fn handover_decision_serde_round_trip() {
        let decision = HandoverDecision::SwitchModel {
            model: "deepseek-v3".to_string(),
        };
        let json = serde_json::to_string(&decision).unwrap();
        assert!(
            json.contains("\"type\":\"switch_model\""),
            "实际输出: {json}"
        );
        let back: HandoverDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(back, decision);

        // 内部标签形式：非载荷变体序列化为 {"type":"suspend"}
        let suspend_json = serde_json::to_value(HandoverDecision::Suspend).unwrap();
        assert_eq!(suspend_json["type"], "suspend");
        let back: HandoverDecision =
            serde_json::from_value(suspend_json).expect("suspend 应可反序列化");
        assert_eq!(back, HandoverDecision::Suspend);
    }

    #[test]
    fn consultation_new_defaults_undecided() {
        let consultation = ModelHandoverConsultation::new("conv_1", FailureClass::RateLimited);
        assert!(consultation.user_decision.is_none());
        assert!(consultation.decided_at.is_none());
        assert!(consultation.scope.is_none());
        assert_eq!(consultation.failure_class, FailureClass::RateLimited);
    }

    #[test]
    fn central_decision_new_fills_audit_fields() {
        let decision = CentralDecision::new("conv_1", "work_1", "evt_1");
        assert!(!decision.id.is_empty());
        assert!(!decision.created_at.is_empty());
        assert_eq!(decision.source_event_id, "evt_1");
    }
}
