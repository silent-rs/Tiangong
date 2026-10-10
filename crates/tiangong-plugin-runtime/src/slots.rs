//! UI 挂载点（Slot）协议。
//!
//! Slot 是宿主与插件之间的稳定锚点契约：插件在 manifest 中声明把 UI 贡献挂到
//! 哪个 Slot，宿主在对应位置创建标准容器渲染。Slot ID 一经发布即为稳定契约，
//! 新增走版本化公告，删除/改名需保留兼容别名。
//!
//! 本模块只登记「宿主开放了哪些挂载位置」这一通用协议，不感知具体插件。

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Slot 的实例策略：单实例 Slot 全局至多挂载一个贡献，多实例 Slot 可并存多个。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotInstances {
    Singleton,
    Multiple,
}

/// Slot 可注入的上下文键（贡献声明的 `context` 取值范围）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotContextKey {
    Session,
    Turn,
    Message,
    Workspace,
}

impl SlotContextKey {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Turn => "turn",
            Self::Message => "message",
            Self::Workspace => "workspace",
        }
    }
}

/// 单个 Slot 的元数据。
#[derive(Debug, Clone)]
pub struct SlotDescriptor {
    /// 点分层级的稳定 ID，如 `extension.tab`。
    pub id: &'static str,
    /// 实例策略。
    pub instances: SlotInstances,
    /// 可注入的上下文键。
    pub context: &'static [SlotContextKey],
    /// 用途说明（供错误信息与文档展示）。
    pub description: &'static str,
}

/// 首版 Slot 目录（设计文档 6.1）。
///
/// `open_mode` 仅对 `extension.tab` 有意义，由贡献声明与清单校验约束，
/// Slot 本身不承载打开模式。
pub const BUILTIN_SLOTS: &[SlotDescriptor] = &[
    // ── 会话区（session）──
    SlotDescriptor {
        id: "session.turn-node",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session, SlotContextKey::Turn],
        description: "消息流中插入的独立节点（卡片/进度条/结果块）",
    },
    SlotDescriptor {
        id: "session.message-item",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session, SlotContextKey::Message],
        description: "每条消息的附加区（附件渲染、结构化卡片）；声明 render=replace 时按消息渲染声明接管整条消息的显示",
    },
    SlotDescriptor {
        id: "session.message-action",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session, SlotContextKey::Message],
        description: "消息操作按钮区（复制/重试旁的动作按钮）",
    },
    SlotDescriptor {
        id: "session.input-action",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session],
        description: "输入框动作按钮区（附件、语音等按钮旁）",
    },
    SlotDescriptor {
        id: "session.before-input",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session],
        description: "输入框上方（上下文提示、快捷操作条）",
    },
    SlotDescriptor {
        id: "session.after-input",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session],
        description: "输入框下方（附加输入辅助区）",
    },
    SlotDescriptor {
        id: "session.input-status",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session],
        description: "输入区状态行内嵌组件（如分支指示器，随会话上下文变化）",
    },
    SlotDescriptor {
        id: "session.interaction",
        instances: SlotInstances::Singleton,
        context: &[SlotContextKey::Session],
        description: "审批、确认、选择和输入请求的交互处理器界面",
    },
    SlotDescriptor {
        id: "session.input-overlay",
        instances: SlotInstances::Singleton,
        context: &[SlotContextKey::Session],
        description: "按需覆盖输入区的输入模式界面（如按住说话），由插件申请显示与归还",
    },
    SlotDescriptor {
        id: "session.empty-state",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session, SlotContextKey::Workspace],
        description: "空会话占位（自定义新会话引导）",
    },
    // ── 拓展区（extension）──
    SlotDescriptor {
        id: "extension.tab",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session, SlotContextKey::Workspace],
        description: "拓展区新增标签页（App），按 open_mode 决定单例/多 tab",
    },
    SlotDescriptor {
        id: "extension.side",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Session],
        description: "拓展区内部的辅助侧栏",
    },
    // ── 侧边栏（sidebar）──
    SlotDescriptor {
        id: "sidebar.nav-item",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Workspace],
        description: "侧边栏导航项",
    },
    SlotDescriptor {
        id: "sidebar.panel",
        instances: SlotInstances::Singleton,
        context: &[SlotContextKey::Workspace],
        description: "全高度侧边栏面板",
    },
    SlotDescriptor {
        id: "sidebar.bottom",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Workspace],
        description: "侧边栏底部（状态/快捷入口）",
    },
    // ── 设置区（settings）──
    SlotDescriptor {
        id: "settings.plugin-page",
        instances: SlotInstances::Multiple,
        context: &[],
        description: "设置中的插件页（等价旧 contributions，平滑迁移）",
    },
    // ── 全局（global）──
    SlotDescriptor {
        id: "global.status-item",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Workspace],
        description: "状态栏项",
    },
    SlotDescriptor {
        id: "global.command",
        instances: SlotInstances::Multiple,
        context: &[SlotContextKey::Workspace],
        description: "命令面板项",
    },
    SlotDescriptor {
        id: "global.toast-action",
        instances: SlotInstances::Multiple,
        context: &[],
        description: "通知上的动作按钮",
    },
];

/// Slot 注册表：登记合法 Slot 并提供查询与校验。
///
/// 默认装载 [`BUILTIN_SLOTS`]；宿主可通过 [`SlotRegistry::register`] 追加
/// 自定义 Slot（版本化扩展），但 ID 前缀必须与已有分组一致，避免与未来
/// 官方 Slot 冲突。
#[derive(Debug, Clone, Default)]
pub struct SlotRegistry {
    slots: BTreeMap<&'static str, SlotDescriptor>,
}

impl SlotRegistry {
    /// 装载首版内置 Slot 目录。
    pub fn builtin() -> Self {
        Self {
            slots: BUILTIN_SLOTS
                .iter()
                .map(|slot| (slot.id, slot.clone()))
                .collect(),
        }
    }

    /// 追加自定义 Slot。ID 重复或前缀不属于已知分组时拒绝。
    pub fn register(&mut self, descriptor: SlotDescriptor) -> Result<()> {
        let group = descriptor
            .id
            .split_once('.')
            .map(|(group, _)| group)
            .unwrap_or_default();
        if !self
            .slots
            .keys()
            .any(|id| id.starts_with(&format!("{group}.")))
        {
            bail!(
                "Slot {} 的分组 {group} 未登记，不允许新增未知分组（避免与未来官方 Slot 冲突）",
                descriptor.id
            );
        }
        if self.slots.contains_key(descriptor.id) {
            bail!("Slot {} 已登记，不允许重复注册", descriptor.id);
        }
        self.slots.insert(descriptor.id, descriptor);
        Ok(())
    }

    /// 是否为合法 Slot ID。
    pub fn is_valid(&self, id: &str) -> bool {
        self.slots.contains_key(id)
    }

    /// 校验 Slot ID 合法性，未知 ID 返回明确错误。
    pub fn validate(&self, id: &str) -> Result<&SlotDescriptor> {
        self.slots.get(id).ok_or_else(|| {
            anyhow::anyhow!("未知挂载点 {id}，当前天工版本不支持该 Slot（可能需要升级应用）")
        })
    }

    /// 按分组前缀查询（如 `session.`、`extension.`、`sidebar.`、`settings.`、`global.`）。
    pub fn by_prefix(&self, prefix: &str) -> Vec<&SlotDescriptor> {
        self.slots
            .values()
            .filter(|slot| slot.id.starts_with(prefix))
            .collect()
    }

    /// 全部已登记 Slot。
    pub fn list(&self) -> Vec<&SlotDescriptor> {
        self.slots.values().collect()
    }
}

/// 插件声明挂到某个 Slot 的 UI 贡献（manifest `ui.contributions[]`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContribution {
    /// 目标挂载点，必须是 Slot Registry 登记的合法 ID。
    pub slot: String,
    /// 贡献 ID，插件内唯一。
    pub id: String,
    /// 展示标题。
    pub title: String,
    /// 用途说明（矩阵卡片等展示位）。
    #[serde(default)]
    pub description: String,
    /// 图标名或内联 SVG。
    #[serde(default)]
    pub icon: String,
    /// 入口 HTML（相对插件目录）。
    pub entry: String,
    /// 打开模式，仅对 `extension.tab` 生效，缺省 `singleton`。
    #[serde(default)]
    pub open_mode: OpenMode,
    /// 需要注入的上下文键（必须是目标 Slot 声明支持的键）。
    #[serde(default)]
    pub context: Vec<String>,
    /// 沙箱级别，缺省 `shadow`；`native` 需官方签名。
    #[serde(default)]
    pub sandbox: SandboxKind,
    /// 实例是否持有后端资源（宿主接管实例生命周期，见 manifest 同名字段）。
    #[serde(default)]
    pub instance_resources: bool,
    /// 渲染方式（仅 `session.message-item`）：缺省为附加区，`replace` 时只在
    /// 消息的渲染声明指向本贡献时挂载，并替换该消息的默认显示。
    #[serde(default, skip_serializing_if = "MessageItemRender::is_append")]
    pub render: MessageItemRender,
}

/// `session.message-item` 贡献的渲染方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageItemRender {
    /// 附加区：挂载在每条消息下方（默认）。
    #[default]
    Append,
    /// 替换：仅在消息 `meta.render` 指向本贡献时挂载，替换默认显示。
    Replace,
}

impl MessageItemRender {
    pub fn is_append(&self) -> bool {
        matches!(self, Self::Append)
    }
}

/// `render` 仅生效的 Slot。
pub const MESSAGE_ITEM_SLOT: &str = "session.message-item";

/// App 打开模式。仅对 `extension.tab` 生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenMode {
    /// 单例 tab：全局至多一个实例，重复打开聚焦已有。
    #[default]
    Singleton,
    /// 多 tab：每次打开新建实例。
    Multi,
}

/// UI 贡献的沙箱级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// Shadow DOM 挂载主 DOM 树（默认）。
    #[default]
    Shadow,
    /// iframe 强隔离。
    Iframe,
    /// 原生容器（仅官方签名插件）。
    Native,
    /// webview 容器原语：宿主提供的声明式 webview 实例（创建/导航/eval/
    /// 事件），插件在其上构建浏览器类能力（通用原语，非浏览器业务）。
    Webview,
}

/// `open_mode` 仅生效的 Slot。
pub const OPEN_MODE_SLOT: &str = "extension.tab";

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SlotRegistry {
        SlotRegistry::builtin()
    }

    #[test]
    fn 合法_slot_通过校验() {
        let registry = registry();
        for id in [
            "session.turn-node",
            "extension.tab",
            "sidebar.panel",
            "settings.plugin-page",
            "global.command",
        ] {
            let descriptor = registry.validate(id).expect("合法 Slot 应通过");
            assert_eq!(descriptor.id, id);
        }
    }

    #[test]
    fn 非法_slot_明确报错() {
        let error = registry().validate("session.unknown").unwrap_err();
        assert!(error.to_string().contains("未知挂载点"));
        assert!(error.to_string().contains("session.unknown"));

        assert!(!registry().is_valid(""));
        assert!(!registry().is_valid("extension"));
        assert!(!registry().is_valid("nope.tab"));
    }

    #[test]
    fn 前缀查询覆盖完整分组() {
        let registry = registry();
        let session_ids: Vec<&str> = registry
            .by_prefix("session.")
            .into_iter()
            .map(|slot| slot.id)
            .collect();
        // BTreeMap 按字典序返回
        assert_eq!(
            session_ids,
            vec![
                "session.after-input",
                "session.before-input",
                "session.empty-state",
                "session.input-action",
                "session.input-overlay",
                "session.input-status",
                "session.interaction",
                "session.message-action",
                "session.message-item",
                "session.turn-node",
            ]
        );
        assert_eq!(registry.by_prefix("extension.").len(), 2);
        assert_eq!(registry.by_prefix("sidebar.").len(), 3);
        assert_eq!(registry.by_prefix("settings.").len(), 1);
        assert_eq!(registry.by_prefix("global.").len(), 3);
        assert!(registry.by_prefix("unknown.").is_empty());
    }

    #[test]
    fn 注册表拒绝未知分组与重复_slot() {
        let mut registry = registry();
        registry
            .register(SlotDescriptor {
                id: "session.inline-tool",
                instances: SlotInstances::Multiple,
                context: &[SlotContextKey::Session],
                description: "测试用扩展 Slot",
            })
            .expect("已知分组应允许追加");

        assert!(registry.is_valid("session.inline-tool"));

        let duplicate = registry
            .register(SlotDescriptor {
                id: "session.inline-tool",
                instances: SlotInstances::Multiple,
                context: &[],
                description: "重复",
            })
            .unwrap_err();
        assert!(duplicate.to_string().contains("不允许重复注册"));

        let unknown_group = registry
            .register(SlotDescriptor {
                id: "rag.panel",
                instances: SlotInstances::Multiple,
                context: &[],
                description: "未知分组",
            })
            .unwrap_err();
        assert!(unknown_group.to_string().contains("未知分组"));
    }

    #[test]
    fn ui_contribution_序列化为_snake_case() {
        let contribution = UiContribution {
            slot: "extension.tab".to_string(),
            id: "board-tab".to_string(),
            title: "看板".to_string(),
            description: "任务看板面板".to_string(),
            icon: "board".to_string(),
            entry: "index.html".to_string(),
            open_mode: OpenMode::Multi,
            context: vec!["session".to_string()],
            sandbox: SandboxKind::Shadow,
            instance_resources: false,
            render: MessageItemRender::Append,
        };
        let json = serde_json::to_string(&contribution).unwrap();
        assert!(json.contains("\"open_mode\":\"multi\""));
        assert!(json.contains("\"sandbox\":\"shadow\""));
        assert!(json.contains("\"description\":\"任务看板面板\""));

        let parsed: UiContribution = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, contribution);
    }

    #[test]
    fn ui_contribution_缺省值符合协议() {
        let json = r#"{"slot":"settings.plugin-page","id":"p","title":"T","entry":"i.html"}"#;
        let parsed: UiContribution = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.open_mode, OpenMode::Singleton);
        assert_eq!(parsed.sandbox, SandboxKind::Shadow);
        assert!(parsed.context.is_empty());
    }
}
