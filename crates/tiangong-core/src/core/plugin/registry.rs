use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::config::core::CoreConfig;
use crate::permission::TrustMode;
use crate::session::Session;
use crate::tools::extension::{RenamedToolHandler, ToolOverrideHandler, namespace_tool_specs};
use tiangong_llm::tool::ToolSpec;

use super::{Plugin, injection_tool_spec};

pub(crate) struct PreparedPlugins {
    pub plugins: Vec<Arc<dyn Plugin>>,
    pub tools: Vec<ToolSpec>,
    pub tool_overrides: HashMap<String, Arc<dyn ToolOverrideHandler>>,
}

/// 每轮 turn 构建上下文时调用：排序注入生命周期钩子并收集工具声明。
///
/// core 不做任何声明稳定化处理——运行期稳定由 runtime 适配器的冻结
/// 快照保证；工具顺序即插件输出顺序，core 不代为排序。声明读取失败
/// 由适配器降级为空列表，core 不感知。
pub(crate) fn prepare_plugins(
    plugins: &[Arc<dyn Plugin>],
    config: &CoreConfig,
    trust_mode: TrustMode,
    session: &Session,
) -> PreparedPlugins {
    // 提示与工具共用固定顺序，避免加载顺序变化改写请求前缀。
    let mut sorted: Vec<Arc<dyn Plugin>> = plugins.to_vec();
    sorted.sort_by(|left, right| {
        (left.id() != "prompt")
            .cmp(&(right.id() != "prompt"))
            .then_with(|| left.id().cmp(right.id()))
    });

    let plugins = sorted.as_slice();
    let workspace_path = std::path::Path::new(&session.cwd);
    let workspace = workspace_path.is_dir().then_some(workspace_path);

    // 先汇总 exec_env，使依赖 sidecar 的插件能在首次生命周期调用触发启动前
    // 把受控环境注入 sidecar 进程。
    let mut exec_env = BTreeMap::new();
    for plugin in plugins {
        for (key, value) in plugin.exec_env() {
            exec_env.insert(key, value);
        }
    }
    for plugin in plugins {
        plugin.set_exec_env(exec_env.clone());
    }
    for plugin in plugins {
        plugin.on_config_updated(config);
        plugin.set_execution_context(workspace, trust_mode);
    }

    let mut tools = vec![injection_tool_spec()];
    let mut tool_overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    // 插件工具一律以 `{插件id}__{工具名}` 暴露（自行命名的聚合桥除外），
    // 调用时还原原名转发。插件 id 唯一，正常不会冲突；万一仍冲突保留先注册者。
    for plugin in plugins {
        let specs = plugin.tool_specs();
        let named: Vec<(String, ToolSpec)> = if plugin.names_own_tools() {
            specs
                .into_iter()
                .map(|spec| (spec.name.clone(), spec))
                .collect()
        } else {
            namespace_tool_specs(plugin.id(), specs)
        };
        for (original, spec) in named {
            if tool_overrides.contains_key(&spec.name) {
                tracing::warn!(tool = %spec.name, plugin = %plugin.id(), "工具名冲突，保留先注册者");
                continue;
            }
            let handler: Arc<dyn ToolOverrideHandler> = plugin.clone();
            let handler = if spec.name == original {
                handler
            } else {
                Arc::new(RenamedToolHandler::new(handler, original))
            };
            tool_overrides.insert(spec.name.clone(), handler);
            tools.push(spec);
        }
    }
    PreparedPlugins {
        plugins: sorted,
        tools,
        tool_overrides,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::extension::{PromptSectionProvider, ToolOverrideHandler};

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    struct OrderedPlugin {
        id: String,
        names: &'static [&'static str],
        own: bool,
    }
    impl Plugin for OrderedPlugin {
        fn id(&self) -> &str {
            &self.id
        }
        fn names_own_tools(&self) -> bool {
            self.own
        }
    }
    impl crate::tools::extension::ToolSpecProvider for OrderedPlugin {
        fn tool_specs(&self) -> Vec<ToolSpec> {
            self.names.iter().map(|name| tool(name)).collect()
        }
    }
    impl PromptSectionProvider for OrderedPlugin {}
    impl ToolOverrideHandler for OrderedPlugin {}

    /// 顺序语义锁定：tools 顺序 = 内置注入工具 + 插件 id 字典序（prompt
    /// 置顶）+ 插件自身输出序（core 不排序）；插件工具一律按插件前缀暴露。
    #[test]
    fn prepare_keeps_plugin_order_and_namespaces_all_tools() {
        let marker = format!("order-{}", line!());
        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(OrderedPlugin {
                id: format!("{marker}-zeta"),
                names: &["z_b_first", "a_second"],
                own: false,
            }),
            Arc::new(OrderedPlugin {
                id: format!("{marker}-alpha"),
                names: &["alpha_tool", "z_b_first", "alpha_tool"],
                own: false,
            }),
            Arc::new(OrderedPlugin {
                id: format!("{marker}-bridge"),
                names: &["call_local_plugin"],
                own: true,
            }),
        ];
        let session = Session::new("顺序");
        let prepared = prepare_plugins(
            &plugins,
            &CoreConfig::default(),
            TrustMode::default(),
            &session,
        );
        let names: Vec<&str> = prepared.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "plugin_injection".to_string(),
                format!("{marker}-alpha__alpha_tool"),
                format!("{marker}-alpha__z_b_first"),
                "call_local_plugin".to_string(),
                format!("{marker}-zeta__z_b_first"),
                format!("{marker}-zeta__a_second"),
            ],
            "tools 顺序应为：内置注入工具 + 插件 id 序 + 插件输出序，插件工具全部加前缀，自行命名的插件原样"
        );
        assert!(
            prepared
                .tool_overrides
                .contains_key(&format!("{marker}-zeta__a_second")),
            "加前缀的工具应能路由到原插件"
        );
        assert!(
            !prepared.tool_overrides.contains_key("a_second"),
            "原名不再暴露"
        );
    }
}
