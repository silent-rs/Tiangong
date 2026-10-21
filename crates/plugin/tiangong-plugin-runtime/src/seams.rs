//! 能力接缝（Capability Seam）注册骨架。
//!
//! Seam 是「宿主向插件开放的一类扩展点」的契约集合。Seam Hub 登记各插件
//! 在某类 Seam 上的注册项，为通用能力提供统一的注册与查询入口。
//!
//! 本模块只登记「谁注册了什么能力」，不感知具体插件业务、不解析业务负载。

use serde::{Deserialize, Serialize};

/// 接缝类别（设计文档 4.2 首版接缝清单）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeamKind {
    /// 工具：声明并执行 Agent 可调用的工具。
    Tool,
    /// 提示词：向 system prompt 注入段落。
    Prompt,
    /// 生命周期：会话/轮次钩子。
    Lifecycle,
    /// UI：贡献界面到挂载点。
    Ui,
    /// 事件：订阅宿主事件流。
    Event,
    /// 存储：数据目录与 sidecar。
    Storage,
}

/// 单个接缝注册项：某插件在某类接缝上的一个参与声明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeamRegistration {
    /// 接缝类别。
    pub kind: SeamKind,
    /// 注册方插件 ID。
    pub plugin_id: String,
    /// 注册项标识（如 UI 贡献 ID、工具名、处理器名），插件内唯一。
    pub key: String,
}

/// Seam Hub：按接缝类别聚合的注册表骨架。
///
/// `register` / `lookup` 已可用；`route` 是定型占位接口，具体路由策略
/// （审批路由表、交互处理器接管等）由后续任务实现。
#[derive(Debug, Clone, Default)]
pub struct SeamHub {
    registrations: Vec<SeamRegistration>,
}

impl SeamHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个接缝注册项。同 `(kind, plugin_id, key)` 重复注册时后者覆盖前者，
    /// 支持热加载场景下以最新声明刷新。
    pub fn register(&mut self, registration: SeamRegistration) {
        self.retain_registration(&registration);
        self.registrations.push(registration);
    }

    /// 撤销某插件在某类接缝上的全部注册项（卸载/禁用时回滚）。
    pub fn unregister_plugin(&mut self, kind: SeamKind, plugin_id: &str) {
        self.registrations
            .retain(|item| !(item.kind == kind && item.plugin_id == plugin_id));
    }

    /// 查询某类接缝的全部注册项。
    pub fn lookup(&self, kind: SeamKind) -> Vec<&SeamRegistration> {
        self.registrations
            .iter()
            .filter(|item| item.kind == kind)
            .collect()
    }

    /// 把一条请求路由到该接缝的处理器。
    ///
    /// 接口契约：`payload` 为不透明序列化负载（宿主不解析），返回处理结果。
    /// 当前为占位实现，具体路由策略按通用能力逐步接入。
    pub fn route(&self, kind: SeamKind, payload: &str) -> anyhow::Result<String> {
        let _ = payload;
        anyhow::bail!("接缝 {:?} 的路由尚未实现：当前版本仅提供注册与查询", kind)
    }

    fn retain_registration(&mut self, registration: &SeamRegistration) {
        self.registrations.retain(|item| item != registration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_lookup_往返一致() {
        let mut hub = SeamHub::new();
        hub.register(SeamRegistration {
            kind: SeamKind::Tool,
            plugin_id: "com.example.tools".to_string(),
            key: "example-tool".to_string(),
        });
        hub.register(SeamRegistration {
            kind: SeamKind::Ui,
            plugin_id: "com.example.board".to_string(),
            key: "board-tab".to_string(),
        });

        let tools = hub.lookup(SeamKind::Tool);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].plugin_id, "com.example.tools");
    }

    #[test]
    fn 重复注册覆盖且卸载回滚() {
        let mut hub = SeamHub::new();
        for key in ["v1", "v2"] {
            hub.register(SeamRegistration {
                kind: SeamKind::Ui,
                plugin_id: "com.example.board".to_string(),
                key: key.to_string(),
            });
        }
        assert_eq!(hub.lookup(SeamKind::Ui).len(), 2);

        // 同 (kind, plugin_id, key) 重复注册刷新而非追加
        hub.register(SeamRegistration {
            kind: SeamKind::Ui,
            plugin_id: "com.example.board".to_string(),
            key: "v1".to_string(),
        });
        assert_eq!(hub.lookup(SeamKind::Ui).len(), 2);

        hub.unregister_plugin(SeamKind::Ui, "com.example.board");
        assert!(hub.lookup(SeamKind::Ui).is_empty());
    }

    #[test]
    fn route_占位返回未实现() {
        let hub = SeamHub::new();
        let error = hub.route(SeamKind::Tool, "{}").unwrap_err();
        assert!(error.to_string().contains("尚未实现"));
    }

    #[test]
    fn seam_kind_序列化为_snake_case() {
        assert_eq!(
            serde_json::to_string(&SeamKind::Event).unwrap(),
            "\"event\""
        );
        let parsed: SeamKind = serde_json::from_str("\"tool\"").unwrap();
        assert_eq!(parsed, SeamKind::Tool);
    }
}
